//! Stamp-based state tracking for avocado CLI commands.
//!
//! This module implements a stamp/manifest system inspired by industry-standard build tools
//! (Cargo fingerprints, Nix derivations, Bazel action cache) that:
//!
//! 1. Tracks successful completion of each command at per-component granularity
//! 2. Detects staleness via content-addressable hashing (config + package list)
//! 3. Enforces command ordering with dependency resolution from config

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;

/// Get the local machine's CPU architecture
///
/// Returns the architecture string (e.g., "x86_64", "aarch64") for the current machine.
/// This is used to track which host architecture the SDK was installed for.
pub fn get_local_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64"
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        std::env::consts::ARCH
    }
}

/// Current stamp format version. Bumped from 1 → 2 in the per-step input-hash
/// rework: each component step now has its own narrow hash, so old stamps
/// written under the broader shared hashes cannot be compared with current
/// inputs. Any stamp at an older version is treated as stale.
pub const STAMP_VERSION: u32 = 2;

/// Command types that can have stamps
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StampCommand {
    Install,
    Build,
    Image,
    Sign,
    Provision,
    CompileDeps,
}

impl fmt::Display for StampCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StampCommand::Install => write!(f, "install"),
            StampCommand::Build => write!(f, "build"),
            StampCommand::Image => write!(f, "image"),
            StampCommand::Sign => write!(f, "sign"),
            StampCommand::Provision => write!(f, "provision"),
            StampCommand::CompileDeps => write!(f, "compile-deps"),
        }
    }
}

/// Component types that can have stamps
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StampComponent {
    Sdk,
    Extension,
    Runtime,
    Rootfs,
    Initramfs,
}

impl fmt::Display for StampComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StampComponent::Sdk => write!(f, "sdk"),
            StampComponent::Extension => write!(f, "ext"),
            StampComponent::Runtime => write!(f, "runtime"),
            StampComponent::Rootfs => write!(f, "rootfs"),
            StampComponent::Initramfs => write!(f, "initramfs"),
        }
    }
}

/// Input hashes that determine if a stamp is stale
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StampInputs {
    /// Hash of the relevant config section (e.g., sdk.dependencies, ext.<name>.dependencies)
    pub config_hash: String,
    /// Hash of the declared package list from config
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_list_hash: Option<String>,
}

impl StampInputs {
    /// Create new stamp inputs with config hash
    pub fn new(config_hash: String) -> Self {
        Self {
            config_hash,
            package_list_hash: None,
        }
    }

    /// Create stamp inputs with both hashes. Used by the sysroot install
    /// steps, which fold the lockfile pins in force at install time into
    /// `package_list_hash` so a re-pin invalidates independently of config.
    pub fn with_package_list(config_hash: String, package_list_hash: String) -> Self {
        Self {
            config_hash,
            package_list_hash: Some(package_list_hash),
        }
    }
}

/// Output state captured after successful command
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StampOutputs {
    /// Hash of the installed package list (name-version-release)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_packages_hash: Option<String>,
    /// Number of packages installed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_count: Option<u32>,
}

/// A stamp representing successful completion of a command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    /// Stamp format version
    pub version: u32,
    /// Command that was executed
    pub command: StampCommand,
    /// Component type
    pub component: StampComponent,
    /// Component name (e.g., extension name, runtime name). None for SDK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_name: Option<String>,
    /// Target architecture
    pub target: String,
    /// When the command completed successfully
    pub timestamp: DateTime<Utc>,
    /// Whether the command succeeded
    pub success: bool,
    /// Input hashes used for staleness detection
    pub inputs: StampInputs,
    /// Output state captured after success
    pub outputs: StampOutputs,
    /// CLI version that wrote the stamp
    pub cli_version: String,
}

