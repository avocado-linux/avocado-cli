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

use crate::utils::container::SdkContainer;
use crate::utils::volume::VolumeState;

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

/// Current stamp format version. Any stamp at an older version is treated as
/// stale.
///
/// - 1 → 2: per-step input hashes — each component step got its own narrow
///   hash, so stamps written under the broader shared hashes could not be
///   compared with current inputs.
/// - 2 → 3: input-hash coverage — compile/install script content,
///   `package_files` source trees, runtime `var_files` content, `permissions`,
///   `image` (kab args, verity), `signing`, `container_args`, `src_dir`, and a
///   strict `package_list_hash` comparison. Stamps written without those
///   inputs would read as current across edits they never saw.
/// - 3 → 4: output digests — steps record `content_hash` and downstream steps
///   fold it into their inputs. A version-3 image stamp was written without
///   its upstream's digest in the input, so it cannot be compared with one that
///   has it.
pub const STAMP_VERSION: u32 = 4;

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
    /// Digest of what the step produced — a sysroot's tree hash, an image's
    /// sha256. Computed in the container, where the output lives, and folded
    /// into the *next* step's input hash: a step that re-runs and produces
    /// identical bytes stops the rebuild there instead of cascading. This is
    /// the closed half of fingerprinting — it moves iff the bytes moved,
    /// whatever input caused it, including inputs the host cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
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

    /// Create rootfs image stamp
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rootfs_image(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Image,
            StampComponent::Rootfs,
            None,
            target.to_string(),
            inputs,
            outputs,
        )
    }

    /// Create initramfs image stamp
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn initramfs_image(target: &str, inputs: StampInputs, outputs: StampOutputs) -> Self {
        Self::new(
            StampCommand::Image,
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

        // Strict: a recorded hash on one side and none on the other is stale,
        // not a match by omission. The permissive `(Some, Some)`-only form
        // let a stamp with no recorded pins pass against any current set,
        // which is an under-invalidation — the failure direction this module
        // refuses. Pre-version-3 stamps never reach this line (the version
        // check above fails them), so nothing written under the old rule is
        // compared under the new one.
        self.inputs.package_list_hash == current_inputs.package_list_hash
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

    /// Rootfs image requirement
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rootfs_image() -> Self {
        Self::new(StampCommand::Image, StampComponent::Rootfs, None)
    }

    /// Initramfs image requirement
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn initramfs_image() -> Self {
        Self::new(StampCommand::Image, StampComponent::Initramfs, None)
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
    /// Stamp exists but its JSON does not parse. Kept apart from `Missing`
    /// so the error can say the file is there and cannot be read.
    Unreadable { reason: String },
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
    /// Requirements whose stamp exists but could not be parsed
    pub unreadable: Vec<(StampRequirement, String)>,
}

impl StampValidationResult {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if all requirements are satisfied
    pub fn is_satisfied(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty() && self.unreadable.is_empty()
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

    /// Add a requirement whose stamp exists but could not be parsed
    pub fn add_unreadable(&mut self, req: StampRequirement, reason: String) {
        self.unreadable.push((req, reason));
    }

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
            unreadable: self.unreadable,
            runs_on: runs_on.map(|s| s.to_string()),
            search_root: None,
        }
    }
}

/// Which docker daemon a stamp read went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampDaemon {
    /// No `DOCKER_HOST` in the environment: the host's default daemon.
    Host,
    /// `DOCKER_HOST` is the avocado-vm's forwarded socket.
    AvocadoVm,
    /// `DOCKER_HOST` points somewhere other than the avocado-vm socket.
    DockerHost(String),
}

impl StampDaemon {
    /// The daemon `docker` resolves to in this process. `DOCKER_HOST` is set
    /// process-wide by VM routing, so reading the environment here sees the
    /// effective value.
    pub fn current() -> Self {
        if crate::utils::container::is_vm_routing_active() {
            return Self::AvocadoVm;
        }
        match std::env::var("DOCKER_HOST") {
            Ok(host) if !host.is_empty() => Self::DockerHost(host),
            _ => Self::Host,
        }
    }
}

impl fmt::Display for StampDaemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => write!(f, "the host docker daemon"),
            Self::AvocadoVm => write!(f, "the avocado-vm docker daemon"),
            Self::DockerHost(host) => write!(f, "the docker daemon at DOCKER_HOST={host}"),
        }
    }
}

/// Where a stamp read looked. Stamps live at `/opt/_avocado/<target>/.stamps`
/// inside the project's docker volume, on whichever daemon the process routed
/// to. A stamp written under a different target, volume or daemon is
/// invisible to the read, and that is the usual reason one is "missing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StampSearchRoot {
    pub target: String,
    /// `None` when the project directory has no `.avocado-state`.
    pub volume: Option<String>,
    pub daemon: StampDaemon,
}

impl StampSearchRoot {
    /// The root `container` reads stamps from for `target`.
    ///
    /// The volume name comes from `.avocado-state` in `container.cwd`, which is
    /// the directory `get_or_create_volume` keys on (not `src_dir`). Only ever
    /// read it here: `get_or_create_volume` would mint a fresh, empty volume
    /// from inside an error path.
    pub fn for_container(container: &SdkContainer, target: &str) -> Self {
        let volume = VolumeState::load_from_dir(&container.cwd)
            .ok()
            .flatten()
            .map(|state| state.volume_name);
        Self {
            target: target.to_string(),
            volume,
            daemon: StampDaemon::current(),
        }
    }

    /// The `$AVOCADO_PREFIX/.stamps` directory inside the container.
    pub fn stamps_dir(&self) -> String {
        format!("/opt/_avocado/{}/.stamps", self.target)
    }
}

/// Error when stamp validation fails
#[derive(Debug)]
pub struct StampValidationError {
    pub context: String,
    pub missing: Vec<StampRequirement>,
    pub stale: Vec<(StampRequirement, String)>,
    pub unreadable: Vec<(StampRequirement, String)>,
    /// Remote host if using --runs-on (for fix command suggestions)
    pub runs_on: Option<String>,
    /// Where the read looked, when the caller knows.
    pub search_root: Option<StampSearchRoot>,
}

impl std::error::Error for StampValidationError {}

impl StampValidationError {
    /// Record where the failed read looked so the message can name it.
    pub fn with_search_root(mut self, root: StampSearchRoot) -> Self {
        self.search_root = Some(root);
        self
    }

    /// Lines naming the searched root. Shared by both renderers so the prose
    /// and JSON paths cannot drift.
    fn search_root_lines(&self) -> Vec<String> {
        let Some(root) = &self.search_root else {
            return Vec::new();
        };
        let mut looked = format!("Looked in {} (target {})", root.stamps_dir(), root.target);
        if let Some(volume) = &root.volume {
            looked.push_str(&format!(", docker volume {volume}"));
        }
        looked.push_str(&format!(", via {}", root.daemon));
        if let Some(remote) = &self.runs_on {
            looked.push_str(&format!(", exported over NFS to {remote} (--runs-on)"));
        }
        looked.push('.');
        vec![
            looked,
            "Stamps written under a different target, volume or daemon are not found here."
                .to_string(),
        ]
    }

