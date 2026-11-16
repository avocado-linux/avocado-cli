//! Docker volume management utilities for Avocado CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command as AsyncCommand;
use uuid::Uuid;

use crate::utils::output::{print_info, OutputLevel};

/// Volume state configuration stored in .avocado-state file
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeState {
    /// The docker volume name
    pub volume_name: String,
    /// Original source path where avocado config was located
    pub source_path: String,
    /// Container tool being used (docker/podman)
    pub container_tool: String,
}

/// What `volume rm` or `volume inspect` says about a volume that does not exist. Docker prints
/// `get <name>: no such volume` (older daemons: `No such volume: <name>`),
/// podman prints `no volume with name "<name>" found: no such volume`.
fn volume_already_gone(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("no such volume")
}

/// What `volume rm` says when a container still holds the volume. Docker
/// prints `remove <name>: volume is in use - [<id>]`, podman prints
/// `volume is being used`.
fn volume_in_use(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("volume is in use") || stderr.contains("volume is being used")
}

/// How long `clean` waits for removed containers to release the volume.
/// A `--rm` container that was killed stays `Dead` and holds the volume
/// until the daemon finishes its removal, which takes about a second.
const VOLUME_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Time between `volume rm` tries while the volume is still in use.
const VOLUME_RELEASE_POLL: Duration = Duration::from_millis(250);

impl VolumeState {
    /// Create a new volume state with a generated UUID-based name
    pub fn new(source_path: PathBuf, container_tool: String) -> Self {
        let uuid = Uuid::new_v4();
        let volume_name = format!("avo-{uuid}");

        Self {
            volume_name,
            source_path: source_path.to_string_lossy().to_string(),
            container_tool,
        }
    }

    /// Load volume state from .avocado-state file in the given directory
    pub fn load_from_dir(dir_path: &Path) -> Result<Option<Self>> {
        let state_file = dir_path.join(".avocado-state");

        if !state_file.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&state_file).with_context(|| {
            format!("Failed to read volume state file: {}", state_file.display())
        })?;

        let state: VolumeState =
            serde_json::from_str(&content).with_context(|| "Failed to parse volume state file")?;

        Ok(Some(state))
    }

    /// Load the volume state from the process working directory.
    ///
    /// `SdkContainer` keys the volume on its `cwd`, so install, build and the
    /// stamps code find `.avocado-state` in the working directory, not in
    /// `src_dir`. Not every reader agrees: runtime provision keys its state
    /// file copy on `src_dir`, and `profiles`, `save` and `load` read from the
    /// config file's directory.
    pub fn load_from_current_dir() -> Result<Option<Self>> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_from_dir(&cwd)
    }

    /// Save volume state to .avocado-state file in the given directory
    pub fn save_to_dir(&self, dir_path: &Path) -> Result<()> {
        let state_file = dir_path.join(".avocado-state");

        let content = serde_json::to_string_pretty(self)
            .with_context(|| "Failed to serialize volume state")?;

        fs::write(&state_file, content).with_context(|| {
            format!(
                "Failed to write volume state file: {}",
                state_file.display()
            )
        })?;

        Ok(())
    }
}

/// Decide whether an `avo-<uuid>` volume is abandoned, given the
/// `avocado.source_path` label recorded on the volume when it was created.
///
/// Returns `Some(reason)` when the volume is a stale leftover safe to
/// reap, or `None` when it is still bound to a live project. Shared by the
/// init-time reaper and `avocado prune` so both agree on what "abandoned"
/// means.
///
/// When the project's state file names a different volume, this one is
/// stale only if that volume exists on the daemon `container_tool` talks
/// to. If it is not here it can be on another daemon, so this one is kept;
/// any other inspect failure keeps it too.
pub async fn avo_abandonment_reason(
    container_tool: &str,
    volume_name: &str,
    source_path: Option<&str>,
) -> Option<String> {
    let source_path = match source_path {
        Some(path) => path,
        None => return Some("no source_path label found".to_string()),
    };

    let source_dir = Path::new(source_path);
    if !source_dir.exists() {
        return Some(format!("source directory '{source_path}' does not exist"));
    }

    let state_file = source_dir.join(".avocado-state");
    if !state_file.exists() {
        return Some(format!("no .avocado-state file in '{source_path}'"));
    }

    match VolumeState::load_from_dir(source_dir) {
        Ok(Some(state)) if state.volume_name == volume_name => None,
        Ok(Some(state)) => match volume_exists_on(container_tool, &state.volume_name).await {
            Ok(true) => Some(format!(
                ".avocado-state links to '{}', not this volume",
                state.volume_name
            )),
            Ok(false) | Err(_) => None,
        },
        Ok(None) => Some(format!("could not read .avocado-state in '{source_path}'")),
        Err(e) => Some(format!("error reading .avocado-state: {e}")),
    }
}

/// Whether `volume_name` exists on the daemon `container_tool` talks to.
async fn volume_exists_on(container_tool: &str, volume_name: &str) -> Result<bool> {
    let output = AsyncCommand::new(container_tool)
        .args(["volume", "inspect", volume_name])
        .output()
        .await
        .with_context(|| "Failed to check if volume exists")?;

    inspect_outcome(
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
        volume_name,
    )
}

/// The daemon a missing volume was looked for in, as the user knows it.
/// Under VM routing the CLI set `DOCKER_HOST` itself, so its socket path
/// means nothing to the user.
fn daemon_description(docker_host: Option<String>, vm_routing: bool) -> String {
    if vm_routing {
        return "the avocado-vm".to_string();
    }
    match docker_host {
        Some(host) => format!("the docker daemon at DOCKER_HOST={host}"),
        None => "the local docker daemon".to_string(),
    }
}