impl Stamp {
    /// Create a new stamp for a successful command
    pub fn new(
        command: StampCommand,
        component: StampComponent,
        component_name: Option<String>,
        target: String,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self {
            version: STAMP_VERSION,
            command,
            component,
            component_name,
            target,
            timestamp: Utc::now(),
            success: true,
            inputs,
            outputs,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Create SDK install stamp
    pub fn sdk_install(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Install,
            StampComponent::Sdk,
            None,
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create compile-deps install stamp
    ///
    /// Tracks the target-sysroot compile dependencies installation.
    /// Stored under `sdk/{host_arch}/compile-deps.stamp`.
    pub fn compile_deps_install(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::CompileDeps,
            StampComponent::Sdk,
            None,
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create extension install stamp
    pub fn ext_install(
        name: &str,
        target: &str,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self::new(
            StampCommand::Install,
            StampComponent::Extension,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create extension build stamp
    pub fn ext_build(name: &str, target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Build,
            StampComponent::Extension,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create extension image stamp
    pub fn ext_image(name: &str, target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Image,
            StampComponent::Extension,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create runtime install stamp
    pub fn runtime_install(
        name: &str,
        target: &str,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self::new(
            StampCommand::Install,
            StampComponent::Runtime,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create runtime build stamp
    pub fn runtime_build(
        name: &str,
        target: &str,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self::new(
            StampCommand::Build,
            StampComponent::Runtime,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create runtime sign stamp
    pub fn runtime_sign(
        name: &str,
        target: &str,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self::new(
            StampCommand::Sign,
            StampComponent::Runtime,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create runtime provision stamp
    pub fn runtime_provision(
        name: &str,
        target: &str,
        inputs: StampInputs,
        outputs: StampOutputs,
    ) -> Self {
        Self::new(
            StampCommand::Provision,
            StampComponent::Runtime,
            Some(name.to_string()),
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create rootfs install stamp
    pub fn rootfs_install(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Install,
            StampComponent::Rootfs,
            None,
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create initramfs install stamp
    pub fn initramfs_install(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Install,
            StampComponent::Initramfs,
            None,
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Get the stamp file path relative to $AVOCADO_PREFIX/.stamps/
    ///
    /// For SDK stamps, the path includes the target architecture (which represents
    /// the host architecture where the SDK runs) to support --runs-on with different architectures.
    pub fn relative_path(&self) -> String {
        match (&self.component, &self.component_name) {
            (StampComponent::Sdk, _) => format!("sdk/{}/{}.stamp", self.target, self.command),
            (StampComponent::Extension, Some(name)) => {
                format!("ext/{}/{}.stamp", name, self.command)
            }
            (StampComponent::Runtime, Some(name)) => {
                format!("runtime/{}/{}.stamp", name, self.command)
            }
            (StampComponent::Rootfs, _) => format!("rootfs/{}.stamp", self.command),
            (StampComponent::Initramfs, _) => format!("initramfs/{}.stamp", self.command),
            _ => panic!("Component name required for Extension and Runtime"),
        }
    }

    /// Check if the stamp inputs match the current inputs
    pub fn is_current(&self, current_inputs: &StampInputs) -> bool {
        // Stamp format version must match — older stamps were written
        // against the pre-split shared hash functions and cannot be
        // compared against the new narrower per-step hashes.
        if self.version != STAMP_VERSION {
            return false;
        }

        // Config hash must always match
        if self.inputs.config_hash != current_inputs.config_hash {
            return false;
        }

        // If both have package list hashes, they must match
        if let (Some(stamp_pkg), Some(current_pkg)) = (
            &self.inputs.package_list_hash,
            &current_inputs.package_list_hash,
        ) {
            if stamp_pkg != current_pkg {
                return false;
            }
        }

        true
    }

    /// Serialize to JSON
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("Failed to serialize stamp to JSON")
    }

    /// Deserialize from JSON
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("Failed to parse stamp JSON")
    }
}

/// A requirement for a stamp that must exist before a command can proceed
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StampRequirement {
    pub command: StampCommand,
    pub component: StampComponent,
    pub component_name: Option<String>,
    /// Host architecture for SDK stamps (e.g., "x86_64", "aarch64").
    /// This tracks the CPU architecture of the machine running the SDK container,
    /// which is different from the target architecture (what you're building FOR).
    /// Required for SDK stamps to support --runs-on with different architectures.
    pub host_arch: Option<String>,
}

impl StampRequirement {
    pub fn new(command: StampCommand, component: StampComponent, name: Option<&str>) -> Self {
        Self {
            command,
            component,
            component_name: name.map(|s| s.to_string()),
            host_arch: None,
        }
    }

    /// SDK install requirement for the local host architecture
    pub fn sdk_install() -> Self {
        Self::sdk_install_for_arch(get_local_arch())
    }

    /// SDK install requirement for a specific host architecture
    ///
    /// Use this when checking SDK stamps for --runs-on with a remote host
    /// that may have a different architecture than the local machine.
    pub fn sdk_install_for_arch(arch: &str) -> Self {
        Self {
            command: StampCommand::Install,
            component: StampComponent::Sdk,
            component_name: None,
            host_arch: Some(arch.to_string()),
        }
    }

    /// Compile-deps install requirement for the local host architecture
    pub fn compile_deps_install() -> Self {
        Self::compile_deps_install_for_arch(get_local_arch())
    }

    /// Compile-deps install requirement for a specific host architecture
    pub fn compile_deps_install_for_arch(arch: &str) -> Self {
        Self {
            command: StampCommand::CompileDeps,
            component: StampComponent::Sdk,
            component_name: None,
            host_arch: Some(arch.to_string()),
        }
    }

    /// Extension install requirement
    pub fn ext_install(name: &str) -> Self {
        Self::new(StampCommand::Install, StampComponent::Extension, Some(name))
    }

    /// Extension build requirement
    pub fn ext_build(name: &str) -> Self {
        Self::new(StampCommand::Build, StampComponent::Extension, Some(name))
    }

    /// Extension image requirement
    pub fn ext_image(name: &str) -> Self {
        Self::new(StampCommand::Image, StampComponent::Extension, Some(name))
    }

    /// Runtime install requirement
    pub fn runtime_install(name: &str) -> Self {
        Self::new(StampCommand::Install, StampComponent::Runtime, Some(name))
    }

    /// Runtime build requirement
    pub fn runtime_build(name: &str) -> Self {
        Self::new(StampCommand::Build, StampComponent::Runtime, Some(name))
    }

    /// Runtime sign requirement (used in tests and for API completeness)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn runtime_sign(name: &str) -> Self {
        Self::new(StampCommand::Sign, StampComponent::Runtime, Some(name))
    }

    /// Runtime provision requirement (used in tests and for API completeness)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn runtime_provision(name: &str) -> Self {
        Self::new(StampCommand::Provision, StampComponent::Runtime, Some(name))
    }

    /// Rootfs install requirement
    pub fn rootfs_install() -> Self {
        Self::new(StampCommand::Install, StampComponent::Rootfs, None)
    }

    /// Initramfs install requirement
    pub fn initramfs_install() -> Self {
        Self::new(StampCommand::Install, StampComponent::Initramfs, None)
    }

    /// Get the stamp file path relative to $AVOCADO_PREFIX/.stamps/
    ///
    /// For SDK stamps, the path includes the host architecture to support
    /// running on remotes with different CPU architectures via --runs-on.
    pub fn relative_path(&self) -> String {
        match (&self.component, &self.component_name, &self.host_arch) {
            (StampComponent::Sdk, _, Some(arch)) => {
                format!("sdk/{}/{}.stamp", arch, self.command)
            }
            (StampComponent::Sdk, _, None) => {
                // Fallback for SDK without explicit arch (use local arch)
                format!("sdk/{}/{}.stamp", get_local_arch(), self.command)
            }
            (StampComponent::Extension, Some(name), _) => {
                format!("ext/{}/{}.stamp", name, self.command)
            }
            (StampComponent::Runtime, Some(name), _) => {
                format!("runtime/{}/{}.stamp", name, self.command)
            }
            (StampComponent::Rootfs, _, _) => format!("rootfs/{}.stamp", self.command),
            (StampComponent::Initramfs, _, _) => format!("initramfs/{}.stamp", self.command),
            _ => panic!("Component name required for Extension and Runtime"),
        }
    }

    /// Human-readable description
    pub fn description(&self) -> String {
        match (&self.component, &self.component_name, &self.host_arch) {
            (StampComponent::Sdk, _, Some(arch)) => {
                format!("SDK {} ({})", self.command, arch)
            }
            (StampComponent::Sdk, _, None) => format!("SDK {}", self.command),
            (StampComponent::Extension, Some(name), _) => {
                format!("extension '{}' {}", name, self.command)
            }
            (StampComponent::Runtime, Some(name), _) => {
                format!("runtime '{}' {}", name, self.command)
            }
            (StampComponent::Rootfs, _, _) => format!("rootfs {}", self.command),
            (StampComponent::Initramfs, _, _) => format!("initramfs {}", self.command),
            _ => format!("{} {}", self.component, self.command),
        }
    }

    /// Suggested fix command
    ///
    /// For SDK stamps with a specific host architecture (from --runs-on), the fix
    /// command will suggest running on the remote to install the SDK for that arch.
    #[allow(dead_code)]
    pub fn fix_command(&self) -> String {
        self.fix_command_with_remote(None)
    }

    /// Suggested fix command with optional remote host for --runs-on
    pub fn fix_command_with_remote(&self, runs_on: Option<&str>) -> String {
        match (&self.component, &self.component_name, &self.command) {
            (StampComponent::Sdk, _, StampCommand::Install)
            | (StampComponent::Sdk, _, StampCommand::CompileDeps) => match runs_on {
                Some(remote) => format!("avocado sdk install --runs-on {remote}"),
                None => "avocado sdk install".to_string(),
            },
            (StampComponent::Extension, Some(name), StampCommand::Install) => {
                format!("avocado ext install {name}")
            }
            (StampComponent::Extension, Some(name), StampCommand::Build) => {
                format!("avocado ext build {name}")
            }
            (StampComponent::Extension, Some(name), StampCommand::Image) => {
                format!("avocado ext image {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Install) => {
                format!("avocado runtime install {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Build) => {
                format!("avocado runtime build {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Sign) => {
                format!("avocado runtime sign {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Provision) => {
                format!("avocado runtime provision {name}")
            }
            (StampComponent::Rootfs, _, StampCommand::Install) => {
                "avocado rootfs install".to_string()
            }
            (StampComponent::Initramfs, _, StampCommand::Install) => {
                "avocado initramfs install".to_string()
            }
            _ => format!("avocado {} {}", self.component, self.command),
        }
    }
}

impl fmt::Display for StampRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.relative_path())
    }
}

/// Status of a stamp requirement check
/// Status of a stamp requirement check
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum StampStatus {
    /// Stamp exists and is current (stamp data available for future caching/logging)
    Current(#[allow(unused)] Stamp),
    /// Stamp exists but is stale (inputs changed) - stamp data for future caching
    Stale {
        #[allow(unused)]
        stamp: Stamp,
        reason: String,
    },
    /// Stamp does not exist
    Missing,
}

/// Result of validating all required stamps
#[derive(Debug, Default)]
pub struct StampValidationResult {
    /// Requirements that are satisfied
    pub satisfied: Vec<StampRequirement>,
    /// Requirements that are missing
    pub missing: Vec<StampRequirement>,
    /// Requirements that are stale
    pub stale: Vec<(StampRequirement, String)>,
}

impl StampValidationResult {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if all requirements are satisfied
    pub fn is_satisfied(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty()
    }

    /// Add a satisfied requirement
    pub fn add_satisfied(&mut self, req: StampRequirement) {
        self.satisfied.push(req);
    }

    /// Add a missing requirement
    pub fn add_missing(&mut self, req: StampRequirement) {
        self.missing.push(req);
    }

    /// Add a stale requirement
    pub fn add_stale(&mut self, req: StampRequirement, reason: String) {
        self.stale.push((req, reason));
    }

    /// Convert to an error with actionable messages
    /// Convert to an error with actionable messages
    pub fn into_error(self, context: &str) -> StampValidationError {
        self.into_error_with_runs_on(context, None)
    }

    /// Convert to an error with actionable messages, including --runs-on hint
    pub fn into_error_with_runs_on(
        self,
        context: &str,
        runs_on: Option<&str>,
    ) -> StampValidationError {
        StampValidationError {
            context: context.to_string(),
            missing: self.missing,
            stale: self.stale,
            runs_on: runs_on.map(|s| s.to_string()),
        }
    }
}

/// Error when stamp validation fails
#[derive(Debug)]
pub struct StampValidationError {
    pub context: String,
    pub missing: Vec<StampRequirement>,
    pub stale: Vec<(StampRequirement, String)>,
    /// Remote host if using --runs-on (for fix command suggestions)
    pub runs_on: Option<String>,
}

impl std::error::Error for StampValidationError {}

impl StampValidationError {
    /// Collect unique fix commands, using runs_on hint for SDK install commands
    fn fix_commands(&self) -> Vec<String> {
        let runs_on_ref = self.runs_on.as_deref();
        let local_arch = get_local_arch();

        let mut fixes: Vec<String> = self
            .missing
            .iter()
            .chain(self.stale.iter().map(|(req, _)| req))
            .flat_map(|req| {
                // For SDK install stamps with a different architecture than local,
                // offer both --runs-on and --sdk-arch alternatives
                if req.component == StampComponent::Sdk
                    && req.command == StampCommand::Install
                    && req.host_arch.as_deref() != Some(local_arch)
                {
                    if let Some(arch) = &req.host_arch {
                        let mut cmds = vec![format!("avocado sdk install --sdk-arch {arch}")];
                        if let Some(remote) = runs_on_ref {
                            cmds.push(format!("avocado sdk install --runs-on {remote}"));
                        }
                        return cmds;
                    }
                }
                vec![req.fix_command_with_remote(runs_on_ref)]
            })
            .collect();
        fixes.sort();
        fixes.dedup();
        fixes
    }

    /// The record [`Self::print_and_exit`] emits under `--output json`, where
    /// the prose path is suppressed wholesale (`tui_is_active` is true) and a
    /// consumer would otherwise see a bare exit(1) with no reason. `build` and
    /// `install` always run through JSON under the desktop app.
    fn json_error_event(&self) -> serde_json::Value {
        serde_json::json!({ "event": "error", "message": self.to_string() })
    }

    /// Print the error with formatted [ERROR]/[INFO] tags matching CLI output style,
    /// then exit with a non-zero status code.
    pub fn print_and_exit(&self) -> ! {
        use crate::utils::output::{print_error, print_info, print_warning, OutputLevel};

        // Shut down any active TUI renderer before printing.  When a TUI is
        // active, print_error/print_info are suppressed and process::exit
        // bypasses Drop guards, so without this the error is invisible and the
        // terminal is left in a broken state.
        if let Some(renderer) = crate::utils::tui::get_active_renderer() {
            renderer.shutdown();
        }

        if crate::utils::output_format::is_json_output_active() {
            crate::utils::output_format::emit_json_event(&self.json_error_event());
        }

        print_error(
            &format!("{} - dependencies not satisfied", self.context),
            OutputLevel::Normal,
        );

        if !self.missing.is_empty() {
            print_info("Missing steps:", OutputLevel::Normal);
            for req in &self.missing {
                print_info(
                    &format!("  - {} ({})", req.description(), req.relative_path()),
                    OutputLevel::Normal,
                );
            }
        }

        if !self.stale.is_empty() {
            print_warning("Stale steps (config changed):", OutputLevel::Normal);
            for (req, reason) in &self.stale {
                print_warning(
                    &format!(
                        "  - {} ({}: {})",
                        req.description(),
                        req.relative_path(),
                        reason
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        print_info("To fix:", OutputLevel::Normal);
        for fix in self.fix_commands() {
            print_info(&format!("  {fix}"), OutputLevel::Normal);
        }

        std::process::exit(1);
    }
}

impl fmt::Display for StampValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} - dependencies not satisfied\n", self.context)?;

        if !self.missing.is_empty() {
            writeln!(f, "  Missing steps:")?;
            for req in &self.missing {
                writeln!(f, "    - {} ({})", req.description(), req.relative_path())?;
            }
            writeln!(f)?;
        }

        if !self.stale.is_empty() {
            writeln!(f, "  Stale steps (config changed):")?;
            for (req, reason) in &self.stale {
                writeln!(
                    f,
                    "    - {} ({}: {})",
                    req.description(),
                    req.relative_path(),
                    reason
                )?;
            }
            writeln!(f)?;
        }

        writeln!(f, "To fix:")?;
        for fix in self.fix_commands() {
            writeln!(f, "  {fix}")?;
        }

        Ok(())
    }
}

/// Compute SHA256 hash of a string
pub fn compute_hash(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    let mut hex = String::with_capacity(result.len() * 2);
    for b in result.iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!("sha256:{hex}")
}

/// Compute hash of a YAML value (for config sections)
pub fn compute_config_hash(value: &serde_yaml::Value) -> Result<String> {
    // Serialize to canonical JSON for consistent hashing
    let json = serde_json::to_string(value).context("Failed to serialize config for hashing")?;
    Ok(compute_hash(&json))
}

// ─── Per-step input-hash helpers ────────────────────────────────────────
//
// The hash functions below split each component's inputs into narrow,
// step-scoped subsets. Adding a field to `runtime build`'s hash should NOT
// invalidate `runtime install`'s stamp; this is enforced via separate
// `compute_<component>_<step>_input_hash` functions, each pulling only the
// keys that actually affect that step.
//
// `narrow_kernel_for_hash` and `hash_script_at` are shared building blocks
// to keep the hash-data construction consistent across components.

/// Extract the subset of a `kernel:` YAML block that actually affects what
/// gets installed or built. Returns a fresh mapping with only `package`,
/// `version`, `compile`, `install` keys (when present). Unknown / new
/// fields are deliberately ignored so cosmetic kernel-block edits
/// (comments, metadata, future additions that don't drive selection) do
/// not invalidate stamps.
fn narrow_kernel_for_hash(kernel: &serde_yaml::Value) -> serde_yaml::Value {
    let mut out = serde_yaml::Mapping::new();
    for key in ["package", "version", "compile", "install"] {
        if let Some(v) = kernel.get(key) {
            out.insert(serde_yaml::Value::String(key.to_string()), v.clone());
        }
    }
    serde_yaml::Value::Mapping(out)
}

/// Hash the contents of a project-relative script file. The returned
/// string is embedded into a hash mapping alongside the original relative
/// path so the stamp invalidates on either (a) path changes, or (b)
/// script-content edits.
///
/// Missing files hash to the literal `"missing"` sentinel — that way, a
/// stamp written when the file existed will invalidate if the file is
/// later removed, and adding the file later (path unchanged) invalidates
/// the old "missing" stamp.
fn hash_script_at(project_root: &Path, rel_path: &str) -> String {
    let abs = project_root.join(rel_path);
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let result = hasher.finalize();
            let mut hex = String::with_capacity(result.len() * 2);
            for b in result.iter() {
                use std::fmt::Write;
                let _ = write!(hex, "{b:02x}");
            }
            format!("sha256:{hex}")
        }
        Err(_) => "missing".to_string(),
    }
}

/// Build the `{path, content_sha256}` mapping that we embed into input
/// hashes for `post_build` / `post_install` hooks. Both fields go into
/// the parent mapping so a path swap OR a content edit invalidates.
fn script_hash_value(project_root: &Path, rel_path: &str) -> serde_yaml::Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert(
        serde_yaml::Value::String("path".to_string()),
        serde_yaml::Value::String(rel_path.to_string()),
    );
    m.insert(
        serde_yaml::Value::String("content_sha256".to_string()),
        serde_yaml::Value::String(hash_script_at(project_root, rel_path)),
    );
    serde_yaml::Value::Mapping(m)
}

/// Fold a digest of an overlay's tree into `hash_data` under `key`, so that an
/// edit to any overlay file forces a rebuild. The overlay is applied to the
/// sysroot by a plain `cp` (not RPM), so without this its file contents are
/// invisible to the install stamp and a change silently never reaches the image
/// (ENG-2440). A verbatim overlay hashes raw bytes; one that opts into
/// preprocessing (`overlay: { ..., preprocess: ... }`) hashes the post-`{{ }}`
/// content, so a changed template value (e.g. a new claim token) invalidates
/// too. Only the SHA-256 is stored — never the resolved plaintext.
// The (target, runtime, cli_target_board) trio mirrors the interpolation
// context the materialize step builds; bundling them into a context struct is
// the right cleanup once a fourth CLI override lands.
#[allow(clippy::too_many_arguments)]
fn fold_overlay_content_hash(
    hash_data: &mut serde_yaml::Mapping,
    key: &str,
    overlay: &serde_yaml::Value,
    config: &serde_yaml::Value,
    project_root: &Path,
    target: Option<&str>,
    runtime: Option<&str>,
    cli_target_board: Option<&str>,
) -> Result<()> {
    use crate::utils::overlay_preprocess::{parse_overlay_config, PreprocessSpec};
    let spec = PreprocessSpec::from_overlay_value(overlay);
    // `dir` via the shared parser so a bare-string overlay (`overlay: mydir`)
    // hashes the right tree, not the "overlay" default.
    let (dir, _opaque) = parse_overlay_config(overlay);
    // Build the same interpolation context the build's materialize step uses, so
    // the digest reflects the exact rendered overlay content. `target` keeps
    // `{{ avocado.target }}` accurate and `cli_target_board` keeps
    // `{{ avocado.target.board }}` accurate (so a --target-board switch
    // invalidates the stamp); `runtime` (for the ext path) makes
    // `{{ avocado.runtime }}`-dependent content invalidate the stamp when the
    // selected runtime changes — the ext-build stamp isn't otherwise runtime-keyed.
    let mut context = crate::utils::interpolation::AvocadoContext::from_main_config(
        config,
        target,
        cli_target_board,
    );
    if let Some(rt) = runtime {
        context.runtime = Some(rt.to_string());
    }
    // Propagate digest errors rather than dropping the content hash, which would
    // let a broken overlay silently skip rebuild invalidation.
    if let Some(digest) = crate::utils::overlay_preprocess::overlay_content_digest(
        project_root,
        &dir,
        &spec,
        config,
        &context,
    )? {
        hash_data.insert(
            serde_yaml::Value::String(key.to_string()),
            serde_yaml::Value::String(digest),
        );
    }
    Ok(())
}

/// Compute input hash for SDK install
///
/// Includes only inputs that affect the SDK toolchain install itself:
/// `sdk.packages`, `sdk.image`, `sdk.repo_url`, `sdk.repo_release`.
///
/// **Does NOT include `rootfs.packages` / `initramfs.packages`** —
/// the rootfs and initramfs sysroots are populated by separate
/// `rootfs install` / `initramfs install` steps with their own stamps.
/// The orchestrating `avocado sdk install` command writes each of those
/// stamps independently, so a rootfs-package change invalidates only
/// the rootfs-install stamp and not the entire SDK toolchain install.
pub fn compute_sdk_input_hash(config: &serde_yaml::Value) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    if let Some(sdk) = config.get("sdk") {
        if let Some(deps) = sdk.get("packages") {
            hash_data.insert(
                serde_yaml::Value::String("sdk.dependencies".to_string()),
                deps.clone(),
            );
        }
        if let Some(image) = sdk.get("image") {
            hash_data.insert(
                serde_yaml::Value::String("sdk.image".to_string()),
                image.clone(),
            );
        }
        if let Some(repo_url) = sdk.get("repo_url") {
            hash_data.insert(
                serde_yaml::Value::String("sdk.repo_url".to_string()),
                repo_url.clone(),
            );
        }
        if let Some(repo_release) = sdk.get("repo_release") {
            hash_data.insert(
                serde_yaml::Value::String("sdk.repo_release".to_string()),
                repo_release.clone(),
            );
        }
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for extension install
/// Includes: ext.<name>.dependencies
/// Compute input hash for compile-deps install
///
/// Includes the sorted set of active compile section names and their packages.
/// When active runtimes change, the set of active compile sections changes,
/// causing this hash to change and the stamp to become stale.
pub fn compute_compile_deps_input_hash(
    config: &serde_yaml::Value,
    active_compile_sections: &[String],
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // Include sorted list of active compile section names
    let sections_value = serde_yaml::Value::Sequence(
        active_compile_sections
            .iter()
            .map(|s| serde_yaml::Value::String(s.clone()))
            .collect(),
    );
    hash_data.insert(
        serde_yaml::Value::String("active_compile_sections".to_string()),
        sections_value,
    );

    // Include the packages from each active compile section
    if let Some(sdk) = config.get("sdk") {
        if let Some(compile) = sdk.get("compile") {
            for section_name in active_compile_sections {
                if let Some(section) = compile.get(section_name) {
                    if let Some(packages) = section.get("packages") {
                        hash_data.insert(
                            serde_yaml::Value::String(format!(
                                "sdk.compile.{section_name}.packages"
                            )),
                            packages.clone(),
                        );
                    }
                }
            }
        }
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for **extension install**, folding in the state of the
/// extensions this one was seeded from (`dep_state` empty for an extension
/// with no dependencies).
///
/// Includes only inputs that affect the package-install step:
/// - `ext.<name>.packages` (what gets installed)
/// - `ext.<name>.types` (sysext/confext drives a small set of auto-included packages)
/// - `ext.<name>.source` (where the extension is fetched from)
///
/// Deliberately excludes `image`, `var_files`, `subvolumes`, `post_build`,
/// `filesystem`, `permissions`, `overlay`, `version`, and all merge/service
/// fields — those affect build/image output, not what gets installed.
///
/// An extension de-duplicated against a dependency only ships the files that
/// dependency does *not* provide, so its image is a function of the
/// dependency's contents as well as its own config. Without that in the hash,
/// changing a dependency leaves every dependent's stamp valid and their
/// sysroots stale.
///
/// The dangerous direction is subtle: if a dependency **drops** a package, the
/// dependency rebuilds correctly while the dependent keeps an image that
/// omitted those files precisely because the dependency used to supply them.
/// Nothing then provides them, and the gap only appears in the merged `/usr`
/// on-device. Over-invalidating costs a rebuild; under-invalidating ships a
/// broken image.
///
/// `dep_state` is `(dependency name, fingerprint)` — typically the
/// dependency's resolved package versions plus its own source version, taken
/// from the lock. Topological install order guarantees those are current
/// before a dependent's hash is computed.
///
/// Known gap: a fingerprint built from the lock's declared packages does not
/// move when a *transitive* rpm dependency drifts beneath the dependency (say
/// openssl bumping under openssh while openssh's own version holds). Catching
/// that needs the dependency's full sysroot NVRA set.
pub fn compute_ext_install_input_hash_with_deps(
    config: &serde_yaml::Value,
    ext_name: &str,
    dep_state: &[(String, String)],
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    if !dep_state.is_empty() {
        // Sorted so the hash does not depend on map iteration order.
        let mut sorted = dep_state.to_vec();
        sorted.sort();
        let mut deps = serde_yaml::Mapping::new();
        for (name, fingerprint) in sorted {
            deps.insert(
                serde_yaml::Value::String(name),
                serde_yaml::Value::String(fingerprint),
            );
        }
        hash_data.insert(
            serde_yaml::Value::String(format!("ext.{ext_name}.seeded_from")),
            serde_yaml::Value::Mapping(deps),
        );
    }

    if let Some(ext) = config.get("extensions").and_then(|e| e.get(ext_name)) {
        if let Some(deps) = ext.get("packages") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.dependencies")),
                deps.clone(),
            );
        }
        if let Some(types) = ext.get("types") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.types")),
                types.clone(),
            );
        }
        if let Some(source) = ext.get("source") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.source")),
                source.clone(),
            );
        }
        // The DECLARED dependency edges, independent of `dep_state`. dep_state
        // carries the dependencies' resolved content, but it is reconstructed
        // from the graph and the lock — and when either is unavailable the
        // reader degrades to an empty dep_state. Without this field, that
        // degraded hash equals a pre-`depends_on` stamp exactly (the plain
        // hash never saw the edges), so a freshly added dependency could
        // validate against a sysroot never seeded from it. Folding the config
        // value in makes the degradation genuinely one-directional: any edit
        // to `depends_on` moves the hash whether or not the lock loads.
        if let Some(depends_on) = ext.get("depends_on") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.depends_on")),
                depends_on.clone(),
            );
        }
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Fingerprint one dependency for `seeded_from` hashing: the dependency's
/// resolved source version plus its resolved package versions, from the lock.
///
/// Shared by the stamp WRITER (`ext install`) and the stamp READERS
/// (`ext build` / `ext image` via
/// [`compute_ext_install_input_hash_current`]): the two sides drifting is
/// exactly the bug this function exists to prevent — install stamped a
/// deps-aware hash while build/image validated a plain one, so every
/// `depends_on` extension read as stale forever and died at build.
pub fn ext_dep_fingerprint(
    lock_file: &crate::utils::lockfile::LockFile,
    target: &str,
    graph: &crate::utils::ext_deps::DependencyGraph,
    dep: &str,
) -> String {
    let mut memo: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut visiting: std::collections::HashSet<String> = std::collections::HashSet::new();
    ext_dep_fingerprint_inner(lock_file, target, graph, dep, &mut visiting, &mut memo)
}

/// TRANSITIVE on purpose. A dependency's own `source version | package map`
/// does not move when something UNDERNEATH it changes: for `app -> mid ->
/// base`, a base package change rebuilds mid's sysroot (the rpmdb app was
/// seeded from) while mid's own lock rows stay put — so a fingerprint of
/// mid's own state alone left app's stamp valid over a changed seed. Each
/// fingerprint therefore folds in the fingerprints of the dependency's own
/// dependencies, sorted, so a change anywhere in the chain reaches every
/// downstream dependent.
///
/// Package versions are read through the any-scope accessor: a
/// runtime-scoped install records them only under
/// `runtimes.<r>.extensions.<ext>`, and reading just the global map saw an
/// empty set for those.
///
/// The digest is hashed rather than concatenated so deep chains don't grow
/// the stamp input unboundedly. `resolve` proves the graph acyclic; the
/// `visiting` guard is defensive.
fn ext_dep_fingerprint_inner(
    lock_file: &crate::utils::lockfile::LockFile,
    target: &str,
    graph: &crate::utils::ext_deps::DependencyGraph,
    dep: &str,
    visiting: &mut std::collections::HashSet<String>,
    memo: &mut std::collections::HashMap<String, String>,
) -> String {
    if let Some(hit) = memo.get(dep) {
        return hit.clone();
    }
    if !visiting.insert(dep.to_string()) {
        return "<cycle>".to_string();
    }

    let versions = lock_file
        .get_extension_packages_any_scope(target, dep)
        .map(|pkgs| {
            let mut v: Vec<String> = pkgs.iter().map(|(k, val)| format!("{k}={val:?}")).collect();
            v.sort();
            v.join(",")
        })
        .unwrap_or_default();
    let source = lock_file
        .get_extension_source(target, dep)
        .and_then(|s| s.version.clone())
        .unwrap_or_default();

    let mut state = format!("{source}|{versions}");
    if let Some(node) = graph.get(dep) {
        let mut dep_names: Vec<&str> = node.depends_on.iter().map(|d| d.name.as_str()).collect();
        dep_names.sort();
        for name in dep_names {
            let sub = ext_dep_fingerprint_inner(lock_file, target, graph, name, visiting, memo);
            state.push_str(&format!("|{name}={sub}"));
        }
    }

    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(state.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    visiting.remove(dep);
    memo.insert(dep.to_string(), digest.clone());
    digest
}

/// Compute the ext-install input hash the way `ext install` STAMPS it, for a
/// reader validating that stamp.
///
/// Resolves the extension's direct `depends_on` edges from the composed
/// config's dependency graph and fingerprints each from the lock, then folds
/// them in via [`compute_ext_install_input_hash_with_deps`]. An extension
/// with no dependencies degrades to the plain hash, byte-identical to what a
/// dependency-free install stamped.
///
/// Degraded inputs fall back to an empty `dep_state` on purpose — and that is
/// only safe because the hash also folds in the declared `depends_on` config
/// value directly. Without that field, an empty-dep_state fallback would be
/// byte-identical to a pre-`depends_on` stamp, so a freshly declared
/// dependency plus a broken graph or unloadable lock would VALIDATE a sysroot
/// never seeded from it. With it, any `depends_on` edit moves the hash
/// unconditionally, so the degradation can only read STALE —
/// over-invalidation costs a rebuild, which is the correct failure direction
/// for a validator.
pub fn compute_ext_install_input_hash_current(
    composed: &crate::utils::config::ComposedConfig,
    ext_name: &str,
    target: &str,
    lock_src_dir: &Path,
) -> Result<StampInputs> {
    let dep_names: Vec<String> =
        match crate::utils::ext_deps::DependencyGraph::from_composed(composed, target) {
            Ok(graph) => graph
                .get(ext_name)
                .map(|node| node.depends_on.iter().map(|d| d.name.clone()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

    let dep_state: Vec<(String, String)> = if dep_names.is_empty() {
        Vec::new()
    } else {
        match (
            crate::utils::lockfile::LockFile::load(lock_src_dir),
            crate::utils::ext_deps::DependencyGraph::from_composed(composed, target),
        ) {
            (Ok(lock_file), Ok(graph)) => dep_names
                .iter()
                .map(|dep| {
                    (
                        dep.clone(),
                        ext_dep_fingerprint(&lock_file, target, &graph, dep),
                    )
                })
                .collect(),
            _ => Vec::new(),
        }
    };

    compute_ext_install_input_hash_with_deps(&composed.merged_value, ext_name, &dep_state)
}

/// Compute input hash for **extension build**.
///
/// Includes the install inputs (so a package change invalidates build too)
/// plus build-only inputs: `image` (kabtool args), `overlay`, and the
/// `post_build` hook (both the relative path and its file content).
///
/// Excludes `var_files`, `subvolumes`, and the resolved `filesystem` —
/// those only affect the image step.
pub fn compute_ext_build_input_hash(
    config: &serde_yaml::Value,
    ext_name: &str,
    project_root: &Path,
    target: Option<&str>,
    runtime: Option<&str>,
    cli_target_board: Option<&str>,
) -> Result<StampInputs> {
    let hash_data = ext_build_hash_data(
        config,
        ext_name,
        project_root,
        target,
        runtime,
        cli_target_board,
    )?;
    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for **extension image**.
///
/// Includes the build inputs plus image-only inputs: `var_files`,
/// `subvolumes`, and the resolved `filesystem` format.
pub fn compute_ext_image_input_hash(
    config: &serde_yaml::Value,
    ext_name: &str,
    filesystem: Option<&str>,
    project_root: &Path,
    target: Option<&str>,
    runtime: Option<&str>,
    cli_target_board: Option<&str>,
) -> Result<StampInputs> {
    let mut hash_data = ext_build_hash_data(
        config,
        ext_name,
        project_root,
        target,
        runtime,
        cli_target_board,
    )?;

    if let Some(ext) = config.get("extensions").and_then(|e| e.get(ext_name)) {
        if let Some(var_files) = ext.get("var_files") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.var_files")),
                var_files.clone(),
            );
        }
        if let Some(subvolumes) = ext.get("subvolumes") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.subvolumes")),
                subvolumes.clone(),
            );
        }
    }
    if let Some(fs) = filesystem {
        hash_data.insert(
            serde_yaml::Value::String(format!("ext.{ext_name}.filesystem")),
            serde_yaml::Value::String(fs.to_string()),
        );
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Shared mapping construction for `ext build` (and the subset used by
/// `ext image`). Keeping both steps' shared inputs in one place avoids
/// drift between the two hash functions.
fn ext_build_hash_data(
    config: &serde_yaml::Value,
    ext_name: &str,
    project_root: &Path,
    target: Option<&str>,
    runtime: Option<&str>,
    cli_target_board: Option<&str>,
) -> Result<serde_yaml::Mapping> {
    let mut hash_data = serde_yaml::Mapping::new();

    if let Some(ext) = config.get("extensions").and_then(|e| e.get(ext_name)) {
        // Install-time inputs are also build-time inputs — a package change
        // invalidates everything downstream.
        if let Some(deps) = ext.get("packages") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.dependencies")),
                deps.clone(),
            );
        }
        if let Some(types) = ext.get("types") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.types")),
                types.clone(),
            );
        }
        if let Some(source) = ext.get("source") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.source")),
                source.clone(),
            );
        }
        // Build-only inputs.
        if let Some(image) = ext.get("image") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.image")),
                image.clone(),
            );
        }
        if let Some(overlay) = ext.get("overlay") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.overlay")),
                overlay.clone(),
            );
            fold_overlay_content_hash(
                &mut hash_data,
                &format!("ext.{ext_name}.overlay_content"),
                overlay,
                config,
                project_root,
                target,
                runtime,
                cli_target_board,
            )?;
        }
        if let Some(post_build) = ext.get("post_build").and_then(|v| v.as_str()) {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.post_build")),
                script_hash_value(project_root, post_build),
            );
        }
    }

    Ok(hash_data)
}

/// The inputs to a sysroot install stamp that cannot be read out of the
/// merged YAML: the *effective* package set (config default already
/// applied), the SDK feed identity the install resolves against, and the
/// lockfile pins currently in force.
///
/// These are what make the hash trustworthy enough to *skip* an install on.
/// Hashing the raw `rootfs.packages` node alone cannot tell an absent
/// section from one that spells out the default meta-package, and says
/// nothing about a snapshot bump or a hand-edited `avocado.lock`.
pub struct SysrootStampInputs<'a> {
    /// Effective package map — `Config::get_{rootfs,initramfs}_packages`.
    pub packages: &'a std::collections::HashMap<String, serde_yaml::Value>,
    /// `sdk.repo_url`: a feed switch must invalidate.
    pub repo_url: Option<&'a str>,
    /// `sdk.repo_release`: the resolved snapshot, which `avocado update` moves.
    pub repo_release: Option<&'a str>,
    /// `sdk.disable_weak_dependencies`: changes what dnf pulls in.
    pub disable_weak_dependencies: bool,
    /// `--dnf-args`, which are interpolated straight into the install
    /// transaction. Part of the hash because they change what the transaction
    /// resolves (`--enablerepo=…`), so an up-to-date sysroot must not
    /// short-circuit past a run that passes different ones. `--force` is
    /// deliberately *not* here: it only selects `-y` and interactivity, not
    /// content.
    pub dnf_args: Option<&'a [String]>,
    /// Locked NVR pins for this sysroot, as recorded in `avocado.lock`.
    pub locked_packages: Option<&'a std::collections::HashMap<String, String>>,
}

/// Digest of a sysroot's lockfile pins as `name=version` lines ordered by
/// name.
///
/// Deliberately always returns a hash rather than `None` for an empty pin
/// set: [`Stamp::is_current`] only compares `package_list_hash` when *both*
/// sides carry one, so returning `None` after `avocado unlock` cleared the
/// section would let a stamp written against real pins compare equal by
/// omission. An empty set hashing to its own distinct value makes that read
/// as stale.
fn package_list_hash(locked: Option<&std::collections::HashMap<String, String>>) -> String {
    let mut lines: Vec<String> = locked
        .map(|pins| {
            pins.iter()
                .map(|(name, version)| format!("{name}={version}"))
                .collect()
        })
        .unwrap_or_default();
    lines.sort();
    compute_hash(&lines.join("\n"))
}

/// Render the effective package map as a deterministically ordered YAML
/// mapping. `HashMap` iteration order varies per process and
/// [`compute_config_hash`] serializes in insertion order, so the keys have
/// to be sorted here or the hash is unstable between runs.
fn packages_for_hash(
    packages: &std::collections::HashMap<String, serde_yaml::Value>,
) -> serde_yaml::Value {
    let mut names: Vec<&String> = packages.keys().collect();
    names.sort();
    let mut out = serde_yaml::Mapping::new();
    for name in names {
        out.insert(
            serde_yaml::Value::String(name.clone()),
            packages[name].clone(),
        );
    }
    serde_yaml::Value::Mapping(out)
}

/// Shared input-hash core for the rootfs and initramfs installs, which take
/// identical inputs under different config sections. `section` is the
/// top-level key (`"rootfs"` / `"initramfs"`).
fn compute_sysroot_install_input_hash(
    section: &str,
    config: &serde_yaml::Value,
    project_root: &Path,
    cli_target_board: Option<&str>,
    resolved: &SysrootStampInputs<'_>,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // The effective set, not the raw `<section>.packages` node — an absent
    // section and one that names the default meta-package install the same
    // thing and must hash the same.
    hash_data.insert(
        serde_yaml::Value::String(format!("{section}.packages")),
        packages_for_hash(resolved.packages),
    );

    if let Some(sysroot) = config.get(section) {
        if let Some(overlay) = sysroot.get("overlay") {
            hash_data.insert(
                serde_yaml::Value::String(format!("{section}.overlay")),
                overlay.clone(),
            );
            fold_overlay_content_hash(
                &mut hash_data,
                &format!("{section}.overlay_content"),
                overlay,
                config,
                project_root,
                None,
                None,
                cli_target_board,
            )?;
        }
        if let Some(post_install) = sysroot.get("post_install").and_then(|v| v.as_str()) {
            hash_data.insert(
                serde_yaml::Value::String(format!("{section}.post_install")),
                script_hash_value(project_root, post_install),
            );
        }
    }

    if let Some(kernel) = config.get("kernel") {
        hash_data.insert(
            serde_yaml::Value::String("kernel".to_string()),
            narrow_kernel_for_hash(kernel),
        );
    }

    // Feed identity and resolver flags. A snapshot bump, a feed switch, or a
    // weak-deps flip all change what lands in the sysroot even when every
    // config section above is byte-identical.
    for (key, value) in [
        ("sdk.repo_url", resolved.repo_url),
        ("sdk.repo_release", resolved.repo_release),
    ] {
        if let Some(v) = value {
            hash_data.insert(
                serde_yaml::Value::String(key.to_string()),
                serde_yaml::Value::String(v.to_string()),
            );
        }
    }
    hash_data.insert(
        serde_yaml::Value::String("sdk.disable_weak_dependencies".to_string()),
        serde_yaml::Value::Bool(resolved.disable_weak_dependencies),
    );
    // The SDK image is what actually runs the install — its dnf/rpm config and
    // scriptlet machinery — so a project that repoints `sdk.image` must not keep
    // a sysroot the old image produced. `compute_sdk_input_hash` already folds
    // it; folding it here too keeps the two stamps from disagreeing about what
    // counts as an input.
    if let Some(image) = config.get("sdk").and_then(|s| s.get("image")) {
        hash_data.insert(
            serde_yaml::Value::String("sdk.image".to_string()),
            image.clone(),
        );
    }
    // Order matters to the caller, not just membership: `--enablerepo=a
    // --enablerepo=b` and its reverse are the same transaction, but hashing the
    // sequence as given is the conservative choice — a reorder invalidates and
    // reinstalls, which is wrong-but-safe, where missing a change is not.
    if let Some(args) = resolved.dnf_args.filter(|a| !a.is_empty()) {
        hash_data.insert(
            serde_yaml::Value::String("dnf_args".to_string()),
            serde_yaml::Value::Sequence(
                args.iter()
                    .map(|a| serde_yaml::Value::String(a.clone()))
                    .collect(),
            ),
        );
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::with_package_list(
        config_hash,
        package_list_hash(resolved.locked_packages),
    ))
}

/// Compute input hash for **rootfs install**.
///
/// Includes the effective `rootfs.packages` set, `rootfs.overlay`, and the
/// narrowed kernel selection (`package`/`version`/`compile`/`install` only —
/// adding an unrelated `kernel.metadata` field does NOT invalidate). Also
/// includes the `post_install` hook path and its file contents so an
/// in-place script edit invalidates without `--no-stamps`, the SDK feed
/// identity, and a digest of the sysroot's lockfile pins.
pub fn compute_rootfs_input_hash(
    config: &serde_yaml::Value,
    project_root: &Path,
    cli_target_board: Option<&str>,
    resolved: &SysrootStampInputs<'_>,
) -> Result<StampInputs> {
    compute_sysroot_install_input_hash("rootfs", config, project_root, cli_target_board, resolved)
}

/// Compute input hash for **initramfs install**.
///
/// Same inputs as [`compute_rootfs_input_hash`], read from the `initramfs`
/// config section.
pub fn compute_initramfs_input_hash(
    config: &serde_yaml::Value,
    project_root: &Path,
    cli_target_board: Option<&str>,
    resolved: &SysrootStampInputs<'_>,
) -> Result<StampInputs> {
    compute_sysroot_install_input_hash(
        "initramfs",
        config,
        project_root,
        cli_target_board,
        resolved,
    )
}

/// Compute input hash for **runtime install**.
///
/// Includes only the inputs that affect the package-install step for the
/// runtime sysroot: `runtime.<name>.packages` (merged with per-target
/// overrides) and `runtime.<name>.target`. Excludes kernel, var, var_files,
/// post_build, rootfs/initramfs filesystem, and extension docker_images —
/// those affect the build step, not what gets installed for the runtime
/// itself.
pub fn compute_runtime_install_input_hash(
    merged_runtime: &serde_yaml::Value,
    runtime_name: &str,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    if let Some(deps) = merged_runtime.get("packages") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.dependencies")),
            deps.clone(),
        );
    }
    if let Some(target) = merged_runtime.get("target") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.target")),
            target.clone(),
        );
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for **runtime build**.
///
/// Includes the install inputs plus build-only inputs: the narrowed
/// kernel selection (`package`/`version`/`compile`/`install` only), the
/// runtime-level `var` and `var_files` config, the `post_build` hook
/// (path + content), the rootfs/initramfs filesystem formats this
/// runtime consumes, and any extension `docker_images` that this runtime
/// needs primed at build time.
pub fn compute_runtime_build_input_hash(
    merged_runtime: &serde_yaml::Value,
    runtime_name: &str,
    parsed: &serde_yaml::Value,
    project_root: &Path,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // Install inputs are also build inputs.
    if let Some(deps) = merged_runtime.get("packages") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.dependencies")),
            deps.clone(),
        );
    }
    if let Some(target) = merged_runtime.get("target") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.target")),
            target.clone(),
        );
    }

    // Build-only inputs.
    if let Some(kernel) = merged_runtime.get("kernel") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.kernel")),
            narrow_kernel_for_hash(kernel),
        );
    }

    if let Some(ext_list) = merged_runtime
        .get("extensions")
        .and_then(|e| e.as_sequence())
    {
        for ext_val in ext_list {
            if let Some(spec) =
                crate::utils::runtime_extension::RuntimeExtensionSpec::parse_entry(ext_val)
            {
                let ext_name = spec.name.as_str();
                if let Some(docker_images) = parsed
                    .get("extensions")
                    .and_then(|e| e.get(ext_name))
                    .and_then(|ext| ext.get("docker_images"))
                {
                    hash_data.insert(
                        serde_yaml::Value::String(format!("ext.{ext_name}.docker_images")),
                        docker_images.clone(),
                    );
                }
                // Device-tree overlays are compiled and delivered at runtime-build
                // time, so a declaration change (or an edit to a .dtso the
                // declaration points at) must invalidate this stamp. Fold both the
                // declaration value and each source's content hash, mirroring how
                // the overlay/post_build keys track file contents.
                if let Some(dtos) = parsed
                    .get("extensions")
                    .and_then(|e| e.get(ext_name))
                    .and_then(|ext| ext.get("device_tree_overlays"))
                {
                    hash_data.insert(
                        serde_yaml::Value::String(format!("ext.{ext_name}.device_tree_overlays")),
                        dtos.clone(),
                    );
                    if let Some(seq) = dtos.as_sequence() {
                        for entry in seq {
                            if let Some(src) = entry.get("src").and_then(|v| v.as_str()) {
                                hash_data.insert(
                                    serde_yaml::Value::String(format!(
                                        "ext.{ext_name}.dtso_content.{src}"
                                    )),
                                    script_hash_value(project_root, src),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(var_files) = merged_runtime.get("var_files") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.var_files")),
            var_files.clone(),
        );
    }
    if let Some(var) = merged_runtime.get("var") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.var")),
            var.clone(),
        );
    }
    if let Some(post_build) = merged_runtime.get("post_build").and_then(|v| v.as_str()) {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.post_build")),
            script_hash_value(project_root, post_build),
        );
    }

    if let Some(rootfs) = parsed.get("rootfs") {
        if let Some(fs) = rootfs.get("filesystem") {
            hash_data.insert(
                serde_yaml::Value::String("rootfs.filesystem".to_string()),
                fs.clone(),
            );
        }
    }
    if let Some(initramfs) = parsed.get("initramfs") {
        if let Some(fs) = initramfs.get("filesystem") {
            hash_data.insert(
                serde_yaml::Value::String("initramfs.filesystem".to_string()),
                fs.clone(),
            );
        }
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Generate shell script to write a stamp file
pub fn generate_write_stamp_script(stamp: &Stamp) -> Result<String> {
    let stamp_json = stamp.to_json()?;
    let stamp_path = stamp.relative_path();

    Ok(format!(
        r#"
# Write stamp file
mkdir -p "$AVOCADO_PREFIX/.stamps/$(dirname '{stamp_path}')"
cat > "$AVOCADO_PREFIX/.stamps/{stamp_path}" << 'STAMP_EOF'
{stamp_json}
STAMP_EOF
# Stamp written (use --verbose to see stamp operations)
"#
    ))
}

/// Generate shell script to write an SDK install stamp with dynamic architecture detection.
///
/// This is used when running with --runs-on where the remote host may have a different
/// architecture than the local machine. The arch is determined at runtime using `uname -m`
/// or the AVOCADO_SDK_ARCH environment variable (set by the entrypoint).
pub fn generate_write_sdk_stamp_script_dynamic_arch(inputs: StampInputs) -> String {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let cli_version = env!("CARGO_PKG_VERSION");

    // Build the stamp JSON with shell variable substitution for the arch
    // Note: We use double quotes for the heredoc to allow $SDK_ARCH substitution
    format!(
        r#"
# Write SDK install stamp with dynamic architecture detection
SDK_ARCH="${{AVOCADO_SDK_ARCH:-$(uname -m)}}"
mkdir -p "$AVOCADO_PREFIX/.stamps/sdk/$SDK_ARCH"
cat > "$AVOCADO_PREFIX/.stamps/sdk/$SDK_ARCH/install.stamp" << STAMP_EOF
{{
  "version": {version},
  "command": "install",
  "component": "sdk",
  "component_name": null,
  "target": "$SDK_ARCH",
  "timestamp": "{timestamp}",
  "success": true,
  "inputs": {{
    "config_hash": "{config_hash}"
  }},
  "outputs": {{}},
  "cli_version": "{cli_version}"
}}
STAMP_EOF
# SDK stamp written for architecture: $SDK_ARCH
"#,
        version = STAMP_VERSION,
        timestamp = timestamp,
        config_hash = inputs.config_hash,
        cli_version = cli_version
    )
}

/// Generate shell script to read a stamp file
/// Generate a single shell script that reads multiple stamps and outputs them in a parseable format.
/// Each stamp is output as: `STAMP_PATH:::JSON_CONTENT` (or `STAMP_PATH:::null` if missing)
/// This allows validating all stamps in a single container invocation.
///
/// Note: The stamp JSON is compacted to a single line for reliable line-based parsing.
pub fn generate_batch_read_stamps_script(requirements: &[StampRequirement]) -> String {
    let mut script_parts = Vec::new();

    for req in requirements {
        let stamp_path = req.relative_path();
        // Output format: PATH:::CONTENT (using ::: as delimiter since it won't appear in JSON)
        // Use jq -c to compact JSON to single line, fall back to tr for systems without jq
        script_parts.push(format!(
            r#"echo -n "{stamp_path}:::"; if [ -f "$AVOCADO_PREFIX/.stamps/{stamp_path}" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/{stamp_path}"; echo; else echo "null"; fi"#
        ));
    }

    script_parts.join("\n")
}

/// Parse the output from `generate_batch_read_stamps_script` into a map of path -> JSON content
pub fn parse_batch_stamps_output(
    output: &str,
) -> std::collections::HashMap<String, Option<String>> {
    let mut result = std::collections::HashMap::new();

    for line in output.lines() {
        if let Some((path, content)) = line.split_once(":::") {
            let json = if content == "null" || content.is_empty() {
                None
            } else {
                Some(content.to_string())
            };
            result.insert(path.to_string(), json);
        }
    }

    result
}

/// A (component, command) key paired with the freshly computed input
/// hash for that specific step. Passed into [`validate_stamps_batch`]
/// so each requirement is compared against the correct step-scoped hash.
pub type CurrentInput<'a> = (StampComponent, StampCommand, &'a StampInputs);

/// Validate all stamp requirements from batch output in a single pass.
///
/// `current_inputs` is a slice of (component, command, hash) triples
/// used for staleness detection. A requirement is matched against the
/// triple whose component AND command both match it. Requirements with
/// no matching entry are validated for existence only — appropriate for
/// dependency stamps (e.g. SDK stamps when building an extension) whose
/// content hash was verified when they were created.
pub fn validate_stamps_batch(
    requirements: &[StampRequirement],
    batch_output: &str,
    current_inputs: &[CurrentInput<'_>],
) -> StampValidationResult {
    validate_stamps_parsed(
        requirements,
        &parse_batch_stamps_output(batch_output),
        current_inputs,
    )
}

/// [`validate_stamps_batch`] for a caller that already parsed the batch output.
///
/// Split out so a holder of the parsed map doesn't have to keep the raw string
/// alive purely to re-parse it into the map it already has.
pub fn validate_stamps_parsed(
    requirements: &[StampRequirement],
    stamp_data: &std::collections::HashMap<String, Option<String>>,
    current_inputs: &[CurrentInput<'_>],
) -> StampValidationResult {
    let mut validation = StampValidationResult::new();

    for req in requirements {
        let stamp_path = req.relative_path();
        let json_content = stamp_data.get(&stamp_path).and_then(|v| v.as_ref());

        let inputs_for_req = current_inputs
            .iter()
            .find(|(component, command, _)| req.component == *component && req.command == *command)
            .map(|(_, _, inputs)| *inputs);

        check_stamp_requirement(
            req,
            json_content.map(|s| s.as_str()),
            inputs_for_req,
            &mut validation,
        );
    }

    validation
}

/// Generate shell script to compute package list hash
/// (For future caching/staleness detection based on installed packages)
#[allow(unused)]
pub fn generate_package_hash_script(installroot: &str) -> String {
    format!(
        r#"rpm --root={installroot} -qa --queryformat '%{{NAME}}-%{{VERSION}}-%{{RELEASE}}\n' 2>/dev/null | LC_ALL=C sort | sha256sum | cut -d' ' -f1"#
    )
}

/// Generate shell script to check if stamp exists
/// (For future quick existence checks without reading full content)
#[allow(unused)]
pub fn generate_stamp_exists_script(req: &StampRequirement) -> String {
    let stamp_path = req.relative_path();
    format!(r#"test -f "$AVOCADO_PREFIX/.stamps/{stamp_path}""#)
}

use crate::utils::config::RuntimeExtDep;

/// Resolve required stamps for a command based on component type and dependencies
///
/// Note: For runtime build, use `resolve_required_stamps_detailed` instead to properly
/// handle versioned extensions (which don't require build stamps).
pub fn resolve_required_stamps(
    cmd: StampCommand,
    component: StampComponent,
    component_name: Option<&str>,
    ext_dependencies: &[String],
) -> Vec<StampRequirement> {
    resolve_required_stamps_for_arch(cmd, component, component_name, ext_dependencies, None)
}

/// Resolve required stamps with a specific host architecture for SDK stamps
///
/// Use this when using `--runs-on` with a remote host that may have a different
/// CPU architecture than the local machine. The `host_arch` parameter specifies
/// the architecture of the remote host (e.g., "aarch64", "x86_64").
///
/// When `host_arch` is None, the local machine's architecture is used.
pub fn resolve_required_stamps_for_arch(
    cmd: StampCommand,
    component: StampComponent,
    component_name: Option<&str>,
    ext_dependencies: &[String],
    host_arch: Option<&str>,
) -> Vec<StampRequirement> {
    // Helper to create SDK install requirement with the correct arch
    let sdk_install = || match host_arch {
        Some(arch) => StampRequirement::sdk_install_for_arch(arch),
        None => StampRequirement::sdk_install(),
    };

    let compile_deps_install = || match host_arch {
        Some(arch) => StampRequirement::compile_deps_install_for_arch(arch),
        None => StampRequirement::compile_deps_install(),
    };

    match (cmd, component) {
        // SDK install has no dependencies
        (StampCommand::Install, StampComponent::Sdk) => vec![],

        // Compile-deps install requires SDK install
        (StampCommand::CompileDeps, StampComponent::Sdk) => {
            vec![sdk_install()]
        }

        // Extension install requires SDK install
        (StampCommand::Install, StampComponent::Extension) => {
            vec![sdk_install()]
        }

        // Runtime install requires SDK install
        (StampCommand::Install, StampComponent::Runtime) => {
            vec![sdk_install()]
        }

        // Extension build requires SDK install + compile-deps + own extension install
        (StampCommand::Build, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![
                sdk_install(),
                compile_deps_install(),
                StampRequirement::ext_install(ext_name),
            ]
        }

        // Extension image requires SDK install + compile-deps + own extension install + own extension build
        (StampCommand::Image, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![
                sdk_install(),
                compile_deps_install(),
                StampRequirement::ext_install(ext_name),
                StampRequirement::ext_build(ext_name),
            ]
        }

        // Runtime build requires SDK + compile-deps + own install + ALL extension deps (install AND build)
        // Note: This doesn't distinguish versioned extensions - use resolve_required_stamps_detailed
        (StampCommand::Build, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            let mut reqs = vec![
                sdk_install(),
                compile_deps_install(),
                StampRequirement::runtime_install(runtime_name),
            ];

            // Add extension dependencies (both install and build)
            for ext_name in ext_dependencies {
                reqs.push(StampRequirement::ext_install(ext_name));
                reqs.push(StampRequirement::ext_build(ext_name));
            }

            reqs
        }

        // Sign requires SDK install + runtime build
        // SDK install is needed because signing runs in the SDK container
        (StampCommand::Sign, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            vec![sdk_install(), StampRequirement::runtime_build(runtime_name)]
        }

        // Provision requires SDK install + runtime build
        // SDK install is needed because provisioning runs in the SDK container
        // When using --runs-on, this ensures the SDK is installed for the remote's arch
        (StampCommand::Provision, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            vec![sdk_install(), StampRequirement::runtime_build(runtime_name)]
        }

        // Other combinations have no requirements
        _ => vec![],
    }
}

/// Resolve required stamps for runtime build with detailed extension dependency info
///
/// This properly handles different extension types:
/// - Local extensions: require install + build + image stamps
/// - External extensions: require install + build + image stamps
/// - Versioned extensions: DEPRECATED - should error during config parsing
///   Remote extensions are now defined in the ext section with source: field
pub fn resolve_required_stamps_for_runtime_build(
    runtime_name: &str,
    ext_dependencies: &[RuntimeExtDep],
) -> Vec<StampRequirement> {
    resolve_required_stamps_for_runtime_build_with_arch(runtime_name, ext_dependencies, None)
}

/// Resolve required stamps for runtime build with a specific host architecture
///
/// Use this when using `--runs-on` with a remote host that may have a different
/// CPU architecture than the local machine.
pub fn resolve_required_stamps_for_runtime_build_with_arch(
    runtime_name: &str,
    ext_dependencies: &[RuntimeExtDep],
    host_arch: Option<&str>,
) -> Vec<StampRequirement> {
    let sdk_install = match host_arch {
        Some(arch) => StampRequirement::sdk_install_for_arch(arch),
        None => StampRequirement::sdk_install(),
    };

    let compile_deps_install = match host_arch {
        Some(arch) => StampRequirement::compile_deps_install_for_arch(arch),
        None => StampRequirement::compile_deps_install(),
    };

    let mut reqs = vec![
        sdk_install,
        compile_deps_install,
        StampRequirement::rootfs_install(),
        StampRequirement::initramfs_install(),
        StampRequirement::runtime_install(runtime_name),
    ];

    // All extensions now require install + build + image stamps
    // Extension source configuration (repo, git, path) is defined in the ext section
    for ext_dep in ext_dependencies {
        let ext_name = ext_dep.name();
        reqs.push(StampRequirement::ext_install(ext_name));
        reqs.push(StampRequirement::ext_build(ext_name));
        reqs.push(StampRequirement::ext_image(ext_name));
    }

    reqs
}

/// Validate a single stamp requirement against the stamp JSON output
///
/// Returns the status of the stamp (current, stale, or missing)
pub fn validate_stamp(
    _req: &StampRequirement,
    stamp_json: Option<&str>,
    current_inputs: Option<&StampInputs>,
) -> StampStatus {
    match stamp_json {
        Some(json) if json.trim() != "null" && !json.trim().is_empty() => {
            // Try to parse the stamp
            match Stamp::from_json(json) {
                Ok(stamp) => {
                    // If we have current inputs, check for staleness
                    if let Some(inputs) = current_inputs {
                        if stamp.is_current(inputs) {
                            StampStatus::Current(stamp)
                        } else {
                            StampStatus::Stale {
                                stamp,
                                reason: "config hash mismatch".to_string(),
                            }
                        }
                    } else {
                        // No inputs to check, assume current
                        StampStatus::Current(stamp)
                    }
                }
                Err(_) => {
                    // Failed to parse, treat as missing
                    StampStatus::Missing
                }
            }
        }
        _ => StampStatus::Missing,
    }
}

/// Validate a stamp requirement and update the validation result
pub fn check_stamp_requirement(
    req: &StampRequirement,
    stamp_json: Option<&str>,
    current_inputs: Option<&StampInputs>,
    result: &mut StampValidationResult,
) {
    match validate_stamp(req, stamp_json, current_inputs) {
        StampStatus::Current(_) => {
            result.add_satisfied(req.clone());
        }
        StampStatus::Stale { reason, .. } => {
            result.add_stale(req.clone(), reason);
        }
        StampStatus::Missing => {
            result.add_missing(req.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The effective rootfs package set for a default project — what
    /// `Config::get_rootfs_packages` returns when `rootfs.packages` is absent.
    fn default_rootfs_packages() -> std::collections::HashMap<String, serde_yaml::Value> {
        std::collections::HashMap::from([(
            "avocado-pkg-rootfs".to_string(),
            serde_yaml::Value::String("*".to_string()),
        )])
    }

    /// Resolved inputs for hash tests: the default package set, no feed
    /// identity, no lock pins. Tests exercising a specific resolved input
    /// build their own [`SysrootStampInputs`].
    fn test_sysroot_inputs(
        packages: &std::collections::HashMap<String, serde_yaml::Value>,
    ) -> SysrootStampInputs<'_> {
        SysrootStampInputs {
            packages,
            repo_url: None,
            repo_release: None,
            disable_weak_dependencies: false,
            dnf_args: None,
            locked_packages: None,
        }
    }

    /// `compute_rootfs_input_hash`'s config hash for a default package set —
    /// the shape most of the hash tests below want, since they vary a config
    /// section and assert on the resulting hash.
    fn rootfs_config_hash(config: &serde_yaml::Value, project_root: &Path) -> String {
        let packages = default_rootfs_packages();
        compute_rootfs_input_hash(config, project_root, None, &test_sysroot_inputs(&packages))
            .unwrap()
            .config_hash
    }

    #[test]
    fn test_stamp_creation() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs, outputs);

        assert_eq!(stamp.command, StampCommand::Install);
        assert_eq!(stamp.component, StampComponent::Sdk);
        assert!(stamp.component_name.is_none());
        assert_eq!(stamp.target, "qemux86-64");
        assert!(stamp.success);
    }

    #[test]
    fn test_stamp_relative_path() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();

        // SDK stamps now include the host architecture in the path
        let sdk_stamp = Stamp::sdk_install("x86_64", inputs.clone(), outputs.clone());
        assert_eq!(sdk_stamp.relative_path(), "sdk/x86_64/install.stamp");

        let sdk_stamp_arm = Stamp::sdk_install("aarch64", inputs.clone(), outputs.clone());
        assert_eq!(sdk_stamp_arm.relative_path(), "sdk/aarch64/install.stamp");

        let ext_stamp = Stamp::ext_install("my-ext", "qemux86-64", inputs.clone(), outputs.clone());
        assert_eq!(ext_stamp.relative_path(), "ext/my-ext/install.stamp");

        let ext_build = Stamp::ext_build("my-ext", "qemux86-64", inputs.clone(), outputs.clone());
        assert_eq!(ext_build.relative_path(), "ext/my-ext/build.stamp");

        let rt_stamp = Stamp::runtime_build("my-rt", "qemux86-64", inputs, outputs);
        assert_eq!(rt_stamp.relative_path(), "runtime/my-rt/build.stamp");
    }

    #[test]
    fn test_stamp_requirement_description() {
        let req = StampRequirement::sdk_install();
        // SDK description now includes architecture
        assert_eq!(
            req.description(),
            format!("SDK install ({})", get_local_arch())
        );
        assert_eq!(req.fix_command(), "avocado sdk install");

        let req = StampRequirement::ext_install("gpu-driver");
        assert_eq!(req.description(), "extension 'gpu-driver' install");
        assert_eq!(req.fix_command(), "avocado ext install gpu-driver");

        let req = StampRequirement::runtime_build("my-runtime");
        assert_eq!(req.description(), "runtime 'my-runtime' build");
        assert_eq!(req.fix_command(), "avocado runtime build my-runtime");
    }

    #[test]
    fn test_stamp_is_current() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs.clone(), outputs);

        // Same inputs should be current
        assert!(stamp.is_current(&inputs));

        // Different config hash should not be current
        let different = StampInputs::new("sha256:def456".to_string());
        assert!(!stamp.is_current(&different));
    }

    #[test]
    fn test_stamp_json_roundtrip() {
        let inputs = StampInputs::with_package_list(
            "sha256:abc123".to_string(),
            "sha256:pkg456".to_string(),
        );
        let outputs = StampOutputs {
            installed_packages_hash: Some("sha256:installed789".to_string()),
            package_count: Some(42),
        };
        let stamp = Stamp::ext_install("test-ext", "qemux86-64", inputs, outputs);

        let json = stamp.to_json().unwrap();
        let parsed = Stamp::from_json(&json).unwrap();

        assert_eq!(stamp.command, parsed.command);
        assert_eq!(stamp.component, parsed.component);
        assert_eq!(stamp.component_name, parsed.component_name);
        assert_eq!(stamp.inputs.config_hash, parsed.inputs.config_hash);
    }

    #[test]
    fn test_validation_result() {
        let mut result = StampValidationResult::new();
        assert!(result.is_satisfied());

        result.add_missing(StampRequirement::sdk_install());
        assert!(!result.is_satisfied());

        result.add_stale(
            StampRequirement::ext_install("my-ext"),
            "config hash mismatch".to_string(),
        );
        assert!(!result.is_satisfied());

        let error = result.into_error("Cannot build extension 'test'");
        let error_msg = error.to_string();
        assert!(error_msg.contains("Missing steps:"));
        assert!(error_msg.contains("Stale steps"));
        assert!(error_msg.contains("avocado sdk install"));
    }

    #[test]
    fn test_compute_hash() {
        let hash1 = compute_hash("hello world");
        let hash2 = compute_hash("hello world");
        let hash3 = compute_hash("different");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert!(hash1.starts_with("sha256:"));
    }

    #[test]
    fn test_resolve_required_stamps_sdk_install() {
        // SDK install has no dependencies
        let reqs = resolve_required_stamps(StampCommand::Install, StampComponent::Sdk, None, &[]);
        assert!(reqs.is_empty());
    }

    #[test]
    fn test_resolve_required_stamps_ext_install() {
        // Extension install requires SDK install
        let reqs = resolve_required_stamps(
            StampCommand::Install,
            StampComponent::Extension,
            Some("my-ext"),
            &[],
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
    }

    #[test]
    fn test_resolve_required_stamps_ext_build() {
        // Extension build requires SDK install + compile-deps + own extension install
        let reqs = resolve_required_stamps(
            StampCommand::Build,
            StampComponent::Extension,
            Some("my-ext"),
            &[],
        );
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::compile_deps_install());
        assert_eq!(reqs[2], StampRequirement::ext_install("my-ext"));
    }

    #[test]
    fn test_resolve_required_stamps_runtime_install() {
        // Runtime install requires SDK install
        let reqs = resolve_required_stamps(
            StampCommand::Install,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_with_extensions() {
        // Runtime build requires SDK + own install + ALL extension deps
        let ext_deps = vec!["ext-a".to_string(), "ext-b".to_string()];
        let reqs = resolve_required_stamps(
            StampCommand::Build,
            StampComponent::Runtime,
            Some("my-runtime"),
            &ext_deps,
        );

        // Should have: SDK install, compile-deps, runtime install, ext-a install, ext-a build, ext-b install, ext-b build
        assert_eq!(reqs.len(), 7);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::compile_deps_install());
        assert_eq!(reqs[2], StampRequirement::runtime_install("my-runtime"));
        assert_eq!(reqs[3], StampRequirement::ext_install("ext-a"));
        assert_eq!(reqs[4], StampRequirement::ext_build("ext-a"));
        assert_eq!(reqs[5], StampRequirement::ext_install("ext-b"));
        assert_eq!(reqs[6], StampRequirement::ext_build("ext-b"));
    }

    #[test]
    fn test_resolve_required_stamps_sign() {
        // Sign requires SDK install + runtime build
        let reqs = resolve_required_stamps(
            StampCommand::Sign,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
        );
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::runtime_build("my-runtime"));
    }

    #[test]
    fn test_resolve_required_stamps_provision() {
        // Provision requires SDK install + runtime build
        let reqs = resolve_required_stamps(
            StampCommand::Provision,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
        );
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::runtime_build("my-runtime"));
    }

    #[test]
    fn test_generate_write_stamp_script() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs, outputs);

        let script = generate_write_stamp_script(&stamp).unwrap();
        assert!(script.contains("mkdir -p"));
        assert!(script.contains(".stamps/sdk"));
        assert!(script.contains("install.stamp"));
    }

    #[test]
    fn test_stamp_validation_error_display() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::sdk_install());
        result.add_missing(StampRequirement::ext_install("gpu-driver"));
        result.add_stale(
            StampRequirement::ext_install("old-ext"),
            "config changed".to_string(),
        );

        let error = result.into_error("Cannot build runtime 'my-runtime'");
        let error_str = error.to_string();

        // Check error message contains key elements
        assert!(error_str.contains("Cannot build runtime 'my-runtime'"));
        assert!(error_str.contains("Missing steps:"));
        // SDK stamp path now includes local architecture
        assert!(error_str.contains(&format!("sdk/{}/install.stamp", get_local_arch())));
        assert!(error_str.contains("ext/gpu-driver/install.stamp"));
        assert!(error_str.contains("Stale steps"));
        assert!(error_str.contains("config changed"));
        assert!(error_str.contains("To fix:"));
        assert!(error_str.contains("avocado sdk install"));
        assert!(error_str.contains("avocado ext install gpu-driver"));
    }

    #[test]
    fn test_validate_stamp_missing() {
        let req = StampRequirement::sdk_install();
        let status = validate_stamp(&req, None, None);
        assert!(matches!(status, StampStatus::Missing));

        let status = validate_stamp(&req, Some("null"), None);
        assert!(matches!(status, StampStatus::Missing));

        let status = validate_stamp(&req, Some(""), None);
        assert!(matches!(status, StampStatus::Missing));
    }

    #[test]
    fn test_validate_stamp_current() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs.clone(), outputs);
        let json = stamp.to_json().unwrap();

        let req = StampRequirement::sdk_install();
        let status = validate_stamp(&req, Some(&json), Some(&inputs));

        assert!(matches!(status, StampStatus::Current(_)));
    }

    #[test]
    fn test_validate_stamp_stale() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs, outputs);
        let json = stamp.to_json().unwrap();

        // Different inputs should be stale
        let different_inputs = StampInputs::new("sha256:different".to_string());
        let req = StampRequirement::sdk_install();
        let status = validate_stamp(&req, Some(&json), Some(&different_inputs));

        assert!(matches!(status, StampStatus::Stale { .. }));
    }

    #[test]
    fn test_check_stamp_requirement_updates_result() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::sdk_install("qemux86-64", inputs.clone(), outputs);
        let json = stamp.to_json().unwrap();

        let req = StampRequirement::sdk_install();
        let mut result = StampValidationResult::new();

        // Current stamp should be satisfied
        check_stamp_requirement(&req, Some(&json), Some(&inputs), &mut result);
        assert!(result.is_satisfied());
        assert_eq!(result.satisfied.len(), 1);

        // Missing stamp should fail
        let mut result2 = StampValidationResult::new();
        check_stamp_requirement(&req, None, None, &mut result2);
        assert!(!result2.is_satisfied());
        assert_eq!(result2.missing.len(), 1);

        // Stale stamp should fail
        let different_inputs = StampInputs::new("sha256:different".to_string());
        let mut result3 = StampValidationResult::new();
        check_stamp_requirement(&req, Some(&json), Some(&different_inputs), &mut result3);
        assert!(!result3.is_satisfied());
        assert_eq!(result3.stale.len(), 1);
    }

    #[test]
    fn test_resolve_required_stamps_for_runtime_build_with_multiple_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Test with multiple extensions:
        // All extensions are now Local type - source config (repo, git, path) is in ext section
        let ext_deps = vec![
            RuntimeExtDep::Local("app".to_string()),
            RuntimeExtDep::Local("config-dev".to_string()),
            RuntimeExtDep::Local("avocado-ext-dev".to_string()),
        ];

        let reqs = resolve_required_stamps_for_runtime_build("my-runtime", &ext_deps);

        // Should have:
        // - SDK install (1)
        // - compile-deps install (1)
        // - rootfs install (1)
        // - initramfs install (1)
        // - Runtime install (1)
        // - app install + build + image (3)
        // - config-dev install + build + image (3)
        // - avocado-ext-dev install + build + image (3)
        // Total: 14
        assert_eq!(reqs.len(), 14);

        // Verify SDK, compile-deps, rootfs, initramfs, and runtime install are present
        assert!(reqs.contains(&StampRequirement::sdk_install()));
        assert!(reqs.contains(&StampRequirement::compile_deps_install()));
        assert!(reqs.contains(&StampRequirement::rootfs_install()));
        assert!(reqs.contains(&StampRequirement::initramfs_install()));
        assert!(reqs.contains(&StampRequirement::runtime_install("my-runtime")));

        // Verify all extensions have install, build, and image
        assert!(reqs.contains(&StampRequirement::ext_install("app")));
        assert!(reqs.contains(&StampRequirement::ext_build("app")));
        assert!(reqs.contains(&StampRequirement::ext_image("app")));

        assert!(reqs.contains(&StampRequirement::ext_install("config-dev")));
        assert!(reqs.contains(&StampRequirement::ext_build("config-dev")));
        assert!(reqs.contains(&StampRequirement::ext_image("config-dev")));

        assert!(reqs.contains(&StampRequirement::ext_install("avocado-ext-dev")));
        assert!(reqs.contains(&StampRequirement::ext_build("avocado-ext-dev")));
        assert!(reqs.contains(&StampRequirement::ext_image("avocado-ext-dev")));
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_local_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with extensions (all are now Local type)
        let ext_deps = vec![
            RuntimeExtDep::Local("app".to_string()),
            RuntimeExtDep::Local("config-dev".to_string()),
        ];

        let reqs = resolve_required_stamps_for_runtime_build("dev", &ext_deps);

        // Should have:
        // - SDK install (1)
        // - compile-deps install (1)
        // - rootfs install (1)
        // - initramfs install (1)
        // - Runtime install (1)
        // - app install + build + image (3)
        // - config-dev install + build + image (3)
        // Total: 11
        assert_eq!(reqs.len(), 11);

        // Verify local extensions require install, build, and image
        assert!(reqs.contains(&StampRequirement::ext_install("app")));
        assert!(reqs.contains(&StampRequirement::ext_build("app")));
        assert!(reqs.contains(&StampRequirement::ext_image("app")));
        assert!(reqs.contains(&StampRequirement::ext_install("config-dev")));
        assert!(reqs.contains(&StampRequirement::ext_build("config-dev")));
        assert!(reqs.contains(&StampRequirement::ext_image("config-dev")));
    }

    #[test]
    fn test_resolve_required_stamps_ext_image() {
        // Extension image requires SDK install + ext install + ext build
        let reqs = resolve_required_stamps(
            StampCommand::Image,
            StampComponent::Extension,
            Some("my-ext"),
            &[],
        );
        assert_eq!(reqs.len(), 4);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::compile_deps_install());
        assert_eq!(reqs[2], StampRequirement::ext_install("my-ext"));
        assert_eq!(reqs[3], StampRequirement::ext_build("my-ext"));
    }

    #[test]
    fn test_ext_image_stamp_creation_and_path() {
        let inputs = StampInputs::new("sha256:abc123".to_string());
        let outputs = StampOutputs::default();
        let stamp = Stamp::ext_image("my-ext", "qemux86-64", inputs, outputs);

        assert_eq!(stamp.command, StampCommand::Image);
        assert_eq!(stamp.component, StampComponent::Extension);
        assert_eq!(stamp.component_name, Some("my-ext".to_string()));
        assert_eq!(stamp.relative_path(), "ext/my-ext/image.stamp");
    }

    #[test]
    fn test_ext_image_requirement_description_and_fix() {
        let req = StampRequirement::ext_image("gpu-driver");
        assert_eq!(req.description(), "extension 'gpu-driver' image");
        assert_eq!(req.fix_command(), "avocado ext image gpu-driver");
        assert_eq!(req.relative_path(), "ext/gpu-driver/image.stamp");
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_no_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with NO extension dependencies
        let ext_deps: Vec<RuntimeExtDep> = vec![];

        let reqs = resolve_required_stamps_for_runtime_build("minimal-runtime", &ext_deps);

        // Should have SDK install + compile-deps + rootfs + initramfs + runtime install
        assert_eq!(reqs.len(), 5);
        assert!(reqs.contains(&StampRequirement::sdk_install()));
        assert!(reqs.contains(&StampRequirement::compile_deps_install()));
        assert!(reqs.contains(&StampRequirement::rootfs_install()));
        assert!(reqs.contains(&StampRequirement::initramfs_install()));
        assert!(reqs.contains(&StampRequirement::runtime_install("minimal-runtime")));
    }

    #[test]
    fn test_runtime_ext_dep_name() {
        use crate::utils::config::RuntimeExtDep;

        // Test the Local variant (the primary way to specify extensions)
        let local = RuntimeExtDep::Local("my-local-ext".to_string());
        assert_eq!(local.name(), "my-local-ext");
    }

    #[test]
    fn test_generate_batch_read_stamps_script() {
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
        ];

        let script = generate_batch_read_stamps_script(&requirements);

        // Should contain all three stamp paths (SDK path includes local arch)
        assert!(script.contains(&format!("sdk/{}/install.stamp", get_local_arch())));
        assert!(script.contains("ext/my-ext/install.stamp"));
        assert!(script.contains("ext/my-ext/build.stamp"));

        // Should use ::: as delimiter
        assert!(script.contains(":::"));

        // Each stamp read should be on its own line
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_parse_batch_stamps_output() {
        let arch = get_local_arch();
        let output = format!(
            r#"sdk/{arch}/install.stamp:::{{"version":"1.0.0","command":"install","component":"sdk"}}
ext/my-ext/install.stamp:::{{"version":"1.0.0","command":"install","component":"ext"}}
ext/my-ext/build.stamp:::null"#
        );

        let result = parse_batch_stamps_output(&output);

        assert_eq!(result.len(), 3);
        assert!(result
            .get(&format!("sdk/{arch}/install.stamp"))
            .unwrap()
            .is_some());
        assert!(result.get("ext/my-ext/install.stamp").unwrap().is_some());
        assert!(result.get("ext/my-ext/build.stamp").unwrap().is_none());
    }

    #[test]
    fn test_validate_stamps_batch_all_present() {
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
        ];

        // Create valid stamp JSON - use compact (single-line) format like batch script does
        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_stamp = Stamp::ext_install(
            "my-ext",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );

        // Use serde_json::to_string (compact) instead of to_string_pretty
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let ext_json = serde_json::to_string(&ext_stamp).unwrap();

        let output = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            get_local_arch(),
            sdk_json,
            ext_json
        );

        let result = validate_stamps_batch(&requirements, &output, &[]);

        assert!(result.is_satisfied());
        assert_eq!(result.satisfied.len(), 2);
        assert!(result.missing.is_empty());
        assert!(result.stale.is_empty());
    }

    #[test]
    fn test_validate_stamps_batch_some_missing() {
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
        ];

        // Only SDK stamp is present - use compact JSON format
        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();

        let output = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::null\next/my-ext/build.stamp:::null",
            get_local_arch(),
            sdk_json
        );

        let result = validate_stamps_batch(&requirements, &output, &[]);

        assert!(!result.is_satisfied());
        assert_eq!(result.satisfied.len(), 1);
        assert_eq!(result.missing.len(), 2);
        assert!(result.stale.is_empty());
    }

    #[test]
    fn test_validate_stamps_batch_empty_output() {
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
        ];

        let result = validate_stamps_batch(&requirements, "", &[]);

        assert!(!result.is_satisfied());
        assert!(result.satisfied.is_empty());
        assert_eq!(result.missing.len(), 2);
    }

    // ========================================================================
    // Command Dependency Chain Tests
    // ========================================================================
    // These tests document the dependency requirements for each command.

    #[test]
    fn test_ext_package_requires_sdk_install_ext_install_ext_build() {
        // ext package requires: SDK install + ext install + ext build
        // This is the most demanding extension command
        let reqs = [
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
        ];

        // Verify fix commands are correct
        assert_eq!(reqs[0].fix_command(), "avocado sdk install");
        assert_eq!(reqs[1].fix_command(), "avocado ext install my-ext");
        assert_eq!(reqs[2].fix_command(), "avocado ext build my-ext");

        // Verify descriptions are helpful (SDK now includes architecture)
        assert_eq!(
            reqs[0].description(),
            format!("SDK install ({})", get_local_arch())
        );
        assert_eq!(reqs[1].description(), "extension 'my-ext' install");
        assert_eq!(reqs[2].description(), "extension 'my-ext' build");
    }

    #[test]
    fn test_ext_checkout_requires_sdk_install_ext_install() {
        // ext checkout requires: SDK install + ext install (but NOT build)
        // Checkout is for extracting files from installed sysroot
        let reqs = [
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
        ];

        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].fix_command(), "avocado sdk install");
        assert_eq!(reqs[1].fix_command(), "avocado ext install my-ext");
    }

    #[test]
    fn test_sdk_compile_requires_sdk_install() {
        // sdk compile requires: SDK install only
        // Compile runs scripts in the SDK container after packages are installed
        let reqs = [StampRequirement::sdk_install()];

        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].fix_command(), "avocado sdk install");
        assert_eq!(
            reqs[0].relative_path(),
            format!("sdk/{}/install.stamp", get_local_arch())
        );
    }

    #[test]
    fn test_hitl_server_requires_sdk_install_ext_install_ext_build_for_each_extension() {
        // HITL server requires for each extension: SDK install + ext install + ext build
        let extensions = vec!["ext-a", "ext-b"];
        let mut reqs = vec![StampRequirement::sdk_install()];
        for ext in &extensions {
            reqs.push(StampRequirement::ext_install(ext));
            reqs.push(StampRequirement::ext_build(ext));
        }

        // Total: 1 SDK + 2 per extension = 5
        assert_eq!(reqs.len(), 5);

        // Verify all paths are correct (SDK path includes local arch)
        assert_eq!(
            reqs[0].relative_path(),
            format!("sdk/{}/install.stamp", get_local_arch())
        );
        assert_eq!(reqs[1].relative_path(), "ext/ext-a/install.stamp");
        assert_eq!(reqs[2].relative_path(), "ext/ext-a/build.stamp");
        assert_eq!(reqs[3].relative_path(), "ext/ext-b/install.stamp");
        assert_eq!(reqs[4].relative_path(), "ext/ext-b/build.stamp");
    }

    // ========================================================================
    // Clean Lifecycle Tests
    // ========================================================================
    // These tests verify that clean commands remove the right stamps.

    #[test]
    fn test_ext_clean_stamp_path_matches_ext_install_and_build() {
        // Extension clean should remove stamps at ext/<name>/
        // Verify stamp paths are consistent with what clean removes
        let ext_name = "gpu-driver";

        let install_stamp = StampRequirement::ext_install(ext_name);
        let build_stamp = StampRequirement::ext_build(ext_name);

        // Both should be under ext/<name>/
        assert_eq!(
            install_stamp.relative_path(),
            "ext/gpu-driver/install.stamp"
        );
        assert_eq!(build_stamp.relative_path(), "ext/gpu-driver/build.stamp");

        // Clean removes: rm -rf "$AVOCADO_PREFIX/.stamps/ext/<name>"
        // This matches the parent directory of both stamps
        let install_path = install_stamp.relative_path();
        let install_parent = std::path::Path::new(&install_path)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();
        let build_path = build_stamp.relative_path();
        let build_parent = std::path::Path::new(&build_path)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();

        assert_eq!(install_parent, "ext/gpu-driver");
        assert_eq!(build_parent, "ext/gpu-driver");
    }

    #[test]
    fn test_runtime_clean_stamp_path_matches_runtime_install_and_build() {
        // Runtime clean should remove stamps at runtime/<name>/
        let runtime_name = "my-runtime";

        let install_stamp = StampRequirement::runtime_install(runtime_name);
        let build_stamp = StampRequirement::runtime_build(runtime_name);
        let sign_stamp = StampRequirement::runtime_sign(runtime_name);
        let provision_stamp = StampRequirement::runtime_provision(runtime_name);

        // All should be under runtime/<name>/
        assert_eq!(
            install_stamp.relative_path(),
            "runtime/my-runtime/install.stamp"
        );
        assert_eq!(
            build_stamp.relative_path(),
            "runtime/my-runtime/build.stamp"
        );
        assert_eq!(sign_stamp.relative_path(), "runtime/my-runtime/sign.stamp");
        assert_eq!(
            provision_stamp.relative_path(),
            "runtime/my-runtime/provision.stamp"
        );

        // Clean removes: rm -rf "$AVOCADO_PREFIX/.stamps/runtime/<name>"
        // All stamps share the same parent directory
        let stamps = [install_stamp, build_stamp, sign_stamp, provision_stamp];
        for stamp in &stamps {
            let path = stamp.relative_path();
            let parent = std::path::Path::new(&path)
                .parent()
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(parent, "runtime/my-runtime");
        }
    }

    #[test]
    fn test_sdk_clean_stamp_path_matches_sdk_install() {
        // SDK clean should remove stamps at sdk/{arch}/
        let install_stamp = StampRequirement::sdk_install();

        assert_eq!(
            install_stamp.relative_path(),
            format!("sdk/{}/install.stamp", get_local_arch())
        );

        // Clean removes: rm -rf "$AVOCADO_PREFIX/.stamps/sdk/{arch}"
        let path = install_stamp.relative_path();
        let parent = std::path::Path::new(&path)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(parent, format!("sdk/{}", get_local_arch()));
    }

    #[test]
    fn test_clean_then_build_requires_reinstall() {
        // After cleaning, all stamps are gone, so build should require install
        // Simulate: clean ext my-ext -> stamps gone -> ext build requires install first

        // Initially satisfied
        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_install_stamp = Stamp::ext_install(
            "my-ext",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );

        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
        ];

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let ext_json = serde_json::to_string(&ext_install_stamp).unwrap();

        // Before clean: all satisfied
        let output_before = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            get_local_arch(),
            sdk_json,
            ext_json
        );
        let result_before = validate_stamps_batch(&requirements, &output_before, &[]);
        assert!(result_before.is_satisfied());

        // After ext clean: SDK still there, ext stamps gone
        let output_after_ext_clean = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::null",
            get_local_arch(),
            sdk_json
        );
        let result_after = validate_stamps_batch(&requirements, &output_after_ext_clean, &[]);
        assert!(!result_after.is_satisfied());
        assert_eq!(result_after.missing.len(), 1);
        assert_eq!(
            result_after.missing[0].relative_path(),
            "ext/my-ext/install.stamp"
        );
    }

    #[test]
    fn test_clean_all_stamps_requires_full_reinstall() {
        // After `avocado clean --stamps`, everything is gone
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("ext-a"),
            StampRequirement::ext_build("ext-a"),
            StampRequirement::runtime_install("my-runtime"),
            StampRequirement::runtime_build("my-runtime"),
        ];

        // After clean --stamps: all stamps return null
        let output = format!(
            r#"sdk/{}/install.stamp:::null
ext/ext-a/install.stamp:::null
ext/ext-a/build.stamp:::null
runtime/my-runtime/install.stamp:::null
runtime/my-runtime/build.stamp:::null"#,
            get_local_arch()
        );

        let result = validate_stamps_batch(&requirements, &output, &[]);

        assert!(!result.is_satisfied());
        assert!(result.satisfied.is_empty());
        assert_eq!(result.missing.len(), 5);
    }

    // ========================================================================
    // Staleness Detection Tests
    // ========================================================================

    #[test]
    fn test_stale_stamp_detected_after_config_change() {
        // When config changes, stamps become stale
        let original_inputs = StampInputs::new("sha256:original".to_string());
        let changed_inputs = StampInputs::new("sha256:changed".to_string());

        let stamp = Stamp::ext_install(
            "my-ext",
            "qemux86-64",
            original_inputs,
            StampOutputs::default(),
        );
        let json = serde_json::to_string(&stamp).unwrap();

        let requirements = vec![StampRequirement::ext_install("my-ext")];
        let output = format!("ext/my-ext/install.stamp:::{json}");

        // With changed inputs, stamp should be stale
        let result = validate_stamps_batch(
            &requirements,
            &output,
            &[(
                StampComponent::Extension,
                StampCommand::Install,
                &changed_inputs,
            )],
        );

        assert!(!result.is_satisfied());
        assert!(result.satisfied.is_empty());
        assert!(result.missing.is_empty());
        assert_eq!(result.stale.len(), 1);
    }

    #[test]
    fn test_stale_ext_requires_reinstall_before_build() {
        // If extension install stamp is stale, build should also fail
        let original_inputs = StampInputs::new("sha256:original".to_string());

        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            original_inputs.clone(),
            StampOutputs::default(),
        );
        let ext_install_stamp = Stamp::ext_install(
            "my-ext",
            "qemux86-64",
            original_inputs,
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let ext_json = serde_json::to_string(&ext_install_stamp).unwrap();

        // Build requirements
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
        ];

        let output = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            get_local_arch(),
            sdk_json,
            ext_json
        );

        // With changed inputs (simulating extension config change).
        // Only the extension stamp should be stale — SDK stamp uses its own hash.
        let changed_inputs = StampInputs::new("sha256:config-v2".to_string());
        let result = validate_stamps_batch(
            &requirements,
            &output,
            &[(
                StampComponent::Extension,
                StampCommand::Install,
                &changed_inputs,
            )],
        );

        assert!(!result.is_satisfied());
        // Only the extension stamp should be stale, SDK stamp is a dependency (existence only)
        assert_eq!(result.stale.len(), 1);
        assert_eq!(result.satisfied.len(), 1);
    }

    // ========================================================================
    // Error Message Quality Tests
    // ========================================================================

    #[test]
    fn test_error_message_includes_all_missing_fix_commands() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::sdk_install());
        result.add_missing(StampRequirement::ext_install("app"));
        result.add_missing(StampRequirement::ext_build("app"));

        let error = result.into_error("Cannot build runtime");
        let msg = error.to_string();

        // Should include all fix commands
        assert!(msg.contains("avocado sdk install"));
        assert!(msg.contains("avocado ext install app"));
        assert!(msg.contains("avocado ext build app"));
    }

    #[test]
    fn test_json_error_event_carries_reason_and_remedy() {
        let mut result = StampValidationResult::new();
        result.add_stale(
            StampRequirement::rootfs_install(),
            "config hash mismatch".to_string(),
        );
        let event = result
            .into_error("Cannot build runtime 'dev'")
            .json_error_event();

        // The desktop maps `event: "error"` to a top-level run_error and
        // renders `message`; anything else is silently ignored by its parser.
        assert_eq!(event["event"], "error");
        let msg = event["message"].as_str().expect("message is a string");
        assert!(msg.contains("rootfs install"), "{msg}");
        assert!(msg.contains("avocado rootfs install"), "{msg}");
    }

    /// `print_and_exit` ships this string verbatim as the `{"event":"error"}`
    /// message under `--output json`, where the prose path is suppressed — so
    /// the remedy has to survive into it or the desktop shows a bare failure.
    #[test]
    fn test_stale_sysroot_error_carries_its_install_command() {
        let mut result = StampValidationResult::new();
        result.add_stale(
            StampRequirement::rootfs_install(),
            "config hash mismatch".to_string(),
        );
        result.add_stale(
            StampRequirement::initramfs_install(),
            "config hash mismatch".to_string(),
        );

        let msg = result.into_error("Cannot build runtime 'dev'").to_string();
        assert!(msg.contains("Stale steps"));
        assert!(msg.contains("avocado rootfs install"), "{msg}");
        assert!(msg.contains("avocado initramfs install"), "{msg}");
    }

    #[test]
    fn test_error_message_distinguishes_missing_and_stale() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::sdk_install());
        result.add_stale(
            StampRequirement::ext_install("stale-ext"),
            "config hash changed".to_string(),
        );

        let error = result.into_error("Cannot proceed");
        let msg = error.to_string();

        // Should have separate sections
        assert!(msg.contains("Missing steps:"));
        assert!(msg.contains("Stale steps"));
        assert!(msg.contains("config hash changed"));
    }

    // ========================================================================
    // Architecture-Specific SDK Stamp Tests
    // ========================================================================

    #[test]
    fn test_sdk_install_stamp_uses_host_architecture() {
        // SDK stamps now use the host architecture in the path
        let local_arch = get_local_arch();

        let req = StampRequirement::sdk_install();
        assert_eq!(req.host_arch, Some(local_arch.to_string()));
        assert_eq!(
            req.relative_path(),
            format!("sdk/{local_arch}/install.stamp")
        );
    }

    #[test]
    fn test_sdk_install_for_specific_architecture() {
        // Test creating SDK stamp requirement for a specific architecture
        let req_x86 = StampRequirement::sdk_install_for_arch("x86_64");
        assert_eq!(req_x86.host_arch, Some("x86_64".to_string()));
        assert_eq!(req_x86.relative_path(), "sdk/x86_64/install.stamp");

        let req_arm = StampRequirement::sdk_install_for_arch("aarch64");
        assert_eq!(req_arm.host_arch, Some("aarch64".to_string()));
        assert_eq!(req_arm.relative_path(), "sdk/aarch64/install.stamp");
    }

    #[test]
    fn test_sdk_stamps_for_different_architectures_are_distinct() {
        // Stamps for different architectures should have different paths
        let req_x86 = StampRequirement::sdk_install_for_arch("x86_64");
        let req_arm = StampRequirement::sdk_install_for_arch("aarch64");

        assert_ne!(req_x86.relative_path(), req_arm.relative_path());
        assert_ne!(req_x86, req_arm);
    }

    #[test]
    fn test_resolve_required_stamps_for_arch() {
        // Resolving stamps for a specific architecture
        // Runtime build (which provision depends on) requires SDK install
        let reqs = resolve_required_stamps_for_arch(
            StampCommand::Build,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
            Some("aarch64"),
        );

        // Should include SDK stamp for aarch64 (runtime build requires SDK)
        assert!(reqs
            .iter()
            .any(|r| r.relative_path() == "sdk/aarch64/install.stamp"));
    }

    #[test]
    fn test_sdk_description_includes_architecture() {
        let req = StampRequirement::sdk_install_for_arch("aarch64");
        assert!(req.description().contains("aarch64"));
    }

    #[test]
    fn test_fix_command_with_runs_on() {
        let req = StampRequirement::sdk_install_for_arch("aarch64");

        // Without runs-on, should suggest regular install
        assert_eq!(req.fix_command(), "avocado sdk install");

        // With runs-on, should suggest install on the remote
        assert_eq!(
            req.fix_command_with_remote(Some("user@remote")),
            "avocado sdk install --runs-on user@remote"
        );
    }

    #[test]
    fn test_validation_error_includes_sdk_arch_hint_for_different_arch() {
        let mut result = StampValidationResult::new();
        // Use an architecture different from local to trigger --sdk-arch suggestion
        let different_arch = if get_local_arch() == "aarch64" {
            "x86_64"
        } else {
            "aarch64"
        };
        result.add_missing(StampRequirement::sdk_install_for_arch(different_arch));

        // Without runs_on, fix should suggest --sdk-arch for different architecture
        let error = result.into_error("Cannot provision");
        let msg = error.to_string();
        assert!(
            msg.contains(&format!("avocado sdk install --sdk-arch {different_arch}")),
            "Expected --sdk-arch suggestion in: {msg}"
        );
    }

    #[test]
    fn test_validation_error_with_runs_on_includes_both_alternatives() {
        let mut result = StampValidationResult::new();
        // Use an architecture different from local to trigger both suggestions
        let different_arch = if get_local_arch() == "aarch64" {
            "x86_64"
        } else {
            "aarch64"
        };
        result.add_missing(StampRequirement::sdk_install_for_arch(different_arch));

        // With runs_on, fix should include BOTH --sdk-arch and --runs-on alternatives
        let error = result.into_error_with_runs_on("Cannot provision", Some("user@remote"));
        let msg = error.to_string();
        assert!(
            msg.contains(&format!("avocado sdk install --sdk-arch {different_arch}")),
            "Expected --sdk-arch suggestion in: {msg}"
        );
        assert!(
            msg.contains("avocado sdk install --runs-on user@remote"),
            "Expected --runs-on suggestion in: {msg}"
        );
    }

    #[test]
    fn test_runtime_input_hash_includes_kernel() {
        let without_kernel: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-img-rootfs: "*"
target: "x86_64"
"#,
        )
        .unwrap();

        let with_kernel: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-img-rootfs: "*"
