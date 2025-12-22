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
    pub fn relative_path(&self) -> String {
        match (&self.component, &self.component_name) {
            (StampComponent::Sdk, _) => format!("sdk/{}.stamp", self.command),
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
}

impl StampRequirement {
    pub fn new(command: StampCommand, component: StampComponent, name: Option<&str>) -> Self {
        Self {
            command,
            component,
            component_name: name.map(|s| s.to_string()),
        }
    }

    /// SDK install requirement
    pub fn sdk_install() -> Self {
        Self::new(StampCommand::Install, StampComponent::Sdk, None)
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
    pub fn relative_path(&self) -> String {
        match (&self.component, &self.component_name) {
            (StampComponent::Sdk, _) => format!("sdk/{}.stamp", self.command),
            (StampComponent::Extension, Some(name)) => {
                format!("ext/{}/{}.stamp", name, self.command)
            }
            (StampComponent::Runtime, Some(name)) => {
                format!("runtime/{}/{}.stamp", name, self.command)
            }
            _ => panic!("Component name required for Extension and Runtime"),
        }
    }

    /// Human-readable description
    pub fn description(&self) -> String {
        match (&self.component, &self.component_name) {
            (StampComponent::Sdk, _) => format!("SDK {}", self.command),
            (StampComponent::Extension, Some(name)) => {
                format!("extension '{}' {}", name, self.command)
            }
            (StampComponent::Runtime, Some(name)) => {
                format!("runtime '{}' {}", name, self.command)
            }
            _ => format!("{} {}", self.component, self.command),
        }
    }

    /// Suggested fix command
    pub fn fix_command(&self) -> String {
        match (&self.component, &self.component_name, &self.command) {
            (StampComponent::Sdk, _, StampCommand::Install) => "avocado sdk install".to_string(),
            (StampComponent::Extension, Some(name), StampCommand::Install) => {
                format!("avocado ext install -e {}", name)
            }
            (StampComponent::Extension, Some(name), StampCommand::Build) => {
                format!("avocado ext build -e {}", name)
            }
            (StampComponent::Extension, Some(name), StampCommand::Image) => {
                format!("avocado ext image -e {}", name)
            }
            (StampComponent::Runtime, Some(name), StampCommand::Install) => {
                format!("avocado runtime install -r {}", name)
            }
            (StampComponent::Runtime, Some(name), StampCommand::Build) => {
                format!("avocado runtime build -r {}", name)
            }
            (StampComponent::Runtime, Some(name), StampCommand::Sign) => {
                format!("avocado runtime sign -r {}", name)
            }
            (StampComponent::Runtime, Some(name), StampCommand::Provision) => {
                format!("avocado runtime provision -r {}", name)
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
    pub fn into_error(self, context: &str) -> StampValidationError {
        StampValidationError {
            context: context.to_string(),
            missing: self.missing,
            stale: self.stale,
        }
    }
}

/// Error when stamp validation fails
#[derive(Debug)]
pub struct StampValidationError {
    pub context: String,
    pub missing: Vec<StampRequirement>,
    pub stale: Vec<(StampRequirement, String)>,
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

        // Collect unique fix commands
        let mut fixes: Vec<String> = self
            .missing
            .iter()
            .chain(self.stale.iter().map(|(req, _)| req))
            .map(|req| req.fix_command())
            .collect();
        fixes.sort();
        fixes.dedup();

        for fix in fixes {
            writeln!(f, "  {}", fix)?;
        }

        Ok(())
    }
}

/// Compute SHA256 hash of a string
pub fn compute_hash(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    format!("sha256:{:x}", result)
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
        if let Some(deps) = sdk.get("dependencies") {
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
    if let Some(ext) = config.get("ext").and_then(|e| e.get(ext_name)) {
        if let Some(deps) = ext.get("dependencies") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{}.dependencies", ext_name)),
                deps.clone(),
            );
        }
        // Also include types as they affect build
        if let Some(types) = ext.get("types") {
            hash_data.insert(
                serde_yaml::Value::String(format!("ext.{}.types", ext_name)),
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
    if let Some(deps) = merged_runtime.get("dependencies") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{}.dependencies", runtime_name)),
            deps.clone(),
        );
    }

    // Include target if specified
    if let Some(target) = merged_runtime.get("target") {
        hash_data.insert(
            serde_yaml::Value::String(format!("runtime.{}.target", runtime_name)),
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
    match (cmd, component) {
        // SDK install has no dependencies
        (StampCommand::Install, StampComponent::Sdk) => vec![],

        // Extension install requires SDK install
        (StampCommand::Install, StampComponent::Extension) => {
            vec![StampRequirement::sdk_install()]
        }

        // Runtime install requires SDK install
        (StampCommand::Install, StampComponent::Runtime) => {
            vec![StampRequirement::sdk_install()]
        }

        // Extension build requires SDK install + own extension install
        (StampCommand::Build, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![
                StampRequirement::sdk_install(),
                StampRequirement::ext_install(ext_name),
            ]
        }

        // Extension image requires SDK install + own extension install + own extension build
        (StampCommand::Image, StampComponent::Extension) => {
            let ext_name = component_name.expect("Extension name required");
            vec![
                StampRequirement::sdk_install(),
                StampRequirement::ext_install(ext_name),
                StampRequirement::ext_build(ext_name),
            ]
        }

        // Runtime build requires SDK + own install + ALL extension deps (install AND build)
        // Note: This doesn't distinguish versioned extensions - use resolve_required_stamps_detailed
        (StampCommand::Build, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            let mut reqs = vec![
                StampRequirement::sdk_install(),
                StampRequirement::runtime_install(runtime_name),
            ];

            // Add extension dependencies (both install and build)
            for ext_name in ext_dependencies {
                reqs.push(StampRequirement::ext_install(ext_name));
                reqs.push(StampRequirement::ext_build(ext_name));
            }

            reqs
        }

        // Sign requires runtime build
        (StampCommand::Sign, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            vec![StampRequirement::runtime_build(runtime_name)]
        }

        // Provision requires runtime build
        (StampCommand::Provision, StampComponent::Runtime) => {
            let runtime_name = component_name.expect("Runtime name required");
            vec![StampRequirement::runtime_build(runtime_name)]
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
/// - Versioned extensions: NO stamp requirements - they're prebuilt packages
///   installed directly via DNF during `runtime install`. The package repository
///   contains the complete extension images, so no local build/image steps needed.
pub fn resolve_required_stamps_for_runtime_build(
    runtime_name: &str,
    ext_dependencies: &[RuntimeExtDep],
) -> Vec<StampRequirement> {
    let mut reqs = vec![
        StampRequirement::sdk_install(),
        StampRequirement::runtime_install(runtime_name),
    ];

    for ext_dep in ext_dependencies {
        let ext_name = ext_dep.name();

        match ext_dep {
            // Local extensions: require install + build + image stamps
            RuntimeExtDep::Local(_) => {
                reqs.push(StampRequirement::ext_install(ext_name));
                reqs.push(StampRequirement::ext_build(ext_name));
                reqs.push(StampRequirement::ext_image(ext_name));
            }
            // External extensions: require install + build + image stamps
            RuntimeExtDep::External { .. } => {
                reqs.push(StampRequirement::ext_install(ext_name));
                reqs.push(StampRequirement::ext_build(ext_name));
                reqs.push(StampRequirement::ext_image(ext_name));
            }
            // Versioned extensions: NO stamp requirements
            // They're prebuilt packages from the package repository, installed
            // directly via DNF during `runtime install`. No local ext install,
            // ext build, or ext image steps are needed.
            RuntimeExtDep::Versioned { .. } => {
                // No stamps required - covered by runtime install
            }
        }
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

        let sdk_stamp = Stamp::sdk_install("qemux86-64", inputs.clone(), outputs.clone());
        assert_eq!(sdk_stamp.relative_path(), "sdk/install.stamp");

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
        assert_eq!(req.description(), "SDK install");
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
        // Sign requires runtime build
        let reqs = resolve_required_stamps(
            StampCommand::Sign,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0], StampRequirement::runtime_build("my-runtime"));
    }

    #[test]
    fn test_resolve_required_stamps_provision() {
        // Provision requires runtime build
        let reqs = resolve_required_stamps(
            StampCommand::Provision,
            StampComponent::Runtime,
            Some("my-runtime"),
            &[],
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0], StampRequirement::runtime_build("my-runtime"));
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
        assert!(error_str.contains("sdk/install.stamp"));
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
    fn test_resolve_required_stamps_for_runtime_build_with_mixed_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Test with mixed extension types:
        // - local-ext: needs install + build + image stamps
        // - external-ext: needs install + build + image stamps
        // - versioned-ext: NO stamps (prebuilt package from repo)
        let ext_deps = vec![
            RuntimeExtDep::Local("local-ext".to_string()),
            RuntimeExtDep::External {
                name: "external-ext".to_string(),
                config_path: "../external/avocado.yaml".to_string(),
            },
            RuntimeExtDep::Versioned {
                name: "versioned-ext".to_string(),
                version: "1.0.0".to_string(),
            },
        ];

        let reqs = resolve_required_stamps_for_runtime_build("my-runtime", &ext_deps);

        // Should have:
        // - SDK install (1)
        // - Runtime install (1)
        // - local-ext install + build + image (3)
        // - external-ext install + build + image (3)
        // - versioned-ext: NOTHING (prebuilt package from repo)
        // Total: 8
        assert_eq!(reqs.len(), 8);

        // Verify SDK and runtime install are present
        assert!(reqs.contains(&StampRequirement::sdk_install()));
        assert!(reqs.contains(&StampRequirement::runtime_install("my-runtime")));

        // Verify local extension has install, build, and image
        assert!(reqs.contains(&StampRequirement::ext_install("local-ext")));
        assert!(reqs.contains(&StampRequirement::ext_build("local-ext")));
        assert!(reqs.contains(&StampRequirement::ext_image("local-ext")));

        // Verify external extension has install, build, and image
        assert!(reqs.contains(&StampRequirement::ext_install("external-ext")));
        assert!(reqs.contains(&StampRequirement::ext_build("external-ext")));
        assert!(reqs.contains(&StampRequirement::ext_image("external-ext")));

        // Verify versioned extension has NO stamps at all
        // (they're prebuilt packages installed via DNF during runtime install)
        assert!(!reqs.contains(&StampRequirement::ext_install("versioned-ext")));
        assert!(!reqs.contains(&StampRequirement::ext_build("versioned-ext")));
        assert!(!reqs.contains(&StampRequirement::ext_image("versioned-ext")));
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_only_versioned_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with ONLY versioned extensions (common for prebuilt extensions from package repo)
        // Example: avocado-ext-dev, avocado-ext-sshd-dev
        // Versioned extensions are prebuilt packages - NO stamps required
        let ext_deps = vec![
            RuntimeExtDep::Versioned {
                name: "avocado-ext-dev".to_string(),
                version: "0.1.0".to_string(),
            },
            RuntimeExtDep::Versioned {
                name: "avocado-ext-sshd-dev".to_string(),
                version: "0.1.0".to_string(),
            },
        ];

        let reqs = resolve_required_stamps_for_runtime_build("dev", &ext_deps);

        // Should ONLY have SDK install + runtime install (2 total)
        // Versioned extensions don't add any stamp requirements
        assert_eq!(reqs.len(), 2);
        assert!(reqs.contains(&StampRequirement::sdk_install()));
        assert!(reqs.contains(&StampRequirement::runtime_install("dev")));

        // Verify NO extension stamps are required for versioned extensions
        assert!(!reqs.contains(&StampRequirement::ext_install("avocado-ext-dev")));
        assert!(!reqs.contains(&StampRequirement::ext_build("avocado-ext-dev")));
        assert!(!reqs.contains(&StampRequirement::ext_image("avocado-ext-dev")));
        assert!(!reqs.contains(&StampRequirement::ext_install("avocado-ext-sshd-dev")));
        assert!(!reqs.contains(&StampRequirement::ext_build("avocado-ext-sshd-dev")));
        assert!(!reqs.contains(&StampRequirement::ext_image("avocado-ext-sshd-dev")));
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_only_external_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with ONLY external extensions (from external config files)
        let ext_deps = vec![
            RuntimeExtDep::External {
                name: "avocado-ext-peridio".to_string(),
                config_path: "avocado-ext-peridio/avocado.yml".to_string(),
            },
            RuntimeExtDep::External {
                name: "custom-ext".to_string(),
                config_path: "../custom/avocado.yaml".to_string(),
            },
        ];

        let reqs = resolve_required_stamps_for_runtime_build("my-runtime", &ext_deps);

        // Should have:
        // - SDK install (1)
        // - Runtime install (1)
        // - avocado-ext-peridio install + build + image (3)
        // - custom-ext install + build + image (3)
        // Total: 8
        assert_eq!(reqs.len(), 8);

        // Verify external extensions require install, build, and image
        assert!(reqs.contains(&StampRequirement::ext_install("avocado-ext-peridio")));
        assert!(reqs.contains(&StampRequirement::ext_build("avocado-ext-peridio")));
        assert!(reqs.contains(&StampRequirement::ext_image("avocado-ext-peridio")));
        assert!(reqs.contains(&StampRequirement::ext_install("custom-ext")));
        assert!(reqs.contains(&StampRequirement::ext_build("custom-ext")));
        assert!(reqs.contains(&StampRequirement::ext_image("custom-ext")));
    }

    #[test]
    fn test_resolve_required_stamps_runtime_build_only_local_extensions() {
        use crate::utils::config::RuntimeExtDep;

        // Runtime with ONLY local extensions (defined in main config)
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

        let local = RuntimeExtDep::Local("my-local-ext".to_string());
        assert_eq!(local.name(), "my-local-ext");

        let external = RuntimeExtDep::External {
            name: "my-external-ext".to_string(),
            config_path: "path/to/config.yaml".to_string(),
        };
        assert_eq!(external.name(), "my-external-ext");

        let versioned = RuntimeExtDep::Versioned {
            name: "my-versioned-ext".to_string(),
            version: "1.2.3".to_string(),
        };
        assert_eq!(versioned.name(), "my-versioned-ext");
    }

    #[test]
    fn test_generate_batch_read_stamps_script() {
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
        ];

        let script = generate_batch_read_stamps_script(&requirements);

        // Should contain all three stamp paths
        assert!(script.contains("sdk/install.stamp"));
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
        let output = r#"sdk/install.stamp:::{"version":"1.0.0","command":"install","component":"sdk"}
ext/my-ext/install.stamp:::{"version":"1.0.0","command":"install","component":"ext"}
ext/my-ext/build.stamp:::null"#;

        let result = parse_batch_stamps_output(output);

        assert_eq!(result.len(), 3);
        assert!(result.get("sdk/install.stamp").unwrap().is_some());
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
            "sdk/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            sdk_json, ext_json
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
            "sdk/install.stamp:::{}\next/my-ext/install.stamp:::null\next/my-ext/build.stamp:::null",
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
        let reqs = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
        ];

        // Verify fix commands are correct
        assert_eq!(reqs[0].fix_command(), "avocado sdk install");
        assert_eq!(reqs[1].fix_command(), "avocado ext install -e my-ext");
        assert_eq!(reqs[2].fix_command(), "avocado ext build -e my-ext");

        // Verify descriptions are helpful
        assert_eq!(reqs[0].description(), "SDK install");
        assert_eq!(reqs[1].description(), "extension 'my-ext' install");
        assert_eq!(reqs[2].description(), "extension 'my-ext' build");
    }

    #[test]
    fn test_ext_checkout_requires_sdk_install_ext_install() {
        // ext checkout requires: SDK install + ext install (but NOT build)
        // Checkout is for extracting files from installed sysroot
        let reqs = vec![
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
        let reqs = vec![StampRequirement::sdk_install()];

        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].fix_command(), "avocado sdk install");
        assert_eq!(reqs[0].relative_path(), "sdk/install.stamp");
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

        // Verify all paths are correct
        assert_eq!(reqs[0].relative_path(), "sdk/install.stamp");
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
        // SDK clean should remove stamps at sdk/
        let install_stamp = StampRequirement::sdk_install();

        assert_eq!(install_stamp.relative_path(), "sdk/install.stamp");

        // Clean removes: rm -rf "$AVOCADO_PREFIX/.stamps/sdk"
        let path = install_stamp.relative_path();
        let parent = std::path::Path::new(&path)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(parent, "sdk");
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
            "sdk/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            sdk_json, ext_json
        );
        let result_before = validate_stamps_batch(&requirements, &output_before, None);
        assert!(result_before.is_satisfied());

        // After ext clean: SDK still there, ext stamps gone
        let output_after_ext_clean = format!(
            "sdk/install.stamp:::{}\next/my-ext/install.stamp:::null",
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
        let output = r#"sdk/install.stamp:::null
ext/ext-a/install.stamp:::null
ext/ext-a/build.stamp:::null
runtime/my-runtime/install.stamp:::null
runtime/my-runtime/build.stamp:::null"#;

        let result = validate_stamps_batch(&requirements, output, None);

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
        let output = format!("ext/my-ext/install.stamp:::{}", json);

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
            "sdk/install.stamp:::{}\next/my-ext/install.stamp:::{}",
            sdk_json, ext_json
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
}
