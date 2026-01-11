//! Stamp-based state tracking for avocado CLI commands.
//!
//! This module implements a stamp/manifest system inspired by industry-standard build tools
//! (Cargo fingerprints, Nix derivations, Bazel action cache) that:
//!
//! 1. Tracks successful completion of each command at per-component granularity
//! 2. Detects staleness via content-addressable hashing (config + package list)
//! 3. Enforces command ordering with dependency resolution from config

// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

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

/// Current stamp format version
pub const STAMP_VERSION: u32 = 1;

/// Command types that can have stamps
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StampCommand {
    Install,
    Build,
    Image,
    Sign,
    Provision,
}

impl fmt::Display for StampCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StampCommand::Install => write!(f, "install"),
            StampCommand::Build => write!(f, "build"),
            StampCommand::Image => write!(f, "image"),
            StampCommand::Sign => write!(f, "sign"),
            StampCommand::Provision => write!(f, "provision"),
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
}

impl fmt::Display for StampComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StampComponent::Sdk => write!(f, "sdk"),
            StampComponent::Extension => write!(f, "ext"),
            StampComponent::Runtime => write!(f, "runtime"),
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

    /// Create stamp inputs with both hashes (for future output-based staleness detection)
    #[allow(unused)]
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
            _ => panic!("Component name required for Extension and Runtime"),
        }
    }

    /// Check if the stamp inputs match the current inputs
    pub fn is_current(&self, current_inputs: &StampInputs) -> bool {
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
            (StampComponent::Sdk, _, StampCommand::Install) => match runs_on {
                Some(remote) => format!("avocado sdk install --runs-on {remote}"),
                None => "avocado sdk install".to_string(),
            },
            (StampComponent::Extension, Some(name), StampCommand::Install) => {
                format!("avocado ext install -e {name}")
            }
            (StampComponent::Extension, Some(name), StampCommand::Build) => {
                format!("avocado ext build -e {name}")
            }
            (StampComponent::Extension, Some(name), StampCommand::Image) => {
                format!("avocado ext image -e {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Install) => {
                format!("avocado runtime install -r {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Build) => {
                format!("avocado runtime build -r {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Sign) => {
                format!("avocado runtime sign -r {name}")
            }
            (StampComponent::Runtime, Some(name), StampCommand::Provision) => {
                format!("avocado runtime provision -r {name}")
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

impl fmt::Display for StampValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Error: {} - dependencies not satisfied\n", self.context)?;

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

        // Collect unique fix commands, using runs_on hint for SDK install commands
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

        for fix in fixes {
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
    format!("sha256:{result:x}")
}

/// Compute hash of a YAML value (for config sections)
pub fn compute_config_hash(value: &serde_yaml::Value) -> Result<String> {
    // Serialize to canonical JSON for consistent hashing
    let json = serde_json::to_string(value).context("Failed to serialize config for hashing")?;
    Ok(compute_hash(&json))
}

/// Compute input hash for SDK install
/// Includes: sdk.dependencies, sdk.image, repo URLs
pub fn compute_sdk_input_hash(config: &serde_yaml::Value) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // Include sdk.dependencies
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
pub fn compute_ext_input_hash(config: &serde_yaml::Value, ext_name: &str) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // Include ext.<name>.dependencies
    if let Some(ext) = config.get("extensions").and_then(|e| e.get(ext_name)) {
        if let Some(deps) = ext.get("packages") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.dependencies")),
                deps.clone(),
            );
        }
        // Also include types as they affect build
        if let Some(types) = ext.get("types") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{ext_name}.types")),
                types.clone(),
            );
        }
    }

    let config_hash = compute_config_hash(&serde_yaml::Value::Mapping(hash_data))?;
    Ok(StampInputs::new(config_hash))
}

/// Compute input hash for runtime install
/// Includes: runtime.<name>.dependencies (merged with target)
pub fn compute_runtime_input_hash(
    merged_runtime: &serde_yaml::Value,
    runtime_name: &str,
) -> Result<StampInputs> {
    let mut hash_data = serde_yaml::Mapping::new();

    // Include the merged dependencies section
    if let Some(deps) = merged_runtime.get("packages") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.dependencies")),
            deps.clone(),
        );
    }

    // Include target if specified
    if let Some(target) = merged_runtime.get("target") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{runtime_name}.target")),
            target.clone(),
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