target: "x86_64"
kernel:
  package: kernel-image
  version: "*"
"#,
        )
        .unwrap();

        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_without = compute_runtime_build_input_hash(
            &without_kernel,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &with_kernel,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();

        // Hashes should differ when kernel config is added
        assert_ne!(hash_without.config_hash, hash_with.config_hash);
    }

    #[test]
    fn test_runtime_input_hash_kernel_change_triggers_rebuild() {
        let kernel_package: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-img-rootfs: "*"
kernel:
  package: kernel-image
  version: "*"
"#,
        )
        .unwrap();

        let kernel_compile: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-img-rootfs: "*"
kernel:
  compile: kernel-build
  install: kernel-install.sh
"#,
        )
        .unwrap();

        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_package = compute_runtime_build_input_hash(
            &kernel_package,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();
        let hash_compile = compute_runtime_build_input_hash(
            &kernel_compile,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();

        // Switching kernel mode should produce a different hash
        assert_ne!(hash_package.config_hash, hash_compile.config_hash);
    }

    #[test]
    fn test_ext_input_hash_includes_var_files() {
        let config_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    version: "1.0.0"
    types: [sysext]
    packages:
      foo: "*"
"#,
        )
        .unwrap();

        let config_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    version: "1.0.0"
    types: [sysext]
    packages:
      foo: "*"
    var_files:
      - "var/lib/docker/**"
"#,
        )
        .unwrap();

        let hash_without = compute_ext_image_input_hash(
            &config_without,
            "my-ext",
            None,
            std::path::Path::new("."),
            None,
            None,
            None,
        )
        .unwrap();
        let hash_with = compute_ext_image_input_hash(
            &config_with,
            "my-ext",
            None,
            std::path::Path::new("."),
            None,
            None,
            None,
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding var_files should change the ext input hash"
        );
    }

    #[test]
    fn test_runtime_input_hash_includes_ext_docker_images() {
        // Runtime references extension "app" which has docker_images
        let runtime: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
extensions:
  - app
"#,
        )
        .unwrap();

        let parsed_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  app:
    version: "1.0.0"
    types: [sysext]