    /// Collect unique fix commands, using runs_on hint for SDK install commands
    fn fix_commands(&self) -> Vec<String> {
        let runs_on_ref = self.runs_on.as_deref();
        let local_arch = get_local_arch();

        let mut fixes: Vec<String> = self
            .missing
            .iter()
            .chain(self.stale.iter().map(|(req, _)| req))
            .chain(self.unreadable.iter().map(|(req, _)| req))
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
            print_warning("Stale steps:", OutputLevel::Normal);
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

        if !self.unreadable.is_empty() {
            print_warning(
                "Unreadable steps (stamp exists but could not be parsed; rerunning the step rewrites it):",
                OutputLevel::Normal,
            );
            for (req, reason) in &self.unreadable {
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

        for line in self.search_root_lines() {
            print_info(&line, OutputLevel::Normal);
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
            writeln!(f, "  Stale steps:")?;
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

        if !self.unreadable.is_empty() {
            writeln!(
                f,
                "  Unreadable steps (stamp exists but could not be parsed; rerunning the step rewrites it):"
            )?;
            for (req, reason) in &self.unreadable {
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

        let search_root_lines = self.search_root_lines();
        if !search_root_lines.is_empty() {
            for line in &search_root_lines {
                writeln!(f, "  {line}")?;
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
/// A path the config declares but that does not exist is an error, not a
/// sentinel. The build would fail on it anyway (`bash <missing>`), so this
/// surfaces the misconfiguration at the stamp check instead of mid-build —
/// and, more to the point, a sentinel collides: every unresolvable path
/// hashed to the same literal, so two extensions whose scripts the host
/// could not see read as identical regardless of content. Callers that
/// hash a path the host legitimately cannot reach must not call this; see
/// [`ext_content_root`].
fn hash_script_at(project_root: &Path, rel_path: &str) -> Result<String> {
    crate::utils::overlay_preprocess::path_content_digest(project_root, rel_path)?.ok_or_else(
        || {
            anyhow::anyhow!(
                "Config names `{rel_path}` but it does not exist under {}",
                project_root.display()
            )
        },
    )
}

/// Build the `{path, content_sha256}` mapping that we embed into input
/// hashes for scripts and source trees. Both fields go into the parent
/// mapping so a path swap OR a content edit invalidates.
fn script_hash_value(project_root: &Path, rel_path: &str) -> Result<serde_yaml::Value> {
    let mut m = serde_yaml::Mapping::new();
    m.insert(
        serde_yaml::Value::String("path".to_string()),
        serde_yaml::Value::String(rel_path.to_string()),
    );
    m.insert(
        serde_yaml::Value::String("content_sha256".to_string()),
        serde_yaml::Value::String(hash_script_at(project_root, rel_path)?),
    );
    Ok(serde_yaml::Value::Mapping(m))
}

/// Fold `script_hash_value` for `rel_path` under `key` — the one-line form
/// every "this config key names a file the build reads" site uses.
fn fold_file_content(
    hash_data: &mut serde_yaml::Mapping,
    key: &str,
    root: &Path,
    rel_path: &str,
) -> Result<()> {
    hash_data.insert(
        serde_yaml::Value::String(key.to_string()),
        script_hash_value(root, rel_path)?,
    );
    Ok(())
}

/// Where an extension's own files live on the host, if anywhere.
///
/// - No `source:` — a local extension; its files are under the project root.
/// - `source: {type: path, path: P}` — a working copy on the host; `P`
///   resolves against the project root the same way `ext fetch` resolves it.
/// - `source: {type: git, ...}` — fetched into the SDK volume. The host never
///   sees those bytes, so nothing here can hash them. `source` itself (url +
///   ref) is already folded; the content behind it is Layer 2's job — the
///   in-container tree digest — not this function's. Returns `None`, and
///   callers emit no content key rather than a meaningless one.
fn ext_content_root(ext: &serde_yaml::Value, project_root: &Path) -> Option<std::path::PathBuf> {
    let Some(source) = ext.get("source") else {
        return Some(project_root.to_path_buf());
    };
    match source.get("type").and_then(|t| t.as_str()) {
        Some("path") => source.get("path").and_then(|p| p.as_str()).map(|p| {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                project_root.join(p)
            }
        }),
        _ => None,
    }
}

/// Every `packages.<pkg>.{compile, install}` pair under a `packages:` map,
/// resolved to the script files they run: `compile` names an
/// `sdk.compile.<section>` whose own `compile:` is a project-root-relative
/// script; `install` is a script relative to the owning component's source.
/// Folds both contents. `content_root` is where `install` scripts live —
/// `None` skips them (the host cannot see a git-fetched extension's files).
fn fold_package_scripts(
    hash_data: &mut serde_yaml::Mapping,
    prefix: &str,
    packages: &serde_yaml::Value,
    config: &serde_yaml::Value,
    project_root: &Path,
    content_root: Option<&Path>,
) -> Result<()> {
    let Some(pkgs) = packages.as_mapping() else {
        return Ok(());
    };
    for (pkg, spec) in pkgs {
        let Some(pkg) = pkg.as_str() else { continue };
        if let Some(section) = spec.get("compile").and_then(|v| v.as_str()) {
            if let Some(script) = config
                .get("sdk")
                .and_then(|s| s.get("compile"))
                .and_then(|c| c.get(section))
                .and_then(|sec| sec.get("compile"))
                .and_then(|v| v.as_str())
            {
                fold_file_content(
                    hash_data,
                    &format!("{prefix}.packages.{pkg}.compile_script"),
                    project_root,
                    script,
                )?;
            }
        }
        if let (Some(install), Some(root)) =
            (spec.get("install").and_then(|v| v.as_str()), content_root)
        {
            fold_file_content(
                hash_data,
                &format!("{prefix}.packages.{pkg}.install_script"),
                root,
                install,
            )?;
        }
    }
    Ok(())
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
        // Extra container args reach every SDK run — a `-v host:ctr` mount
        // changes what the install can see, so it is part of the environment
        // the sysroot was produced in. Machine-specific args make the stamp
        // machine-specific, which is the correct reading of them.
        if let Some(args) = sdk.get("container_args") {
            hash_data.insert(
                serde_yaml::Value::String("sdk.container_args".to_string()),
                args.clone(),
            );
        }
    }
    // `src_dir` moves the base every relative path in the config resolves
    // against; the content behind those paths is hashed elsewhere, but the
    // base itself is an input to where the SDK mounts `/opt/src` from.
    if let Some(src_dir) = config.get("src_dir") {
        hash_data.insert(
            serde_yaml::Value::String("src_dir".to_string()),
            src_dir.clone(),
        );
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

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
/// The image step reads the built sysroot and a handful of image-only config
/// keys. The sysroot enters as `ext build`'s recorded output digest — the chain
/// link — and nothing the *build* read (overlay, `post_build`, compile scripts,
/// `package_files`) is folded here: those reach the image only through the tree,
/// and the digest already says whether the tree changed. Folding them directly
/// would make the image re-run for a build input edit that left the tree
/// byte-identical, which is exactly the cascade the digest exists to stop.
///
/// The exclude list the imager applies is folded too, so a change to
/// `package_state_paths()` invalidates every image by itself.
// The interpolation trio (target, runtime, cli_target_board) is kept for
// signature stability with the build hash; the image itself does not
// interpolate. A context struct once a fourth CLI override lands.
#[allow(clippy::too_many_arguments)]
pub fn compute_ext_image_input_hash(
    config: &serde_yaml::Value,
    ext_name: &str,
    filesystem: Option<&str>,
    _project_root: &Path,
    _target: Option<&str>,
    _runtime: Option<&str>,
    _cli_target_board: Option<&str>,
    build_content_hash: Option<&str>,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();
    let key = |k: &str| serde_yaml::Value::String(format!("ext.{ext_name}.{k}"));

    // What `ext build` actually produced. Strict: an absent digest is folded
    // as such, so a stamp written with one never matches a run without one.
    hash_data.insert(
        key("build_content_hash"),
        serde_yaml::Value::String(build_content_hash.unwrap_or("<none>").to_string()),
    );

    if let Some(ext) = config.get("extensions").and_then(|e| e.get(ext_name)) {
        // `version` names the image file; `types` decides what is imaged;
        // `image` carries type/args/verity; `var_files` are excluded from the
        // image; `subvolumes` shape the var partition it feeds.
        for k in ["version", "types", "image", "var_files", "subvolumes"] {
            if let Some(v) = ext.get(k) {
                hash_data.insert(key(k), v.clone());
            }
        }
        // A kab-wrapped image is signed with the keyset; a rotated key must
        // re-wrap. Only when the image is kab — a stray env var must not churn
        // raw images. Set but unreadable is an error, like a missing script.
        let is_kab = ext
            .get("image")
            .and_then(|i| i.get("type"))
            .and_then(|t| t.as_str())
            == Some("kab");
        if is_kab {
            if let Ok(keyset) = std::env::var("KAB_KEYSET_FILE") {
                let digest = crate::utils::overlay_preprocess::path_content_digest(
                    Path::new("/"),
                    keyset.trim_start_matches('/'),
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!("KAB_KEYSET_FILE is set but `{keyset}` does not exist")
                })?;
                hash_data.insert(key("kab_keyset"), serde_yaml::Value::String(digest));
            }
        }
    }
    if let Some(fs) = filesystem {
        hash_data.insert(key("filesystem"), serde_yaml::Value::String(fs.to_string()));
    }
    hash_data.insert(
        key("image_excludes"),
        serde_yaml::Value::Sequence(
            package_state_paths()
                .into_iter()
                .map(|p| serde_yaml::Value::String(p.to_string()))
                .collect(),
        ),
    );
    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for **rootfs image** / **initramfs image**.
///
/// `section` is `"rootfs"` / `"initramfs"`; `resolved_section` is what
/// `Config::resolve_image_section` returns for the build target — never the
/// raw node, so a `target-<name>:` override reaches the hash. `config` is the
/// merged config the build reads (`parsed`); `runtime_name` is set on the
/// `runtime build` path, which inlines both image steps. Folded:
///
/// - `<section>.install.content_hash` — the install stamp's digest. What the
///   step images is the sysroot, which the host cannot see; this chains the
///   two stamps, overlay and all.
/// - `<section>.image_section` — the whole resolved section, not a picked
///   subset, so an image-side key this module has not heard of still
///   invalidates (over-invalidation is the safe side).
/// - `<section>.post_install` — by content, so an in-place edit invalidates.
///   Absent folds nothing, as the install hash does.
/// - `permissions` — the bodies behind `<section>.permissions: <name>` refs.
/// - `source_date_epoch` — `mkfs.erofs -T` and the cpio mtime normalization.
/// - `sdk.image` — the container whose mkfs/cpio/zstd produce the bytes.
/// - `runtimes.<rt>.{var,version,rootfs,initramfs}`, and the same four under
///   each `runtimes.<rt>.target-*:` block since the build resolves those
///   overrides for its target: `var` drives the initramfs encrypt marker,
///   `version` rides the kab `-v`, `rootfs`/`initramfs` carry inline
///   per-runtime permissions. Deliberately NOT the whole `runtimes.<rt>` node
///   — it holds `extensions` and `packages`, and re-imaging the rootfs
///   because an extension was added is exactly the inner loop this stamp
///   exists to protect.
/// - `<section>.kab_keyset` — the keyset file's digest, only when
///   `image.type` is `kab` and `KAB_KEYSET_FILE` is set: a rotated key must
///   invalidate and the path alone cannot show it. Set but unreadable is an
///   error, like a declared script that is missing. Nothing is folded for a
///   non-kab image, so a stray env var cannot churn it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn compute_sysroot_image_input_hash(
    section: &str,
    resolved_section: &serde_yaml::Value,
    install_content_hash: &str,
    config: &serde_yaml::Value,
    runtime_name: Option<&str>,
    project_root: &Path,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();
    hash_data.insert(
        serde_yaml::Value::String(format!("{section}.install.content_hash")),
        serde_yaml::Value::String(install_content_hash.to_string()),
    );
    hash_data.insert(
        serde_yaml::Value::String(format!("{section}.image_section")),
        resolved_section.clone(),
    );
    if let Some(post_install) = resolved_section
        .get("post_install")
        .and_then(|v| v.as_str())
    {
        fold_file_content(
            &mut hash_data,
            &format!("{section}.post_install"),
            project_root,
            post_install,
        )?;
    }
    for (key, value) in [
        ("permissions", config.get("permissions")),
        ("source_date_epoch", config.get("source_date_epoch")),
        ("sdk.image", config.get("sdk").and_then(|s| s.get("image"))),
    ] {
        if let Some(v) = value {
            hash_data.insert(serde_yaml::Value::String(key.to_string()), v.clone());
        }
    }
    let runtime = runtime_name.and_then(|rt| Some((rt, config.get("runtimes")?.get(rt)?)));
    if let Some((rt, runtime)) = runtime {
        // The base block plus every `target-*:` override block: the build
        // resolves those for its target, and folding all of them
        // over-invalidates across targets rather than missing an opt-in.
        let overrides = runtime
            .as_mapping()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| {
                let k = k.as_str()?;
                k.starts_with("target-")
                    .then(|| (format!("runtimes.{rt}.{k}"), v))
            });
        for (prefix, node) in std::iter::once((format!("runtimes.{rt}"), runtime)).chain(overrides)
        {
            for k in ["var", "version", "rootfs", "initramfs"] {
                if let Some(v) = node.get(k) {
                    hash_data.insert(
                        serde_yaml::Value::String(format!("{prefix}.{k}")),
                        v.clone(),
                    );
                }
            }
        }
    }
    let is_kab = resolved_section
        .get("image")
        .and_then(|i| i.get("type"))
        .and_then(|t| t.as_str())
        == Some("kab");
    if let Some(keyset) = std::env::var("KAB_KEYSET_FILE").ok().filter(|_| is_kab) {
        // Against the cwd, which is how the build's own existence check
        // resolves it.
        let digest = crate::utils::overlay_preprocess::path_content_digest(Path::new(""), &keyset)?
            .ok_or_else(|| {
                anyhow::anyhow!("KAB_KEYSET_FILE points to '{keyset}' but the file does not exist.")
            })?;
        hash_data.insert(
            serde_yaml::Value::String(format!("{section}.kab_keyset")),
            serde_yaml::Value::String(digest),
        );
    }
    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Mapping construction for `ext build`'s input hash: everything the build
/// reads, by content where it is a file.
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
        let content_root = ext_content_root(ext, project_root);
        let key = |k: &str| serde_yaml::Value::String(format!("ext.{ext_name}.{k}"));

        // Install-time inputs are also build-time inputs — a package change
        // invalidates everything downstream.
        if let Some(deps) = ext.get("packages") {
            hash_data.insert(key("dependencies"), deps.clone());
            fold_package_scripts(
                &mut hash_data,
                &format!("ext.{ext_name}"),
                deps,
                config,
                project_root,
                content_root.as_deref(),
            )?;
        }
        // YAML-only inputs the build bakes into the image: the resolved
        // version names the image file and rides the kabtool `-v`; the rest
        // become unit wiring, sysusers entries, passwd/group edits, module
        // lists. None of them is a file, so the node is the whole input.
        for k in [
            "types",
            "source",
            "image",
            "version",
            "enable_services",
            "on_merge",
            "on_unmerge",
            "sysusers",
            "kernel_modules",
            "ld_so_conf_d",
            "scopes",
            "reload_service_manager",
            "users",
            "groups",
        ] {
            if let Some(v) = ext.get(k) {
                hash_data.insert(key(k), v.clone());
            }
        }
        // `version: {file, key}` reads the version out of a file at build time.
        if let (Some(file), Some(root)) = (
            ext.get("version")
                .and_then(|v| v.get("file"))
                .and_then(|f| f.as_str()),
            content_root.as_deref(),
        ) {
            fold_file_content(
                &mut hash_data,
                &format!("ext.{ext_name}.version_file"),
                root,
                file,
            )?;
        }
        if let Some(overlay) = ext.get("overlay") {
            hash_data.insert(key("overlay"), overlay.clone());
            if let Some(root) = content_root.as_deref() {
                fold_overlay_content_hash(
                    &mut hash_data,
                    &format!("ext.{ext_name}.overlay_content"),
                    overlay,
                    config,
                    root,
                    target,
                    runtime,
                    cli_target_board,
                )?;
            }
        }
        if let (Some(post_build), Some(root)) = (
            ext.get("post_build").and_then(|v| v.as_str()),
            content_root.as_deref(),
        ) {
            fold_file_content(
                &mut hash_data,
                &format!("ext.{ext_name}.post_build"),
                root,
                post_build,
            )?;
        }
        // A compiled extension's source. `package_files` is the declared set of
        // files the extension is built from — it is what `ext package` stages,
        // and the closest thing to a manifest of what a compile script reads.
        // Folded only when something is compiled; without a compile step the
        // list only feeds RPM packaging, which has no stamp.
        let has_compile = ext
            .get("packages")
            .and_then(|p| p.as_mapping())
            .is_some_and(|m| m.values().any(|spec| spec.get("compile").is_some()));
        if let (true, Some(files), Some(root)) = (
            has_compile,
            ext.get("package_files").and_then(|v| v.as_sequence()),
            content_root.as_deref(),
        ) {
            let patterns: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
            hash_data.insert(
                key("package_files"),
                serde_yaml::Value::String(package_files_digest(root, &patterns)?),
            );
        }
    }

    Ok(hash_data)
}

/// One digest over every file a `package_files` list names, patterns expanded
/// with the same semantics as the packaging script's `shopt -s globstar`:
/// `*` stays within a path component, `**` crosses them. Matches are sorted,
/// each contributes its relative path and content digest, and a pattern that
/// matches nothing contributes its own text — so removing the last match
/// still moves the digest, as the packaging step would notice too.
fn package_files_digest(root: &Path, patterns: &[&str]) -> Result<String> {
    use crate::utils::overlay_preprocess::path_content_digest;
    let is_glob = |p: &str| p.contains(['*', '?', '[']);
    let mut lines: Vec<String> = Vec::new();
    let mut globs: Vec<(&str, globset::GlobMatcher)> = Vec::new();

    // Literals need no walk at all, so they are resolved directly and the tree
    // is walked ONCE for every glob together. One walk per pattern made this
    // O(patterns x files), which is real cost on a large source tree during
    // stamp hashing — paid on every build, including the ones where nothing
    // changed and the stamp is about to say so.
    for pat in patterns {
        if !is_glob(pat) {
            match path_content_digest(root, pat)? {
                Some(d) => lines.push(format!("{pat}\t{d}")),
                None => anyhow::bail!(
                    "package_files names `{pat}` but it does not exist under {}",
                    root.display()
                ),
            }
            continue;
        }
        globs.push((
            pat,
            globset::GlobBuilder::new(pat)
                .literal_separator(true)
                .build()
                .with_context(|| format!("Invalid package_files pattern `{pat}`"))?
                .compile_matcher(),
        ));
    }

    if !globs.is_empty() {
        let mut matched = vec![false; globs.len()];
        for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
            let entry = entry.with_context(|| format!("Failed to walk {}", root.display()))?;
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let Some(rel) = rel.to_str() else { continue };
            if rel.is_empty() {
                continue;
            }
            // Digested once per entry, but still emitted once per matching
            // pattern: two patterns covering the same file contribute two
            // identical lines, exactly as the per-pattern walk did, so this
            // change is a speed-up and not a digest change.
            let mut digest = None;
            for (i, (_, glob)) in globs.iter().enumerate() {
                if !glob.is_match(rel) {
                    continue;
                }
                matched[i] = true;
                if digest.is_none() {
                    digest = Some(path_content_digest(root, rel)?);
                }
                if let Some(Some(d)) = &digest {
                    lines.push(format!("{rel}\t{d}"));
                }
            }
        }
        // A pattern matching nothing still contributes, so removing the last
        // match moves the digest rather than reading as "nothing declared".
        for (i, (pat, _)) in globs.iter().enumerate() {
            if !matched[i] {
                lines.push(format!("{pat}\t(no match)"));
            }
        }
    }

    lines.sort();
    Ok(compute_hash(&lines.join("\n")))
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
            fold_file_content(
                &mut hash_data,
                &format!("{section}.post_install"),
                project_root,
                post_install,
            )?;
        }
        // Baked into /etc/{passwd,shadow,group} in the image; adding a user or
        // changing a password hash must invalidate.
        if let Some(perms) = sysroot.get("permissions") {
            hash_data.insert(
                serde_yaml::Value::String(format!("{section}.permissions")),
                perms.clone(),
            );
        }
    }
    if let Some(perms) = config.get("permissions") {
        hash_data.insert(
            serde_yaml::Value::String("permissions".to_string()),
            perms.clone(),
        );
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
    // The encrypt marker and the package union both depend on the declared
    // scope, so a `targets:` edit has to invalidate the stamp: narrowing a
    // scope otherwise leaves a stale initramfs carrying its encrypt marker,
    // and widening one leaves the marker missing until something else forces
    // a rebuild.
    if let Some(targets) = merged_runtime.get("targets") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.targets")),
            targets.clone(),
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
    upstream_content_hashes: &std::collections::BTreeMap<String, String>,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // What the steps this build consumes actually produced — each required
    // extension's image and build digests, keyed `<name>.image` / `<name>.build`.
    // An extension whose rebuild left both byte-identical does not move this.
    for (key, digest) in upstream_content_hashes {
        hash_data.insert(
            serde_yaml::Value::String(format!("ext.{key}.content_hash")),
            serde_yaml::Value::String(digest.clone()),
        );
    }

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
    // The encrypt marker and the package union both depend on the declared
    // scope, so a `targets:` edit has to invalidate the stamp: narrowing a
    // scope otherwise leaves a stale initramfs carrying its encrypt marker,
    // and widening one leaves the marker missing until something else forces
    // a rebuild.
    if let Some(targets) = merged_runtime.get("targets") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.targets")),
            targets.clone(),
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
                    // The .dtso lives in the extension's own source tree, so it
                    // resolves against that extension's root — a git-fetched
                    // extension's is in the volume and is left to Layer 2.
                    let ext_node = parsed.get("extensions").and_then(|e| e.get(ext_name));
                    let root = ext_node.and_then(|e| ext_content_root(e, project_root));
                    if let (Some(seq), Some(root)) = (dtos.as_sequence(), root.as_deref()) {
                        for entry in seq {
                            if let Some(src) = entry.get("src").and_then(|v| v.as_str()) {
                                fold_file_content(
                                    &mut hash_data,
                                    &format!("ext.{ext_name}.dtso_content.{src}"),
                                    root,
                                    src,
                                )?;
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
        fold_file_content(
            &mut hash_data,
            &format!("runtime.{runtime_name}.post_build"),
            project_root,
            post_build,
        )?;
    }
    // `runtimes.<r>.packages.<pkg>.{compile,install}` run scripts the same way
    // an extension's do; the runtime's files are always under the project root.
    if let Some(pkgs) = merged_runtime.get("packages") {
        fold_package_scripts(
            &mut hash_data,
            &format!("runtime.{runtime_name}"),
            pkgs,
            parsed,
            project_root,
            Some(project_root),
        )?;
    }
    // `kernel.install` is a script; `narrow_kernel_for_hash` above keeps only
    // its path. Fold the content too.
    if let Some(install) = merged_runtime
        .get("kernel")
        .and_then(|k| k.get("install"))
        .and_then(|v| v.as_str())
    {
        fold_file_content(
            &mut hash_data,
            &format!("runtime.{runtime_name}.kernel.install_script"),
            project_root,
            install,
        )?;
    }
    // Runtime `var_files[].source` are host files copied into the var image.
    // (Extension `var_files` are globs over the built sysroot, not host paths,
    // so their node — folded by the ext image hash — is the whole input.)
    if let Some(seq) = merged_runtime
        .get("var_files")
        .and_then(|v| v.as_sequence())
    {
        for entry in seq {
            if let Some(src) = entry.get("source").and_then(|v| v.as_str()) {
                fold_file_content(
                    &mut hash_data,
                    &format!("runtime.{runtime_name}.var_files_content.{src}"),
                    project_root,
                    src,
                )?;
            }
        }
    }
    // The boot FIT is assembled and signed in this step, from these keys.
    if let Some(signing) = merged_runtime.get("signing") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.signing")),
            signing.clone(),
        );
    }

    // rootfs / initramfs: filesystem picks the mkfs; `image` carries the kab
    // args and the dm-verity opt-in, both of which change what is built here.
    // `permissions` is baked into the image's /etc.
    for section in ["rootfs", "initramfs"] {
        if let Some(node) = parsed.get(section) {
            for k in ["filesystem", "image", "permissions"] {
                if let Some(v) = node.get(k) {
                    hash_data.insert(
                        serde_yaml::Value::String(format!("{section}.{k}")),
                        v.clone(),
                    );
                }
            }
        }
    }
    if let Some(perms) = parsed.get("permissions") {
        hash_data.insert(
            serde_yaml::Value::String("permissions".to_string()),
            perms.clone(),
        );
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

/// The `outputs.content_hash` recorded on one stamp in a batch-read result, if
/// that stamp exists and carries one. Downstream steps fold this into their
/// own input hash — the chain link between what one step produced and what
/// the next consumed.
pub fn content_hash_from_batch(batch_output: &str, req: &StampRequirement) -> Option<String> {
    parse_batch_stamps_output(batch_output)
        .get(&req.relative_path())
        .and_then(|json| json.as_deref())
        .and_then(|json| Stamp::from_json(json).ok())
        .and_then(|stamp| stamp.outputs.content_hash)
}

/// The output digests of every extension in `ext_names` that `runtime build`
/// consumes, read from one batch stamp read, keyed `<name>.image` and
/// `<name>.build`. Both matter: the image is what ships, but `var_files` are
/// copied out of the built sysroot into the var partition and never enter the
/// image, so the image digest alone is blind to them. Extensions whose stamp is
/// absent or carries no digest are simply not present — their absence is itself
/// part of the input.
pub fn ext_content_hashes_from_batch(
    batch_output: &str,
    ext_names: impl IntoIterator<Item = String>,
) -> std::collections::BTreeMap<String, String> {
    let parsed = parse_batch_stamps_output(batch_output);
    let digest_of = |req: StampRequirement| {
        parsed
            .get(&req.relative_path())
            .and_then(|json| json.as_deref())
            .and_then(|json| Stamp::from_json(json).ok())
            .and_then(|stamp| stamp.outputs.content_hash)
    };
    let mut out = std::collections::BTreeMap::new();
    for name in ext_names {
        if let Some(h) = digest_of(StampRequirement::ext_image(&name)) {
            out.insert(format!("{name}.image"), h);
        }
        if let Some(h) = digest_of(StampRequirement::ext_build(&name)) {
            out.insert(format!("{name}.build"), h);
        }
    }
    out
}

/// Placeholder the digest-bearing stamp writer substitutes in the container.
pub const CONTENT_HASH_PLACEHOLDER: &str = "__AVOCADO_CONTENT_HASH__";

/// Like [`generate_write_stamp_script`], but the stamp's `outputs.content_hash`
/// is computed in the container, where the produced bytes live. `digest_script`
/// is shell that sets `AVOCADO_CONTENT_HASH` to a hex digest; it runs first,
/// and the write is refused if it left the variable empty or non-hex — a stamp
/// with no digest would let every downstream step read "nothing changed".
///
/// The JSON stays inside a quoted heredoc and only the placeholder is
/// substituted, with `sed` on a value that has just been checked to be hex. An
/// unquoted heredoc would have expanded `$` and backticks in every path the
/// config happened to name.
///
/// The substitution is anchored to the whole `"content_hash": "<placeholder>"`
/// field and refused if that field is not there. The placeholder alone appeared
/// in the pattern before, and the stamp carries user-controlled strings —
/// `inputs.config_hash`, `outputs.exports`, a path from the config — so a value
/// that happened to contain it would have been rewritten too, corrupting the
/// stamp rather than failing.
pub fn generate_write_stamp_script_with_digest(
    stamp: &Stamp,
    digest_script: &str,
) -> Result<String> {
    let mut with_placeholder = stamp.clone();
    with_placeholder.outputs.content_hash = Some(CONTENT_HASH_PLACEHOLDER.to_string());
    let stamp_json = with_placeholder.to_json()?;
    let stamp_path = stamp.relative_path();

    Ok(format!(
        r#"
# Digest what this step produced, then write the stamp carrying it.
{digest_script}
case "$AVOCADO_CONTENT_HASH" in
    *[!0-9a-f]*|"") echo "ERROR: content digest for {stamp_path} is empty or not hex: '$AVOCADO_CONTENT_HASH'" >&2; exit 1 ;;
esac
_avocado_stamp="$AVOCADO_PREFIX/.stamps/{stamp_path}"
mkdir -p "$(dirname "$_avocado_stamp")" || exit 1
cat > "$_avocado_stamp" << 'STAMP_EOF' || exit 1
{stamp_json}
STAMP_EOF
sed -i "s/\"{placeholder}\"/\"sha256:$AVOCADO_CONTENT_HASH\"/" "$AVOCADO_PREFIX/.stamps/{stamp_path}"
"#,
        placeholder = CONTENT_HASH_PLACEHOLDER,
    ))
}

/// Paths a sysroot digest ignores, and an extension image must exclude: the
/// package-manager state. rpmdb bytes embed install timestamps and dnf caches
/// churn on every transaction; a digest that followed them would move on every
/// install that changed nothing, and an image that shipped them would carry
/// bytes the digest never saw. The two lists are the same list on purpose —
/// [`BUILD_STATE_PATHS`] plus rpm's newer default dbpath — so the digest and
/// the image agree about what is in the image.
///
/// [`BUILD_STATE_PATHS`]: crate::commands::rootfs::image::BUILD_STATE_PATHS
pub fn package_state_paths() -> Vec<&'static str> {
    crate::commands::rootfs::image::BUILD_STATE_PATHS
        .iter()
        .copied()
        .chain(std::iter::once("usr/lib/sysimage/rpm"))
        .collect()
}

/// Shell that sets `AVOCADO_CONTENT_HASH` to a digest of a sysroot directory:
/// the sorted NEVRA set from its rpmdb, plus a tree hash of everything the
/// image would carry. Same recipe as the rootfs/initramfs build id
/// ([`crate::commands::rootfs::image::render_build_id_block`]) — path, type,
/// mode, ownership, symlink target, file content — with [`package_state_paths`]
/// pruned. Ownership is hashed: for a format that flattens it (erofs) that
/// over-invalidates one step, and the image's own digest stops the cascade
/// there.
///
/// Fails closed. A missing sysroot, an unreadable file, or any failed pipeline
/// stage exits non-zero instead of yielding a digest of nothing — a stamp
/// carrying an accepted hash over a broken tree would read as "current" to
/// every downstream step. And it never writes: the rpm query runs only when
/// the database directory already exists, because `rpm -qa` on a root without
/// one creates it, inside the tree being measured.
///
/// `sysroot` is a shell expression for the directory; `rpm_dbpath` the rpmdb's
/// location inside it (`None` for rpm's default, `/var/lib/rpm`).
pub fn render_sysroot_digest_script(sysroot: &str, rpm_dbpath: Option<&str>) -> String {
    let dbpath = rpm_dbpath.unwrap_or("/var/lib/rpm");
    let mut prune: Vec<String> = package_state_paths()
        .into_iter()
        .map(|p| format!("-path ./{p}"))
        .collect();
    let db_rel = dbpath.trim_start_matches('/');
    if !package_state_paths().contains(&db_rel) {
        prune.push(format!("-path ./{db_rel}"));
    }
    let prune = prune.join(" -o ");
    format!(
        r#"_avocado_sysroot="{sysroot}"
[ -d "$_avocado_sysroot" ] || {{ echo "ERROR: sysroot to digest does not exist: $_avocado_sysroot" >&2; exit 1; }}
_avocado_pkgs=""
if [ -d "$_avocado_sysroot{dbpath}" ]; then
    _avocado_pkgs=$(set -o pipefail; rpm --dbpath {dbpath} -qa --queryformat '%{{NEVRA}}\n' --root "$_avocado_sysroot" | LC_ALL=C sort | sha256sum | awk '{{print $1}}') || exit 1
fi
_avocado_meta=$(set -o pipefail; cd "$_avocado_sysroot" && find . \( {prune} \) -prune -o -printf '%y %m %U %G %P\t%l\n' | LC_ALL=C sort) || exit 1
_avocado_content=$(set -o pipefail; cd "$_avocado_sysroot" && find . \( {prune} \) -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum) || exit 1
AVOCADO_CONTENT_HASH=$(printf '%s\n%s\n%s\n' "$_avocado_pkgs" "$_avocado_meta" "$_avocado_content" | sha256sum | awk '{{print $1}}')"#
    )
}

/// Shell that sets `AVOCADO_CONTENT_HASH` to the sha256 of one file.
pub fn render_file_digest_script(path: &str) -> String {
    format!(r#"AVOCADO_CONTENT_HASH=$(sha256sum "{path}" | awk '{{print $1}}')"#)
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

/// One shell line that removes a step's own stamp. Prepended to the step's
/// build script under `--no-stamps`: a step that redoes its work without
/// recording it must not leave last time's stamp — and last time's output
/// digest — for a downstream step to trust. With the stamp gone, downstream
/// either runs with `--no-stamps` too or fails its precondition loudly;
/// neither silently skips over stale bytes.
pub fn remove_own_stamp_line(req: &StampRequirement) -> String {
    format!(
        "rm -f \"$AVOCADO_PREFIX/.stamps/{}\"\n",
        req.relative_path()
    )
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
                            // Name the real cause. After a CLI upgrade every
                            // stamp fails the version check with the config
                            // untouched; telling the user their config changed
                            // sends them to the wrong place.
                            let reason = if stamp.version != STAMP_VERSION {
                                format!(
                                    "stamp format changed (v{} → v{STAMP_VERSION}); re-run the step to refresh it",
                                    stamp.version
                                )
                            } else {
                                "config hash mismatch".to_string()
                            };
                            StampStatus::Stale { stamp, reason }
                        }
                    } else {
                        // No inputs to check, assume current
                        StampStatus::Current(stamp)
                    }
                }
                Err(e) => StampStatus::Unreadable {
                    reason: format!("{e:#}"),
                },
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
        StampStatus::Unreadable { reason } => {
            result.add_unreadable(req.clone(), reason);
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
            ..Default::default()
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
    // Unreadable stamps and the searched root
    // ========================================================================

    #[test]
    fn test_validate_stamp_unreadable_is_not_missing() {
        let req = StampRequirement::sdk_install();

        let status = validate_stamp(&req, Some("{not json"), None);
        assert!(
            matches!(status, StampStatus::Unreadable { .. }),
            "malformed JSON must not be reported as missing: {status:?}"
        );

        // Valid JSON that is not a stamp is unreadable too, not missing.
        let status = validate_stamp(&req, Some(r#"{"command":"install"}"#), None);
        assert!(
            matches!(status, StampStatus::Unreadable { .. }),
            "{status:?}"
        );

        if let StampStatus::Unreadable { reason } = validate_stamp(&req, Some("{not json"), None) {
            assert!(reason.contains("parse"), "{reason}");
        }
    }

    #[test]
    fn test_unreadable_stamp_makes_validation_unsatisfied() {
        let req = StampRequirement::runtime_build("dev");
        let mut result = StampValidationResult::new();
        check_stamp_requirement(&req, Some("{not json"), None, &mut result);

        assert!(!result.is_satisfied());
        assert_eq!(result.unreadable.len(), 1);
        assert!(result.missing.is_empty());
        assert!(result.stale.is_empty());
        assert_eq!(
            result.unreadable[0].0.relative_path(),
            "runtime/dev/build.stamp"
        );
    }

    #[test]
    fn test_unreadable_stamp_has_its_own_section_and_a_fix_command() {
        let mut result = StampValidationResult::new();
        check_stamp_requirement(
            &StampRequirement::runtime_build("dev"),
            Some("{not json"),
            None,
            &mut result,
        );
        let msg = result.into_error("Cannot deploy runtime 'dev'").to_string();

        assert!(msg.contains("Unreadable steps"), "{msg}");
        assert!(msg.contains("runtime/dev/build.stamp"), "{msg}");
        assert!(!msg.contains("Missing steps"), "{msg}");
        assert!(msg.contains("avocado runtime build dev"), "{msg}");
    }

    fn search_root(volume: Option<&str>) -> StampSearchRoot {
        StampSearchRoot {
            target: "qemuarm64".to_string(),
            volume: volume.map(|v| v.to_string()),
            daemon: StampDaemon::AvocadoVm,
        }
    }

    #[test]
    fn test_error_display_names_the_searched_root() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::runtime_build("dev"));
        let error = result
            .into_error("Cannot deploy runtime 'dev'")
            .with_search_root(search_root(Some("avo-0123-test")));
        let msg = error.to_string();

        assert!(msg.contains("/opt/_avocado/qemuarm64/.stamps"), "{msg}");
        assert!(msg.contains("target qemuarm64"), "{msg}");
        assert!(msg.contains("docker volume avo-0123-test"), "{msg}");
        assert!(msg.contains("avocado-vm docker daemon"), "{msg}");
        assert!(!msg.contains("--runs-on"), "{msg}");
    }

    #[test]
    fn test_error_display_without_search_root_is_unchanged() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::runtime_build("dev"));
        let msg = result.into_error("Cannot deploy runtime 'dev'").to_string();
        assert!(!msg.contains("Looked in"), "{msg}");
    }

    #[test]
    fn test_json_error_event_message_carries_the_searched_root() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::runtime_build("dev"));
        let event = result
            .into_error("Cannot deploy runtime 'dev'")
            .with_search_root(search_root(Some("avo-0123-test")))
            .json_error_event();

        // The desktop renders only `message`; sibling fields would be dropped.
        assert_eq!(event["event"], "error");
        assert!(event.get("target").is_none());
        let msg = event["message"].as_str().expect("message is a string");
        assert!(msg.contains("/opt/_avocado/qemuarm64/.stamps"), "{msg}");
        assert!(msg.contains("avo-0123-test"), "{msg}");
    }

