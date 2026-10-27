use crate::commands::connect::client::{self as connect_client, ConnectClient};
use crate::utils::{
    config::{ComposedConfig, Config},
    container::{is_docker_desktop, is_vm_routing_active, RunConfig, SdkContainer},
    lockfile::LockFile,
    output::{print_info, print_success, print_warning, OutputLevel},
    output_format::{emit_json_event, is_json_output_active},
    stamps::{generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement},
    target::resolve_target_required,
    update_repo::{self, HashCollectionOutput},
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

const DEFAULT_DEPLOY_REPO_PORT: u16 = 8585;
const DEPLOY_STAGING_DIR: &str = ".avocado/deploy-staging";

/// Parsed representation of a device connection string.
///
/// Accepts formats: `host`, `user@host`, `host:port`, `user@host:port`
#[derive(Debug, Clone, PartialEq)]
struct DeviceSpec {
    user: String,
    host: String,
    port: Option<u16>,
}

impl DeviceSpec {
    fn parse(device: &str) -> Result<Self> {
        let (user, host_port) = if let Some(at_pos) = device.find('@') {
            let user = &device[..at_pos];
            anyhow::ensure!(!user.is_empty(), "Empty user in device string '{device}'");
            (user.to_string(), &device[at_pos + 1..])
        } else {
            ("root".to_string(), device)
        };

        anyhow::ensure!(
            !host_port.is_empty(),
            "Empty host in device string '{device}'"
        );

        let (host, port) = if let Some(colon_pos) = host_port.rfind(':') {
            let maybe_port = &host_port[colon_pos + 1..];
            match maybe_port.parse::<u16>() {
                Ok(p) => {
                    let h = &host_port[..colon_pos];
                    anyhow::ensure!(!h.is_empty(), "Empty host in device string '{device}'");
                    (h.to_string(), Some(p))
                }
                // Not a valid port number -- treat the whole thing as a hostname
                // (e.g. IPv6 addresses like ::1)
                Err(_) => (host_port.to_string(), None),
            }
        } else {
            (host_port.to_string(), None)
        };

        Ok(Self { user, host, port })
    }

    /// SSH destination in `user@host` form.
    fn ssh_destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// SSH port arguments: `["-p", "<port>"]` if a port was specified, empty otherwise.
    fn ssh_port_args(&self) -> String {
        match self.port {
            Some(p) => format!("-p {p}"),
            None => String::new(),
        }
    }
}

pub struct RuntimeDeployCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    device: String,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    connect_sign: bool,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

/// Deploy phase identifiers used for NDJSON `task_registered` / `step` events
/// when `--output json` is active. Names mirror the human-facing phase
/// labels in `execute()` so the desktop app can render them directly.
const PHASE_STAMPS: &str = "stamps";
const PHASE_HASH: &str = "hash-collection";
const PHASE_METADATA: &str = "metadata-sign";
const PHASE_DEPLOY: &str = "deploy";

impl RuntimeDeployCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        device: String,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            target,
            device,
            container_args,
            dnf_args,
            no_stamps: false,
            connect_sign: false,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Route Phase 2 TUF metadata signing through the Connect platform
    pub fn with_connect_sign(mut self, connect_sign: bool) -> Self {
        self.connect_sign = connect_sign;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Register a phase as a known step so the desktop UI can render it
    /// before it starts running. No-op in human-output mode.
    fn emit_phase_register(name: &str, label: &str) {
        if is_json_output_active() {
            emit_json_event(&serde_json::json!({
                "event": "task_registered",
                "name": name,
                "label": label,
            }));
        }
    }

    /// Transition a phase to the given status (`running`, `success`, `failed`).
    /// No-op in human-output mode.
    fn emit_phase_status(name: &str, status: &str) {
        if is_json_output_active() {
            emit_json_event(&serde_json::json!({
                "event": "step",
                "name": name,
                "status": status,
            }));
        }
    }

    /// Annotate the most recent failed phase with an error message so the
    /// desktop UI can surface it inline next to the step. No-op in human mode.
    fn emit_phase_error(name: &str, message: &str) {
        if is_json_output_active() {
            emit_json_event(&serde_json::json!({
                "event": "step_error",
                "name": name,
                "message": message,
            }));
        }
    }

    /// Compose a phase failure message that carries the container script's
    /// own stderr diagnostics, so both the CLI error and the JSON step_error
    /// event name the real problem.
    fn container_failure_message(context: &str, stderr: &str) -> String {
        crate::utils::container::container_failure_message(context, stderr)
    }

    /// `--connect-sign` reads credentials from the saved Connect profile only,
    /// so `AVOCADO_CONNECT_TOKEN` on its own is not enough; CI has to save a
    /// profile with `avocado login --token`.
    fn connect_sign_login_message() -> String {
        "--connect-sign requires an active Connect session. Run `avocado login` first. \
         For non-interactive use, run `avocado login --token <token>` to save a profile; \
         AVOCADO_CONNECT_TOKEN alone is not read for signing."
            .to_string()
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(
                &self.config_path,
                self.target.as_deref(),
            )?),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        let runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // Validate kernel consistency before deploy. Same non-fatal warning
        // policy as provision: surfaces drift early but allows deploy to
        // continue while v1 rootfs auto-append is still active. Phase 5
        // promotes to fatal after the auto-append is removed.
        if let Ok(src_dir) = std::env::current_dir() {
            if let Ok(lock_file) = LockFile::load(&src_dir) {
                if let Err(e) = lock_file.validate_kernel_consistency(&target_arch) {
                    print_warning(
                        &format!(
                            "Kernel consistency warning before deploy: {e} \
                             (deploy will continue; this becomes a hard error in a future release)"
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Pre-register all phases so the desktop UI can render the
        // step list before any work starts. Order here matches the
        // execution order below.
        if !self.no_stamps {
            Self::emit_phase_register(PHASE_STAMPS, "Verify build stamps");
        }
        Self::emit_phase_register(PHASE_HASH, "Collect artifact hashes");
        Self::emit_phase_register(PHASE_METADATA, "Sign TUF metadata");
        Self::emit_phase_register(PHASE_DEPLOY, "Deploy to device");

        // Validate stamps before proceeding (unless --no-stamps).
        // Deploy needs the runtime to be built; provision is not required —
        // deploy and provision are independent consumers of the build output.
        if !self.no_stamps {
            Self::emit_phase_status(PHASE_STAMPS, "running");
            let required = vec![StampRequirement::new(
                crate::utils::stamps::StampCommand::Build,
                crate::utils::stamps::StampComponent::Runtime,
                Some(&self.runtime_name),
            )];

            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            // Capture the full result so a container failure is reported as
            // such, not silently converted into a "missing stamps" error.
            let output = match container_helper.run_in_container_capture(run_config).await {
                Ok(out) if out.success => out.stdout,
                Ok(out) => {
                    if self.verbose {
                        crate::utils::container::print_failure_notice(&format!(
                            "Full container stderr:\n{}",
                            out.stderr
                        ));
                    }
                    let msg = Self::container_failure_message(
                        "Reading build stamps failed in the SDK container",
                        &out.stderr,
                    );
                    Self::emit_phase_error(PHASE_STAMPS, &msg);
                    Self::emit_phase_status(PHASE_STAMPS, "failed");
                    return Err(anyhow::anyhow!(msg));
                }
                Err(e) => {
                    Self::emit_phase_error(PHASE_STAMPS, &e.to_string());
                    Self::emit_phase_status(PHASE_STAMPS, "failed");
                    return Err(e);
                }
            };

            let validation = validate_stamps_batch(&required, output.trim(), &[]);

            if !validation.is_satisfied() {
                let msg = format!("Cannot deploy runtime '{}'", self.runtime_name);
                Self::emit_phase_error(PHASE_STAMPS, &msg);
                Self::emit_phase_status(PHASE_STAMPS, "failed");
                validation
                    .into_error(&msg)
                    .with_search_root(crate::utils::stamps::StampSearchRoot::for_container(
                        &container_helper,
                        &target_arch,
                    ))
                    .print_and_exit();
            }
            Self::emit_phase_status(PHASE_STAMPS, "success");
        }

        print_info(
            &format!(
                "Deploying runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );

        // --- Phase 1: Hash collection ---
        print_info(
            "Phase 1: Collecting artifact hashes...",
            OutputLevel::Normal,
        );
        Self::emit_phase_status(PHASE_HASH, "running");

        let hash_script = self.create_hash_collection_script(&target_arch);
        let hash_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: hash_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        // Capture the full result so the script's own diagnostics (e.g. the
        // signing-key prerequisite for --connect-sign) reach the user instead
        // of a generic "produced no output" error.
        let hash_output = match container_helper
            .run_in_container_capture(hash_run_config)
            .await
        {
            Ok(out) if out.success => out.stdout.trim().to_string(),
            Ok(out) => {
                if self.verbose {
                    crate::utils::container::print_failure_notice(&format!(
                        "Full container stderr:\n{}",
                        out.stderr
                    ));
                }
                let msg = Self::container_failure_message(
                    "Hash collection failed in the SDK container",
                    &out.stderr,
                );
                Self::emit_phase_error(PHASE_HASH, &msg);
                Self::emit_phase_status(PHASE_HASH, "failed");
                return Err(anyhow::anyhow!(msg));
            }
            Err(e) => {
                Self::emit_phase_error(PHASE_HASH, &e.to_string());
                Self::emit_phase_status(PHASE_HASH, "failed");
                return Err(e);
            }
        };

        let collection: HashCollectionOutput = match serde_json::from_str(&hash_output) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Failed to parse hash collection output: {e}");
                Self::emit_phase_error(PHASE_HASH, &msg);
                Self::emit_phase_status(PHASE_HASH, "failed");
                return Err(anyhow::anyhow!(msg));
            }
        };

        if self.verbose {
            print_info(
                &format!(
                    "Collected hashes for {} target file(s).",
                    collection.targets.len()
                ),
                OutputLevel::Normal,
            );
        }
        Self::emit_phase_status(PHASE_HASH, "success");

        // --- Phase 2: Generate and sign TUF metadata ---
        print_info(
            if self.connect_sign {
                "Phase 2: Signing TUF metadata via Connect..."
            } else {
                "Phase 2: Generating signed TUF metadata..."
            },
            OutputLevel::Normal,
        );
        Self::emit_phase_status(PHASE_METADATA, "running");

        let project_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let staging_dir = project_dir.join(DEPLOY_STAGING_DIR);

        let delegations_dir = staging_dir.join("delegations");
        // Group all metadata work in a single fallible block so any
        // failure is attributed to PHASE_METADATA without wrapping every
        // `?` site individually.
        let metadata_result: Result<()> = async {
            std::fs::create_dir_all(&delegations_dir).with_context(|| {
                format!(
                    "Failed to create deploy staging directory: {}",
                    delegations_dir.display()
                )
            })?;

            if self.connect_sign {
                // Route Phase 2 through the Connect signing endpoint.
                let connect_config = connect_client::load_config()?
                    .ok_or_else(|| anyhow::anyhow!(Self::connect_sign_login_message()))?;
                let org_hint = config.connect.as_ref().and_then(|c| c.org.as_deref());
                let (_, profile) = connect_config.resolve_profile(None, org_hint)?;
                let org = org_hint
                    .map(str::to_owned)
                    .or_else(|| profile.organization_id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "--connect-sign requires a Connect org. Set `connect.org` in \
                             avocado.yaml, or authenticate with an org-associated account."
                        )
                    })?;
                let client = ConnectClient::from_profile(profile)?;

                let signed = client
                    .sign_for_deploy(&org, &collection.targets, &collection.runtime_uuid)
                    .await?;

                std::fs::write(staging_dir.join("targets.json"), &signed.targets_json)
                    .context("Failed to write targets.json")?;
                std::fs::write(staging_dir.join("snapshot.json"), &signed.snapshot_json)
                    .context("Failed to write snapshot.json")?;
                std::fs::write(staging_dir.join("timestamp.json"), &signed.timestamp_json)
                    .context("Failed to write timestamp.json")?;
                std::fs::write(
                    delegations_dir.join(format!("runtime-{}.json", collection.runtime_uuid)),
                    &signed.delegated_targets_json,
                )
                .context("Failed to write delegated targets json")?;

                print_info(
                    "Connect-signed metadata expires after 7 days. \
                     Re-run `avocado deploy --connect-sign` to refresh.",
                    OutputLevel::Normal,
                );
            } else {
                // Default: sign locally with the project or dev key. Warn when a
                // Connect server key is configured, since the device may hold a
                // server-key anchor from a Connect OTA that a local signature does
                // not satisfy.
                let has_server_key = config
                    .get_server_key_for_runtime(&self.runtime_name)
                    .is_some();
                if let Some(warning) =
                    crate::utils::update_signing::local_deploy_signing_warning(has_server_key)
                {
                    print_warning(warning, OutputLevel::Normal);
                }

                let signing_key_name = config.get_runtime_signing_key_name(&self.runtime_name);
                let content_key_name = config.get_runtime_content_key_name(&self.runtime_name);

                let mut resolved_signing_key_name = signing_key_name.clone();
                let signer = match crate::utils::update_signing::resolve_signing_key(
                    signing_key_name.as_deref(),
                )? {
                    Some(s) => s,
                    None => {
                        // Auto-generate a development signing key for local deploys
                        let dev_key_name = crate::utils::signing_keys::ensure_dev_signing_key()?;
                        resolved_signing_key_name = Some(dev_key_name.clone());
                        crate::utils::update_signing::resolve_signing_key(Some(&dev_key_name))?
                            .expect("dev signing key was just created")
                    }
                };
                let content_signer = crate::utils::update_signing::resolve_content_key(
                    content_key_name.as_deref(),
                    resolved_signing_key_name.as_deref(),
                )?
                .expect("signing key is set, so content key resolution should return Some");

                let repo_metadata = update_repo::generate_repo_metadata(
                    &collection.targets,
                    &collection.runtime_uuid,
                    &signer,
                    &content_signer,
                )?;

                std::fs::write(
                    staging_dir.join("targets.json"),
                    &repo_metadata.targets_json,
                )
                .context("Failed to write targets.json")?;
                std::fs::write(
                    staging_dir.join("snapshot.json"),
                    &repo_metadata.snapshot_json,
                )
                .context("Failed to write snapshot.json")?;
                std::fs::write(
                    staging_dir.join("timestamp.json"),
                    &repo_metadata.timestamp_json,
                )
                .context("Failed to write timestamp.json")?;
                std::fs::write(
                    delegations_dir.join(format!("runtime-{}.json", collection.runtime_uuid)),
                    &repo_metadata.delegated_targets_json,
                )
                .context("Failed to write delegated targets json")?;
            }
            if let Some(root_json) = &collection.root_json {
                std::fs::write(staging_dir.join("root.json"), root_json)
                    .context("Failed to write root.json to staging")?;
            }
            Ok(())
        }
        .await;

        if let Err(e) = metadata_result {
            Self::emit_phase_error(PHASE_METADATA, &e.to_string());
            Self::emit_phase_status(PHASE_METADATA, "failed");
            return Err(e);
        }

        if self.verbose {
            print_info(
                &format!("Wrote signed metadata to {}", staging_dir.display()),
                OutputLevel::Normal,
            );
        }
        Self::emit_phase_status(PHASE_METADATA, "success");

        // --- Phase 3: Serve repo and trigger update ---
        print_info(
            "Phase 3: Serving update repository and triggering device update...",
            OutputLevel::Normal,
        );
        Self::emit_phase_status(PHASE_DEPLOY, "running");

        let deploy_script = match self.create_deploy_script(&target_arch, &collection.runtime_uuid)
        {
            Ok(s) => s,
            Err(e) => {
                Self::emit_phase_error(PHASE_DEPLOY, &e.to_string());
                Self::emit_phase_status(PHASE_DEPLOY, "failed");
                return Err(e);
            }
        };

        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.clone());
        env_vars.insert("AVOCADO_SDK_TARGET".to_string(), target_arch.clone());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), self.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), self.device.clone());

        if let Ok(repo_host) = std::env::var("AVOCADO_DEPLOY_REPO_HOST") {
            env_vars.insert("AVOCADO_DEPLOY_REPO_HOST".to_string(), repo_host);
        }
        if let Ok(repo_port) = std::env::var("AVOCADO_DEPLOY_REPO_PORT") {
            env_vars.insert("AVOCADO_DEPLOY_REPO_PORT".to_string(), repo_port);
        }

        // On macOS/Windows the deploy container runs inside a Linux VM, so
        // the TUF repo server it starts is unreachable by the device unless
        // we hand the device this host's LAN IP, publish the repo port out
        // of the container, and (avocado-vm only) open a qemu slirp forward.
        // No-op on Linux (native docker), where deploy already works.
        let repo_port: u16 = std::env::var("AVOCADO_DEPLOY_REPO_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_DEPLOY_REPO_PORT);
        let mut container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        // Whether the SDK container requests host networking. On avocado-vm the
        // published-port (`-p`) trick is unnecessary and discarded by docker,
        // so the shim skips it; on Docker Desktop host networking traps the
        // repo port in the LinuxKit VM, so the shim strips it instead (see
        // prepare_mac_deploy_net).
        let host_net = container_args
            .as_deref()
            .map(args_have_host_network)
            .unwrap_or(false);
        let MacDeployNet {
            env: mac_env,
            container_args: mac_args,
            strip_host_net,
            forward,
        } = prepare_mac_deploy_net(&self.device, repo_port, host_net, self.verbose).await;
        env_vars.extend(mac_env);
        if strip_host_net {
            if let Some(args) = container_args.as_mut() {
                strip_host_network_args(args);
            }
        }
        if !mac_args.is_empty() {
            container_args.get_or_insert_with(Vec::new).extend(mac_args);
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: deploy_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            env_vars: Some(env_vars),
            container_args,
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let run_outcome = container_helper
            .run_in_container(run_config)
            .await
            .context("Failed to deploy runtime");
        // Tear down the VM port forward (best-effort) regardless of outcome.
        if let Some(fwd) = forward {
            fwd.close().await;
        }
        let deploy_result = match run_outcome {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging_dir);
                Self::emit_phase_error(PHASE_DEPLOY, &e.to_string());
                Self::emit_phase_status(PHASE_DEPLOY, "failed");
                return Err(e);
            }
        };

        // Clean up staging directory
        let _ = std::fs::remove_dir_all(&staging_dir);

        if !deploy_result {
            let msg = "Failed to deploy runtime";
            Self::emit_phase_error(PHASE_DEPLOY, msg);
            Self::emit_phase_status(PHASE_DEPLOY, "failed");
            return Err(anyhow::anyhow!(msg));
        }

        Self::emit_phase_status(PHASE_DEPLOY, "success");

        print_success(
            &format!(
                "Successfully deployed runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Phase 1: Generate a shell script that enumerates built artifacts,
    /// computes their SHA-256 hashes and sizes, and outputs structured JSON.
    fn create_hash_collection_script(&self, _target_arch: &str) -> String {
        format!(
            r#"
set -e

RUNTIME_NAME="{runtime_name}"
VAR_STAGING="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
IMAGES_DIR="$VAR_STAGING/lib/avocado/images"

# Find the active manifest (prefer active symlink over find)
ACTIVE_LINK="$VAR_STAGING/lib/avocado/active"
RUNTIME_UUID=""
if [ -L "$ACTIVE_LINK" ]; then
    ACTIVE_TARGET=$(readlink "$ACTIVE_LINK")
    MANIFEST_FILE="$VAR_STAGING/lib/avocado/$ACTIVE_TARGET/manifest.json"
    # Extract UUID from path like "runtimes/<uuid>"
    RUNTIME_UUID=$(basename "$ACTIVE_TARGET")
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_STAGING/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
    if [ -n "$MANIFEST_FILE" ]; then
        RUNTIME_UUID=$(basename "$(dirname "$MANIFEST_FILE")")
    fi
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    echo "ERROR: No manifest.json found in $VAR_STAGING/lib/avocado/runtimes/" >&2
    exit 1
fi
if [ -z "$RUNTIME_UUID" ]; then
    echo "ERROR: Could not determine runtime UUID from active symlink" >&2
    exit 1
fi
# Extension dm-verity trees (<image_id>.verity) are not published by this path
# yet - only the images are - and a device given a root_hash without its tree
# refuses the extension. Refuse here instead, where the author can act on it.
# Only EXTENSION root hashes matter: the manifest's top-level root_hash is the
# rootfs verity hash, which travels in the boot FIT and needs no sidecar here.
# Exit 0: an extension carries root_hash (key present, as `connect upload`
# tests it). Exit 1: none does. Exit 2: the manifest could not be read or is
# not the expected shape - that must stop the deploy too, never pass as "no
# verity". The script runs under set -e, so the probe's non-zero exits must be
# captured, not allowed to abort the script.
VERITY_RC=0
python3 -c '
import json, sys
try:
    m = json.load(open(sys.argv[1]))
    exts = m["extensions"]
    if not isinstance(exts, list):
        raise TypeError("extensions is not a list")
except Exception as e:
    print("manifest %s: %s" % (sys.argv[1], e), file=sys.stderr)
    sys.exit(2)
sys.exit(0 if any(isinstance(e, dict) and "root_hash" in e for e in exts) else 1)
' "$MANIFEST_FILE" || VERITY_RC=$?
case $VERITY_RC in
    0)
        echo "ERROR: this runtime has extensions with image.verity: true; deploy does not publish their dm-verity hash trees yet, so the device would refuse them. Provision instead, or build without verity." >&2
        exit 1 ;;
    1) ;;
    *)
        echo "ERROR: could not read the runtime manifest to check for extension verity" >&2
        exit 1 ;;
esac

# Read root.json
ROOT_JSON_FILE="$VAR_STAGING/lib/avocado/metadata/root.json"
if [ ! -f "$ROOT_JSON_FILE" ]; then
    echo "ERROR: No root.json found at $ROOT_JSON_FILE" >&2
    echo "       The runtime has no update-authority (root.json) baked into its build." >&2
    echo "       This usually means no signing key is configured. Set 'signing.key' for" >&2
    echo "       the runtime (for a Connect project, run 'avocado connect trust promote-root --key <KEY>')" >&2
    echo "       and rebuild. --connect-sign requires a local signing key for this reason." >&2
    exit 1
fi

# Start building JSON output
echo -n '{{"targets":['

FIRST=true

# Hash the manifest
HASH=$(sha256sum "$MANIFEST_FILE" | awk '{{print $1}}')
SIZE=$(stat -c '%s' "$MANIFEST_FILE")
echo -n '{{"name":"manifest.json","sha256":"'"$HASH"'","size":'"$SIZE"'}}'
FIRST=false

# Image entries (.raw only — this path serves no .kab), read from the manifest
# rather than re-hashed; see manifest_image_targets_snippet.
{image_targets}

# Read and escape root.json for embedding
ROOT_JSON_ESCAPED=$(python3 -c "import json,sys; print(json.dumps(open(sys.argv[1]).read()))" "$ROOT_JSON_FILE")

echo -n '],"root_json":'
echo -n "$ROOT_JSON_ESCAPED"
echo -n ',"runtime_uuid":"'"$RUNTIME_UUID"'"'
echo -n '}}'
"#,
            runtime_name = self.runtime_name,
            image_targets = update_repo::manifest_image_targets_snippet(&["raw"]),
        )
    }

    /// Phase 3: Generate a shell script that assembles the TUF repo,
    /// starts an HTTP server, SSHes into the device, and triggers the update.
    fn create_deploy_script(&self, _target_arch: &str, runtime_uuid: &str) -> Result<String> {
        let port = DEFAULT_DEPLOY_REPO_PORT;
        let spec = DeviceSpec::parse(&self.device)?;

        let script = format!(
            r#"
set -e

RUNTIME_NAME="{runtime_name}"
RUNTIME_UUID="{runtime_uuid}"
SSH_DEST="{ssh_dest}"
SSH_PORT_ARGS="{ssh_port_args}"
DEVICE_HOST="{device_host}"
PORT="${{AVOCADO_DEPLOY_REPO_PORT:-{port}}}"
REPO_DIR="/tmp/avocado-deploy-repo"
VAR_STAGING="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
IMAGES_DIR="$VAR_STAGING/lib/avocado/images"
STAGING_DIR="/opt/src/{staging_dir}"

# Clean up any previous repo
rm -rf "$REPO_DIR"
mkdir -p "$REPO_DIR/metadata/delegations" "$REPO_DIR/targets"

# Copy metadata from deploy-staging (signed in Phase 2)
cp "$STAGING_DIR/targets.json" "$REPO_DIR/metadata/targets.json"
cp "$STAGING_DIR/snapshot.json" "$REPO_DIR/metadata/snapshot.json"
cp "$STAGING_DIR/timestamp.json" "$REPO_DIR/metadata/timestamp.json"
cp "$STAGING_DIR/delegations/runtime-${{RUNTIME_UUID}}.json" \
   "$REPO_DIR/metadata/delegations/runtime-${{RUNTIME_UUID}}.json"

# Copy root.json from build output
cp "$VAR_STAGING/lib/avocado/metadata/root.json" "$REPO_DIR/metadata/root.json"
cp "$VAR_STAGING/lib/avocado/metadata/1.root.json" "$REPO_DIR/metadata/1.root.json"

# Link manifest.json into targets/ (prefer active symlink over find)
ACTIVE_LINK="$VAR_STAGING/lib/avocado/active"
if [ -L "$ACTIVE_LINK" ]; then
    MANIFEST_FILE="$VAR_STAGING/lib/avocado/$(readlink "$ACTIVE_LINK")/manifest.json"
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_STAGING/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
fi
if [ -n "$MANIFEST_FILE" ] && [ -f "$MANIFEST_FILE" ]; then
    cp "$MANIFEST_FILE" "$REPO_DIR/targets/manifest.json"
fi

# Link image .raw files (content-addressable by UUIDv5) into targets/
if [ -d "$IMAGES_DIR" ]; then
    for RAW_FILE in "$IMAGES_DIR"/*.raw; do
        [ -f "$RAW_FILE" ] || continue
        BASENAME=$(basename "$RAW_FILE")
        ln -sf "$RAW_FILE" "$REPO_DIR/targets/$BASENAME"
    done
fi

echo "TUF repository assembled at $REPO_DIR"
ls -la "$REPO_DIR/metadata/"
ls -la "$REPO_DIR/targets/"

repo_port_open() {{
    python3 -c 'import socket,sys; s=socket.socket(); s.settimeout(0.5); sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1])))==0 else 1)' "$PORT" 2>/dev/null
}}

# The device would fetch from whatever server holds the port. If an earlier
# deploy's server still holds it, the new server cannot bind and the device
# gets the old repo, which reads as a successful deploy.
if repo_port_open; then
    echo "ERROR: port $PORT is already in use, so the update repository cannot be served." >&2
    echo "       A deploy container from an earlier run may still be running. Stop it," >&2
    echo "       or set AVOCADO_DEPLOY_REPO_PORT to a free port." >&2
    exit 1
fi

# The readiness wait fetches this file, so it passes only for this run's
# server and never for some other listener on the port.
DEPLOY_NONCE=$(python3 -c 'import uuid; print(uuid.uuid4())')
printf '%s' "$DEPLOY_NONCE" > "$REPO_DIR/.avocado-deploy-nonce"

repo_serves_nonce() {{
    python3 -c '
import sys, urllib.request
opener = urllib.request.build_opener(urllib.request.ProxyHandler({{}}))
try:
    body = opener.open("http://127.0.0.1:%s/.avocado-deploy-nonce" % sys.argv[1], timeout=1).read()
except Exception:
    sys.exit(1)
sys.exit(0 if body.decode() == sys.argv[2] else 1)
' "$PORT" "$DEPLOY_NONCE" 2>/dev/null
}}

# Start HTTP server bound to all interfaces so the device can reach it.
# Use an inline server rather than `python3 -m http.server`: the latter calls
# socket.getfqdn() in server_bind() BEFORE listen(), and on hosts with slow
# reverse DNS (e.g. a container resolver with no PTR record) that lookup blocks
# ~10s, so the socket accepts no connections until it returns -- long past the
# readiness wait below, making the device fetch fail with "connection refused".
# Neutralizing getfqdn makes the server listen immediately.
python3 -c '
import sys, socket, functools, http.server
socket.getfqdn = lambda *a: ""
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=sys.argv[2])
http.server.HTTPServer(("0.0.0.0", int(sys.argv[1])), handler).serve_forever()
' "$PORT" "$REPO_DIR" &
HTTP_PID=$!

stop_repo_server() {{
    kill "$HTTP_PID" 2>/dev/null || true
    wait "$HTTP_PID" 2>/dev/null || true
    rm -rf "$REPO_DIR"
}}
trap stop_repo_server EXIT

# Wait until this run's repo server answers before pointing the device at it.
# A fixed sleep races server startup latency; poll so any bind delay is
# tolerated (bounded to ~30s). The server is backgrounded, so `set -e` cannot
# catch a bind failure -- check the process is still alive each iteration and
# fail fast with an actionable message if it dies or never binds.
REPO_READY=
for _ in $(seq 1 60); do
    if ! kill -0 "$HTTP_PID" 2>/dev/null; then
        echo "ERROR: repo server (pid $HTTP_PID) exited before listening on port $PORT" >&2
        exit 1
    fi
    if repo_serves_nonce; then
        REPO_READY=1
        break
    fi
    sleep 0.5
done
if [ -z "$REPO_READY" ]; then
    echo "ERROR: repo server did not become reachable on port $PORT within 30s" >&2
    exit 1
fi

# Determine the IP the device should use to reach this HTTP server.
# AVOCADO_DEPLOY_REPO_HOST overrides all auto-detection (useful for QEMU
# user-mode networking where the host is at 10.0.2.2).
if [ -n "${{AVOCADO_DEPLOY_REPO_HOST:-}}" ]; then
    HOST_IP="$AVOCADO_DEPLOY_REPO_HOST"
else
    # When the SSH target is a loopback address the device is likely a QEMU
    # VM with port-forwarded SSH.  Inside the VM, 127.0.0.1 is the VM's own
    # loopback -- not the host -- so we must resolve a real non-loopback IP.
    case "$DEVICE_HOST" in
        127.*|localhost|::1)
            HOST_IP=$(ip -4 addr show scope global 2>/dev/null | awk '/inet /{{print $2}}' | cut -d/ -f1 | head -n 1)
            ;;
        *)
            HOST_IP=$(ip route get "$DEVICE_HOST" 2>/dev/null | awk '{{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}}' | head -n 1)
            if [ -z "$HOST_IP" ]; then
                HOST_IP=$(ip -4 addr show scope global 2>/dev/null | awk '/inet /{{print $2}}' | cut -d/ -f1 | head -n 1)
            fi
            ;;
    esac
fi

if [ -z "$HOST_IP" ]; then
    echo "ERROR: Could not determine host IP address" >&2
    echo "       Set AVOCADO_DEPLOY_REPO_HOST to the IP the device can reach this host on." >&2
    exit 1
fi

REPO_URL="http://${{HOST_IP}}:${{PORT}}"
echo "Serving TUF repository at $REPO_URL"
echo "Connecting to $SSH_DEST via SSH..."

# SSH into device and trigger update (disable set -e to capture exit code)
set +e
ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 \
    -o ServerAliveInterval=5 \
    -o ServerAliveCountMax=3 \
    -o LogLevel=ERROR \
    $SSH_PORT_ARGS \
    "$SSH_DEST" \
    "avocadoctl runtime add --url $REPO_URL"

UPDATE_EXIT=$?
set -e

if [ $UPDATE_EXIT -eq 0 ]; then
    echo "Device update completed successfully."
elif [ $UPDATE_EXIT -eq 255 ]; then
    echo "ERROR: the SSH connection to $SSH_DEST failed or was lost (ssh exit code 255)." >&2
    echo "       If the update had started, the device may have changed its IP address" >&2
    echo "       or restarted its network. The new runtime may already be active." >&2
    echo "       Check on the device with: avocadoctl status" >&2
    exit 255
else
    echo "ERROR: Device update failed with exit code $UPDATE_EXIT" >&2
    exit $UPDATE_EXIT
fi
"#,
            runtime_name = self.runtime_name,
            runtime_uuid = runtime_uuid,
            ssh_dest = spec.ssh_destination(),
            ssh_port_args = spec.ssh_port_args(),
            device_host = spec.host,
            port = port,
            staging_dir = DEPLOY_STAGING_DIR,
        );

        Ok(script)
    }
}

/// Host-side networking shim for `avocado deploy` on macOS/Windows, where
/// the deploy container runs inside a Linux VM (avocado-vm or Docker
/// Desktop). The TUF repo HTTP server it starts is otherwise trapped in
/// the container netns, and the script's in-container host-IP autodetect
/// returns a VM-internal address the device can't reach. No-op on Linux.
/// See docs/features/macos-deploy-port-forwarding.md.
struct MacDeployNet {
    /// Extra env for the deploy container (AVOCADO_DEPLOY_REPO_HOST).
    env: HashMap<String, String>,
    /// Extra `docker run` args (publish the repo port: `-p <port>:<port>`).
    container_args: Vec<String>,
    /// Strip `--network=host` from the merged container args before running.
    /// Docker Desktop traps a host-networked container's ports inside the
    /// LinuxKit VM (unreachable from the Mac/LAN), and `-p` publishing is
    /// incompatible with host networking — so the deploy container must run on
    /// its own bridge netns. The caller applies this to the merged args.
    strip_host_net: bool,
    /// A qemu slirp forward to tear down afterward (avocado-vm only).
    forward: Option<OpenForward>,
}

/// True if `args` request host networking, in either the `--network=host` /
/// `--net=host` form or the space-separated `--network host` / `--net host`
/// form.
fn args_have_host_network(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, a)| {
        a == "--network=host"
            || a == "--net=host"
            || ((a == "--network" || a == "--net")
                && args.get(i + 1).map(|v| v == "host").unwrap_or(false))
    })
}

/// Remove host-networking flags from container args so a published port (`-p`)
/// takes effect. Handles both the `--network=host` / `--net=host` and
/// space-separated `--network host` / `--net host` forms. See
/// [`args_have_host_network`] and `prepare_mac_deploy_net`.
fn strip_host_network_args(args: &mut Vec<String>) {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--network=host" || a == "--net=host" {
            i += 1;
            continue;
        }
        if (a == "--network" || a == "--net")
            && args.get(i + 1).map(|v| v == "host").unwrap_or(false)
        {
            i += 2;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    *args = out;
}

/// A qemu slirp host-forward opened for the duration of one deploy. Teardown
/// is async, so it's closed explicitly by the caller rather than via `Drop`.
struct OpenForward {
    qmp_socket: std::path::PathBuf,
    host_port: u16,
}

impl OpenForward {
    async fn close(self) {
        // The qmp module is unix-only (unix-socket transport), so this teardown
        // only exists on unix. On other platforms OpenForward is never built
        // (the qmp block below is cfg'd out), so this is unreachable there.
        #[cfg(unix)]
        if let Ok(mut c) = crate::utils::vm::qmp::QmpClient::connect(&self.qmp_socket).await {
            let _ = c.hostfwd_remove("net0", "0.0.0.0", self.host_port).await;
        }
        #[cfg(not(unix))]
        let _ = (&self.qmp_socket, self.host_port);
    }
}

/// Build the macOS/Windows deploy networking shim. Returns an empty (no-op)
/// value on Linux so the native-docker path is left exactly as-is.
async fn prepare_mac_deploy_net(
    device: &str,
    port: u16,
    host_net: bool,
    verbose: bool,
) -> MacDeployNet {
    let mut net = MacDeployNet {
        env: HashMap::new(),
        container_args: Vec::new(),
        strip_host_net: false,
        forward: None,
    };

    // Linux runs the container on native docker — no VM in between, deploy
    // already works. Leave it untouched.
    if !is_docker_desktop() {
        return net;
    }

    let vm_routing = is_vm_routing_active();

    // Expose the in-container TUF repo server to the LAN device. The two macOS
    // backends need different bridges:
    //
    //  * Docker Desktop (no avocado-vm): vpnkit only forwards *published*
    //    (`-p`) ports to the Mac, and `-p` is incompatible with host
    //    networking — a host-networked container's ports stay trapped in the
    //    LinuxKit VM, unreachable from the Mac/LAN (the "Connection refused"
    //    people hit). So force the deploy container onto its own bridge netns
    //    (strip `--network=host`) and publish the port on 0.0.0.0. Deploy only
    //    needs outbound SSH + inbound HTTP, both of which work on a bridge.
    //
    //  * avocado-vm: the container stays host-networked *inside qemu* and the
    //    qemu slirp hostfwd below bridges the port to the Mac; docker discards
    //    `-p` under host networking, so only publish when it has its own netns.
    let publish = if vm_routing { !host_net } else { true };
    if !vm_routing && host_net {
        net.strip_host_net = true;
    }
    if publish {
        net.container_args.push("-p".to_string());
        net.container_args.push(format!("0.0.0.0:{port}:{port}"));
    }

    // Both Mac contexts: tell the device the host LAN IP it can reach us on,
    // overriding the broken in-container autodetect. Respect a user override.
    if std::env::var("AVOCADO_DEPLOY_REPO_HOST").is_err() {
        let host = DeviceSpec::parse(device)
            .map(|s| s.host)
            .unwrap_or_default();
        match crate::utils::remote::get_local_ip_for_remote(&host).await {
            Ok(ip) => {
                if verbose {
                    print_info(
                        &format!("deploy: repo host IP {ip} (reachable by device {host})"),
                        OutputLevel::Normal,
                    );
                }
                net.env
                    .insert("AVOCADO_DEPLOY_REPO_HOST".to_string(), ip.to_string());
            }
            Err(e) => print_warning(
                &format!(
                    "deploy: could not determine a LAN IP the device can reach this host on \
                     ({e}). Set AVOCADO_DEPLOY_REPO_HOST to the right address."
                ),
                OutputLevel::Normal,
            ),
        }
    }

    // avocado-vm only: open a slirp hostfwd so the LAN device can reach the
    // published port through qemu. Docker Desktop forwards `-p` to the host
    // itself (vpnkit), so it needs no qemu step.
    if vm_routing {
        let sock = match crate::utils::vm::state::VmPaths::resolve() {
            Ok(p) => p.qmp_socket(),
            Err(e) => {
                print_warning(
                    &format!("deploy: can't locate the avocado-vm for port forwarding ({e})."),
                    OutputLevel::Normal,
                );
                return net;
            }
        };
        // QMP rides a unix-socket transport (the qmp module is unix-only), and
        // avocado-vm routing only happens on unix hosts — so the port-forward
        // setup is compiled in on unix only. Elsewhere it's a no-op.
        #[cfg(unix)]
        match crate::utils::vm::qmp::QmpClient::connect(&sock).await {
            Ok(mut c) => {
                // Clear any stale forward from a prior interrupted deploy, then add.
                let _ = c.hostfwd_remove("net0", "0.0.0.0", port).await;
                match c.hostfwd_add("net0", "0.0.0.0", port, port).await {
                    Ok(()) => {
                        if verbose {
                            print_info(
                                &format!(
                                    "deploy: opened VM port forward 0.0.0.0:{port} → guest :{port}"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        net.forward = Some(OpenForward {
                            qmp_socket: sock,
                            host_port: port,
                        });
                    }
                    Err(e) => print_warning(
                        &format!(
                            "deploy: failed to open the VM port forward for {port} ({e}); the \
                             device may be unable to fetch the repo. Is the avocado-vm running?"
                        ),
                        OutputLevel::Normal,
                    ),
                }
            }
            Err(e) => print_warning(
                &format!(
                    "deploy: couldn't reach the avocado-vm QMP socket for port forwarding ({e})."
                ),
                OutputLevel::Normal,
            ),
        }
        #[cfg(not(unix))]
        let _ = &sock;
    }

    net
}

#[cfg(test)]
mod tests {
    #[test]
    fn hash_collection_refuses_only_extension_root_hashes() {
        // A rootfs verity hash lives at the manifest's top level and needs no
        // sidecar; only extensions[].root_hash must trip the refusal.
        let cmd = RuntimeDeployCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            "root@host".to_string(),
            None,
            None,
        );
        let s = cmd.create_hash_collection_script("x86_64");
        assert!(
            s.contains("\"root_hash\" in e for e in exts"),
            "checks key presence on extensions, not the whole file"
        );
        assert!(
            s.contains("sys.exit(2)"),
            "an unreadable manifest is its own failure"
        );
        assert!(!s.contains("grep -q '\"root_hash\"'"), "no whole-file grep");
    }

    use super::*;

    #[test]
    fn connect_sign_login_message_names_login_and_the_token_path() {
        let msg = RuntimeDeployCommand::connect_sign_login_message();
        assert!(msg.contains("Run `avocado login` first."), "{msg}");
        assert!(msg.contains("`avocado login --token <token>`"), "{msg}");
        assert!(msg.contains("AVOCADO_CONNECT_TOKEN alone"), "{msg}");
        assert!(!msg.contains("connect auth login"), "{msg}");
    }

    /// Runs the extension-verity probe exactly as the container does: under
    /// `set -e`. The probe exits 1 for the ordinary "no extension verity" case,
    /// which must not abort the script - a regression here fails every deploy
    /// of a runtime without extension verity, with an empty stderr.
    fn run_verity_guard(manifest: &str) -> (i32, String, bool) {
        let cmd = RuntimeDeployCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            "root@host".to_string(),
            None,
            None,
        );
        let s = cmd.create_hash_collection_script("x86_64");
        let start = s.find("VERITY_RC=0").expect("probe start");
        let end = s[start..].find("esac").expect("probe end") + start + "esac".len();
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "avocado-verity-guard-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mf = dir.join("manifest.json");
        std::fs::write(&mf, manifest).unwrap();
        let script = format!(
            "set -e\nMANIFEST_FILE='{}'\n{}\necho REACHED_END",
            mf.display(),
            &s[start..end]
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            String::from_utf8_lossy(&out.stdout).contains("REACHED_END"),
        )
    }

    #[test]
    fn verity_guard_lets_a_runtime_without_extension_verity_through_under_set_e() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let (code, err, reached) =
            run_verity_guard(r#"{"root_hash":"abc","extensions":[{"name":"dev"}]}"#);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(reached, "script must continue past the probe");
    }

    #[test]
    fn verity_guard_refuses_extension_root_hash_and_unreadable_manifest() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let (code, err, reached) =
            run_verity_guard(r#"{"extensions":[{"name":"sec","root_hash":"abc"}]}"#);
        assert_eq!(code, 1);
        assert!(!reached);
        assert!(err.contains("image.verity: true"), "stderr: {err}");
        let (code, err, reached) = run_verity_guard("not json");
        assert_eq!(code, 1);
        assert!(!reached);
        assert!(
            err.contains("could not read the runtime manifest"),
            "stderr: {err}"
        );
    }

    // --- DeviceSpec parsing tests ---

    #[test]
    fn test_device_spec_bare_host() {
        let spec = DeviceSpec::parse("192.168.1.100").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "192.168.1.100");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "root@192.168.1.100");
        assert_eq!(spec.ssh_port_args(), "");
    }

    #[test]
    fn test_device_spec_user_at_host() {
        let spec = DeviceSpec::parse("admin@10.0.0.1").unwrap();
        assert_eq!(spec.user, "admin");
        assert_eq!(spec.host, "10.0.0.1");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "admin@10.0.0.1");
    }

    #[test]
    fn test_device_spec_host_with_port() {
        let spec = DeviceSpec::parse("127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_user_host_port() {
        let spec = DeviceSpec::parse("root@127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_hostname_no_port() {
        let spec = DeviceSpec::parse("device.local").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, None);
    }

    #[test]
    fn test_device_spec_hostname_with_port() {
        let spec = DeviceSpec::parse("device.local:22").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, Some(22));
    }

    #[test]
    fn test_device_spec_fqdn_user_port() {
        let spec = DeviceSpec::parse("deploy@edge.company.com:2200").unwrap();
        assert_eq!(spec.user, "deploy");
        assert_eq!(spec.host, "edge.company.com");
        assert_eq!(spec.port, Some(2200));
    }

    #[test]
    fn test_device_spec_empty_fails() {
        assert!(DeviceSpec::parse("").is_err());
    }

    #[test]
    fn test_device_spec_empty_user_fails() {
        assert!(DeviceSpec::parse("@host").is_err());
    }

    // --- RuntimeDeployCommand tests ---

    #[test]
    fn test_new() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.100".to_string(),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.device, "192.168.1.100");
    }

    #[test]
    fn test_create_deploy_script_bare_host() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device.local".to_string(),
            None,
            None,
        );

        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let script = cmd.create_deploy_script("x86_64", uuid).unwrap();

        assert!(script.contains("RUNTIME_NAME=\"test-runtime\""));
        assert!(script.contains(&format!("RUNTIME_UUID=\"{uuid}\"")));
        assert!(script.contains("SSH_DEST=\"root@device.local\""));
        assert!(script.contains("DEVICE_HOST=\"device.local\""));
        assert!(script.contains("SSH_PORT_ARGS=\"\""));
        assert!(script.contains("http.server.HTTPServer((\"0.0.0.0\", int(sys.argv[1]))"));
        assert!(script.contains("avocadoctl runtime add --url"));
        assert!(script.contains("ssh -o StrictHostKeyChecking=no"));
        assert!(script.contains("delegations/runtime-${RUNTIME_UUID}.json"));
    }

    #[test]
    fn test_create_deploy_script_with_user_and_port() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("qemux86-64".to_string()),
            "root@127.0.0.1:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("qemux86-64", "test-uuid").unwrap();

        assert!(script.contains("SSH_DEST=\"root@127.0.0.1\""));
        assert!(script.contains("SSH_PORT_ARGS=\"-p 2222\""));
        assert!(script.contains("DEVICE_HOST=\"127.0.0.1\""));
        assert!(script.contains("$SSH_PORT_ARGS"));
        assert!(script.contains("\"$SSH_DEST\""));

        // Loopback device host should use ip addr instead of ip route get
        assert!(script.contains("127.*|localhost"));
        assert!(script.contains("ip -4 addr show scope global"));
    }

    #[test]
    fn test_create_deploy_script_host_with_port_no_user() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "10.0.0.42:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64", "test-uuid").unwrap();

        assert!(script.contains("SSH_DEST=\"root@10.0.0.42\""));
        assert!(script.contains("SSH_PORT_ARGS=\"-p 2222\""));
        assert!(script.contains("DEVICE_HOST=\"10.0.0.42\""));
    }

    #[test]
    fn test_new_with_container_args() {
        let container_args = Some(vec![
            "--privileged".to_string(),
            "--network=host".to_string(),
        ]);
        let dnf_args = Some(vec!["--nogpgcheck".to_string()]);

        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            true,
            Some("aarch64".to_string()),
            "192.168.1.50".to_string(),
            container_args.clone(),
            dnf_args.clone(),
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert!(cmd.verbose);
        assert_eq!(cmd.device, "192.168.1.50");
        assert_eq!(cmd.container_args, container_args);
        assert_eq!(cmd.dnf_args, dnf_args);
    }

    #[test]
    fn test_create_deploy_script_with_ip() {
        let cmd = RuntimeDeployCommand::new(
            "edge-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("qemux86-64".to_string()),
            "10.0.0.42".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("qemux86-64", "test-uuid").unwrap();

        assert!(script.contains("SSH_DEST=\"root@10.0.0.42\""));
        assert!(script.contains("DEVICE_HOST=\"10.0.0.42\""));
    }

    #[test]
    fn test_create_deploy_script_with_hostname() {
        let cmd = RuntimeDeployCommand::new(
            "production".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("aarch64".to_string()),
            "edge-device.company.com".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("aarch64", "test-uuid").unwrap();

        assert!(script.contains("SSH_DEST=\"root@edge-device.company.com\""));
        assert!(script.contains("DEVICE_HOST=\"edge-device.company.com\""));
    }

    #[test]
    fn test_hash_collection_script() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.10".to_string(),
            None,
            None,
        );

        let script = cmd.create_hash_collection_script("x86_64");

        assert!(script.contains("RUNTIME_NAME=\"my-runtime\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("manifest.json"));
        assert!(script.contains("root.json"));
        assert!(script.contains(r#""targets":["#));
        assert!(script.contains("runtime_uuid"));
    }

    #[test]
    fn test_environment_variables_setup() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.10".to_string(),
            None,
            None,
        );

        let target_arch = "x86_64";
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.to_string());
        env_vars.insert("AVOCADO_SDK_TARGET".to_string(), target_arch.to_string());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), cmd.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), cmd.device.clone());

        assert_eq!(env_vars.get("AVOCADO_TARGET"), Some(&"x86_64".to_string()));
        assert_eq!(
            env_vars.get("AVOCADO_SDK_TARGET"),
            Some(&"x86_64".to_string())
        );
        assert_eq!(
            env_vars.get("AVOCADO_RUNTIME"),
            Some(&"my-runtime".to_string())
        );
        assert_eq!(
            env_vars.get("AVOCADO_DEPLOY_MACHINE"),
            Some(&"192.168.1.10".to_string())
        );
        assert_eq!(env_vars.len(), 4);
    }

    #[test]
    fn test_deploy_script_supports_repo_host_override() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "root@127.0.0.1:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64", "test-uuid").unwrap();

        assert!(script.contains("AVOCADO_DEPLOY_REPO_HOST"));
        assert!(script.contains("HOST_IP=\"$AVOCADO_DEPLOY_REPO_HOST\""));
    }

    #[test]
    fn test_deploy_script_non_loopback_uses_ip_route() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.50".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64", "test-uuid").unwrap();

        assert!(script.contains("ip route get \"$DEVICE_HOST\""));
    }

    #[test]
    fn test_deploy_script_binds_all_interfaces() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64", "test-uuid").unwrap();

        // The inline repo server binds all interfaces so the device can reach it.
        assert!(script.contains("(\"0.0.0.0\", int(sys.argv[1]))"));
    }

    #[test]
    fn test_deploy_script_contains_cleanup() {
        let cmd = RuntimeDeployCommand::new(
            "test".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64", "test-uuid").unwrap();

        assert!(script.contains("trap stop_repo_server EXIT"));
        assert!(script.contains("kill \"$HTTP_PID\""));
    }

    // --- host-networking detection / stripping (Docker Desktop deploy) ---

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_args_have_host_network_eq_form() {
        assert!(args_have_host_network(&sv(&[
            "--privileged",
            "--network=host"
        ])));
        assert!(args_have_host_network(&sv(&["--net=host"])));
    }

    #[test]
    fn test_args_have_host_network_space_form() {
        assert!(args_have_host_network(&sv(&["--network", "host"])));
        assert!(args_have_host_network(&sv(&["--net", "host"])));
    }

    #[test]
    fn test_args_have_host_network_none() {
        assert!(!args_have_host_network(&sv(&["--privileged"])));
        // `--network` followed by something other than host is not host networking.
        assert!(!args_have_host_network(&sv(&["--network", "bridge"])));
        assert!(!args_have_host_network(&sv(&[])));
    }

    #[test]
    fn test_strip_host_network_eq_form() {
        let mut args = sv(&["--privileged", "--network=host", "-v", "/x:/y"]);
        strip_host_network_args(&mut args);
        assert_eq!(args, sv(&["--privileged", "-v", "/x:/y"]));
        assert!(!args_have_host_network(&args));
    }

    #[test]
    fn test_strip_host_network_space_form() {
        let mut args = sv(&["--net", "host", "--privileged"]);
        strip_host_network_args(&mut args);
        assert_eq!(args, sv(&["--privileged"]));
    }

    #[test]
    fn test_strip_host_network_preserves_non_host_network() {
        // A non-host `--network <name>` must survive untouched.
        let mut args = sv(&["--network", "mynet", "--privileged"]);
        strip_host_network_args(&mut args);
        assert_eq!(args, sv(&["--network", "mynet", "--privileged"]));
    }

    #[test]
    fn test_strip_host_network_noop_when_absent() {
        let mut args = sv(&["--privileged", "-p", "8585:8585"]);
        let before = args.clone();
        strip_host_network_args(&mut args);
        assert_eq!(args, before);
    }

    // --- connect-sign builder and nudge ---

    #[test]
    fn test_connect_sign_default_false() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            "192.168.1.1".to_string(),
            None,
            None,
        );
        assert!(!cmd.connect_sign);
    }

    #[test]
    fn test_with_connect_sign_sets_flag() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            "192.168.1.1".to_string(),
            None,
            None,
        )
        .with_connect_sign(true);
        assert!(cmd.connect_sign);
    }

    // --- deploy script behaviour, run under sh ---

    fn have_python3() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn deploy_script() -> String {
        RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "root@192.0.2.10".to_string(),
            None,
            None,
        )
        .create_deploy_script("x86_64", "test-uuid")
        .unwrap()
    }

    /// The text of `script` from `start` up to `end` (exclusive). An `end`
    /// of `None` runs to the end of the script.
    fn script_section(script: &str, start: &str, end: Option<&str>) -> String {
        let from = script.find(start).expect("section start");
        let to = match end {
            Some(e) => script[from..].find(e).expect("section end") + from,
            None => script.len(),
        };
        script[from..to].to_string()
    }

    fn run_sh(script: &str) -> std::process::Output {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap()
    }

    /// Runs the ssh step of the deploy script with a stub `ssh` that records
    /// its arguments and exits with `ssh_exit`.
    fn run_ssh_step(ssh_exit: i32) -> (std::process::Output, String) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let args_file = dir.path().join("ssh-args");
        let stub = bin.join("ssh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit {ssh_exit}\n",
                args_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let section = script_section(&deploy_script(), "# SSH into device", None);
        let script = format!(
            "set -e\nPATH='{}':\"$PATH\"\nSSH_DEST=root@192.0.2.10\nSSH_PORT_ARGS=\nREPO_URL=http://192.0.2.1:1\n{section}",
            bin.display()
        );
        let out = run_sh(&script);
        let args = std::fs::read_to_string(&args_file).unwrap_or_default();
        (out, args)
    }

    #[test]
    fn deploy_ssh_sends_keepalives_so_a_lost_link_ends_the_session() {
        let (out, args) = run_ssh_step(0);
        assert!(out.status.success(), "{out:?}");
        let args: Vec<&str> = args.lines().collect();
        assert!(
            args.contains(&"ServerAliveInterval=5"),
            "ssh args: {args:?}"
        );
        assert!(
            args.contains(&"ServerAliveCountMax=3"),
            "ssh args: {args:?}"
        );
    }

    #[test]
    fn deploy_reports_a_lost_ssh_link_and_how_to_check_the_device() {
        let (out, _) = run_ssh_step(255);
        assert_eq!(out.status.code(), Some(255));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("SSH connection"), "stderr: {err}");
        assert!(err.contains("avocadoctl status"), "stderr: {err}");
        assert!(!err.contains("Device update failed"), "stderr: {err}");
    }

    /// Runs the repo server start and readiness wait on `port`, with an
    /// empty repo. Prints REACHED_READY when the wait passes.
    fn run_repo_server_step(port: u16) -> std::process::Output {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("metadata")).unwrap();
        std::fs::create_dir_all(repo.join("targets")).unwrap();
        let section = script_section(
            &deploy_script(),
            "echo \"TUF repository assembled",
            Some("# Determine the IP the device should use"),
        );
        let script = format!(
            "set -e\nPORT={port}\nREPO_DIR='{}'\n{section}\necho REACHED_READY\n",
            repo.display()
        );
        run_sh(&script)
    }

    #[test]
    fn deploy_refuses_a_repo_port_that_an_old_server_still_holds() {
        if !have_python3() {
            return;
        }
        // An earlier deploy container's server, still bound to the port.
        let stale = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = stale.local_addr().unwrap().port();
        let out = run_repo_server_step(port);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains("REACHED_READY"),
            "the readiness wait accepted a server from another run\nstderr: {stderr}"
        );
        assert!(!out.status.success());
        assert!(stderr.contains("already in use"), "stderr: {stderr}");
        assert!(
            stderr.contains("AVOCADO_DEPLOY_REPO_PORT"),
            "stderr: {stderr}"
        );
        drop(stale);
    }

    #[test]
    fn deploy_repo_server_becomes_ready_on_a_free_port() {
        if !have_python3() {
            return;
        }
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let out = run_repo_server_step(port);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("REACHED_READY"),
            "stdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn deploy_readiness_probe_accepts_only_this_runs_server() {
        if !have_python3() {
            return;
        }
        let script = deploy_script();
        let probe = script_section(&script, "repo_serves_nonce() {", Some("\n}\n")) + "\n}\n";
        let probe_with = |port: u16, nonce: &str| {
            run_sh(&format!(
                "PORT={port}\nDEPLOY_NONCE={nonce}\n{probe}\nrepo_serves_nonce"
            ))
            .status
            .success()
        };

        // A listener that accepts and never answers, like a stuck old server.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!probe_with(silent.local_addr().unwrap().port(), "abc"));

        // A real server that serves another run's nonce, then this run's.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".avocado-deploy-nonce"), "abc").unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut server = std::process::Command::new("python3")
            .args(["-m", "http.server", "--bind", "127.0.0.1"])
            .arg(port.to_string())
            .arg("--directory")
            .arg(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut up = false;
        for _ in 0..100 {
            if probe_with(port, "abc") {
                up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let other = probe_with(port, "xyz");
        let _ = server.kill();
        let _ = server.wait();
        assert!(up, "the probe never saw this run's nonce");
        assert!(!other, "the probe accepted another run's nonce");
    }

    #[test]
    fn deploy_interrupt_still_exits_once_the_repo_server_cleanup_is_set() {
        // In the container the entrypoint's INT/TERM traps come first, then
        // the deploy script sets its own EXIT cleanup. The INT trap must
        // still end the script.
        let entry = crate::utils::container::SdkContainer::new()
            .create_entrypoint_script(true, None, None, "x86_64", false, false, false);
        let traps = script_section(&entry, "cleanup() {", Some("trap 'cleanup 130' INT"))
            + "trap 'cleanup 130' INT\n";
        let server_cleanup =
            script_section(&deploy_script(), "HTTP_PID=$!", Some("\nREPO_READY=\n"));
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "{traps}\nREPO_DIR='{}'\n{server_cleanup}\nkill -INT $$\nsleep 1\necho AFTER_INT\n",
            dir.path().join("repo").display()
        );
        let out = run_sh(&script);
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("AFTER_INT"),
            "the script kept running after SIGINT"
        );
        assert_eq!(out.status.code(), Some(130), "{out:?}");
    }
}