"#,
        )
        .unwrap();

        let parsed_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  app:
    version: "1.0.0"
    types: [sysext]
    docker_images:
      - image: "docker.io/library/redis"
        tag: "7-alpine"
"#,
        )
        .unwrap();

        let hash_without = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_without,
            std::path::Path::new("."),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_with,
            std::path::Path::new("."),
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding docker_images to an extension should change the runtime input hash"
        );
    }

    #[test]
    fn test_runtime_input_hash_includes_device_tree_overlays() {
        let runtime: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
extensions:
  - board
"#,
        )
        .unwrap();

        let parsed_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  board:
    version: "1.0.0"
"#,
        )
        .unwrap();

        let parsed_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  board:
    version: "1.0.0"
    device_tree_overlays:
      - name: spi-fast
        src: overlays/spi-fast.dtso
"#,
        )
        .unwrap();

        let hash_without =
            compute_runtime_build_input_hash(&runtime, "dev", &parsed_without, Path::new("."))
                .unwrap();
        let hash_with =
            compute_runtime_build_input_hash(&runtime, "dev", &parsed_with, Path::new("."))
                .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "declaring a device-tree overlay must change the runtime input hash"
        );
    }

    #[test]
    fn test_runtime_input_hash_tracks_dtso_content() {
        let runtime: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
extensions:
  - board
"#,
        )
        .unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  board:
    version: "1.0.0"
    device_tree_overlays:
      - name: spi-fast
        src: overlays/spi-fast.dtso