    #[test]
    fn test_search_root_without_volume_omits_the_volume_clause() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::runtime_build("dev"));
        let msg = result
            .into_error("Cannot deploy runtime 'dev'")
            .with_search_root(search_root(None))
            .to_string();

        assert!(msg.contains("/opt/_avocado/qemuarm64/.stamps"), "{msg}");
        assert!(msg.contains("target qemuarm64"), "{msg}");
        assert!(!msg.contains("docker volume"), "{msg}");
    }

    #[test]
    fn test_search_root_names_the_runs_on_host() {
        let mut result = StampValidationResult::new();
        result.add_missing(StampRequirement::runtime_build("dev"));
        let msg = result
            .into_error_with_runs_on("Cannot provision runtime 'dev'", Some("user@remote"))
            .with_search_root(StampSearchRoot {
                daemon: StampDaemon::Host,
                ..search_root(Some("avo-0123-test"))
            })
            .to_string();

        assert!(msg.contains("host docker daemon"), "{msg}");
        assert!(
            msg.contains("exported over NFS to user@remote (--runs-on)"),
            "{msg}"
        );
    }

    #[test]
    fn test_daemon_display_names_a_custom_docker_host() {
        let daemon = StampDaemon::DockerHost("tcp://10.0.0.5:2375".to_string());
        assert_eq!(
            daemon.to_string(),
            "the docker daemon at DOCKER_HOST=tcp://10.0.0.5:2375"
        );
    }

    #[test]
    fn test_search_root_for_container_reads_avocado_state_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let container = SdkContainer {
            cwd: dir.path().to_path_buf(),
            ..SdkContainer::new()
        };

        // No .avocado-state: no volume, and none gets written.
        let root = StampSearchRoot::for_container(&container, "qemuarm64");
        assert_eq!(root.target, "qemuarm64");
        assert_eq!(root.volume, None);
        assert_eq!(root.stamps_dir(), "/opt/_avocado/qemuarm64/.stamps");
        assert!(!dir.path().join(".avocado-state").exists());

        // With one, the recorded name is reported verbatim.
        VolumeState::new(dir.path().to_path_buf(), "docker".to_string())
            .save_to_dir(dir.path())
            .unwrap();
        let expected = VolumeState::load_from_dir(dir.path())
            .unwrap()
            .unwrap()
            .volume_name;
        let root = StampSearchRoot::for_container(&container, "qemuarm64");
        assert_eq!(root.volume, Some(expected));

        // A corrupt state file degrades to "no volume" rather than failing.
        std::fs::write(dir.path().join(".avocado-state"), "{not json").unwrap();
        let root = StampSearchRoot::for_container(&container, "qemuarm64");
        assert_eq!(root.volume, None);
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
            &Default::default(),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &with_kernel,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
            &Default::default(),
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

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("kernel-install.sh");
        std::fs::write(&script, "cp Image $DEST\n").unwrap();
        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_package = compute_runtime_build_input_hash(
            &kernel_package,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        let hash_compile = compute_runtime_build_input_hash(
            &kernel_compile,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();

        // Switching kernel mode should produce a different hash
        assert_ne!(hash_package.config_hash, hash_compile.config_hash);

        // The install script's content is an input, not just its path.
        std::fs::write(&script, "cp Image $DEST && depmod\n").unwrap();
        let hash_edited = compute_runtime_build_input_hash(
            &kernel_compile,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        assert_ne!(hash_compile.config_hash, hash_edited.config_hash);

        // A declared script that does not exist is an error, not a sentinel.
        std::fs::remove_file(&script).unwrap();
        assert!(compute_runtime_build_input_hash(
            &kernel_compile,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default()
        )
        .is_err());
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
            &Default::default(),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_with,
            std::path::Path::new("."),
            &Default::default(),
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

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("overlays")).unwrap();
        let dtso = dir.path().join("overlays/spi-fast.dtso");
        std::fs::write(&dtso, "/dts-v1/; /plugin/;\n").unwrap();

        let hash_without = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_without,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_with,
            dir.path(),
            &Default::default(),
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "declaring a device-tree overlay must change the runtime input hash"
        );

        std::fs::write(
            &dtso,
            "/dts-v1/; /plugin/; &spi0 {{ status = \"okay\"; }};\n",
        )
        .unwrap();
        let hash_edited = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed_with,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        assert_ne!(
            hash_with.config_hash, hash_edited.config_hash,
            "editing the .dtso source must change the runtime input hash"
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
        let hash_v1 = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed,
            tmp.path(),
            &Default::default(),
        )
        .unwrap();

        std::fs::write(&dtso, "/dts-v1/;\n/plugin/;\n/ { /* v2 edited */ };\n").unwrap();
        let hash_v2 = compute_runtime_build_input_hash(
            &runtime,
            "dev",
            &parsed,
            tmp.path(),
            &Default::default(),
        )
        .unwrap();

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

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("files/data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("config.toml"), "mode = 1\n").unwrap();

        let empty_parsed = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let hash_without = compute_runtime_build_input_hash(
            &runtime_without,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime_with,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();

        assert_ne!(
            hash_without.config_hash, hash_with.config_hash,
            "Adding var_files should change the runtime input hash"
        );

        // The source directory's content is an input: var_files are copied
        // into the var image, so an edit there must reach the device.
        std::fs::write(data.join("config.toml"), "mode = 2\n").unwrap();
        let hash_edited = compute_runtime_build_input_hash(
            &runtime_with,
            "dev",
            &empty_parsed,
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        assert_ne!(
            hash_with.config_hash, hash_edited.config_hash,
            "editing a var_files source must change the runtime input hash"
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
            &Default::default(),
        )
        .unwrap();
        let hash_with = compute_runtime_build_input_hash(
            &runtime_with,
            "dev",
            &empty_parsed,
            std::path::Path::new("."),
            &Default::default(),
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

    /// `compute_ext_build_input_hash` against a real project root.
    fn ext_build_hash_at(root: &Path, yaml: &str) -> Result<String> {
        let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        Ok(compute_ext_build_input_hash(&v, "my-ext", root, None, None, None)?.config_hash)
    }

    /// A compiled extension: the compile script (via `sdk.compile.<section>`),
    /// the install script, and the `package_files` source tree are all inputs
    /// whose *content* moves the build hash. This is the case that motivated
    /// the coverage work — editing source under `package_files` used to move
    /// nothing.
    #[test]
    fn ext_build_hash_tracks_compile_install_and_source_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("compile.sh"), "cargo build\n").unwrap();
        std::fs::write(root.join("install.sh"), "install -D target/app $DEST\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let yaml = r#"
sdk:
  compile:
    app-compile:
      compile: compile.sh
extensions:
  my-ext:
    package_files: [Cargo.toml, src]
    packages:
      app:
        compile: app-compile
        install: install.sh
"#;
        let base = ext_build_hash_at(root, yaml).unwrap();

        std::fs::write(root.join("compile.sh"), "cargo build --release\n").unwrap();
        let compile_edit = ext_build_hash_at(root, yaml).unwrap();
        assert_ne!(base, compile_edit, "compile script content");

        std::fs::write(root.join("install.sh"), "install -Dm755 target/app $DEST\n").unwrap();
        let install_edit = ext_build_hash_at(root, yaml).unwrap();
        assert_ne!(compile_edit, install_edit, "install script content");

        std::fs::write(root.join("src/main.rs"), "fn main() { run() }\n").unwrap();
        let source_edit = ext_build_hash_at(root, yaml).unwrap();
        assert_ne!(install_edit, source_edit, "source under package_files");

        // Adding a file the pattern covers moves it; a stable tree does not.
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let added = ext_build_hash_at(root, yaml).unwrap();
        assert_ne!(source_edit, added, "new file under package_files");
        assert_eq!(
            added,
            ext_build_hash_at(root, yaml).unwrap(),
            "stable across calls"
        );

        // Without a compile step, package_files only feeds packaging, which has
        // no stamp — so it is not folded and its content does not move the hash.
        let uncompiled = r#"
extensions:
  my-ext:
    package_files: [Cargo.toml, src]
    packages:
      bash: "*"
"#;
        let a = ext_build_hash_at(root, uncompiled).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn x() {}").unwrap();
        assert_eq!(a, ext_build_hash_at(root, uncompiled).unwrap());
    }

    /// `package_files` patterns expand like the packaging script's
    /// `shopt -s globstar`: `*` stays inside a path component, `**` crosses.
    #[test]
    fn package_files_digest_expands_globs_like_globstar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("top.sh"), "1").unwrap();
        std::fs::write(root.join("a/mid.sh"), "2").unwrap();
        std::fs::write(root.join("a/b/deep.sh"), "3").unwrap();

        let star = package_files_digest(root, &["*.sh"]).unwrap();
        let globstar = package_files_digest(root, &["**/*.sh"]).unwrap();
        assert_ne!(star, globstar, "`*` must not cross `/`; `**` must");

        // Only deep.sh is under a/b, so editing top.sh cannot move a digest of
        // patterns that exclude it — proving `*.sh` matched top.sh alone.
        std::fs::write(root.join("a/b/deep.sh"), "3!").unwrap();
        assert_eq!(star, package_files_digest(root, &["*.sh"]).unwrap());
        assert_ne!(globstar, package_files_digest(root, &["**/*.sh"]).unwrap());

        // A pattern matching nothing still contributes, so removing the last
        // match moves the digest rather than reading as "nothing declared".
        let none = package_files_digest(root, &["*.none"]).unwrap();
        assert_ne!(none, package_files_digest(root, &[]).unwrap());

        // A literal path that does not exist is an error, like a script.
        assert!(package_files_digest(root, &["missing.txt"]).is_err());
    }

    /// One walk for every glob instead of one walk per glob — same digest.
    ///
    /// The speed-up is only safe if the output is unchanged, and the subtle part
    /// is overlapping patterns: a file matched by two of them contributed two
    /// identical lines under the per-pattern walk, so the digest is not the same
    /// as a deduplicated one. These values were computed with the per-pattern
    /// implementation; if a refactor dedupes, they move and this fails.
    #[test]
    fn package_files_digest_is_unchanged_by_the_single_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("one.sh"), "1").unwrap();
        std::fs::write(root.join("a/two.sh"), "2").unwrap();

        // Overlapping patterns: `**/*.sh` and `*.sh` both cover one.sh.
        let overlapping = package_files_digest(root, &["**/*.sh", "*.sh"]).unwrap();
        let single = package_files_digest(root, &["**/*.sh"]).unwrap();
        assert_ne!(
            overlapping, single,
            "a doubly-matched file must still contribute twice"
        );

        // And order of declaration does not matter, because the lines are sorted.
        assert_eq!(
            overlapping,
            package_files_digest(root, &["*.sh", "**/*.sh"]).unwrap()
        );

        // A literal alongside globs is resolved without a walk and still folded.
        let with_literal = package_files_digest(root, &["**/*.sh", "one.sh"]).unwrap();
        assert_ne!(with_literal, single);
    }

    /// The digest is substituted into `outputs.content_hash` and nowhere else.
    ///
    /// The stamp carries user-controlled strings — an export value, a path from
    /// the config — and the substitution used to match the bare placeholder, so
    /// one of those containing it was rewritten too. That corrupts the stamp
    /// silently, which is the worst outcome for a record other steps trust. The
    /// script now anchors on the whole field and refuses to run if it is absent.
    #[test]
    fn the_digest_substitution_touches_only_the_content_hash_field() {
        let outputs = StampOutputs {
            exports: Some(
                [(
                    "SOME_EXPORT".to_string(),
                    super::CONTENT_HASH_PLACEHOLDER.to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };
        let st = Stamp::new(
            StampCommand::Build,
            StampComponent::Extension,
            Some("my-ext".to_string()),
            "qemux86-64".to_string(),
            StampInputs::new(super::CONTENT_HASH_PLACEHOLDER.to_string()),
            outputs,
        );
        let script =
            super::generate_write_stamp_script_with_digest(&st, "AVOCADO_CONTENT_HASH=deadbeef")
                .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("AVOCADO_PREFIX", dir.path())
            .output()
            .expect("sh should run");
        assert!(
            out.status.success(),
            "script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let written =
            std::fs::read_to_string(dir.path().join(".stamps").join(st.relative_path())).unwrap();
        let parsed = Stamp::from_json(&written).expect("stamp must still be valid JSON");
        assert_eq!(
            parsed.outputs.content_hash.as_deref(),
            Some("sha256:deadbeef"),
            "the digest must land in content_hash"
        );
        assert_eq!(
            parsed.inputs.config_hash,
            super::CONTENT_HASH_PLACEHOLDER,
            "a user-controlled input must not be rewritten"
        );
        assert_eq!(
            parsed.outputs.exports.as_ref().unwrap()["SOME_EXPORT"],
            super::CONTENT_HASH_PLACEHOLDER,
            "a user-controlled export must not be rewritten"
        );
    }

    /// `version: {file, key}` reads the version out of a file at build time;
    /// that file's content is an input.
    #[test]
    fn ext_build_hash_tracks_version_file_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("VERSION"), "1.0.0\n").unwrap();
        let yaml = r#"
extensions:
  my-ext:
    version:
      file: VERSION
    packages:
      bash: "*"
"#;
        let a = ext_build_hash_at(dir.path(), yaml).unwrap();
        std::fs::write(dir.path().join("VERSION"), "1.0.1\n").unwrap();
        assert_ne!(a, ext_build_hash_at(dir.path(), yaml).unwrap());
    }

    /// Where an extension's files live decides what can be hashed. A local
    /// extension and a `source: {type: path}` one resolve on the host and are
    /// hashed at the right root; a git-fetched one lives only in the SDK
    /// volume, so no content key is emitted and — crucially — its declared
    /// scripts are not reported as missing. Its `source` (url + ref) is still
    /// folded; the bytes behind it are Layer 2's to digest.
    #[test]
    fn ext_content_root_follows_the_source_kind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("vendored/ext")).unwrap();
        std::fs::write(root.join("vendored/ext/post.sh"), "echo a\n").unwrap();

        // path source: the script is under the path root, not the project root.
        let path_src = r#"
extensions:
  my-ext:
    source:
      type: path
      path: vendored/ext
    post_build: post.sh
"#;
        let a = ext_build_hash_at(root, path_src).unwrap();
        std::fs::write(root.join("vendored/ext/post.sh"), "echo b\n").unwrap();
        assert_ne!(a, ext_build_hash_at(root, path_src).unwrap());

        // git source: the same declaration hashes fine with no file on the host,
        // and does not change when an unrelated host file appears.
        let git_src = r#"
extensions:
  my-ext:
    source:
      type: git
      url: https://example.invalid/ext.git
      ref: v1
    post_build: post.sh
"#;
        let g = ext_build_hash_at(root, git_src).unwrap();
        std::fs::write(root.join("post.sh"), "echo host\n").unwrap();
        assert_eq!(g, ext_build_hash_at(root, git_src).unwrap());
        assert_ne!(
            g,
            ext_build_hash_at(root, &git_src.replace("ref: v1", "ref: v2")).unwrap(),
            "the ref still moves it"
        );

        // local: a declared script that is absent is an error.
        std::fs::remove_file(root.join("post.sh")).unwrap();
        let local = r#"
extensions:
  my-ext:
    post_build: post.sh
"#;
        assert!(ext_build_hash_at(root, local).is_err());
    }

    /// Inputs the runtime build bakes into images that the hash used to miss:
    /// `permissions` (passwd/group), `image` (kab args and the dm-verity
    /// opt-in), and `signing` (the FIT key). Flipping `verity: true` in
    /// particular must invalidate — a skip there ships an unverified rootfs the
    /// config asked to verify.
    #[test]
    fn runtime_build_hash_tracks_permissions_image_and_signing() {
        let runtime: serde_yaml::Value = serde_yaml::from_str("packages:\n  a: '*'\n").unwrap();
        let hash = |parsed: &str, rt: &serde_yaml::Value| {
            let p: serde_yaml::Value = serde_yaml::from_str(parsed).unwrap();
            compute_runtime_build_input_hash(rt, "dev", &p, Path::new("."), &Default::default())
                .unwrap()
                .config_hash
        };
        let base = hash("rootfs:\n  filesystem: erofs\n", &runtime);
        assert_ne!(
            base,
            hash(
                "rootfs:\n  filesystem: erofs\n  image:\n    verity: true\n",
                &runtime
            ),
            "verity flip"
        );
        assert_ne!(
            base,
            hash(
                "rootfs:\n  filesystem: erofs\n  permissions:\n    users: [{name: app}]\n",
                &runtime
            ),
            "rootfs permissions"
        );
        assert_ne!(
            base,
            hash(
                "rootfs:\n  filesystem: erofs\npermissions:\n  p:\n    users: [{name: app}]\n",
                &runtime
            ),
            "top-level permissions"
        );
        let signed: serde_yaml::Value =
            serde_yaml::from_str("packages:\n  a: '*'\nsigning:\n  fit_key: boot\n").unwrap();
        assert_ne!(
            base,
            hash("rootfs:\n  filesystem: erofs\n", &signed),
            "signing"
        );
    }

    /// The SDK's environment: extra container args and `src_dir` are inputs.
    #[test]
    fn sdk_hash_tracks_container_args_and_src_dir() {
        let h = |y: &str| {
            let v: serde_yaml::Value = serde_yaml::from_str(y).unwrap();
            compute_sdk_input_hash(&v).unwrap().config_hash
        };
        let base = h("sdk:\n  image: img\n");
        assert_ne!(
            base,
            h("sdk:\n  image: img\n  container_args: ['-v', '/dev:/dev']\n")
        );
        assert_ne!(base, h("sdk:\n  image: img\nsrc_dir: ../proj\n"));
    }

    /// Run a generated stamp-writing script the way the container would:
    /// `$AVOCADO_PREFIX` pointed at a temp dir, bash executing it. Returns the
    /// exit status and the stamp read back, if one was written.
    #[cfg(unix)]
    fn run_stamp_script(prefix: &Path, script: &str) -> (bool, Option<Stamp>) {
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .env("AVOCADO_PREFIX", prefix)
            .stderr(std::process::Stdio::null())
            .status()
            .expect("bash on PATH");
        let stamp = std::fs::read_to_string(
            prefix
                .join(".stamps")
                .join(StampRequirement::ext_build("e").relative_path()),
        )
        .ok()
        .map(|j| Stamp::from_json(&j).unwrap());
        (status.success(), stamp)
    }

    /// The digest computed in the container lands in `outputs.content_hash`,
    /// and only there: every other field is exactly what the host serialized —
    /// including a path that would have been mangled by an unquoted heredoc.
    /// A digest that is empty or not hex refuses to write anything.
    #[cfg(unix)]
    #[test]
    fn digest_stamp_writer_substitutes_only_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        // Hostile-looking, must survive unexpanded; and the placeholder text as
        // *data* must survive the substitution, which is anchored on the quoted
        // JSON value, not the bare string.
        let inputs = StampInputs::with_package_list(
            "c$HOME`whoami`".to_string(),
            format!("x{CONTENT_HASH_PLACEHOLDER}y"),
        );
        let stamp = Stamp::ext_build("e", "qemux86-64", inputs, StampOutputs::default());

        let ok_script =
            generate_write_stamp_script_with_digest(&stamp, "AVOCADO_CONTENT_HASH=deadbeef")
                .unwrap();
        let (ok, written) = run_stamp_script(dir.path(), &ok_script);
        assert!(ok);
        let written = written.expect("stamp written");
        assert_eq!(
            written.outputs.content_hash.as_deref(),
            Some("sha256:deadbeef")
        );
        assert_eq!(
            written.inputs.config_hash, "c$HOME`whoami`",
            "no shell expansion"
        );
        assert_eq!(
            written.inputs.package_list_hash.as_deref(),
            Some(format!("x{CONTENT_HASH_PLACEHOLDER}y").as_str()),
            "placeholder as data is untouched"
        );

        for bad in [
            "AVOCADO_CONTENT_HASH=",
            "AVOCADO_CONTENT_HASH='not hex!'",
            "true",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let script = generate_write_stamp_script_with_digest(&stamp, bad).unwrap();
            let (ok, written) = run_stamp_script(dir.path(), &script);
            assert!(!ok, "{bad}: refused");
            assert!(written.is_none(), "{bad}: nothing written");
        }
    }

    /// The sysroot digest is stable over an unchanged tree, moves on a content
    /// edit, and does NOT move on package-manager bookkeeping — the rpmdb and
    /// dnf caches churn on every install, and a digest that followed them would
    /// never let anything skip.
    #[cfg(unix)]
    #[test]
    fn sysroot_digest_tracks_content_and_ignores_package_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sysroot");
        for d in [
            "usr/bin",
            "etc",
            "var/lib/rpm",
            "var/lib/dnf",
            "var/cache/dnf",
            "var/lib/extension.d/rpm",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("usr/bin/app"), "binary v1").unwrap();
        std::fs::write(root.join("etc/app.conf"), "mode=1").unwrap();

        // A stub `rpm` first on PATH: the host's rpm (if any) has its own default
        // dbpath and would answer for the host, not this tree. The real query runs
        // inside the SDK; here the property under test is the tree hash.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("rpm"), "#!/bin/sh\nexit 0\n").unwrap();
        #[allow(clippy::permissions_set_readonly_false)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join("rpm"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let digest = |dbpath: Option<&str>| -> String {
            let script = format!(
                "{}\necho \"$AVOCADO_CONTENT_HASH\"",
                render_sysroot_digest_script(&root.display().to_string(), dbpath)
            );
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(&script)
                .env("PATH", &path_env)
                .stderr(std::process::Stdio::null())
                .output()
                .expect("bash on PATH");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };

        let d1 = digest(None);
        assert_eq!(d1.len(), 64, "hex sha256: {d1}");
        assert_eq!(d1, digest(None), "stable");

        // Package-manager state is pruned.
        std::fs::write(
            root.join("var/lib/rpm/Packages"),
            "rpmdb bytes with timestamps",
        )
        .unwrap();
        std::fs::write(root.join("var/lib/dnf/history"), "x").unwrap();
        std::fs::write(root.join("var/cache/dnf/repo"), "x").unwrap();
        assert_eq!(d1, digest(None), "rpmdb/dnf churn must not move the digest");
        std::fs::write(root.join("var/lib/extension.d/rpm/Packages"), "x").unwrap();
        assert_eq!(
            digest(Some("/var/lib/extension.d/rpm")),
            digest(Some("/var/lib/extension.d/rpm")),
            "stable with an explicit dbpath"
        );
        let with_ext_db = digest(Some("/var/lib/extension.d/rpm"));
        std::fs::write(root.join("var/lib/extension.d/rpm/Packages"), "y").unwrap();
        assert_eq!(
            with_ext_db,
            digest(Some("/var/lib/extension.d/rpm")),
            "explicit dbpath pruned"
        );

        // Real content is not.
        std::fs::write(root.join("usr/bin/app"), "binary v2").unwrap();
        let d2 = digest(None);
        assert_ne!(d1, d2, "file content");
        std::fs::write(root.join("etc/new.conf"), "").unwrap();
        assert_ne!(d2, digest(None), "new file");
    }

    /// The digest fails closed and never writes. A missing sysroot exits
    /// non-zero rather than digesting nothing into an accepted hash, and a tree
    /// with no rpmdb comes out of the run without one — `rpm -qa` would have
    /// created it inside the tree being measured.
    #[cfg(unix)]
    #[test]
    fn sysroot_digest_fails_closed_and_never_creates_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let run = |root: &Path| {
            let script = format!(
                "{}\necho \"$AVOCADO_CONTENT_HASH\"",
                render_sysroot_digest_script(&root.display().to_string(), None)
            );
            std::process::Command::new("bash")
                .arg("-c")
                .arg(&script)
                .stderr(std::process::Stdio::null())
                .output()
                .expect("bash on PATH")
        };
        let missing = run(&dir.path().join("nope"));
        assert!(
            !missing.status.success(),
            "missing sysroot must fail, not digest nothing"
        );
        assert!(String::from_utf8(missing.stdout).unwrap().trim().is_empty());

        let root = dir.path().join("sysroot");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/app"), "x").unwrap();
        let ok = run(&root);
        assert!(ok.status.success());
        assert_eq!(String::from_utf8(ok.stdout).unwrap().trim().len(), 64);
        assert!(
            !root.join("var/lib/rpm").exists(),
            "the query must not create an rpmdb"
        );
        assert!(!root.join("usr/lib/sysimage").exists());

        // And through the writer: a failing digest writes no stamp.
        let stamp = Stamp::ext_build(
            "e",
            "qemux86-64",
            StampInputs::new("c".into()),
            StampOutputs::default(),
        );
        let script = generate_write_stamp_script_with_digest(
            &stamp,
            &render_sysroot_digest_script(&dir.path().join("nope").display().to_string(), None),
        )
        .unwrap();
        let (ok, written) = run_stamp_script(dir.path(), &script);
        assert!(!ok && written.is_none());
    }

    /// `--no-stamps` prepends removal of the step's own stamp to its script,
    /// so an unrecorded run leaves no digest for a downstream step to trust.
    #[test]
    fn remove_own_stamp_line_names_the_stamp() {
        let line = remove_own_stamp_line(&StampRequirement::ext_build("app"));
        assert_eq!(
            line,
            "rm -f \"$AVOCADO_PREFIX/.stamps/ext/app/build.stamp\"\n"
        );
    }

    /// A stamp from an older format is reported as such, not as a config change.
    #[test]
    fn stale_reason_names_a_format_change() {
        let inputs = StampInputs::new("c".into());
        let mut old =
            Stamp::ext_install("app", "qemux86-64", inputs.clone(), StampOutputs::default());
        old.version = STAMP_VERSION - 1;
        match validate_stamp(
            &StampRequirement::ext_install("app"),
            Some(&old.to_json().unwrap()),
            Some(&inputs),
        ) {
            StampStatus::Stale { reason, .. } => {
                assert!(reason.contains("stamp format changed"), "{reason}");
                assert!(
                    reason.contains(&format!("v{}", STAMP_VERSION - 1)),
                    "{reason}"
                );
            }
            other => panic!("expected stale, got {other:?}"),
        }
        let current =
            Stamp::ext_install("app", "qemux86-64", inputs.clone(), StampOutputs::default());
        match validate_stamp(
            &StampRequirement::ext_install("app"),
            Some(&current.to_json().unwrap()),
            Some(&StampInputs::new("d".into())),
        ) {
            StampStatus::Stale { reason, .. } => assert_eq!(reason, "config hash mismatch"),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    /// The image hash chains on the build's output digest and folds only what
    /// the imager itself reads. A build-only input — `post_build`, an overlay —
    /// must NOT move it: it reaches the image through the tree, and the digest
    /// says whether the tree changed. That is what lets a rebuild with identical
    /// bytes stop before re-imaging.
    #[test]
    fn ext_image_hash_chains_on_the_build_digest_not_build_inputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("post.sh"), "a").unwrap();
        let img = |yaml: &str, digest: Option<&str>| {
            let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            compute_ext_image_input_hash(
                &v,
                "my-ext",
                Some("erofs"),
                dir.path(),
                None,
                None,
                None,
                digest,
            )
            .unwrap()
            .config_hash
        };
        let base = "extensions:\n  my-ext:\n    version: '1.0.0'\n    post_build: post.sh\n";
        let a = img(base, Some("sha256:t1"));

        // Build-only inputs: no effect on the image hash.
        std::fs::write(dir.path().join("post.sh"), "b").unwrap();
        assert_eq!(a, img(base, Some("sha256:t1")), "post_build content");
        assert_eq!(
            a,
            img(
                &base.replace(
                    "post_build: post.sh",
                    "post_build: other.sh\n    overlay: ov"
                ),
                Some("sha256:t1")
            ),
            "post_build path / overlay"
        );

        // The tree digest and the image's own inputs: effect.
        assert_ne!(a, img(base, Some("sha256:t2")), "build digest");
        assert_ne!(a, img(base, None), "absent digest is not a match");
        assert_ne!(
            a,
            img(&base.replace("1.0.0", "1.0.1"), Some("sha256:t1")),
            "version"
        );
        assert_ne!(
            a,
            img(
                &(base.to_string() + "    image:\n      verity: true\n"),
                Some("sha256:t1")
            ),
            "image.verity"
        );
        let v: serde_yaml::Value = serde_yaml::from_str(base).unwrap();
        assert_ne!(
            a,
            compute_ext_image_input_hash(
                &v,
                "my-ext",
                Some("squashfs"),
                dir.path(),
                None,
                None,
                None,
                Some("sha256:t1")
            )
            .unwrap()
            .config_hash,
            "filesystem"
        );
    }

    /// Batch-read output is `path:::json` per line, `null` for a missing stamp.
    fn batch_line(req: &StampRequirement, stamp: Option<&Stamp>) -> String {
        let body = stamp
            .map(|s| s.to_json().unwrap().replace('\n', ""))
            .unwrap_or_else(|| "null".to_string());
        format!("{}:::{}\n", req.relative_path(), body)
    }

    /// The chain: an upstream digest read from a batch result is folded into the
    /// downstream input hash, so a changed digest changes the input and an
    /// identical one does not.
    #[test]
    fn downstream_inputs_fold_upstream_content_hashes() {
        let with_digest = |h: &str| {
            Stamp::ext_build(
                "app",
                "qemux86-64",
                StampInputs::new("i".into()),
                StampOutputs {
                    content_hash: Some(h.to_string()),
                    ..Default::default()
                },
            )
        };
        let batch = batch_line(
            &StampRequirement::ext_build("app"),
            Some(&with_digest("sha256:aa")),
        ) + &batch_line(&StampRequirement::ext_image("app"), None);
        assert_eq!(
            content_hash_from_batch(&batch, &StampRequirement::ext_build("app")).as_deref(),
            Some("sha256:aa")
        );
        assert_eq!(
            content_hash_from_batch(&batch, &StampRequirement::ext_image("app")),
            None
        );

        // ext image input moves with the build digest.
        let cfg: serde_yaml::Value =
            serde_yaml::from_str("extensions:\n  my-ext:\n    packages:\n      bash: '*'\n")
                .unwrap();
        let img = |up: Option<&str>| {
            compute_ext_image_input_hash(&cfg, "my-ext", None, Path::new("."), None, None, None, up)
                .unwrap()
                .config_hash
        };
        assert_eq!(img(Some("sha256:aa")), img(Some("sha256:aa")));
        assert_ne!(img(Some("sha256:aa")), img(Some("sha256:bb")));
        assert_ne!(img(Some("sha256:aa")), img(None));

        // runtime build input moves with any required extension's image digest.
        let image_stamp = |h: &str| {
            Stamp::ext_image(
                "app",
                "qemux86-64",
                StampInputs::new("i".into()),
                StampOutputs {
                    content_hash: Some(h.to_string()),
                    ..Default::default()
                },
            )
        };
        let batch_a = batch_line(
            &StampRequirement::ext_image("app"),
            Some(&image_stamp("sha256:11")),
        );
        let batch_b = batch_line(
            &StampRequirement::ext_image("app"),
            Some(&image_stamp("sha256:22")),
        );
        let map_a = ext_content_hashes_from_batch(&batch_a, ["app".to_string()]);
        let map_b = ext_content_hashes_from_batch(&batch_b, ["app".to_string()]);
        assert_eq!(
            map_a.get("app.image").map(String::as_str),
            Some("sha256:11")
        );
        let rt: serde_yaml::Value = serde_yaml::from_str("packages:\n  a: '*'\n").unwrap();
        let empty = serde_yaml::Value::Mapping(Default::default());
        let rb = |m: &std::collections::BTreeMap<String, String>| {
            compute_runtime_build_input_hash(&rt, "dev", &empty, Path::new("."), m)
                .unwrap()
                .config_hash
        };
        assert_ne!(rb(&map_a), rb(&map_b));
        assert_ne!(rb(&map_a), rb(&Default::default()));
    }

    /// `is_current` compares `package_list_hash` strictly. A recorded `None`
    /// against a current `Some` (or the reverse) is stale — never a match by
    /// omission.
    #[test]
    fn is_current_is_strict_about_package_list_hash() {
        let mk = |pkg: Option<&str>| {
            let inputs = match pkg {
                Some(p) => StampInputs::with_package_list("c".into(), p.into()),
                None => StampInputs::new("c".into()),
            };
            Stamp::rootfs_install("qemux86-64", inputs, StampOutputs::default())
        };
        assert!(mk(None).is_current(&StampInputs::new("c".into())));
        assert!(mk(Some("p")).is_current(&StampInputs::with_package_list("c".into(), "p".into())));
        assert!(!mk(None).is_current(&StampInputs::with_package_list("c".into(), "p".into())));
        assert!(!mk(Some("p")).is_current(&StampInputs::new("c".into())));
        assert!(!mk(Some("p")).is_current(&StampInputs::with_package_list("c".into(), "q".into())));
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
            &Default::default(),
        )
        .unwrap()
        .config_hash;
        let b_b = compute_runtime_build_input_hash(
            &runtime_node,
            "dev",
            &parsed_b,
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .config_hash;
        assert_ne!(
            b_a, b_b,
            "runtime build SHOULD invalidate on filesystem swap"
        );
    }

    /// The encrypt marker and the package union both depend on the declared
    /// scope, so editing `targets:` has to invalidate both stamps. Without it,
    /// narrowing a scope leaves a stale initramfs still carrying its encrypt
    /// marker, and widening one leaves the marker missing until some unrelated
    /// edit forces a rebuild.
    #[test]
    fn a_targets_scope_edit_invalidates_the_runtime_stamps() {
        let node = |yaml: &str| -> serde_yaml::Value { serde_yaml::from_str(yaml).unwrap() };
        let empty = node("{}");
        let narrow = node("targets: [jetson-agx-thor]\n");
        let wide = node("targets: [jetson-agx-thor, jetson-agx-orin]\n");

        let install = |n: &serde_yaml::Value| {
            compute_runtime_install_input_hash(n, "dev")
                .unwrap()
                .config_hash
        };
        let build = |n: &serde_yaml::Value| {
            compute_runtime_build_input_hash(
                n,
                "dev",
                &empty,
                std::path::Path::new("."),
                &Default::default(),
            )
            .unwrap()
            .config_hash
        };

        assert_ne!(
            install(&narrow),
            install(&wide),
            "runtime install must invalidate when the declared scope changes"
        );
        assert_ne!(
            build(&narrow),
            build(&wide),
            "runtime build writes the encrypt marker, so it must invalidate too"
        );
        // Adding a scope where there was none also counts.
        assert_ne!(install(&empty), install(&narrow));
        assert_ne!(build(&empty), build(&narrow));
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

    /// `compute_sysroot_image_input_hash`'s config hash over a YAML section
    /// body against config `cfg`, with a fixed upstream digest; `rt` selects
    /// the runtime-build path.
    fn image_hash_in(
        section: &str,
        yaml: &str,
        cfg: &str,
        rt: Option<&str>,
        root: &Path,
    ) -> Result<String> {
        let node: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let cfg: serde_yaml::Value = serde_yaml::from_str(cfg).unwrap();
        Ok(
            compute_sysroot_image_input_hash(section, &node, "sha256:aa", &cfg, rt, root)?
                .config_hash,
        )
    }

    /// The standalone-command shape: empty config, no runtime.
    fn image_hash(section: &str, yaml: &str, root: &Path) -> Result<String> {
        image_hash_in(section, yaml, "{}", None, root)
    }

    #[test]
    fn sysroot_image_hash_is_deterministic() {
        let yaml = "filesystem: erofs\nimage:\n  type: kab\n  args: -v 1\n";
        let root = Path::new(".");
        assert_eq!(
            image_hash("rootfs", yaml, root).unwrap(),
            image_hash("rootfs", yaml, root).unwrap()
        );
    }

    /// The chain edge: the image step's real input is the sysroot, and the
    /// install stamp's digest is how it reaches the hash.
    #[test]
    fn sysroot_image_hash_chains_install_content_hash() {
        let node: serde_yaml::Value = serde_yaml::from_str("filesystem: erofs\n").unwrap();
        let empty = serde_yaml::Value::Mapping(Default::default());
        let hash = |up: &str| {
            compute_sysroot_image_input_hash("rootfs", &node, up, &empty, None, Path::new("."))
                .unwrap()
                .config_hash
        };
        assert_ne!(hash("sha256:aa"), hash("sha256:bb"));
    }

    #[test]
    fn sysroot_image_hash_tracks_filesystem_image_and_permissions() {
        let root = Path::new(".");
        let base = image_hash("rootfs", "filesystem: erofs\n", root).unwrap();
        for (label, yaml) in [
            ("filesystem", "filesystem: erofs-lz4\n"),
            (
                "image.verity",
                "filesystem: erofs\nimage:\n  verity: true\n",
            ),
            (
                "image arg",
                "filesystem: erofs\nimage:\n  type: kab\n  args: -v 1\n",
            ),
            (
                "permissions",
                "filesystem: erofs\npermissions:\n  users: [{name: app}]\n",
            ),
            // Not a key this module picks — the whole resolved section is
            // folded so an unforeseen image-side key still invalidates.
            (
                "unpicked section key",
                "filesystem: erofs\nfuture_knob: 1\n",
            ),
            (
                "unpicked image key",
                "filesystem: erofs\nimage:\n  compression: zstd\n",
            ),
        ] {
            assert_ne!(base, image_hash("rootfs", yaml, root).unwrap(), "{label}");
        }
    }

    #[test]
    fn sysroot_image_hash_tracks_post_install_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = tmp.path().join("post.sh");
        let yaml = "post_install: post.sh\n";

        std::fs::write(&script, b"echo v1\n").unwrap();
        let h1 = image_hash("rootfs", yaml, tmp.path()).unwrap();
        std::fs::write(&script, b"echo v2\n").unwrap();
        let h2 = image_hash("rootfs", yaml, tmp.path()).unwrap();
        assert_ne!(h1, h2, "same path, edited content must invalidate");

        std::fs::remove_file(&script).unwrap();
        assert!(
            image_hash("rootfs", yaml, tmp.path()).is_err(),
            "a declared but missing post_install is an error, not a sentinel"
        );
    }

    #[test]
    fn rootfs_and_initramfs_image_hashes_differ_for_identical_inputs() {
        let yaml = "filesystem: erofs\n";
        let root = Path::new(".");
        assert_ne!(
            image_hash("rootfs", yaml, root).unwrap(),
            image_hash("initramfs", yaml, root).unwrap()
        );
    }

    /// The hash is computed over `Config::resolve_image_section`'s output, so
    /// a `target-<name>:` override reaches it — the raw node would read the
    /// same for every target.
    #[test]
    fn sysroot_image_hash_sees_target_overrides() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("foo.sh"), b"echo foo\n").unwrap();
        let yaml = "rootfs:\n  filesystem: erofs\n  target-foo:\n    post_install: foo.sh\n";
        let config = crate::utils::config::Config::load_from_str(yaml).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let hash = |target: &str| {
            let section = config
                .resolve_image_section(&parsed, "rootfs", target)
                .unwrap();
            compute_sysroot_image_input_hash(
                "rootfs",
                &section,
                "sha256:aa",
                &parsed,
                None,
                tmp.path(),
            )
            .unwrap()
            .config_hash
        };
        assert_ne!(hash("foo"), hash("bar"));
    }

    /// The inputs the image step reads from outside its own section, each
    /// moving the hash on its own.
    #[test]
    fn sysroot_image_hash_tracks_config_and_runtime_inputs() {
        let root = Path::new(".");
        let hash = |cfg: &str| {
            image_hash_in(
                "initramfs",
                "filesystem: cpio.zst\n",
                cfg,
                Some("dev"),
                root,
            )
            .unwrap()
        };
        let dev = "runtimes:\n  dev:\n    version: '1'\n";
        let base = hash(dev);
        for (label, extra) in [
            ("var.encrypt", "    var:\n      encrypt: true\n"),
            ("var.hardware", "    var:\n      hardware: caam\n"),
            // The build resolves `target-<x>:` overrides inside the runtime
            // block; a per-target opt-in must not be invisible here.
            (
                "target-scoped var.encrypt",
                "    target-foo:\n      var:\n        encrypt: true\n",
            ),
            (
                "inline runtime rootfs permissions",
                "    rootfs:\n      permissions:\n        users: [{name: app}]\n",
            ),
            (
                "inline runtime initramfs permissions",
                "    initramfs:\n      permissions:\n        users: [{name: app}]\n",
            ),
            ("source_date_epoch", "source_date_epoch: 1700000000\n"),
            ("sdk.image", "sdk:\n  image: other\n"),
        ] {
            assert_ne!(base, hash(&format!("{dev}{extra}")), "{label}");
        }
        assert_ne!(
            base,
            hash("runtimes:\n  dev:\n    version: '2'\n"),
            "runtime version"
        );
    }

    /// `<section>.permissions: <name>` is only a ref; the body it names lives
    /// in top-level `permissions:` and an edit there has to invalidate.
    #[test]
    fn sysroot_image_hash_tracks_named_permissions_body() {
        let root = Path::new(".");
        let hash =
            |cfg: &str| image_hash_in("rootfs", "permissions: p\n", cfg, None, root).unwrap();
        assert_ne!(
            hash("permissions:\n  p:\n    users: [{name: app}]\n"),
            hash("permissions:\n  p:\n    users: [{name: app, uid: 1001}]\n")
        );
    }

    /// `runtimes.<rt>` is folded narrowly. Adding an extension or a package
    /// to the runtime must not re-image the rootfs — that inner loop is what
    /// this stamp exists to protect — and the standalone path ignores the
    /// runtimes block entirely.
    #[test]
    fn sysroot_image_hash_ignores_runtime_extensions_and_packages() {
        let root = Path::new(".");
        let hash = |cfg: &str, rt: Option<&str>| {
            image_hash_in("rootfs", "filesystem: erofs\n", cfg, rt, root).unwrap()
        };
        let a = "runtimes:\n  dev:\n    version: '1'\n    var:\n      encrypt: true\n";
        let b = format!(
            "{a}    extensions:\n      app: {{version: '1'}}\n    packages:\n      vim: '*'\n"
        );
        assert_eq!(hash(a, Some("dev")), hash(&b, Some("dev")));
        assert_eq!(
            hash(a, None),
            hash("{}", None),
            "standalone ignores runtimes"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sysroot_image_hash_tracks_kab_keyset_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keyset = tmp.path().join("kab.keyset");
        let root = Path::new(".");
        let kab = "image:\n  type: kab\n  args: -v 1\n";
        let raw = "image:\n  type: raw\n";

        std::env::set_var("KAB_KEYSET_FILE", &keyset);
        std::fs::write(&keyset, b"key-v1").unwrap();
        let h1 = image_hash("rootfs", kab, root).unwrap();
        std::fs::write(&keyset, b"key-v2").unwrap();
        assert_ne!(
            h1,
            image_hash("rootfs", kab, root).unwrap(),
            "a rotated keyset must invalidate"
        );
        let h_raw = image_hash("rootfs", raw, root).unwrap();

        std::env::set_var("KAB_KEYSET_FILE", tmp.path().join("missing"));
        assert!(
            image_hash("rootfs", kab, root).is_err(),
            "set but unreadable is an error, not a sentinel"
        );
        assert_eq!(
            h_raw,
            image_hash("rootfs", raw, root).unwrap(),
            "a non-kab image never reads the keyset"
        );

        std::env::remove_var("KAB_KEYSET_FILE");
        assert_eq!(
            h_raw,
            image_hash("rootfs", raw, root).unwrap(),
            "a stray KAB_KEYSET_FILE must not churn non-kab images"
        );
    }

    #[test]
    fn sysroot_image_stamp_paths_and_requirements() {
        let inputs = StampInputs::new("sha256:abc".to_string());
        let stamp = Stamp::rootfs_image("qemux86-64", inputs.clone(), StampOutputs::default());
        assert_eq!(stamp.command, StampCommand::Image);
        assert_eq!(stamp.component, StampComponent::Rootfs);
        assert_eq!(stamp.relative_path(), "rootfs/image.stamp");
        let stamp = Stamp::initramfs_image("qemux86-64", inputs, StampOutputs::default());
        assert_eq!(stamp.relative_path(), "initramfs/image.stamp");

        let req = StampRequirement::rootfs_image();
        assert_eq!(req.relative_path(), "rootfs/image.stamp");
        assert_eq!(req.description(), "rootfs image");
        assert_eq!(req.fix_command(), "avocado rootfs image");
        let req = StampRequirement::initramfs_image();
        assert_eq!(req.relative_path(), "initramfs/image.stamp");
        assert_eq!(req.description(), "initramfs image");
        assert_eq!(req.fix_command(), "avocado initramfs image");
    }
}