fn missing_volume_message(volume_name: &str, daemon: &str) -> String {
    format!(
        "Build volume '{volume_name}' recorded in .avocado-state was not found in {daemon}. \
         Either it was deleted (for example by `avocado vm reset` or a Docker data purge), \
         or it is on a different docker daemon. Run `avocado clean` to start this project \
         over, or switch back to the daemon that has the volume."
    )
}

/// Docker volume manager for Avocado operations
pub struct VolumeManager {
    container_tool: String,
    verbose: bool,
}

impl VolumeManager {
    /// Create a new volume manager
    pub fn new(container_tool: String, verbose: bool) -> Self {
        Self {
            container_tool,
            verbose,
        }
    }

    /// Get or create a docker volume for the given source directory
    pub async fn get_or_create_volume(&self, source_dir: &Path) -> Result<VolumeState> {
        // Try to load existing volume state
        if let Some(existing_state) = VolumeState::load_from_dir(source_dir)? {
            // Verify the volume still exists
            if self.volume_exists(&existing_state.volume_name).await? {
                if self.verbose {
                    print_info(
                        &format!("Using existing volume: {}", existing_state.volume_name),
                        OutputLevel::Normal,
                    );
                }
                return Ok(existing_state);
            }
            let daemon = daemon_description(
                std::env::var_os("DOCKER_HOST").map(|host| host.to_string_lossy().into_owned()),
                crate::utils::container::is_vm_routing_active(),
            );
            anyhow::bail!(missing_volume_message(&existing_state.volume_name, &daemon));
        }

        // We're about to mint a fresh volume for this project. That's the
        // natural moment to sweep volumes left behind by projects deleted
        // from disk without `avocado clean`: a manual `rm -rf` drops the
        // project folder but not its per-project `avo-<uuid>` volume, so
        // they otherwise accumulate forever. Best-effort by design: a
        // container-tool hiccup must never block volume creation.
        let reaped = self.reap_abandoned_avo_volumes().await;
        if !reaped.is_empty() && self.verbose {
            print_info(
                &format!("Reaped {} abandoned build volume(s).", reaped.len()),
                OutputLevel::Normal,
            );
        }

        // Create new volume state
        let state = VolumeState::new(source_dir.to_path_buf(), self.container_tool.clone());

        // Record state before creation so a concurrent reaper cannot see an
        // unreferenced volume. On create failure, roll back only state that
        // still names this volume.
        state.save_to_dir(source_dir)?;

        // Create the docker volume with metadata
        if let Err(error) = self.create_volume(&state).await {
            if let Ok(Some(recorded)) = VolumeState::load_from_dir(source_dir) {
                if recorded.volume_name == state.volume_name {
                    let _ = fs::remove_file(source_dir.join(".avocado-state"));
                }
            }
            return Err(error);
        }

        if self.verbose {
            print_info(
                &format!("Created new volume: {}", state.volume_name),
                OutputLevel::Normal,
            );
        }

        Ok(state)
    }

    /// Best-effort removal of abandoned `avo-*` volumes left behind by
    /// projects deleted from disk without `avocado clean`. Each `avo-*`
    /// volume is classified with [`avo_abandonment_reason`] and removed
    /// when abandoned. Any container-tool failure (daemon down, volume
    /// still held by a container, inspect error) is swallowed or skipped so
    /// this never blocks the caller, and a failed inspect is skipped rather
    /// than reaped. Returns the names of the volumes actually removed.
    pub async fn reap_abandoned_avo_volumes(&self) -> Vec<String> {
        let names = match self.list_avo_volumes().await {
            Ok(names) => names,
            Err(_) => return Vec::new(),
        };

        let mut removed = Vec::new();
        for name in names {
            // Skip on inspect failure: a transient docker hiccup must never
            // be read as "no source_path label", which would classify an
            // active project's volume as abandoned and delete it.
            let source_path = match self.inspect_source_path_label(&name).await {
                Ok(source_path) => source_path,
                Err(_) => continue,
            };
            if let Some(reason) =
                avo_abandonment_reason(&self.container_tool, &name, source_path.as_deref()).await
            {
                if self.remove_volume(&name).await.is_ok() {
                    if self.verbose {
                        print_info(
                            &format!("Reaped abandoned build volume '{name}': {reason}"),
                            OutputLevel::Normal,
                        );
                    }
                    removed.push(name);
                }
            }
        }
        removed
    }