"#,
        )
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let dtso = tmp.path().join("overlays/spi-fast.dtso");
        std::fs::create_dir_all(dtso.parent().unwrap()).unwrap();

        std::fs::write(&dtso, "/dts-v1/;\n/plugin/;\n/ { /* v1 */ };\n").unwrap();
        let hash_v1 =
            compute_runtime_build_input_hash(&runtime, "dev", &parsed, tmp.path()).unwrap();

        std::fs::write(&dtso, "/dts-v1/;\n/plugin/;\n/ { /* v2 edited */ };\n").unwrap();
        let hash_v2 =
            compute_runtime_build_input_hash(&runtime, "dev", &parsed, tmp.path()).unwrap();

        assert_ne!(
            hash_v1.config_hash, hash_v2.config_hash,
            "editing a .dtso's contents must change the runtime input hash"
        );
    }

    #[test]
    fn test_runtime_input_hash_includes_var_files() {
        let runtime_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
"#,
        )
        .unwrap();

        let runtime_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
var_files:
  - source: "files/data/"
    dest: "lib/myapp/"
"#,
        )
        .unwrap();

        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_without = compute_runtime_build_input_hash(
            &runtime_without,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime_with,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding var_files should change the runtime input hash"
        );
    }

    #[test]
    fn test_ext_input_hash_includes_subvolumes() {
        let config_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    version: "1.0.0"
    types: [sysext]
    packages:
      foo: "*"
"#,
        )
        .unwrap();

        let config_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    version: "1.0.0"
    types: [sysext]
    packages:
      foo: "*"
    subvolumes:
      lib/docker:
        nodatacow: true
        quota: "10G"