/// Validate all stamp requirements from batch output in a single pass
pub fn validate_stamps_batch(
    requirements: &[StampRequirement],
    batch_output: &str,
    current_inputs: Option<&StampInputs>,
) -> StampValidationResult {
    let stamp_data = parse_batch_stamps_output(batch_output);
    let mut validation = StampValidationResult::new();

    for req in requirements {
        let stamp_path = req.relative_path();
        let json_content = stamp_data.get(&stamp_path).and_then(|v| v.as_ref());

        check_stamp_requirement(
            req,
            json_content.map(|s| s.as_str()),
            current_inputs,
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
        r#"rpm --root={installroot} -qa --queryformat '%{{NAME}}-%{{VERSION}}-%{{RELEASE}}\n' 2>/dev/null | sort | sha256sum | cut -d' ' -f1"#
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

    match (cmd, component) {
        // SDK install has no dependencies
        (StampCommand::Install, StampComponent::Sdk) => vec![],

        // Extension install requires SDK install
        (StampCommand::Install, StampComponent::Extension) => {
            vec![sdk_install()]
        }

        // Runtime install requires SDK install
        (StampCommand::Install, StampComponent::Runtime) => {
            vec![sdk_install()]
        }

        // Extension build requires SDK install + own extension install
        (StampCommand::Build, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![sdk_install(), StampRequirement::ext_install(ext_name)]
        }

        // Extension image requires SDK install + own extension install + own extension build
        (StampCommand::Image, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![
                sdk_install(),
                StampRequirement::ext_install(ext_name),
                StampRequirement::ext_build(ext_name),
            ]
        }

        // Runtime build requires SDK + own install + ALL extension deps (install AND build)
        // Note: This doesn't distinguish versioned extensions - use resolve_required_stamps_detailed
        (StampCommand::Build, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            let mut reqs = vec![
                sdk_install(),
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

    let mut reqs = vec![sdk_install, StampRequirement::runtime_install(runtime_name)];

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
        assert_eq!(req.fix_command(), "avocado ext install -e gpu-driver");

        let req = StampRequirement::runtime_build("my-runtime");
        assert_eq!(req.description(), "runtime 'my-runtime' build");
        assert_eq!(req.fix_command(), "avocado runtime build -r my-runtime");
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
        // Extension build requires SDK install + own extension install
        let reqs = resolve_required_stamps(
            StampCommand::Build,
            StampComponent::Extension,
            Some("my-ext"),
            &[],
        );
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::ext_install("my-ext"));
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

        // Should have: SDK install, runtime install, ext-a install, ext-a build, ext-b install, ext-b build
        assert_eq!(reqs.len(), 6);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::runtime_install("my-runtime"));
        assert_eq!(reqs[2], StampRequirement::ext_install("ext-a"));
        assert_eq!(reqs[3], StampRequirement::ext_build("ext-a"));
        assert_eq!(reqs[4], StampRequirement::ext_install("ext-b"));
        assert_eq!(reqs[5], StampRequirement::ext_build("ext-b"));
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
        assert!(error_str.contains("avocado ext install -e gpu-driver"));
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
        // - Runtime install (1)
        // - app install + build + image (3)
        // - config-dev install + build + image (3)
        // - avocado-ext-dev install + build + image (3)
        // Total: 11
        assert_eq!(reqs.len(), 11);

        // Verify SDK and runtime install are present
        assert!(reqs.contains(&StampRequirement::sdk_install()));
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
        // - Runtime install (1)
        // - app install + build + image (3)
        // - config-dev install + build + image (3)
        // Total: 8
        assert_eq!(reqs.len(), 8);

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
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0], StampRequirement::sdk_install());
        assert_eq!(reqs[1], StampRequirement::ext_install("my-ext"));
        assert_eq!(reqs[2], StampRequirement::ext_build("my-ext"));
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
        assert_eq!(req.fix_command(), "avocado ext image -e gpu-driver");
        assert_eq!(req.relative_path(), "ext/gpu-driver/image.stamp");
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_no_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with NO extension dependencies
        let ext_deps: Vec<RuntimeExtDep> = vec![];

        let reqs = resolve_required_stamps_for_runtime_build("minimal-runtime", &ext_deps);

        // Should ONLY have SDK install + runtime install
        assert_eq!(reqs.len(), 2);
        assert!(reqs.contains(&StampRequirement::sdk_install()));
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

        let result = validate_stamps_batch(&requirements, &output, None);

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

        let result = validate_stamps_batch(&requirements, &output, None);

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

        let result = validate_stamps_batch(&requirements, "", None);

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
        assert_eq!(reqs[1].fix_command(), "avocado ext install -e my-ext");
        assert_eq!(reqs[2].fix_command(), "avocado ext build -e my-ext");

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
        assert_eq!(reqs[1].fix_command(), "avocado ext install -e my-ext");
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
        let result_before = validate_stamps_batch(&requirements, &output_before, None);
        assert!(result_before.is_satisfied());

        // After ext clean: SDK still there, ext stamps gone
        let output_after_ext_clean = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::null",
            get_local_arch(),
            sdk_json
        );
        let result_after = validate_stamps_batch(&requirements, &output_after_ext_clean, None);
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

        let result = validate_stamps_batch(&requirements, &output, None);

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
        let result = validate_stamps_batch(&requirements, &output, Some(&changed_inputs));

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

        // With changed inputs (simulating config change)
        let changed_inputs = StampInputs::new("sha256:config-v2".to_string());
        let result = validate_stamps_batch(&requirements, &output, Some(&changed_inputs));

        // Both should be stale since config changed
        assert!(!result.is_satisfied());
        assert_eq!(result.stale.len(), 2);
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
        assert!(msg.contains("avocado ext install -e app"));
        assert!(msg.contains("avocado ext build -e app"));
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
}