    /// List every `avo-*` docker volume name.
    async fn list_avo_volumes(&self) -> Result<Vec<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "ls", "--format", "{{.Name}}"])
            .output()
            .await
            .with_context(|| "Failed to list docker volumes")?;

        if !output.status.success() {
            anyhow::bail!("Failed to list docker volumes");
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("avo-"))
            .map(|s| s.to_string())
            .collect())
    }

    /// Read the `avocado.source_path` label off a volume.
    ///
    /// Returns `Ok(Some(path))` when the label is present, `Ok(None)` when
    /// the volume exists but carries no such label, and `Err` when the
    /// inspect itself failed (daemon down, timeout, unparseable output).
    /// The tri-state is load-bearing for the reap: a failed inspect must
    /// NOT collapse to "no label", or a transient hiccup would classify an
    /// active project's volume as abandoned and delete it. The caller skips
    /// the volume on `Err`.
    async fn inspect_source_path_label(&self, volume_name: &str) -> Result<Option<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "inspect", volume_name])
            .output()
            .await
            .with_context(|| format!("Failed to inspect volume {volume_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to inspect volume {volume_name}: {}", stderr.trim());
        }

        let infos: Vec<VolumeInfo> = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| "Failed to parse volume inspect output")?;
        Ok(infos
            .into_iter()
            .next()
            .and_then(|info| info.labels)
            .and_then(|labels| labels.get("avocado.source_path").cloned()))
    }

    /// Check if a docker volume exists
    async fn volume_exists(&self, volume_name: &str) -> Result<bool> {
        volume_exists_on(&self.container_tool, volume_name).await
    }

    /// Create a docker volume with metadata
    async fn create_volume(&self, state: &VolumeState) -> Result<()> {
        let mut cmd = AsyncCommand::new(&self.container_tool);
        cmd.args(["volume", "create"]);

        // Add label with source path metadata
        cmd.args([
            "--label",
            &format!("avocado.source_path={}", state.source_path),
        ]);

        cmd.arg(&state.volume_name);

        let output = cmd
            .output()
            .await
            .with_context(|| "Failed to create docker volume")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create volume {}: {}", state.volume_name, stderr);
        }

        Ok(())
    }

    /// Remove a docker volume. A volume that is already gone counts as
    /// removed: `.avocado-state` can outlive its volume (a VM reset, a
    /// manual `volume rm`), and the state `clean` wants is "no volume".
    pub async fn remove_volume(&self, volume_name: &str) -> Result<()> {
        match self.try_remove_volume(volume_name).await? {
            None => Ok(()),
            Some(stderr) => anyhow::bail!("Failed to remove volume {volume_name}: {stderr}"),
        }
    }

    /// Run `volume rm` one time. Returns `None` when the volume is gone
    /// (removed now, or not there), and the daemon's stderr when it is not.
    async fn try_remove_volume(&self, volume_name: &str) -> Result<Option<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "rm", volume_name])
            .output()
            .await
            .with_context(|| "Failed to remove docker volume")?;

        if output.status.success() {
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if volume_already_gone(&stderr) {
            return Ok(None);
        }
        Ok(Some(stderr))
    }

    /// Remove a volume right after its containers were removed. `kill` on a
    /// `--rm` container starts its removal in the daemon, so the `rm -f` that
    /// follows can return before the container is gone. Until then the
    /// container is `Dead` and still holds the volume. Try `volume rm` again
    /// while it says "in use", until `timeout`, then fail with its error.
    ///
    /// `removed` holds the IDs of the containers that were removed. If a
    /// container that is not in `removed` holds the volume, it will not go
    /// away by itself, so fail at once.
    async fn remove_volume_once_released(
        &self,
        volume_name: &str,
        removed: &[String],
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut told = false;
        loop {
            let stderr = match self.try_remove_volume(volume_name).await? {
                None => return Ok(()),
                Some(stderr) => stderr,
            };
            if !volume_in_use(&stderr)
                || Instant::now() >= deadline
                || self.held_by_others(volume_name, removed).await?
            {
                anyhow::bail!("Failed to remove volume {volume_name}: {stderr}");
            }
            if self.verbose && !told {
                print_info(
                    "Waiting for the removed containers to release the volume...",
                    OutputLevel::Normal,
                );
                told = true;
            }
            tokio::time::sleep(VOLUME_RELEASE_POLL).await;
        }
    }

    /// True when a container that is not in `removed` uses the volume.
    async fn held_by_others(&self, volume_name: &str, removed: &[String]) -> Result<bool> {
        let holders = self.get_containers_using_volume(volume_name).await?;
        Ok(holders.iter().any(|id| !removed.contains(id)))
    }

    /// Force remove a docker volume by first stopping and removing all containers using it
    pub async fn force_remove_volume(&self, volume_name: &str) -> Result<()> {
        self.force_remove_volume_within(volume_name, VOLUME_RELEASE_TIMEOUT)
            .await
    }

    async fn force_remove_volume_within(&self, volume_name: &str, timeout: Duration) -> Result<()> {
        // Get containers using this volume
        let containers = self.get_containers_using_volume(volume_name).await?;

        if !containers.is_empty() {
            if self.verbose {
                print_info(
                    &format!(
                        "Found {} container(s) using volume, stopping and removing...",
                        containers.len()
                    ),
                    OutputLevel::Normal,
                );
            }

            for container_id in &containers {
                // Kill the container (faster than stop)
                let _ = AsyncCommand::new(&self.container_tool)
                    .args(["kill", container_id])
                    .output()
                    .await;

                // Remove the container
                let output = AsyncCommand::new(&self.container_tool)
                    .args(["rm", "-f", container_id])
                    .output()
                    .await
                    .with_context(|| format!("Failed to remove container {container_id}"))?;

                if self.verbose && output.status.success() {
                    print_info(
                        &format!(
                            "Removed container: {}",
                            &container_id[..12.min(container_id.len())]
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Now remove the volume. With no containers listed, nothing was
        // removed, so there is nothing to wait for.
        if containers.is_empty() {
            self.remove_volume(volume_name).await
        } else {
            self.remove_volume_once_released(volume_name, &containers, timeout)
                .await
        }
    }

    /// Get list of container IDs using a specific volume
    async fn get_containers_using_volume(&self, volume_name: &str) -> Result<Vec<String>> {
        // Use docker ps with filter to find containers using this volume
        // This includes both running and stopped containers
        let output = AsyncCommand::new(&self.container_tool)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("volume={volume_name}"),
                "--format",
                "{{.ID}}",
            ])
            .output()
            .await
            .with_context(|| "Failed to list containers using volume")?;

        if !output.status.success() {
            // If the command fails, return empty list (volume might not exist)
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let containers: Vec<String> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|s| s.to_string())
            .collect();

        Ok(containers)
    }

    /// Name prefix for VS Code extension explorer containers
    const EXPLORER_CONTAINER_PREFIX: &'static str = "avocado-explorer-";

    /// Get list of VS Code extension explorer containers using a specific volume.
    /// These containers are created by the avocado-devtools VS Code extension
    /// to browse volume contents and can be safely stopped automatically.
    pub async fn get_explorer_containers_using_volume(
        &self,
        volume_name: &str,
    ) -> Result<Vec<String>> {
        // Find containers that:
        // 1. Use the specified volume
        // 2. Have a name matching the explorer pattern (avocado-explorer-*)
        let output = AsyncCommand::new(&self.container_tool)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("volume={volume_name}"),
                "--filter",
                &format!("name={}", Self::EXPLORER_CONTAINER_PREFIX),
                "--format",
                "{{.ID}}\t{{.Names}}",
            ])
            .output()
            .await
            .with_context(|| "Failed to list explorer containers")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let containers: Vec<String> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 2 {
                    let id = parts[0];
                    let name = parts[1];
                    // Double-check the name starts with our prefix
                    if name.starts_with(Self::EXPLORER_CONTAINER_PREFIX) {
                        return Some(id.to_string());
                    }
                }
                None
            })
            .collect();

        Ok(containers)
    }

    /// Stop and remove VS Code extension explorer containers using a specific volume.
    /// Returns the IDs of the containers that were stopped.
    pub async fn stop_explorer_containers(&self, volume_name: &str) -> Result<Vec<String>> {
        let containers = self
            .get_explorer_containers_using_volume(volume_name)
            .await?;

        if containers.is_empty() {
            return Ok(containers);
        }

        if self.verbose {
            print_info(
                &format!(
                    "Found {} VS Code explorer container(s) using volume, stopping...",
                    containers.len()
                ),
                OutputLevel::Normal,
            );
        }

        for container_id in &containers {
            // Stop the container gracefully with short timeout
            let _ = AsyncCommand::new(&self.container_tool)
                // stdio-flags-ok: docker stop timeout in seconds, not a tty
                .args(["stop", "-t", "1", container_id])
                .output()
                .await;

            // Remove the container
            let output = AsyncCommand::new(&self.container_tool)
                .args(["rm", "-f", container_id])
                .output()
                .await
                .with_context(|| format!("Failed to remove explorer container {container_id}"))?;

            if self.verbose && output.status.success() {
                print_info(
                    &format!(
                        "Stopped explorer container: {}",
                        &container_id[..12.min(container_id.len())]
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(containers)
    }

    /// Remove a docker volume, automatically stopping any VS Code explorer containers first.
    /// Unlike force_remove_volume, this only stops known safe containers (explorer containers)
    /// and will still fail if other containers are using the volume.
    pub async fn remove_volume_with_explorer_cleanup(&self, volume_name: &str) -> Result<()> {
        self.remove_volume_with_explorer_cleanup_within(volume_name, VOLUME_RELEASE_TIMEOUT)
            .await
    }

    async fn remove_volume_with_explorer_cleanup_within(
        &self,
        volume_name: &str,
        timeout: Duration,
    ) -> Result<()> {
        // First, stop any VS Code explorer containers that might be using this volume
        let stopped = self.stop_explorer_containers(volume_name).await?;

        if stopped.is_empty() {
            return self.remove_volume(volume_name).await;
        }

        if self.verbose {
            print_info(
                &format!(
                    "Stopped {} VS Code explorer container(s) before volume removal",
                    stopped.len()
                ),
                OutputLevel::Normal,
            );
        }

        self.remove_volume_once_released(volume_name, &stopped, timeout)
            .await
    }
}

/// Information about a docker volume
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeInfo {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "Mountpoint")]
    pub mountpoint: String,
    #[serde(rename = "Labels")]
    pub labels: Option<HashMap<String, String>>,
}

/// A failed inspect means "missing" only when the tool says so; an unreachable
/// daemon must not be reported as a missing volume (that error advises `avocado clean`).
fn inspect_outcome(success: bool, stderr: &str, volume_name: &str) -> Result<bool> {
    if success {
        return Ok(true);
    }
    if volume_already_gone(stderr) {
        return Ok(false);
    }
    anyhow::bail!(
        "Could not inspect build volume '{}': {}",
        volume_name,
        stderr.trim()
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn inspect_outcome_separates_missing_volume_from_daemon_errors() {
        assert!(inspect_outcome(true, "", "avo-x").unwrap());
        for missing in [
            "Error response from daemon: get avo-x: no such volume",
            "Error: no such volume avo-x",
            "Error: No such volume: avo-x",
        ] {
            assert!(
                !inspect_outcome(false, missing, "avo-x").unwrap(),
                "{missing}"
            );
        }
        let err = inspect_outcome(
            false,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
            "avo-x",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Could not inspect build volume 'avo-x'"),
            "{err}"
        );
        assert!(err.contains("Is the docker daemon running?"), "{err}");
    }
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_volume_state_creation() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().to_path_buf();
        let container_tool = "docker".to_string();

        let state = VolumeState::new(source_path.clone(), container_tool.clone());

        assert!(state.volume_name.starts_with("avo-"));
        assert_eq!(state.source_path, source_path.to_string_lossy());
        assert_eq!(state.container_tool, container_tool);
    }

    #[test]
    fn test_volume_state_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().to_path_buf();
        let state = VolumeState::new(source_path.clone(), "docker".to_string());

        // Save state
        state.save_to_dir(temp_dir.path()).unwrap();

        // Load state
        let loaded_state = VolumeState::load_from_dir(temp_dir.path())
            .unwrap()
            .unwrap();

        assert_eq!(state.volume_name, loaded_state.volume_name);
        assert_eq!(state.source_path, loaded_state.source_path);
        assert_eq!(state.container_tool, loaded_state.container_tool);
    }

    #[test]
    fn test_load_nonexistent_state() {
        let temp_dir = TempDir::new().unwrap();
        let result = VolumeState::load_from_dir(temp_dir.path()).unwrap();
        assert!(result.is_none());
    }

    /// Refuse a recorded volume the daemon does not have. The message names
    /// both causes (deleted, or on another daemon) and the daemon as the user
    /// knows it, and the state file is left alone.
    #[cfg(unix)]
    async fn assert_missing_volume_is_refused(docker_host: Option<&str>, daemon: &str) -> String {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        tool.volume_is_gone();
        let manager = tool.manager();
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        let state_file = project.path().join(".avocado-state");
        let original = fs::read(&state_file).unwrap();

        let previous_host = std::env::var_os("DOCKER_HOST");
        match docker_host {
            Some(host) => std::env::set_var("DOCKER_HOST", host),
            None => std::env::remove_var("DOCKER_HOST"),
        }
        let result = manager.get_or_create_volume(project.path()).await;
        match previous_host {
            Some(host) => std::env::set_var("DOCKER_HOST", host),
            None => std::env::remove_var("DOCKER_HOST"),
        }

        assert!(
            result.is_err(),
            "missing recorded volume was replaced: {result:?}"
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains(&state.volume_name), "{error}");
        assert!(
            error.contains(&format!("not found in {daemon}.")),
            "{error}"
        );
        assert!(error.contains("it was deleted"), "{error}");
        assert!(error.contains("`avocado vm reset`"), "{error}");
        assert!(error.contains("on a different docker daemon"), "{error}");
        assert!(error.contains("Run `avocado clean`"), "{error}");
        assert_eq!(fs::read(&state_file).unwrap(), original);
        let calls = fs::read_to_string(tool.dir.path().join("calls")).unwrap();
        assert_eq!(
            calls,
            format!("--version\nvolume inspect {}\n", state.volume_name)
        );
        error
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn missing_recorded_volume_is_refused_on_local_daemon() {
        assert_missing_volume_is_refused(None, "the local docker daemon").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn missing_recorded_volume_is_refused_on_docker_host() {
        let host = "unix:///test/avocado/docker.sock";
        assert_missing_volume_is_refused(
            Some(host),
            &format!("the docker daemon at DOCKER_HOST={host}"),
        )
        .await;
    }

    /// Under VM routing the CLI set `DOCKER_HOST` to its own socket, which
    /// the user never chose: name the avocado-vm instead.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn missing_recorded_volume_under_vm_routing_names_the_avocado_vm() {
        let socket = crate::utils::vm::state::VmPaths::resolve()
            .unwrap()
            .docker_socket();
        let host = format!("unix://{}", socket.display());
        let error = assert_missing_volume_is_refused(Some(&host), "the avocado-vm").await;
        assert!(!error.contains(&host), "{error}");
        assert!(!error.contains("DOCKER_HOST"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_volume_state_mints_and_records_a_volume() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let manager = tool.manager();

        let state = manager.get_or_create_volume(project.path()).await.unwrap();

        let recorded = VolumeState::load_from_dir(project.path()).unwrap().unwrap();
        assert!(state.volume_name.starts_with("avo-"));
        assert_eq!(recorded.volume_name, state.volume_name);
        assert_eq!(recorded.source_path, project.path().to_string_lossy());
        assert_eq!(recorded.container_tool, manager.container_tool);
        let calls = fs::read_to_string(tool.dir.path().join("calls")).unwrap();
        assert_eq!(
            calls,
            format!(
                "--version\nvolume ls --format {{{{.Name}}}}\nvolume create --label avocado.source_path={} {}\n",
                state.source_path, state.volume_name
            )
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_volume_create_rolls_back_state_and_allows_retry() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        tool.fail_next_create();
        let manager = tool.manager();

        let error = manager
            .get_or_create_volume(project.path())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .starts_with("Failed to create volume avo-"));
        assert!(error.to_string().ends_with("daemon unavailable\n"));
        assert!(!project.path().join(".avocado-state").exists());

        let state = manager.get_or_create_volume(project.path()).await.unwrap();

        let recorded = VolumeState::load_from_dir(project.path()).unwrap().unwrap();
        assert_eq!(recorded.volume_name, state.volume_name);
        let calls = fs::read_to_string(tool.dir.path().join("calls")).unwrap();
        let created: Vec<_> = calls
            .lines()
            .filter(|line| line.starts_with("volume create "))
            .map(|line| line.split_whitespace().last().unwrap())
            .collect();
        assert_eq!(created.len(), 2);
        assert_ne!(created[0], created[1]);
        assert_eq!(created[1], state.volume_name);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_volume_create_keeps_replaced_state() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        tool.fail_next_create();
        let replacement = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
        fs::write(
            tool.dir.path().join("replacement_state"),
            &replacement_bytes,
        )
        .unwrap();

        let error = tool
            .manager()
            .get_or_create_volume(project.path())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .starts_with("Failed to create volume avo-"));
        assert!(error.to_string().ends_with("daemon unavailable\n"));
        assert_eq!(
            fs::read(project.path().join(".avocado-state")).unwrap(),
            replacement_bytes
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_volume_state_is_reused_without_creating_a_volume() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        let original = fs::read(project.path().join(".avocado-state")).unwrap();

        let reused = tool
            .manager()
            .get_or_create_volume(project.path())
            .await
            .unwrap();

        assert_eq!(reused.volume_name, state.volume_name);
        assert_eq!(
            fs::read(project.path().join(".avocado-state")).unwrap(),
            original
        );
        let calls = fs::read_to_string(tool.dir.path().join("calls")).unwrap();
        assert_eq!(
            calls,
            format!("--version\nvolume inspect {}\n", state.volume_name)
        );
    }

    /// A tool path that cannot run: these cases decide without a call.
    const NO_TOOL: &str = "/nonexistent/avocado/docker";

    #[tokio::test]
    async fn test_abandonment_reason_active_when_state_matches() {
        // A volume whose recorded source directory still holds a
        // .avocado-state file pointing back at it is live: no reason.
        let temp_dir = TempDir::new().unwrap();
        let state = VolumeState::new(temp_dir.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(temp_dir.path()).unwrap();

        assert_eq!(
            avo_abandonment_reason(NO_TOOL, &state.volume_name, Some(&state.source_path)).await,
            None
        );
    }

    #[tokio::test]
    async fn test_abandonment_reason_no_source_path_label() {
        let reason = avo_abandonment_reason(NO_TOOL, "avo-x", None)
            .await
            .unwrap();
        assert!(reason.contains("no source_path label"));
    }

    #[tokio::test]
    async fn test_abandonment_reason_missing_source_dir() {
        let reason =
            avo_abandonment_reason(NO_TOOL, "avo-x", Some("/nonexistent/avocado/project/xyz"))
                .await
                .unwrap();
        assert!(reason.contains("does not exist"));
    }

    #[tokio::test]
    async fn test_abandonment_reason_no_state_file() {
        // Source dir exists but carries no .avocado-state: abandoned.
        let temp_dir = TempDir::new().unwrap();
        let reason =
            avo_abandonment_reason(NO_TOOL, "avo-x", Some(temp_dir.path().to_str().unwrap()))
                .await
                .unwrap();
        assert!(reason.contains("no .avocado-state"));
    }

    #[tokio::test]
    async fn test_abandonment_reason_invalid_state_file() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join(".avocado-state"), "invalid json").unwrap();

        let reason =
            avo_abandonment_reason(NO_TOOL, "avo-x", Some(temp_dir.path().to_str().unwrap()))
                .await
                .unwrap();
        assert!(reason.contains("error reading .avocado-state"));
    }

    /// `load --force` and similar leave the old volume behind on the same
    /// daemon. The state file names a volume that exists here, so the old
    /// one is stale.
    #[cfg(unix)]
    #[tokio::test]
    async fn volume_replaced_on_the_same_daemon_is_abandoned() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        let manager = tool.manager();

        let reason =
            avo_abandonment_reason(&manager.container_tool, "avo-old", Some(&state.source_path))
                .await;

        let reason = reason.expect("a replaced volume on this daemon must be abandoned");
        assert!(reason.contains(&state.volume_name), "{reason}");
        assert!(tool
            .calls()
            .contains(&format!("volume inspect {}\n", state.volume_name)));
    }

    /// The state file names a volume this daemon does not have. It can be on
    /// another daemon, so this volume may be the one a switch back needs.
    #[cfg(unix)]
    #[tokio::test]
    async fn volume_is_kept_when_the_recorded_volume_is_not_on_this_daemon() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        tool.missing_volume(&state.volume_name);
        let manager = tool.manager();

        assert_eq!(
            avo_abandonment_reason(&manager.container_tool, "avo-old", Some(&state.source_path))
                .await,
            None
        );
    }

    /// A failed inspect of the recorded volume says nothing about it: keep.
    #[cfg(unix)]
    #[tokio::test]
    async fn volume_is_kept_when_the_recorded_volume_cannot_be_inspected() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        tool.inspect_fails(&state.volume_name);
        let manager = tool.manager();

        assert_eq!(
            avo_abandonment_reason(&manager.container_tool, "avo-old", Some(&state.source_path))
                .await,
            None
        );
    }

    /// The init-time reaper removes a volume its project replaced on this
    /// daemon, and keeps the replacement.
    #[cfg(unix)]
    #[tokio::test]
    async fn reaper_removes_a_volume_replaced_on_the_same_daemon() {
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(project.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(project.path()).unwrap();
        tool.listed_volume("avo-old", &state.source_path);
        tool.listed_volume(&state.volume_name, &state.source_path);

        let reaped = tool.manager().reap_abandoned_avo_volumes().await;

        assert_eq!(reaped, vec!["avo-old".to_string()]);
        let calls = tool.calls();
        assert!(calls.contains("volume rm avo-old\n"), "{calls}");
        assert!(
            !calls.contains(&format!("volume rm {}", state.volume_name)),
            "{calls}"
        );
    }

    /// A failed inspect of another project's recorded volume keeps that
    /// project's other volume and does not stop a new project's volume
    /// from being created.
    #[cfg(unix)]
    #[tokio::test]
    async fn reaper_inspect_failure_keeps_the_volume_and_does_not_block_creation() {
        let other = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let tool = FakeTool::new(&[], &[], 0);
        let state = VolumeState::new(other.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(other.path()).unwrap();
        tool.listed_volume("avo-old", &state.source_path);
        tool.inspect_fails(&state.volume_name);

        let created = tool
            .manager()
            .get_or_create_volume(project.path())
            .await
            .unwrap();

        let calls = tool.calls();
        assert!(!calls.contains("volume rm"), "{calls}");
        assert!(
            calls.contains(&format!(
                "volume create --label avocado.source_path={} {}",
                created.source_path, created.volume_name
            )),
            "{calls}"
        );
    }

    /// `clean -f` on a project whose recorded volume is already gone must
    /// not fail: the volume being absent is the result `rm` was asked for.
    /// Any other `rm` failure (in use, daemon down) still stops the clean.
    #[test]
    fn a_missing_volume_counts_as_removed() {
        assert!(volume_already_gone(
            "Error response from daemon: get avo-f49a9283: no such volume\n"
        ));
        assert!(volume_already_gone("Error: No such volume: avo-f49a9283\n"));
        assert!(volume_already_gone(
            "Error: no volume with name \"avo-f49a9283\" found: no such volume\n"
        ));
        assert!(!volume_already_gone(
            "Error response from daemon: remove avo-f49a9283: volume is in use\n"
        ));
        assert!(!volume_already_gone(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock\n"
        ));
    }

    #[test]
    fn a_held_volume_is_in_use() {
        assert!(volume_in_use(
            "Error response from daemon: remove avo-f49a9283: volume is in use - [e122764a8f95]\n"
        ));
        assert!(volume_in_use(
            "Error: volume avo-f49a9283 is being used by the following container(s): e122764a8f95: volume is being used\n"
        ));
        assert!(!volume_in_use(
            "Error response from daemon: get avo-f49a9283: no such volume\n"
        ));
        assert!(!volume_in_use(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock\n"
        ));
    }

    /// A stand-in for the container tool. `ps` prints `holders` (or
    /// `explorers` when the call filters on the explorer prefix). After the
    /// first `volume rm`, `ps` also prints `newcomers`: containers that
    /// start while `clean` runs. `volume rm` fails with "volume is in use"
    /// while `busy` is above zero, and with "no such volume" when `gone`
    /// exists. Every call is appended to `calls`.
    #[cfg(unix)]
    struct FakeTool {
        dir: TempDir,
    }

    #[cfg(unix)]
    impl FakeTool {
        fn new(holders: &[&str], explorers: &[&str], busy: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            let d = dir.path().display();
            let script = format!(
                r#"#!/bin/sh
echo "$*" >> "{d}/calls"
case "$*" in
  "volume create"*)
    if [ -e "{d}/fail_create" ]; then
      rm "{d}/fail_create"
      if [ -e "{d}/replacement_state" ]; then
        cp "{d}/replacement_state" "${{4#avocado.source_path=}}/.avocado-state"
      fi
      echo "daemon unavailable" >&2
      exit 1
    fi
    exit 0 ;;
  "volume ls"*) cat "{d}/volumes" 2>/dev/null; exit 0 ;;
  "volume inspect"*)
    if [ -e "{d}/inspect_error/$3" ]; then echo "Cannot connect to the Docker daemon at unix:///test/docker.sock. Is the docker daemon running?" >&2; exit 1; fi
    if [ -e "{d}/gone" ] || [ -e "{d}/missing/$3" ]; then echo "Error response from daemon: get $3: no such volume" >&2; exit 1; fi
    if [ -e "{d}/labels/$3" ]; then cat "{d}/labels/$3"; fi
    exit 0 ;;
  *"name=avocado-explorer-"*) cat "{d}/explorers"; exit 0 ;;
  ps*)
    cat "{d}/holders"
    if grep -q "^volume rm" "{d}/calls"; then cat "{d}/newcomers"; fi
    exit 0 ;;
  "volume rm"*)
    if [ -e "{d}/gone" ]; then echo "Error response from daemon: get $3: no such volume" >&2; exit 1; fi
    n=$(cat "{d}/busy")
    if [ "$n" -gt 0 ]; then
      echo $((n - 1)) > "{d}/busy"
      echo "Error response from daemon: remove $3: volume is in use - [0123456789ab]" >&2
      exit 1
    fi
    exit 0 ;;