"#,
        )
        .unwrap();

        let hash_without = compute_ext_image_input_hash(
            &config_without,
            "my-ext",
            None,
            std::path::Path::new("."),
            None,
            None,
            None,
        )
        .unwrap();
        let hash_with = compute_ext_image_input_hash(
            &config_with,
            "my-ext",
            None,
            std::path::Path::new("."),
            None,
            None,
            None,
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding subvolumes should change the ext input hash"
        );
    }

    #[test]
    fn test_runtime_input_hash_includes_var_config() {
        let runtime_without: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
"#,
        )
        .unwrap();

        let runtime_with: serde_yaml::Value = serde_yaml::from_str(
            r#"
packages:
  avocado-runtime: "*"
var:
  compression: zstd
  subvolumes:
    lib/avocado:
      quota: "500M"
"#,
        )
        .unwrap();

        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_without = compute_runtime_build_input_hash(
            &runtime_without,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime_with,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding var config should change the runtime input hash"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Negative-invalidation tests
    //
    // Each test asserts that mutating a field that the step does NOT care
    // about leaves the step's input hash unchanged. Without these, the
    // per-step split is one refactor away from regressing back to the
    // shared-hash over-invalidation behavior.
    // ────────────────────────────────────────────────────────────────────

    fn ext_with_extras(extras: &str) -> serde_yaml::Value {
        let yaml = format!(
            r#"
extensions:
  my-ext:
    packages:
      foo: "*"
    types: [sysext]
{extras}
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn ext_install_hash(value: &serde_yaml::Value) -> String {
        compute_ext_install_input_hash_with_deps(value, "my-ext", &[])
            .unwrap()
            .config_hash
    }

    fn ext_install_hash_with_deps(value: &serde_yaml::Value, deps: &[(String, String)]) -> String {
        compute_ext_install_input_hash_with_deps(value, "my-ext", deps)
            .unwrap()
            .config_hash
    }

    fn dep(name: &str, fingerprint: &str) -> (String, String) {
        (name.to_string(), fingerprint.to_string())
    }

    /// The writer/reader drift bug: `ext install` stamped a deps-aware hash
    /// while `ext build`/`ext image` validated with the plain (empty-deps)
    /// hash, and the two NEVER agree when dep_state is non-empty — so every
    /// `depends_on` extension read as stale forever and died at build. The
    /// validators now go through `compute_ext_install_input_hash_current`,
    /// which reconstructs dep_state the same way the writer builds it; this
    /// pins the arithmetic fact that made the plain reader unfixable.
    #[test]
    fn a_deps_aware_stamp_never_matches_the_plain_hash() {
        let cfg = ext_with_extras("");
        let plain = ext_install_hash(&cfg);
        let with_deps = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.6p1")]);
        assert_ne!(
            plain, with_deps,
            "a validator using the plain hash can never accept a deps-aware stamp"
        );
    }

    /// `app -> mid -> base`: a change in base's lock state must move MID's
    /// fingerprint even though mid's own lock rows are untouched — the seed
    /// rpmdb app was built from changed. A non-transitive fingerprint left
    /// app's stamp valid over that change.
    #[test]
    fn dependency_fingerprint_propagates_through_chains() {
        let ext_yaml: serde_yaml::Value =
            serde_yaml::from_str("extensions:\n  base: {}\n  mid: {depends_on: [base]}\n").unwrap();
        let graph = crate::utils::ext_deps::DependencyGraph::from_extensions_section(
            ext_yaml.get("extensions").unwrap(),
            "qemux86-64",
            &std::collections::HashSet::new(),
        )
        .unwrap();

        let src = |v: &str| crate::utils::lockfile::ExtensionSourceLock {
            source_type: "package".to_string(),
            package: None,
            version: Some(v.to_string()),
            implied: true,
        };
        let mut lock = crate::utils::lockfile::LockFile::default();
        lock.set_extension_source("t", "mid", src("1.0.0"));
        lock.set_extension_source("t", "base", src("1.0.0"));

        let before = ext_dep_fingerprint(&lock, "t", &graph, "mid");
        lock.set_extension_source("t", "base", src("2.0.0"));
        let after = ext_dep_fingerprint(&lock, "t", &graph, "mid");
        assert_ne!(
            before, after,
            "a change beneath a dependency must move the dependency's fingerprint"
        );

        // And it reaches arbitrary depth: base's change moves base's own
        // fingerprint too, trivially, but the mid case above is the one a
        // per-node fingerprint got wrong.
    }

    /// With a broken graph or an unloadable lock, the reader degrades to an
    /// empty dep_state — and before `depends_on` was folded into the hash
    /// directly, that degraded hash was byte-identical to a pre-`depends_on`
    /// stamp, so a freshly declared dependency could VALIDATE a sysroot never
    /// seeded from it. Declaring an edge must move the hash even with no
    /// dep_state at all.
    #[test]
    fn declaring_a_dependency_moves_the_hash_even_without_dep_state() {
        let without = ext_with_extras("");
        let with = ext_with_extras("    depends_on: [weston-base]");
        assert_ne!(
            ext_install_hash(&without),
            ext_install_hash(&with),
            "a depends_on edit must be visible to the plain (empty dep_state) hash"
        );
    }

    #[test]
    fn dependency_change_invalidates_the_dependent() {
        // The reason this exists: a de-duplicated extension only ships what
        // its dependency does not provide, so its image is a function of the
        // dependency's contents. Without this the dependent's stamp stays
        // valid, its sysroot stays seeded from the old dependency, and if the
        // dependency *dropped* a package nothing provides those files at all.
        let cfg = ext_with_extras("");
        let before = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.6p1")]);
        let after = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.7p1")]);
        assert_ne!(
            before, after,
            "a dependency's package change must invalidate its dependent"
        );
    }

    #[test]
    fn dependency_version_bump_invalidates_the_dependent() {
        let cfg = ext_with_extras("");
        let before = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.6p1")]);
        let after = ext_install_hash_with_deps(&cfg, &[dep("base", "1.3.0|openssh=9.6p1")]);
        assert_ne!(before, after);
    }

    #[test]
    fn identical_dependency_state_is_stable() {
        // Over-invalidating costs a needless rebuild every run, which erodes
        // trust in stamps as much as under-invalidating does.
        let cfg = ext_with_extras("");
        let a = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.6p1")]);
        let b = ext_install_hash_with_deps(&cfg, &[dep("base", "1.2.0|openssh=9.6p1")]);
        assert_eq!(a, b);
    }

    #[test]
    fn dependency_order_does_not_affect_the_hash() {
        let cfg = ext_with_extras("");
        let a = ext_install_hash_with_deps(&cfg, &[dep("base", "1"), dep("mid", "2")]);
        let b = ext_install_hash_with_deps(&cfg, &[dep("mid", "2"), dep("base", "1")]);
        assert_eq!(a, b, "hash must not depend on iteration order");
    }

    #[test]
    fn no_dependencies_matches_the_plain_hash() {
        // Extensions without `depends_on` must keep their existing stamps —
        // this change must not invalidate every extension in every project.
        let cfg = ext_with_extras("");
        assert_eq!(
            ext_install_hash(&cfg),
            ext_install_hash_with_deps(&cfg, &[])
        );
    }

    fn ext_build_hash(value: &serde_yaml::Value) -> String {
        compute_ext_build_input_hash(value, "my-ext", std::path::Path::new("."), None, None, None)
            .unwrap()
            .config_hash
    }

    fn ext_image_hash(value: &serde_yaml::Value) -> String {
        compute_ext_image_input_hash(
            value,
            "my-ext",
            None,
            std::path::Path::new("."),
            None,
            None,
            None,
        )
        .unwrap()
        .config_hash
    }

    #[test]
    fn ext_install_unaffected_by_image_field() {
        let base = ext_with_extras("");
        let with_image = ext_with_extras("    image:\n      type: kab\n      args: \"-v 1.0.0\"");
        assert_eq!(ext_install_hash(&base), ext_install_hash(&with_image));
    }

    #[test]
    fn ext_install_unaffected_by_var_files() {
        let base = ext_with_extras("");
        let with_var = ext_with_extras("    var_files:\n      - \"var/lib/docker/**\"");
        assert_eq!(ext_install_hash(&base), ext_install_hash(&with_var));
    }

    #[test]
    fn ext_install_unaffected_by_subvolumes_and_post_build() {
        let base = ext_with_extras("");
        let with = ext_with_extras(
            "    subvolumes:\n      lib/docker:\n        nodatacow: true\n    post_build: scripts/build.sh",
        );
        assert_eq!(ext_install_hash(&base), ext_install_hash(&with));
    }

    #[test]
    fn ext_install_unaffected_by_metadata_and_runtime_fields() {
        let base = ext_with_extras("");
        let with = ext_with_extras(
            "    version: \"1.0.0\"\n    scopes: [system]\n    enable_services: [foo.service]\n    \
             on_merge: [\"echo hi\"]\n    on_unmerge: [\"echo bye\"]",
        );
        assert_eq!(ext_install_hash(&base), ext_install_hash(&with));
    }

    #[test]
    fn ext_build_unaffected_by_var_files_and_subvolumes() {
        let base = ext_with_extras("");
        let with = ext_with_extras(
            "    var_files:\n      - \"var/lib/docker/**\"\n    subvolumes:\n      lib/x:\n        nodatacow: true",
        );
        assert_eq!(ext_build_hash(&base), ext_build_hash(&with));
    }

    #[test]
    fn ext_build_unaffected_by_filesystem_override() {
        // The filesystem field is image-only — build must not see it.
        let base = ext_with_extras("");
        let with_fs = ext_with_extras("    filesystem: erofs-zst");
        assert_eq!(ext_build_hash(&base), ext_build_hash(&with_fs));
    }

    #[test]
    fn ext_image_includes_var_files_and_subvolumes() {
        let base = ext_with_extras("");
        let with = ext_with_extras(
            "    var_files:\n      - \"var/lib/docker/**\"\n    subvolumes:\n      lib/x:\n        nodatacow: true",
        );
        assert_ne!(ext_image_hash(&base), ext_image_hash(&with));
    }

    #[test]
    fn ext_build_content_changes_invalidate_when_post_build_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = tmp.path().join("build.sh");
        std::fs::write(&script, b"#!/bin/sh\necho original\n").unwrap();

        let config = ext_with_extras("    post_build: build.sh");
        let h1 = compute_ext_build_input_hash(&config, "my-ext", tmp.path(), None, None, None)
            .unwrap()
            .config_hash;

        std::fs::write(&script, b"#!/bin/sh\necho edited\n").unwrap();
        let h2 = compute_ext_build_input_hash(&config, "my-ext", tmp.path(), None, None, None)
            .unwrap()
            .config_hash;

        assert_ne!(
            h1, h2,
            "editing post_build script body should invalidate the build hash"
        );
    }

    fn runtime(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn runtime_install_unaffected_by_build_only_fields() {
        let base = runtime(
            r#"
packages:
  avocado-runtime: "*"
target: "x86_64"
"#,
        );
        let with_build_only = runtime(
            r#"
packages:
  avocado-runtime: "*"
target: "x86_64"
kernel:
  version: "6.6.*"
var:
  compression: zstd
var_files:
  - source: "files/x"
    dest: "lib/x"
post_build: scripts/post.sh
"#,
        );
        let h1 = compute_runtime_install_input_hash(&base, "dev")
            .unwrap()
            .config_hash;
        let h2 = compute_runtime_install_input_hash(&with_build_only, "dev")
            .unwrap()
            .config_hash;
        assert_eq!(h1, h2);
    }

    #[test]
    fn runtime_install_unaffected_by_top_level_rootfs_initramfs_filesystem() {
        let runtime_node = runtime(
            r#"
packages:
  avocado-runtime: "*"
target: "x86_64"
"#,
        );
        let parsed_a: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  filesystem: erofs-lz4
initramfs:
  filesystem: cpio.zst
"#,
        )
        .unwrap();
        let parsed_b: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  filesystem: erofs-zst
initramfs:
  filesystem: cpio
"#,
        )
        .unwrap();
        // install hash ignores the parsed/top-level filesystem entirely.
        let h_a = compute_runtime_install_input_hash(&runtime_node, "dev")
            .unwrap()
            .config_hash;
        let h_b = compute_runtime_install_input_hash(&runtime_node, "dev")
            .unwrap()
            .config_hash;
        assert_eq!(h_a, h_b);
        // sanity: build hash DOES include filesystem
        let b_a = compute_runtime_build_input_hash(
            &runtime_node,
            "dev",
            &parsed_a,
            std::path::Path::new("."),
        )
        .unwrap()
        .config_hash;
        let b_b = compute_runtime_build_input_hash(
            &runtime_node,
            "dev",
            &parsed_b,
            std::path::Path::new("."),
        )
        .unwrap()
        .config_hash;
        assert_ne!(
            b_a, b_b,
            "runtime build SHOULD invalidate on filesystem swap"
        );
    }

    #[test]
    fn sdk_install_unaffected_by_rootfs_initramfs_packages() {
        let base: serde_yaml::Value = serde_yaml::from_str(
            r#"
sdk:
  image: my-sdk:1
  packages:
    sdk-deps: "*"
rootfs:
  packages:
    pkg-a: "*"
initramfs:
  packages:
    pkg-b: "*"
"#,
        )
        .unwrap();
        let bumped: serde_yaml::Value = serde_yaml::from_str(
            r#"
sdk:
  image: my-sdk:1
  packages:
    sdk-deps: "*"
rootfs:
  packages:
    pkg-a: ">=2.0"
initramfs:
  packages:
    pkg-b: ">=3.0"
"#,
        )
        .unwrap();
        let h_base = compute_sdk_input_hash(&base).unwrap().config_hash;
        let h_bumped = compute_sdk_input_hash(&bumped).unwrap().config_hash;
        assert_eq!(
            h_base, h_bumped,
            "rootfs/initramfs package bumps must not invalidate the SDK install stamp"
        );
    }

    #[test]
    fn rootfs_install_ignores_unrelated_kernel_fields() {
        let base: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
kernel:
  version: "6.6.*"
  package: kernel-image
"#,
        )
        .unwrap();
        let with_metadata: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
kernel:
  version: "6.6.*"
  package: kernel-image
  metadata: cosmetic
  description: "added later"
"#,
        )
        .unwrap();
        let h_base = rootfs_config_hash(&base, std::path::Path::new("."));
        let h_extra = rootfs_config_hash(&with_metadata, std::path::Path::new("."));
        assert_eq!(
            h_base, h_extra,
            "adding unrelated keys under `kernel:` must not invalidate the rootfs install stamp"
        );
    }

    #[test]
    fn rootfs_install_invalidates_on_kernel_version_change() {
        let v1: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
kernel:
  version: "6.6.*"
"#,
        )
        .unwrap();
        let v2: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
kernel:
  version: "6.7.*"
"#,
        )
        .unwrap();
        let h_v1 = rootfs_config_hash(&v1, std::path::Path::new("."));
        let h_v2 = rootfs_config_hash(&v2, std::path::Path::new("."));
        assert_ne!(h_v1, h_v2);
    }

    #[test]
    fn rootfs_install_post_install_content_change_invalidates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = tmp.path().join("post.sh");
        std::fs::write(&script, b"#!/bin/sh\necho v1\n").unwrap();

        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
  post_install: post.sh
"#,
        )
        .unwrap();
        let h1 = rootfs_config_hash(&config, tmp.path());

        std::fs::write(&script, b"#!/bin/sh\necho v2\n").unwrap();
        let h2 = rootfs_config_hash(&config, tmp.path());

        assert_ne!(h1, h2);
    }

    #[test]
    fn rootfs_hash_stable_across_absent_and_explicit_default_packages() {
        // The install-skip decision rests on this: a project with no `rootfs:`
        // section and one that spells out the default meta-package install
        // exactly the same thing, so they must hash the same. Hashing the raw
        // config node instead of the effective set makes these differ and
        // forces a reinstall on any project that writes the default out.
        let absent: serde_yaml::Value = serde_yaml::from_str("sdk:\n  image: foo\n").unwrap();
        let explicit: serde_yaml::Value = serde_yaml::from_str(
            r#"
sdk:
  image: foo
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
"#,
        )
        .unwrap();

        assert_eq!(
            rootfs_config_hash(&absent, std::path::Path::new(".")),
            rootfs_config_hash(&explicit, std::path::Path::new(".")),
        );
    }

    #[test]
    fn rootfs_hash_changes_on_added_package() {
        let config: serde_yaml::Value = serde_yaml::from_str("sdk:\n  image: foo\n").unwrap();
        let root = std::path::Path::new(".");

        let base = default_rootfs_packages();
        let mut with_vim = base.clone();
        with_vim.insert(
            "vim".to_string(),
            serde_yaml::Value::String("*".to_string()),
        );

        let h_base = compute_rootfs_input_hash(&config, root, None, &test_sysroot_inputs(&base))
            .unwrap()
            .config_hash;
        let h_vim = compute_rootfs_input_hash(&config, root, None, &test_sysroot_inputs(&with_vim))
            .unwrap()
            .config_hash;

        assert_ne!(h_base, h_vim);
    }

    #[test]
    fn rootfs_hash_changes_on_feed_identity_and_weak_deps() {
        let config: serde_yaml::Value = serde_yaml::from_str("sdk:\n  image: foo\n").unwrap();
        let root = std::path::Path::new(".");
        let packages = default_rootfs_packages();

        let hash_of = |resolved: &SysrootStampInputs<'_>| {
            compute_rootfs_input_hash(&config, root, None, resolved)
                .unwrap()
                .config_hash
        };

        let base = test_sysroot_inputs(&packages);
        let h_base = hash_of(&base);

        // A snapshot bump moves repo_release — this is the hook that makes
        // `avocado update` land instead of being skipped as up to date.
        let h_release = hash_of(&SysrootStampInputs {
            repo_release: Some("2026.9.20260727"),
            ..test_sysroot_inputs(&packages)
        });
        assert_ne!(h_base, h_release, "repo_release must invalidate");

        let h_url = hash_of(&SysrootStampInputs {
            repo_url: Some("https://repo.avocadolinux.org/2026/next"),
            ..test_sysroot_inputs(&packages)
        });
        assert_ne!(h_base, h_url, "repo_url must invalidate");

        let h_weak = hash_of(&SysrootStampInputs {
            disable_weak_dependencies: true,
            ..test_sysroot_inputs(&packages)
        });
        assert_ne!(
            h_base, h_weak,
            "disable_weak_dependencies must invalidate — it changes what dnf pulls"
        );

        // `--dnf-args` reach the transaction verbatim, so an up-to-date sysroot
        // must not short-circuit past a run that passes different ones.
        let args = ["--enablerepo=extra".to_string()];
        let h_dnf = hash_of(&SysrootStampInputs {
            dnf_args: Some(&args),
            ..test_sysroot_inputs(&packages)
        });
        assert_ne!(
            h_base, h_dnf,
            "dnf_args must invalidate — they change what the transaction resolves"
        );

        // An empty list is the same transaction as none at all, so it must not
        // invalidate; otherwise `--dnf-args ''` would force a pointless rebuild.
        let empty: [String; 0] = [];
        let h_empty = hash_of(&SysrootStampInputs {
            dnf_args: Some(&empty),
            ..test_sysroot_inputs(&packages)
        });
        assert_eq!(
            h_base, h_empty,
            "an empty dnf_args list must not invalidate"
        );
    }

    /// The SDK image runs the install, so repointing it has to invalidate — the
    /// sibling `compute_sdk_input_hash` already treats it as an input.
    #[test]
    fn rootfs_stamp_tracks_sdk_image() {
        let root = std::path::Path::new(".");
        let packages = default_rootfs_packages();
        let inputs = test_sysroot_inputs(&packages);

        let a: serde_yaml::Value =
            serde_yaml::from_str("sdk:\n  image: docker.io/avocadolinux/sdk:apollo-edge\n")
                .unwrap();
        let b: serde_yaml::Value =
            serde_yaml::from_str("sdk:\n  image: docker.io/avocadolinux/sdk:dev\n").unwrap();

        let ha = compute_rootfs_input_hash(&a, root, None, &inputs)
            .unwrap()
            .config_hash;
        let hb = compute_rootfs_input_hash(&b, root, None, &inputs)
            .unwrap()
            .config_hash;
        assert_ne!(ha, hb, "sdk.image must invalidate the sysroot stamp");
    }

    #[test]
    fn rootfs_package_list_hash_tracks_lock_pins() {
        let config: serde_yaml::Value = serde_yaml::from_str("sdk:\n  image: foo\n").unwrap();
        let root = std::path::Path::new(".");
        let packages = default_rootfs_packages();

        let pinned: std::collections::HashMap<String, String> =
            std::collections::HashMap::from([(
                "avocado-pkg-rootfs".to_string(),
                "2026.9-r0.0".to_string(),
            )]);
        let repinned: std::collections::HashMap<String, String> = std::collections::HashMap::from(
            [("avocado-pkg-rootfs".to_string(), "2026.10-r0.0".to_string())],
        );

        let inputs_for = |locked: Option<&std::collections::HashMap<String, String>>| {
            compute_rootfs_input_hash(
                &config,
                root,
                None,
                &SysrootStampInputs {
                    locked_packages: locked,
                    ..test_sysroot_inputs(&packages)
                },
            )
            .unwrap()
        };

        let a = inputs_for(Some(&pinned));
        let b = inputs_for(Some(&repinned));
        let cleared = inputs_for(None);

        // The config side is untouched by a re-pin; only the package list moves.
        assert_eq!(a.config_hash, b.config_hash);
        assert_ne!(a.package_list_hash, b.package_list_hash);

        // `avocado unlock` clears the section. That has to read as stale, which
        // is why an empty pin set hashes to a value rather than to None —
        // `is_current` only compares two Some sides.
        assert!(cleared.package_list_hash.is_some());
        assert_ne!(a.package_list_hash, cleared.package_list_hash);

        let stamp = Stamp::rootfs_install("qemux86-64", a.clone(), StampOutputs::default());
        assert!(stamp.is_current(&a));
        assert!(!stamp.is_current(&b), "a re-pin must invalidate the stamp");
        assert!(
            !stamp.is_current(&cleared),
            "avocado unlock must invalidate the stamp"
        );
    }

    #[test]
    fn rootfs_package_list_hash_is_order_independent() {
        // Lock pins come out of a HashMap, so iteration order varies between
        // runs. The digest must not.
        let a = std::collections::HashMap::from([
            ("alpha".to_string(), "1".to_string()),
            ("beta".to_string(), "2".to_string()),
            ("gamma".to_string(), "3".to_string()),
        ]);
        let b = std::collections::HashMap::from([
            ("gamma".to_string(), "3".to_string()),
            ("alpha".to_string(), "1".to_string()),
            ("beta".to_string(), "2".to_string()),
        ]);

        assert_eq!(package_list_hash(Some(&a)), package_list_hash(Some(&b)));
    }

    #[test]
    fn initramfs_hash_is_independent_of_rootfs_section() {
        // The two sysroots install independently; a rootfs-only edit must not
        // invalidate the initramfs stamp (and so reinstall it for nothing).
        let base: serde_yaml::Value = serde_yaml::from_str(
            r#"
initramfs:
  packages:
    avocado-pkg-initramfs: "*"
"#,
        )
        .unwrap();
        let with_rootfs: serde_yaml::Value = serde_yaml::from_str(
            r#"
initramfs:
  packages:
    avocado-pkg-initramfs: "*"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
    vim: "*"
"#,
        )
        .unwrap();

        let packages = std::collections::HashMap::from([(
            "avocado-pkg-initramfs".to_string(),
            serde_yaml::Value::String("*".to_string()),
        )]);
        let root = std::path::Path::new(".");

        let h_base =
            compute_initramfs_input_hash(&base, root, None, &test_sysroot_inputs(&packages))
                .unwrap()
                .config_hash;
        let h_with =
            compute_initramfs_input_hash(&with_rootfs, root, None, &test_sysroot_inputs(&packages))
                .unwrap()
                .config_hash;

        assert_eq!(h_base, h_with);
    }

    #[test]
    fn rootfs_preprocessed_overlay_value_change_invalidates() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("overlay/etc")).unwrap();
        std::fs::write(
            tmp.path().join("overlay/etc/config.toml"),
            "token = \"{{ env.STAMP_OVL_TOKEN }}\"\n",
        )
        .unwrap();

        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
  overlay:
    dir: overlay
    preprocess:
      - etc/config.toml