esac
exit 0
"#
            );
            let tool = dir.path().join("docker");
            fs::write(&tool, script).unwrap();
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(dir.path().join("holders"), lines(holders)).unwrap();
            fs::write(dir.path().join("explorers"), lines(explorers)).unwrap();
            fs::write(dir.path().join("newcomers"), "").unwrap();
            fs::write(dir.path().join("busy"), busy.to_string()).unwrap();
            exec_once(&tool);
            Self { dir }
        }

        fn newcomer(&self, id: &str) {
            fs::write(self.dir.path().join("newcomers"), format!("{id}\n")).unwrap();
        }

        fn manager(&self) -> VolumeManager {
            VolumeManager::new(self.dir.path().join("docker").display().to_string(), false)
        }

        fn volume_is_gone(&self) {
            fs::write(self.dir.path().join("gone"), "").unwrap();
        }

        /// List `name` in `volume ls`, labeled with `source_path` as
        /// `volume create` would.
        fn listed_volume(&self, name: &str, source_path: &str) {
            let mut volumes =
                fs::read_to_string(self.dir.path().join("volumes")).unwrap_or_default();
            volumes.push_str(&format!("{name}\n"));
            fs::write(self.dir.path().join("volumes"), volumes).unwrap();
            let labels = self.dir.path().join("labels");
            fs::create_dir_all(&labels).unwrap();
            let info = serde_json::json!([{
                "Name": name,
                "Driver": "local",
                "Mountpoint": format!("/var/lib/docker/volumes/{name}/_data"),
                "Labels": {"avocado.source_path": source_path},
            }]);
            fs::write(labels.join(name), info.to_string()).unwrap();
        }

        /// `volume inspect <name>` says "no such volume".
        fn missing_volume(&self, name: &str) {
            let dir = self.dir.path().join("missing");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), "").unwrap();
        }

        /// `volume inspect <name>` fails as if the daemon were unreachable.
        fn inspect_fails(&self, name: &str) {
            let dir = self.dir.path().join("inspect_error");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), "").unwrap();
        }

        fn calls(&self) -> String {
            fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
        }

        fn fail_next_create(&self) {
            fs::write(self.dir.path().join("fail_create"), "").unwrap();
        }

        fn volume_rm_calls(&self) -> usize {
            fs::read_to_string(self.dir.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("volume rm"))
                .count()
        }
    }

    /// Run the stub one time before a test uses it. Another test thread can
    /// fork while the stub is still open for writing here; until that child
    /// execs and its close-on-exec copy goes away, exec of the stub fails
    /// with ETXTBSY. Retry on exactly that, as `sdk::install` tests do.
    #[cfg(unix)]
    fn exec_once(tool: &Path) {
        const ETXTBSY: i32 = 26;
        let mut cmd = std::process::Command::new(tool);
        cmd.arg("--version");
        for _ in 0..100 {
            match cmd.status() {
                Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                status => {
                    status.unwrap();
                    return;
                }
            }
        }
        cmd.status().unwrap();
    }

    #[cfg(unix)]
    fn lines(items: &[&str]) -> String {
        items.iter().map(|i| format!("{i}\n")).collect()
    }

    /// `clean -f` kills the containers that hold the volume. A killed `--rm`
    /// container stays `Dead` for a moment and still holds the volume, so the
    /// first `volume rm` tries fail with "in use". The removal tries again.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_waits_for_removed_containers_to_release_the_volume() {
        let tool = FakeTool::new(&["aaa111", "bbb222"], &[], 2);
        tool.manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(tool.volume_rm_calls(), 3);
    }

    /// If the volume is still in use when the wait ends, `clean -f` fails
    /// with the daemon's "in use" error, as it did before the wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_fails_when_the_volume_stays_in_use() {
        let tool = FakeTool::new(&["aaa111"], &[], u32::MAX);
        let err = tool
            .manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("volume is in use"), "{err}");
        assert!(tool.volume_rm_calls() > 1);
    }

    /// A removed container that `ps` still lists (`Dead`) is waited for.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_waits_for_a_removed_container_that_is_still_listed() {
        let tool = FakeTool::new(&["aaa111"], &[], 2);
        tool.manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(tool.volume_rm_calls(), 3);
        let calls = fs::read_to_string(tool.dir.path().join("calls")).unwrap();
        assert!(calls.lines().filter(|l| l.starts_with("ps")).count() >= 3);
    }

    /// A container that `clean -f` did not remove (a new `sdk run`, for
    /// example) will not go away by itself: fail at once, with no wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_does_not_wait_for_a_container_it_did_not_remove() {
        let tool = FakeTool::new(&["aaa111"], &[], u32::MAX);
        tool.newcomer("ddd444");
        let err = tool
            .manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("volume is in use"), "{err}");
        assert_eq!(tool.volume_rm_calls(), 1);
    }

    /// With no containers on the volume there is nothing to wait for: one
    /// `volume rm`, and its error comes back at once.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_without_containers_does_not_wait() {
        let tool = FakeTool::new(&[], &[], 1);
        let err = tool
            .manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("volume is in use"), "{err}");
        assert_eq!(tool.volume_rm_calls(), 1);
    }

    /// A volume that is already gone still counts as removed (ENG-2761).
    #[cfg(unix)]
    #[tokio::test]
    async fn force_remove_of_a_missing_volume_succeeds() {
        let tool = FakeTool::new(&["aaa111"], &[], 0);
        tool.volume_is_gone();
        tool.manager()
            .force_remove_volume_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(tool.volume_rm_calls(), 1);
    }

    /// `clean` without `-f` stops the VS Code explorer containers, then waits
    /// for them to release the volume in the same way.
    #[cfg(unix)]
    #[tokio::test]
    async fn explorer_cleanup_waits_for_stopped_explorers_to_release_the_volume() {
        let tool = FakeTool::new(&["ccc333"], &["ccc333\tavocado-explorer-1"], 2);
        tool.manager()
            .remove_volume_with_explorer_cleanup_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(tool.volume_rm_calls(), 3);
    }

    /// Explorer containers were stopped, but another container also holds
    /// the volume. `clean` fails at once, so the `--force` hint shows with
    /// no wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn explorer_cleanup_does_not_wait_for_other_containers() {
        let tool = FakeTool::new(
            &["aaa111", "ccc333"],
            &["ccc333\tavocado-explorer-1"],
            u32::MAX,
        );
        let err = tool
            .manager()
            .remove_volume_with_explorer_cleanup_within("avo-x", Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("volume is in use"), "{err}");
        assert_eq!(tool.volume_rm_calls(), 1);
    }

    /// Without explorer containers, a volume held by another container
    /// fails at once, so `clean` can show its `--force` hint.
    #[cfg(unix)]
    #[tokio::test]
    async fn explorer_cleanup_without_explorers_does_not_wait() {
        let tool = FakeTool::new(&["aaa111"], &[], 1);
        let err = tool
            .manager()
            .remove_volume_with_explorer_cleanup_within("avo-x", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("volume is in use"), "{err}");
        assert_eq!(tool.volume_rm_calls(), 1);
    }

    /// The state file lives in the working directory, which is where
    /// `get_or_create_volume` writes it. A copy in `src_dir` is not consulted,
    /// so a `src_dir: ..` project run from its config directory finds the
    /// volume the container layer created there.
    #[test]
    #[serial_test::serial]
    fn load_from_current_dir_reads_the_working_directory_only() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().canonicalize().unwrap();
        let config_dir = src_dir.join("avocado");
        fs::create_dir_all(&config_dir).unwrap();
        VolumeState::new(src_dir.clone(), "docker".to_string())
            .save_to_dir(&src_dir)
            .unwrap();

        // Restore the cwd before asserting, so a failure here does not leave
        // later tests running from a deleted directory.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&config_dir).unwrap();
        let from_config_dir = VolumeState::load_from_current_dir();
        std::env::set_current_dir(&src_dir).unwrap();
        let from_src_dir = VolumeState::load_from_current_dir();
        std::env::set_current_dir(prev).unwrap();

        assert!(from_config_dir.unwrap().is_none());
        assert!(from_src_dir.unwrap().is_some());
    }
}