"#,
        )
        .unwrap();

        std::env::set_var("STAMP_OVL_TOKEN", "aaa");
        let h1 = rootfs_config_hash(&config, tmp.path());
        std::env::set_var("STAMP_OVL_TOKEN", "bbb");
        let h2 = rootfs_config_hash(&config, tmp.path());

        // Changing a value referenced by a preprocessed overlay file must
        // invalidate the rootfs install hash so the image rebuilds.
        assert_ne!(h1, h2);
    }

    #[test]
    fn ext_build_hash_reflects_selected_runtime_for_preprocessed_overlay() {
        // An ext overlay whose content depends on `{{ avocado.runtime }}` must
        // produce different build hashes per selected runtime, so switching
        // runtimes doesn't reuse a stale artifact (the ext-build stamp isn't
        // otherwise runtime-keyed).
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("overlay/etc")).unwrap();
        std::fs::write(
            tmp.path().join("overlay/etc/r.conf"),
            "runtime = {{ avocado.runtime }}\n",
        )
        .unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    overlay:
      dir: overlay
      preprocess:
        - etc/r.conf
"#,
        )
        .unwrap();

        let h_dev = compute_ext_build_input_hash(
            &config,
            "my-ext",
            tmp.path(),
            Some("qemux86-64"),
            Some("dev"),
            None,
        )
        .unwrap()
        .config_hash;
        let h_prod = compute_ext_build_input_hash(
            &config,
            "my-ext",
            tmp.path(),
            Some("qemux86-64"),
            Some("prod"),
            None,
        )
        .unwrap()
        .config_hash;
        assert_ne!(h_dev, h_prod);
    }

    #[test]
    fn ext_build_hash_reflects_target_board_override_for_preprocessed_overlay() {
        // An ext overlay whose content depends on `{{ avocado.target.board }}`
        // must produce different build hashes per --target-board value, so a
        // board switch invalidates the stamp instead of reusing a stale
        // artifact. Both calls pass an explicit cli_target_board, which the
        // resolver checks first, so this is independent of AVOCADO_TARGET_BOARD.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("overlay/etc")).unwrap();
        std::fs::write(
            tmp.path().join("overlay/etc/b.conf"),
            "board = {{ avocado.target.board }}\n",
        )
        .unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  my-ext:
    overlay:
      dir: overlay
      preprocess:
        - etc/b.conf
"#,
        )
        .unwrap();

        let h_a = compute_ext_build_input_hash(
            &config,
            "my-ext",
            tmp.path(),
            Some("imx8mp-var-dart"),
            None,
            Some("variscite-sonata"),
        )
        .unwrap()
        .config_hash;
        let h_b = compute_ext_build_input_hash(
            &config,
            "my-ext",
            tmp.path(),
            Some("imx8mp-var-dart"),
            None,
            Some("other-board"),
        )
        .unwrap()
        .config_hash;
        assert_ne!(
            h_a, h_b,
            "switching --target-board must invalidate the ext build stamp"
        );
    }

    #[test]
    fn rootfs_verbatim_overlay_hashes_file_contents() {
        // A verbatim overlay is applied by `cp`, not RPM, so its contents must be
        // folded into the install stamp — editing an overlay file has to make the
        // stamp stale, otherwise the change silently never reaches the image
        // (ENG-2440).
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("overlay/etc")).unwrap();
        std::fs::write(tmp.path().join("overlay/etc/f.txt"), "v1").unwrap();

        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
  overlay:
    dir: overlay
"#,
        )
        .unwrap();

        let h1 = rootfs_config_hash(&config, tmp.path());
        std::fs::write(tmp.path().join("overlay/etc/f.txt"), "v2-different").unwrap();
        let h2 = rootfs_config_hash(&config, tmp.path());
        assert_ne!(
            h1, h2,
            "editing a verbatim overlay file must invalidate the stamp"
        );
    }

    #[test]
    fn rootfs_bare_string_overlay_hashes_file_contents() {
        // The bare-string form (`overlay: dirname`) must hash the named dir, not
        // the "overlay" default — a regression guard for the shared
        // parse_overlay_config path.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("custom/etc")).unwrap();
        std::fs::write(tmp.path().join("custom/etc/f.txt"), "v1").unwrap();

        let config: serde_yaml::Value = serde_yaml::from_str(
            r#"
rootfs:
  packages:
    avocado-pkg-rootfs: "*"
  overlay: custom
"#,
        )
        .unwrap();

        let h1 = rootfs_config_hash(&config, tmp.path());
        std::fs::write(tmp.path().join("custom/etc/f.txt"), "v2-different").unwrap();
        let h2 = rootfs_config_hash(&config, tmp.path());
        assert_ne!(
            h1, h2,
            "editing a bare-string overlay's file must invalidate the stamp"
        );
    }

    #[test]
    fn stamp_version_bump_invalidates_old_stamps() {
        let inputs = StampInputs::new("sha256:abc".to_string());
        let mut stamp = Stamp::sdk_install("x86_64", inputs.clone(), StampOutputs::default());
        // Forge an older version.
        stamp.version = STAMP_VERSION - 1;
        assert!(
            !stamp.is_current(&inputs),
            "older stamp version should be reported as stale"
        );
    }
}
