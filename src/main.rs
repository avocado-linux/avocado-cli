use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::collections::HashMap;
use uuid::Uuid;

mod commands;
mod completion;
mod utils;

use utils::config::Config;

use commands::build::BuildCommand;
use commands::clean::CleanCommand;
use commands::config_show::ConfigShowCommand;
use commands::connect::auth::{
    ConnectAuthLoginCommand, ConnectAuthLogoutCommand, ConnectAuthStatusCommand, OutputFormat,
};
use commands::connect::claim_tokens::{
    ConnectClaimTokensCreateCommand, ConnectClaimTokensDeleteCommand, ConnectClaimTokensListCommand,
};
use commands::connect::clean::ConnectCleanCommand;
use commands::connect::cohorts::{
    ConnectCohortsCreateCommand, ConnectCohortsDeleteCommand, ConnectCohortsListCommand,
};
use commands::connect::deploy::ConnectDeployCommand;
use commands::connect::device_reclaim::{
    ConnectDeviceReclaimApproveCommand, ConnectDeviceReclaimDeleteCommand,
    ConnectDeviceReclaimDenyCommand, ConnectDeviceReclaimListCommand,
};
use commands::connect::devices::{
    ConnectDevicesCreateCommand, ConnectDevicesDeleteCommand, ConnectDevicesListCommand,
};
use commands::connect::init::ConnectInitCommand;
use commands::connect::keys::{
    ConnectKeysApproveCommand, ConnectKeysListCommand, ConnectKeysRegisterCommand,
    ConnectKeysRetireCommand,
};
use commands::connect::orgs::ConnectOrgsListCommand;
use commands::connect::projects::{
    ConnectProjectsCreateCommand, ConnectProjectsDeleteCommand, ConnectProjectsListCommand,
};
use commands::connect::runtimes::ConnectRuntimesListCommand;
use commands::connect::server_key::ConnectServerKeyCommand;
use commands::connect::trust::{
    ConnectTrustPromoteRootCommand, ConnectTrustRotateServerKeyCommand, ConnectTrustStatusCommand,
};
use commands::connect::upload::ConnectUploadCommand;
use commands::ext::{
    ExtBuildCommand, ExtCheckoutCommand, ExtCleanCommand, ExtDepsCommand, ExtDnfCommand,
    ExtFetchCommand, ExtImageCommand, ExtInstallCommand, ExtListCommand, ExtPackageCommand,
};
use commands::fetch::FetchCommand;
use commands::hitl::HitlServerCommand;
use commands::init::InitCommand;
use commands::initramfs::{InitramfsCleanCommand, InitramfsImageCommand, InitramfsInstallCommand};
use commands::install::InstallCommand;
use commands::kernel::KernelImageCommand;
use commands::load::LoadCommand;
use commands::provision::ProvisionCommand;
use commands::prune::PruneCommand;
use commands::rootfs::{RootfsCleanCommand, RootfsImageCommand, RootfsInstallCommand};
use commands::runtime::{
    RuntimeBuildCommand, RuntimeCleanCommand, RuntimeDeployCommand, RuntimeDepsCommand,
    RuntimeDnfCommand, RuntimeInstallCommand, RuntimeListCommand, RuntimeProvisionCommand,
    RuntimeSignCommand,
};
use commands::save::SaveCommand;
use commands::sdk::{
    SdkCleanCommand, SdkCompileCommand, SdkDepsCommand, SdkDnfCommand, SdkInstallCommand,
    SdkPackageCommand, SdkRunCommand,
};
use commands::sign::SignCommand;
use commands::signing_keys::{
    SigningKeysCreateCommand, SigningKeysListCommand, SigningKeysRemoveCommand,
};
use commands::unlock::UnlockCommand;
use commands::update::UpdateCommand;
use commands::upgrade::UpgradeCommand;

#[derive(Parser)]
#[command(name = "avocado")]
#[command(about = "Avocado CLI - A command line interface for Avocado")]
#[command(version)]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Global target architecture
    #[arg(long)]
    target: Option<String>,

    /// Disable stamp validation and writing
    #[arg(long)]
    no_stamps: bool,

    /// Run command on remote host using local volume via NFS (format: user@host)
    #[arg(long, value_name = "USER@HOST", global = true)]
    runs_on: Option<String>,

    /// NFS port for remote execution (auto-selects from 12050-12099 if not specified)
    #[arg(long, global = true)]
    nfs_port: Option<u16>,

    /// SDK container architecture for cross-arch emulation via Docker buildx/QEMU (aarch64 or x86-64)
    #[arg(long, value_name = "ARCH", global = true)]
    sdk_arch: Option<String>,

    /// Disable TUI output (use legacy sequential output with inherited stdio)
    #[arg(long, global = true)]
    no_tui: bool,

    /// On macOS/Windows, don't auto-start the avocado-vm; talk to the local docker daemon directly.
    /// (Equivalent to `AVOCADO_VM_AUTO_START=0`.)
    #[arg(long, global = true)]
    no_vm_auto_start: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// SDK related commands
    Sdk {
        #[command(subcommand)]
        command: SdkCommands,
    },
    /// Extension related commands
    Ext {
        #[command(subcommand)]
        command: ExtCommands,
    },
    /// Rootfs sysroot and image commands
    Rootfs {
        #[command(subcommand)]
        command: RootfsCommands,
    },
    /// Initramfs sysroot and image commands
    Initramfs {
        #[command(subcommand)]
        command: InitramfsCommands,
    },
    /// Kernel image commands
    Kernel {
        #[command(subcommand)]
        command: KernelCommands,
    },
    /// Initialize a new avocado project
    Init {
        /// Directory to initialize (defaults to current directory). When
        /// `--name` is also given, the project is created at
        /// <directory>/<name>/ instead of being written directly into
        /// <directory>.
        directory: Option<String>,
        /// Target architecture (e.g., "qemux86-64")
        #[arg(long)]
        target: Option<String>,
        /// Reference example to initialize from (downloads from avocado-linux/references)
        #[arg(long)]
        reference: Option<String>,
        /// Branch to fetch reference from (defaults to "main")
        #[arg(long)]
        reference_branch: Option<String>,
        /// Specific commit SHA to fetch reference from
        #[arg(long)]
        reference_commit: Option<String>,
        /// Repository to fetch reference from (format: "owner/repo", defaults to "avocado-linux/references")
        #[arg(long)]
        reference_repo: Option<String>,
        /// Name for the new project. When provided, the project is
        /// created in `<directory>/<name>/`. Defaults to the reference
        /// name when initializing from a reference, or to the
        /// destination directory name otherwise.
        #[arg(long)]
        name: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Runtime management commands
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommands,
    },
    /// Hardware-in-the-loop testing commands
    Hitl {
        #[command(subcommand)]
        command: HitlCommands,
    },
    /// Manage the local avocado-vm helper VM (macOS / Windows dev hosts).
    Vm {
        #[command(subcommand)]
        command: VmCommands,
    },
    /// Project configuration introspection (read-only).
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Clean the avocado project by removing docker volumes and state files
    Clean {
        /// Directory to clean (defaults to current directory)
        directory: Option<String>,
        /// Skip cleaning docker volumes (volumes are cleaned by default)
        #[arg(long)]
        skip_volumes: bool,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Also remove stamp files (requires -C/--config and --target)
        #[arg(long)]
        stamps: bool,
        /// Path to avocado.yaml configuration file (required when --stamps or --unlock is used)
        #[arg(short = 'C', long)]
        config: Option<String>,
        /// Target architecture (required when --stamps or --unlock is used)
        #[arg(long)]
        target: Option<String>,
        /// Force removal by killing and removing containers using the volume
        #[arg(short, long)]
        force: bool,
        /// Also unlock (clear lock file entries) for all sysroots (requires -C/--config)
        #[arg(long)]
        unlock: bool,
    },
    /// Install all components, or add specific packages to an extension/runtime/SDK
    ///
    /// Without packages: syncs all sysroots with avocado.yaml (installs missing, removes extraneous).
    /// With packages: adds them to the specified scope and writes to avocado.yaml.
    Install {
        /// Packages to install (when provided, adds to config and installs into the specified scope)
        packages: Vec<String>,
        /// Extension to install packages into (required when adding packages)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to install packages into (or sync when no packages given)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Install packages into the SDK
        #[arg(long)]
        sdk: bool,
        /// Skip writing packages to avocado.yaml
        #[arg(long)]
        no_save: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Output format. JSON skips TUI rendering and emits NDJSON events.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Remove packages from an extension, runtime, or SDK and update avocado.yaml
    Uninstall {
        /// Packages to remove
        #[arg(required = true)]
        packages: Vec<String>,
        /// Extension to remove packages from
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Runtime to remove packages from
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Remove packages from the SDK
        #[arg(long)]
        sdk: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Upgrade the CLI to the latest (or specified) version
    Upgrade {
        /// Controls what version to upgrade to. If not specified, the latest version will be used.
        #[arg(long)]
        version: Option<String>,
    },
    /// Generate a shell completion registration script.
    ///
    /// The output is a small wrapper that, when sourced, makes the shell
    /// call `avocado` itself for each TAB press. That round-trip lets
    /// completions stay live for values that depend on the local
    /// `avocado.yaml` (extension/runtime/target names) and the user's
    /// signing-key registry.
    ///
    /// Install:
    ///   bash: `avocado completion bash > /etc/bash_completion.d/avocado`
    ///         (or add `source <(avocado completion bash)` to ~/.bashrc)
    ///   zsh:  add `source <(avocado completion zsh)` to ~/.zshrc
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Build all components (SDK compile, extensions, and runtime images)
    Build {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to build (if not provided, builds all runtimes)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Extension name to build (if not provided, builds all required extensions)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Output format. JSON skips TUI rendering and emits NDJSON events.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Fetch and refresh repository metadata for sysroots
    Fetch {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name to fetch metadata for (if not provided, fetches for all sysroots)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Runtime name to fetch metadata for (if not provided, fetches for all sysroots)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Provision a runtime (shortcut for 'runtime provision')
    Provision {
        /// Runtime name (must be defined in config)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to provision (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Provision profile to use
        #[arg(long = "profile")]
        provision_profile: Option<String>,
        /// Environment variables to pass to the provision process
        #[arg(long = "env", num_args = 1, action = clap::ArgAction::Append)]
        env: Option<Vec<String>>,
        /// Output path relative to src_dir for provisioning artifacts
        #[arg(long = "out")]
        out: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// List the provisioning profiles available for the resolved
        /// target instead of provisioning. Reads the stone manifest
        /// from the installed SDK volume; requires `avocado install`
        /// to have run.
        #[arg(long = "list", conflicts_with_all = ["name", "force", "env", "out", "provision_profile"])]
        list: bool,
        /// Output format. JSON skips TUI rendering and emits NDJSON events.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Deploy a runtime to a device (shortcut for 'runtime deploy')
    Deploy {
        /// Runtime name (must be defined in config)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to deploy (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Device to deploy to as [user@]host[:port] (e.g. root@192.168.1.100:2222)
        #[arg(short = 'd', long = "device", required = true)]
        device: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Sign TUF metadata via the Connect platform instead of locally.
        /// Use this when deploying to a device that has received a Connect OTA update.
        /// Requires a local signing key configured for the runtime (Level 2:
        /// signing.key + avocado connect trust promote-root --key <KEY>); without one no
        /// root.json is baked and the deploy fails during Phase 1 hash collection.
        #[arg(long)]
        connect_sign: bool,
        /// Output format. JSON skips TUI rendering and emits NDJSON events.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Manage signing keys for extension and image signing
    #[command(name = "signing-keys")]
    SigningKeys {
        #[command(subcommand)]
        command: SigningKeysCommands,
    },
    /// Sign runtime images (shortcut for 'runtime sign')
    Sign {
        /// Runtime name to sign (if not provided, signs all runtimes with signing config)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to sign (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove abandoned Docker volumes no longer associated with active configs
    Prune {
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Perform a dry run without actually removing volumes
        #[arg(long)]
        dry_run: bool,
    },
    /// Save the current build state to a compressed archive
    Save {
        /// Output file path (e.g. state.tar.gz)
        #[arg(short, long)]
        output: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
        /// Include the src_dir contents in the archive
        #[arg(long)]
        include_src: bool,
    },
    /// Load build state from a compressed archive
    Load {
        /// Input archive file path
        #[arg(short, long)]
        input: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
        /// Overwrite existing volume and config if present
        #[arg(short, long)]
        force: bool,
    },
    /// Unlock (remove lock entries for) sysroots to allow package updates
    Unlock {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Unlock a specific extension
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Unlock a specific runtime
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Unlock SDK (rootfs, initramfs, target-sysroot, and all SDK arches)
        #[arg(long)]
        sdk: bool,
        /// Unlock rootfs
        #[arg(long)]
        rootfs: bool,
        /// Unlock initramfs
        #[arg(long)]
        initramfs: bool,
    },
    /// Move a target forward: advance to the latest feed snapshot and re-resolve
    /// packages to their latest versions on the next install (rewrites the lock).
    Update {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Avocado Connect platform commands (auth, upload)
    Connect {
        #[command(subcommand)]
        command: ConnectCommands,
    },
}

#[derive(Subcommand)]
enum ConnectCommands {
    /// Authenticate with the Connect platform
    Auth {
        #[command(subcommand)]
        command: ConnectAuthCommands,
    },
    /// Initialize connect settings in avocado.yaml (org, project, server key, extensions, claim token, device config)
    Init {
        /// Organization ID (skip interactive prompt)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (skip interactive prompt)
        #[arg(long)]
        project: Option<String>,
        /// Cohort ID (skip interactive prompt)
        #[arg(long)]
        cohort: Option<String>,
        /// Runtime to add connect extensions to (default: dev)
        #[arg(short, long, default_value = "dev")]
        runtime: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or NDJSON event stream)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Remove connect configuration (connect section, connect-config extension, and device config overlay)
    Clean {
        /// Runtime to remove connect-config extension from (default: dev)
        #[arg(short, long, default_value = "dev")]
        runtime: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Manage organizations
    Orgs {
        #[command(subcommand)]
        command: ConnectOrgsCommands,
    },
    /// Publish extensions to the feed (super-admin)
    Ext {
        #[command(subcommand)]
        command: ConnectExtCommands,
    },
    /// Manage projects
    Projects {
        #[command(subcommand)]
        command: ConnectProjectsCommands,
    },
    /// Manage devices
    Devices {
        #[command(subcommand)]
        command: ConnectDevicesCommands,
    },
    /// Manage cohorts
    Cohorts {
        #[command(subcommand)]
        command: ConnectCohortsCommands,
    },
    /// List uploaded runtimes on the Connect platform
    Runtimes {
        #[command(subcommand)]
        command: ConnectRuntimesCommands,
    },
    /// Manage claim tokens
    ClaimTokens {
        #[command(subcommand)]
        command: ConnectClaimTokensCommands,
    },
    /// Upload current runtime build to the Connect platform
    Upload {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Runtime name to upload
        runtime: String,
        /// Human-readable version for this upload (e.g. v0.0.2-dev)
        #[arg(long)]
        version: String,
        /// Description for the upload
        #[arg(long)]
        description: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Path to pre-built tarball or artifact directory (skips export from Docker volume)
        #[arg(long)]
        file: Option<String>,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Publish the runtime immediately after upload (draft → published)
        #[arg(long)]
        publish: bool,
        /// Deploy after upload: cohort ID to target
        #[arg(long)]
        deploy_cohort: Option<String>,
        /// Deploy after upload: deployment name (auto-generated if omitted)
        #[arg(long)]
        deploy_name: Option<String>,
        /// Deploy after upload: filter by tags (repeatable)
        #[arg(long)]
        deploy_tag: Vec<String>,
        /// Deploy after upload: activate immediately (skip draft)
        #[arg(long)]
        deploy_activate: bool,
        /// Output format (human prose or NDJSON event stream)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Deploy a runtime to a cohort
    Deploy {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Runtime ID (skip interactive prompt)
        #[arg(long)]
        runtime: Option<String>,
        /// Cohort ID (skip interactive prompt)
        #[arg(long)]
        cohort: Option<String>,
        /// Deployment name (auto-generated if omitted)
        #[arg(long)]
        name: Option<String>,
        /// Description for the deployment
        #[arg(long)]
        description: Option<String>,
        /// Filter by tags — only deploy to devices with these tags (repeatable)
        #[arg(long, short = 't')]
        tag: Vec<String>,
        /// Activate immediately (skip draft status)
        #[arg(long)]
        activate: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or NDJSON event stream)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Retrieve the Connect server's TUF signing public key
    ServerKey {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Manage signing keys registered with the Connect server
    Keys {
        #[command(subcommand)]
        command: ConnectKeysCommands,
    },
    /// Fleet trust posture commands
    Trust {
        #[command(subcommand)]
        command: ConnectTrustCommands,
    },
}

#[derive(Subcommand)]
enum ConnectTrustCommands {
    /// Show fleet trust status for an organization
    Status {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Promote root trust to user control (Level 1 → 2)
    PromoteRoot {
        /// Name of the local root signing key to use
        #[arg(long)]
        key: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Rotate the server signing key
    RotateServerKey {
        /// Name of the local root signing key (required at security level 2)
        #[arg(long)]
        key: Option<String>,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectKeysCommands {
    /// Register a local signing key with the Connect server
    Register {
        /// Key type: content or root
        #[arg(long = "type")]
        key_type: String,
        /// Name of the local signing key (from 'avocado signing-keys list')
        #[arg(long, add = clap_complete::engine::ArgValueCompleter::new(completion::signing_keys))]
        key: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name
        #[arg(long)]
        profile: Option<String>,
    },
    /// Approve a staged delegate key (admin only)
    Approve {
        /// Key ID of the staged key to approve
        keyid: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name
        #[arg(long)]
        profile: Option<String>,
    },
    /// List delegate keys registered with the server
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Filter by key type: content or root
        #[arg(long = "type")]
        key_type: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name
        #[arg(long)]
        profile: Option<String>,
    },
    /// Discard a staged delegate key
    Retire {
        /// Key ID of the staged key to discard
        keyid: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectOrgsCommands {
    /// List organizations you belong to
    List {
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
}

#[derive(Subcommand)]
enum ConnectExtCommands {
    /// Build-once publish a packaged extension RPM to the feed (super-admin)
    Publish {
        /// Path to the extension RPM (from `avocado ext package`)
        rpm: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Extension name (default: parsed from the RPM filename)
        #[arg(long)]
        name: Option<String>,
        /// Extension version (default: parsed from the RPM filename)
        #[arg(long)]
        version: Option<String>,
        /// Extension release (default: parsed, else r0)
        #[arg(long)]
        release: Option<String>,
        /// Extension arch (default: parsed, else noarch)
        #[arg(long)]
        arch: Option<String>,
        /// Override the target machines (comma-separated). Defaults to the project's
        /// `supported_targets` from avocado.yaml; only pass this to override that.
        #[arg(long)]
        targets: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Show the status of a published extension version
    Status {
        /// Version id
        id: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// List published extension versions
    List {
        /// Filter by package name
        #[arg(long)]
        name: Option<String>,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectProjectsCommands {
    /// List projects in an organization
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Create a new project
    Create {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project name
        #[arg(long)]
        name: String,
        /// Project description
        #[arg(long)]
        description: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Delete a project
    Delete {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID to delete
        #[arg(long)]
        id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectDevicesCommands {
    /// List devices in an organization
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Create a new device
    Create {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Device name
        #[arg(long)]
        name: String,
        /// Device identifier (must be unique per org)
        #[arg(long)]
        identifier: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a device
    Delete {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Device ID to delete
        #[arg(long)]
        id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Manage admin-approved device reclaim requests
    Reclaim {
        #[command(subcommand)]
        command: ConnectDeviceReclaimCommands,
    },
}

/// Status filter accepted by the reclaim list / count endpoints.
///
/// Variants are validated at clap parse time (clearer help text, fail-fast
/// on bad input) rather than via a runtime check inside the command. The
/// `as_str` mapping is the over-the-wire form sent to the server.
#[derive(Clone, Copy, Debug, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum ReclaimStatusFilter {
    Pending,
    Approved,
    Completed,
    Denied,
    Expired,
    All,
}

impl ReclaimStatusFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Completed => "completed",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::All => "all",
        }
    }
}

/// Validate `--device-id` as a UUID at clap parse time.
///
/// Without this, malformed input would silently flow into URL string
/// construction (potential query-param injection) and only fail later
/// at the server's 422. Failing at the CLI boundary gives clearer UX
/// and removes the URL-injection vector by construction. Returns the
/// canonical hyphenated form so the URL builder downstream can rely on
/// a UUID-shaped string.
fn parse_uuid(s: &str) -> Result<String, String> {
    Uuid::parse_str(s)
        .map(|u| u.hyphenated().to_string())
        .map_err(|e| format!("'{s}' is not a valid UUID: {e}"))
}

#[derive(Subcommand)]
enum ConnectDeviceReclaimCommands {
    /// List reclaim requests (defaults to pending)
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Filter by status
        #[arg(long, value_enum, default_value_t = ReclaimStatusFilter::Pending)]
        status: ReclaimStatusFilter,
        /// Filter to a single device by id. Returns at most one row when combined
        /// with --status pending (the partial unique index allows one pending
        /// reclaim per device).
        #[arg(long = "device-id", value_parser = parse_uuid)]
        device_id: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Approve a pending reclaim request
    Approve {
        /// Reclaim request ID
        id: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Deny a pending reclaim request
    Deny {
        /// Reclaim request ID
        id: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Reason for denial (max 1024 chars). If omitted, prompts interactively. Pass --reason "" to skip.
        #[arg(long)]
        reason: Option<String>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a denied reclaim request (recovery for typo'd denies)
    Delete {
        /// Reclaim request ID
        id: String,
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectCohortsCommands {
    /// List cohorts in a project
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Create a new cohort
    Create {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Cohort name
        #[arg(long)]
        name: String,
        /// Cohort description
        #[arg(long)]
        description: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a cohort
    Delete {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Cohort ID to delete
        #[arg(long)]
        id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectRuntimesCommands {
    /// List runtimes uploaded to the Connect platform
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (or set connect.project in avocado.yaml)
        #[arg(long)]
        project: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format (human prose or single JSON object)
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
}

#[derive(Subcommand)]
enum ConnectClaimTokensCommands {
    /// List claim tokens in an organization
    List {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Create a new claim token
    Create {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Project ID (skip interactive prompt)
        #[arg(long)]
        project: Option<String>,
        /// Cohort ID (skip interactive prompt)
        #[arg(long)]
        cohort: Option<String>,
        /// Token name
        #[arg(long)]
        name: String,
        /// Tags to associate with devices claimed using this token (repeatable)
        #[arg(long, short = 't')]
        tag: Vec<String>,
        /// Maximum number of times this token can be used
        #[arg(long)]
        max_uses: Option<i64>,
        /// Disable expiration (default: expires in 24h)
        #[arg(long)]
        no_expiration: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Delete a claim token
    Delete {
        /// Organization ID (or set connect.org in avocado.yaml)
        #[arg(long)]
        org: Option<String>,
        /// Claim token ID to delete
        #[arg(long)]
        id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show the parsed avocado.yaml in a stable JSON or YAML-ish summary.
    Show {
        /// Path to avocado.yaml (defaults to ./avocado.yaml)
        #[arg(short = 'c', long, default_value = "./avocado.yaml")]
        config: String,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
        /// Include nested detail (extensions, packages, SDK summary,
        /// runtime↔extension cross-references) under a `detail` key.
        /// Default output is unchanged when this flag is absent so
        /// existing consumers keep working byte-for-byte.
        #[arg(long)]
        detail: bool,
    },
}

#[derive(Subcommand)]
enum ConnectAuthCommands {
    /// Login to the Connect platform
    Login {
        /// API URL (defaults to https://connect.peridio.com or AVOCADO_CONNECT_URL env var)
        #[arg(long)]
        url: Option<String>,
        /// Profile name (defaults to "default")
        #[arg(long)]
        profile: Option<String>,
        /// Use an existing API token instead of browser login
        #[arg(long)]
        token: Option<String>,
        /// Organization id (UUID) to scope the new token to. Required for
        /// non-interactive multi-org logins; ignored when --token is set.
        #[arg(long)]
        org: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Logout from the Connect platform
    Logout {
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Show current auth status
    Status {
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
}

#[derive(Subcommand)]
enum SigningKeysCommands {
    /// Create a new signing key or register an external PKCS#11 key
    Create {
        /// Name for the key (defaults to key ID if not provided)
        name: Option<String>,
        /// PKCS#11 URI for hardware-backed keys (e.g., 'pkcs11:token=YubiKey;object=signing-key')
        #[arg(long)]
        uri: Option<String>,
        /// Hardware device type (tpm, yubikey, or auto-detect)
        #[arg(long, value_name = "DEVICE")]
        pkcs11_device: Option<String>,
        /// PKCS#11 token label (e.g., 'avocado', 'YubiKey PIV'). If not provided, uses the first available token.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Label of existing key to reference in the device
        #[arg(long, value_name = "LABEL")]
        key_label: Option<String>,
        /// Generate a new key in the device
        #[arg(long)]
        generate: bool,
        /// Authentication method for PKCS#11 device (none, prompt, env)
        #[arg(long, default_value = "prompt", value_name = "METHOD")]
        auth: String,
    },
    /// List all registered signing keys
    List,
    /// Remove a signing key
    Remove {
        /// Name or key ID of the key to remove
        #[arg(add = clap_complete::engine::ArgValueCompleter::new(completion::signing_keys))]
        name: String,
        /// Delete hardware key from device (requires confirmation)
        #[arg(long)]
        delete: bool,
    },
}

#[derive(Subcommand)]
enum SdkCommands {
    /// Create and run an SDK container
    Run {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Assign a name to the container
        #[arg(long)]
        name: Option<String>,
        /// Run container in background and print container ID
        #[arg(short, long)]
        detach: bool,
        /// Automatically remove the container when it exits (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        rm: bool,
        /// Drop into interactive shell in container
        #[arg(short, long)]
        interactive: bool,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Source the avocado SDK environment before running command
        #[arg(short = 'E', long)]
        env: bool,
        /// Mount extension sysroot and change working directory to it
        #[arg(short = 'e', long)]
        extension: Option<String>,
        /// Mount runtime sysroot and change working directory to it
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Command and arguments to run in container
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Skip SDK bootstrap initialization and go directly to container prompt
        #[arg(long)]
        no_bootstrap: bool,
    },
    /// List SDK dependencies
    Deps {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Run compile scripts
    Compile {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Specific compile sections to run
        sections: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Run DNF commands in the SDK context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Install dependencies into the SDK
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove the SDK directory
    /// Clean the SDK or run clean scripts for specific compile sections
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Specific compile sections to clean (runs their clean scripts)
        sections: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Package a compiled SDK section into an RPM
    Package {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Compile section to package (must have a 'package' block in config)
        section: String,
        /// Output directory on host for the built RPM(s)
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum RuntimeCommands {
    /// Install dependencies into runtime installroots
    Install {
        /// Runtime name (if not provided, installs for all runtimes)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build a runtime
    Build {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Provision a runtime
    Provision {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Provision profile to use
        #[arg(long = "profile")]
        provision_profile: Option<String>,
        /// Environment variables to pass to the provision process
        #[arg(long = "env", num_args = 1, action = clap::ArgAction::Append)]
        env: Option<Vec<String>>,
        /// Output path relative to src_dir for provisioning artifacts
        #[arg(long = "out")]
        out: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List runtime names
    List {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// List dependencies for a runtime
    Deps {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run DNF commands in a runtime's context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the runtime to operate on
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Clean runtime installroot directory
    Clean {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Deploy a runtime to a device
    Deploy {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Device to deploy to as [user@]host[:port] (e.g. root@192.168.1.100:2222)
        #[arg(short = 'd', long = "device", required = true)]
        device: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Sign TUF metadata via the Connect platform instead of locally.
        /// Use this when deploying to a device that has received a Connect OTA update.
        /// Requires a local signing key configured for the runtime (Level 2:
        /// signing.key + avocado connect trust promote-root --key <KEY>); without one no
        /// root.json is baked and the deploy fails during Phase 1 hash collection.
        #[arg(long)]
        connect_sign: bool,
        /// Output format. JSON skips TUI rendering and emits NDJSON events.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
    /// Sign runtime images
    Sign {
        /// Runtime name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name (deprecated, use positional argument)
        #[arg(short = 'r', long = "runtime", hide = true)]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

/// Validate that a runtime exists in the configuration if provided.
/// This provides early validation with a helpful error message before command execution.
fn validate_runtime_if_provided(config_path: &str, runtime: Option<&String>) -> Result<()> {
    if let Some(runtime_name) = runtime {
        Config::validate_runtime_exists(config_path, runtime_name)
            .with_context(|| format!("Invalid runtime specified: '{runtime_name}'"))?;
    }
    Ok(())
}

/// Validate that a runtime exists in the configuration (for required runtime arguments).
/// This provides early validation with a helpful error message before command execution.
fn validate_runtime_required(config_path: &str, runtime: &str) -> Result<()> {
    Config::validate_runtime_exists(config_path, runtime)
        .with_context(|| format!("Invalid runtime specified: '{runtime}'"))
}

/// Resolve a required runtime name from CLI arg, env var, config default, or
/// sole-runtime auto-resolution.
///
/// Loads `Config` from the given path so the resolver can inspect
/// `default_runtime` and `runtimes:`. When the resolved name comes from the
/// CLI/env/config (not auto-resolution), validates that it matches a defined
/// runtime — same check `validate_runtime_required` performed previously.
fn resolve_runtime_at_path(config_path: &str, cli_runtime: Option<&str>) -> Result<String> {
    let config = Config::load(config_path)
        .with_context(|| format!("Failed to load config: {config_path}"))?;
    crate::utils::runtime::validate_and_log_runtime(cli_runtime, &config)
}

/// Parse environment variable arguments in the format "KEY=VALUE" into a HashMap
fn parse_env_vars(env_args: Option<&Vec<String>>) -> Option<HashMap<String, String>> {
    env_args.map(|args| {
        args.iter()
            .filter_map(|arg| {
                let parts: Vec<&str> = arg.splitn(2, '=').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    eprintln!("[WARNING] Invalid environment variable format: '{arg}'. Expected 'KEY=VALUE'.");
                    None
                }
            })
            .collect()
    })
}

/// Enable JSON output mode for the lifetime of the returned guard and
/// emit a `start` event so the desktop app can attach to the run.
///
/// Returns `None` in human mode (no guard, no event). The caller binds
/// the result to a `let _x = ...` so it lives to the end of the
/// dispatch arm; on drop, the guard clears the global flag.
///
/// Note: this does NOT emit a terminal `complete` / `error` event —
/// the desktop app derives success/failure from the subprocess exit
/// status. Emitting a final event would require wrapping each command
/// in a try-finally pattern across the existing flat dispatch; using
/// exit status is sufficient for the UI today and we can layer in a
/// completion event later if needed.
fn run_with_json_lifecycle(
    output: OutputFormat,
    action: &'static str,
    target: Option<&str>,
    runtime: Option<&str>,
) -> Option<crate::utils::output_format::JsonOutputGuard> {
    if !output.is_json() {
        return None;
    }
    let guard = crate::utils::output_format::JsonOutputGuard::enable();
    crate::utils::output_format::emit_json_event(&serde_json::json!({
        "event": "start",
        "action": action,
        "target": target,
        "runtime": runtime,
    }));
    Some(guard)
}

/// Combine provision profile and env vars into a single HashMap
fn build_env_vars(
    provision_profile: Option<&String>,
    env_args: Option<&Vec<String>>,
) -> Option<HashMap<String, String>> {
    let mut env_vars = parse_env_vars(env_args).unwrap_or_default();

    if let Some(profile) = provision_profile {
        env_vars.insert("AVOCADO_PROVISION_PROFILE".to_string(), profile.clone());
    }

    if env_vars.is_empty() {
        None
    } else {
        Some(env_vars)
    }
}

/// Whether the subcommand spawns docker (and therefore wants VM routing on
/// non-Linux hosts). Conservative allowlist: only commands that we know
/// shell out to docker are included. `Init` doesn't. `Vm` operates on the
/// VM itself. `Upgrade`/`Save`/`Load` etc. don't touch docker.
/// Walk the clap command tree and attach dynamic completers to args by id.
///
/// We match on the field name (which is the arg id for clap-derive), so
/// every `extension: Option<String>` field in every subcommand automatically
/// gets the same completer with no per-site `#[arg(add = ...)]` attribute.
/// Args whose id is something else (`name`, `key`, etc.) need explicit wiring
/// at the call site — done where it's worth doing.
fn attach_completers(mut cmd: clap::Command) -> clap::Command {
    use clap_complete::engine::ArgValueCompleter;
    cmd = cmd.mut_args(|a| match a.get_id().as_str() {
        "extension" => a.add(ArgValueCompleter::new(completion::extensions)),
        "runtime" => a.add(ArgValueCompleter::new(completion::runtimes)),
        "target" => a.add(ArgValueCompleter::new(completion::targets)),
        _ => a,
    });
    let subcommand_names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in subcommand_names {
        cmd = cmd.mut_subcommand(name, attach_completers);
    }
    cmd
}

fn cli_command_with_completers() -> clap::Command {
    attach_completers(Cli::command())
}

fn needs_vm_routing(cmd: &Commands) -> bool {
    matches!(
        cmd,
        Commands::Build { .. }
            | Commands::Provision { .. }
            | Commands::Install { .. }
            | Commands::Uninstall { .. }
            | Commands::Sdk { .. }
            | Commands::Ext { .. }
            | Commands::Rootfs { .. }
            | Commands::Initramfs { .. }
            | Commands::Kernel { .. }
            | Commands::Runtime { .. }
            | Commands::Hitl { .. }
            | Commands::Prune { .. }
            | Commands::Clean { .. }
            | Commands::Sign { .. }
            | Commands::Deploy { .. }
            // `connect upload` runs a prerequisite stamp check + in-container
            // artifact discovery, both of which need Docker. The other connect
            // subcommands are pure Connect-API calls, so they stay off the VM
            // (routing can auto-start it) — gate on the subcommand, not the group.
            | Commands::Connect {
                command: ConnectCommands::Upload { .. }
            }
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Intercept tab-completion requests before any other work. The
    // `COMPLETE` env var is set by the wrapper script that
    // `avocado completion <shell>` installs; when present, this call
    // writes candidates to stdout and exits. When absent, it returns
    // immediately and we fall through to normal CLI dispatch.
    clap_complete::CompleteEnv::with_factory(cli_command_with_completers).complete();

    let cli = Cli::parse();

    // Set AVOCADO_NO_TUI env var so all should_use_tui() calls respect --no-tui
    if cli.no_tui {
        std::env::set_var("AVOCADO_NO_TUI", "1");
    }

    let skip_update_check = matches!(
        cli.command,
        Commands::Upgrade { .. } | Commands::Completion { .. }
    );
    let update_handle = if !skip_update_check {
        Some(tokio::spawn(utils::update_check::check_for_update()))
    } else {
        None
    };

    // On macOS/Windows hosts, transparently route docker through the local
    // avocado-vm helper if running (or auto-startable). Skipped for commands
    // that don't talk to docker (Init, Upgrade, Vm itself) or when the user
    // opted out via --no-vm-auto-start / AVOCADO_VM_AUTO_START=0 / --runs-on.
    // The mode return value used to gate the in-process USB matchmaker;
    // since USB passthrough now flows through Avocado.app's IOUSBHost +
    // vhci_hcd bridge instead, we only care about the side-effect of
    // setting DOCKER_HOST for downstream `docker` invocations.
    if needs_vm_routing(&cli.command) {
        if let Err(e) =
            utils::vm::route::ensure_routed_for_process(cli.no_vm_auto_start, cli.runs_on.is_some())
                .await
        {
            utils::output::print_warning(
                &format!("VM routing unavailable: {e:#}; falling back to local docker."),
                utils::output::OutputLevel::Normal,
            );
        }
    }

    let result = match cli.command {
        Commands::Init {
            directory,
            target,
            reference,
            reference_branch,
            reference_commit,
            reference_repo,
            name,
            output,
        } => {
            let init_cmd = InitCommand::new(
                target.or(cli.target),
                directory,
                reference,
                reference_branch,
                reference_commit,
                reference_repo,
                name,
                output,
            );
            init_cmd.execute().await?;
            Ok(())
        }
        Commands::Clean {
            directory,
            skip_volumes,
            container_tool,
            verbose,
            stamps,
            config,
            target,
            force,
            unlock,
        } => {
            let clean_cmd =
                CleanCommand::new(directory, !skip_volumes, Some(container_tool), verbose)
                    .with_stamps(stamps)
                    .with_config_path(config)
                    .with_target(target.or(cli.target.clone()))
                    .with_force(force)
                    .with_unlock(unlock);
            clean_cmd.execute().await?;
            Ok(())
        }
        Commands::Install {
            packages,
            extension,
            config,
            verbose,
            force,
            runtime,
            sdk,
            no_save,
            target,
            container_args,
            dnf_args,
            output,
        } => {
            let _json_guard =
                run_with_json_lifecycle(output, "install", target.as_deref(), runtime.as_deref());
            // JSON output implies no human at the keyboard — auto-enable
            // --force so dnf gets -y and container starts without -it.
            // Without this, `docker run -it` fails ("cannot attach stdin
            // to a TTY-enabled container") and dnf hangs waiting for
            // confirmation.
            let force = force || output.is_json();
            if packages.is_empty() {
                // No packages specified: sync all from config (original behavior)
                validate_runtime_if_provided(&config, runtime.as_ref())?;

                let install_cmd = InstallCommand::new(
                    config,
                    verbose,
                    force,
                    runtime,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
            } else {
                // Packages specified: add to config and install into specified scope
                use commands::install::PackageAddCommand;
                let scope_count = extension.is_some() as u8 + runtime.is_some() as u8 + sdk as u8;
                if scope_count == 0 {
                    anyhow::bail!(
                        "When installing packages, specify a scope: \
                         -e/--extension <name>, -r/--runtime <name>, or --sdk"
                    );
                }
                if scope_count > 1 {
                    anyhow::bail!(
                        "Specify only one scope: \
                         -e/--extension, -r/--runtime, or --sdk"
                    );
                }

                let add_cmd = PackageAddCommand {
                    packages,
                    extension,
                    runtime,
                    sdk,
                    config_path: config,
                    verbose,
                    force,
                    no_save,
                    target: target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                    no_stamps: cli.no_stamps,
                    runs_on: cli.runs_on.clone(),
                    nfs_port: cli.nfs_port,
                    sdk_arch: cli.sdk_arch.clone(),
                };
                add_cmd.execute().await?;
            }
            Ok(())
        }
        Commands::Uninstall {
            packages,
            extension,
            runtime,
            sdk,
            config,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
        } => {
            use commands::install::PackageRemoveCommand;
            let scope_count = extension.is_some() as u8 + runtime.is_some() as u8 + sdk as u8;
            if scope_count == 0 {
                anyhow::bail!(
                    "When uninstalling packages, specify a scope: \
                     -e/--extension <name>, -r/--runtime <name>, or --sdk"
                );
            }
            if scope_count > 1 {
                anyhow::bail!(
                    "Specify only one scope: \
                     -e/--extension, -r/--runtime, or --sdk"
                );
            }

            let remove_cmd = PackageRemoveCommand {
                packages,
                extension,
                runtime,
                sdk,
                config_path: config,
                verbose,
                force,
                target: target.or(cli.target.clone()),
                container_args,
                dnf_args,
                no_stamps: cli.no_stamps,
                runs_on: cli.runs_on.clone(),
                nfs_port: cli.nfs_port,
                sdk_arch: cli.sdk_arch.clone(),
            };
            remove_cmd.execute().await?;
            Ok(())
        }
        Commands::Build {
            config,
            verbose,
            runtime,
            extension,
            target,
            container_args,
            dnf_args,
            output,
        } => {
            let _json_guard =
                run_with_json_lifecycle(output, "build", target.as_deref(), runtime.as_deref());
            // Validate runtime exists if provided
            validate_runtime_if_provided(&config, runtime.as_ref())?;

            let build_cmd = BuildCommand::new(
                config,
                verbose,
                runtime,
                extension,
                target.or(cli.target.clone()),
                container_args,
                dnf_args,
            )
            .with_no_stamps(cli.no_stamps)
            .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
            .with_sdk_arch(cli.sdk_arch.clone());
            build_cmd.execute().await?;
            Ok(())
        }
        Commands::Fetch {
            config,
            verbose,
            extension,
            runtime,
            target,
            container_args,
            dnf_args,
        } => {
            // Validate runtime exists if provided
            validate_runtime_if_provided(&config, runtime.as_ref())?;

            let fetch_cmd = FetchCommand::new(
                config,
                verbose,
                extension,
                runtime,
                target.or(cli.target),
                container_args,
                dnf_args,
            )
            .with_sdk_arch(cli.sdk_arch.clone());
            fetch_cmd.execute().await?;
            Ok(())
        }
        Commands::Upgrade { version } => {
            let cmd = UpgradeCommand { version };
            cmd.run().await?;
            Ok(())
        }
        Commands::Completion { shell } => {
            // Emit the thin registration wrapper for the requested shell.
            // The wrapper re-invokes `avocado` with `COMPLETE=<shell>` on every
            // TAB press; the call at the top of main() intercepts and serves
            // candidates. This keeps the installed shell script tiny and lets
            // dynamic completers stay live without a regeneration step.
            use clap_complete::env::Shells;
            let shell_name = shell.to_string();
            let shells = Shells::builtins();
            let Some(completer) = shells.completer(&shell_name) else {
                anyhow::bail!("unsupported shell: {shell_name}");
            };
            completer
                .write_registration(
                    "COMPLETE",
                    "avocado",
                    "avocado",
                    "avocado",
                    &mut std::io::stdout(),
                )
                .context("failed to write completion registration")?;
            Ok(())
        }
        Commands::Provision {
            name,
            config,
            verbose,
            force,
            runtime,
            target,
            provision_profile,
            env,
            out,
            container_args,
            dnf_args,
            list,
            output,
        } => {
            // `--list` short-circuits the run: instead of provisioning,
            // we read the stone manifest out of the installed SDK
            // volume and return the profiles available for the
            // resolved target. Same `--config` / `--target` /
            // `--output` flags so callers stay uniform.
            if list {
                let cmd = crate::commands::profiles::ProfilesListCommand {
                    config_path: config,
                    target: target.or(cli.target.clone()),
                    output,
                };
                cmd.execute().await?;
                return Ok(());
            }
            let _json_guard = run_with_json_lifecycle(
                output,
                "provision",
                target.as_deref(),
                name.as_deref().or(runtime.as_deref()),
            );
            // JSON output implies no human at the keyboard — auto-enable
            // --force (see `Install` arm above for rationale).
            let force = force || output.is_json();
            let runtime = resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

            let provision_cmd =
                ProvisionCommand::new(crate::commands::provision::ProvisionConfig {
                    runtime,
                    config_path: config,
                    verbose,
                    force,
                    target: target.or(cli.target),
                    provision_profile: provision_profile.clone(),
                    env_vars: build_env_vars(provision_profile.as_ref(), env.as_ref()),
                    out,
                    container_args,
                    dnf_args,
                    no_stamps: cli.no_stamps,
                    runs_on: cli.runs_on.clone(),
                    nfs_port: cli.nfs_port,
                    sdk_arch: cli.sdk_arch.clone(),
                });
            provision_cmd.execute().await?;
            Ok(())
        }
        Commands::Deploy {
            name,
            config,
            verbose,
            runtime,
            target,
            device,
            container_args,
            dnf_args,
            connect_sign,
            output,
        } => {
            let runtime = resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;
            let _json_guard =
                run_with_json_lifecycle(output, "deploy", target.as_deref(), Some(&runtime));

            let deploy_cmd = RuntimeDeployCommand::new(
                runtime,
                config,
                verbose,
                target.or(cli.target.clone()),
                device,
                container_args,
                dnf_args,
            )
            .with_no_stamps(cli.no_stamps)
            .with_connect_sign(connect_sign)
            .with_sdk_arch(cli.sdk_arch.clone());
            deploy_cmd.execute().await?;
            Ok(())
        }
        Commands::SigningKeys { command } => match command {
            SigningKeysCommands::Create {
                name,
                uri,
                pkcs11_device,
                token,
                key_label,
                generate,
                auth,
            } => {
                let cmd = SigningKeysCreateCommand::new(
                    name,
                    uri,
                    pkcs11_device,
                    token,
                    key_label,
                    generate,
                    auth,
                );
                cmd.execute()?;
                Ok(())
            }
            SigningKeysCommands::List => {
                let cmd = SigningKeysListCommand::new();
                cmd.execute()?;
                Ok(())
            }
            SigningKeysCommands::Remove { name, delete } => {
                let cmd = SigningKeysRemoveCommand::new(name, delete);
                cmd.execute()?;
                Ok(())
            }
        },
        Commands::Sign {
            name,
            config,
            verbose,
            runtime,
            target,
            container_args,
            dnf_args,
        } => {
            let runtime = name.or(runtime);

            // Validate runtime exists if provided
            validate_runtime_if_provided(&config, runtime.as_ref())?;

            let sign_cmd = SignCommand::new(
                config,
                verbose,
                runtime,
                target.or(cli.target),
                container_args,
                dnf_args,
            );
            sign_cmd.execute().await?;
            Ok(())
        }
        Commands::Prune {
            container_tool,
            verbose,
            dry_run,
        } => {
            let prune_cmd = PruneCommand::new(Some(container_tool), verbose, dry_run);
            prune_cmd.execute().await?;
            Ok(())
        }
        Commands::Save {
            output,
            config,
            verbose,
            target,
            container_tool,
            include_src,
        } => {
            let save_cmd = SaveCommand::new(
                output,
                config,
                target.or(cli.target),
                verbose,
                container_tool,
                include_src,
            );
            save_cmd.execute().await?;
            Ok(())
        }
        Commands::Load {
            input,
            config,
            verbose,
            container_tool,
            force,
        } => {
            let load_cmd = LoadCommand::new(input, config, verbose, container_tool, force);
            load_cmd.execute().await?;
            Ok(())
        }
        Commands::Unlock {
            config,
            verbose,
            target,
            extension,
            runtime,
            sdk,
            rootfs,
            initramfs,
        } => {
            // Validate runtime exists if provided
            validate_runtime_if_provided(&config, runtime.as_ref())?;

            let unlock_cmd = UnlockCommand::new(
                config,
                verbose,
                target.or(cli.target),
                extension,
                runtime,
                sdk,
                rootfs,
                initramfs,
            );
            unlock_cmd.execute()?;
            Ok(())
        }
        Commands::Update {
            config,
            verbose,
            target,
        } => {
            let update_cmd = UpdateCommand::new(config, target.or(cli.target), verbose);
            update_cmd.execute().await?;
            Ok(())
        }
        Commands::Runtime { command } => match command {
            RuntimeCommands::Install {
                name,
                runtime,
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let runtime = name.or(runtime);
                // Validate runtime exists if provided
                validate_runtime_if_provided(&config, runtime.as_ref())?;

                let mut install_cmd = RuntimeInstallCommand::new(
                    runtime,
                    config,
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Build {
                name,
                runtime,
                config,
                verbose,
                force: _,
                target,
                container_args,
                dnf_args,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

                let mut build_cmd = RuntimeBuildCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                build_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Provision {
                name,
                runtime,
                config,
                verbose,
                force,
                target,
                provision_profile,
                env,
                out,
                container_args,
                dnf_args,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

                let mut provision_cmd = RuntimeProvisionCommand::new(
                    crate::commands::runtime::provision::RuntimeProvisionConfig {
                        runtime_name: runtime,
                        config_path: config,
                        verbose,
                        force,
                        target: target.or(cli.target),
                        provision_profile: provision_profile.clone(),
                        env_vars: build_env_vars(provision_profile.as_ref(), env.as_ref()),
                        out,
                        container_args,
                        dnf_args,
                        state_file: None, // Resolved from config during execution
                        no_stamps: cli.no_stamps,
                        runs_on: cli.runs_on.clone(),
                        nfs_port: cli.nfs_port,
                        sdk_arch: cli.sdk_arch.clone(),
                    },
                );
                provision_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::List { config, target: _ } => {
                let list_cmd = RuntimeListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Deps {
                name,
                config,
                runtime,
                target: _,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

                let deps_cmd = RuntimeDepsCommand::new(config, runtime);
                deps_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Dnf {
                config,
                verbose,
                runtime,
                target,
                command,
                container_args,
                dnf_args,
            } => {
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

                let dnf_cmd = RuntimeDnfCommand::new(
                    config,
                    runtime,
                    command,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                dnf_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Clean {
                name,
                config,
                verbose,
                runtime,
                target,
                container_args,
                dnf_args,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

                let clean_cmd = RuntimeCleanCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                clean_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Deploy {
                name,
                config,
                verbose,
                runtime,
                target,
                device,
                container_args,
                dnf_args,
                connect_sign,
                output,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;
                let _json_guard =
                    run_with_json_lifecycle(output, "deploy", target.as_deref(), Some(&runtime));

                let deploy_cmd = RuntimeDeployCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    device,
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_connect_sign(connect_sign)
                .with_sdk_arch(cli.sdk_arch.clone());
                deploy_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Sign {
                name,
                config,
                verbose,
                runtime,
                target,
                container_args,
                dnf_args,
            } => {
                let runtime =
                    resolve_runtime_at_path(&config, name.as_deref().or(runtime.as_deref()))?;

                let sign_cmd = RuntimeSignCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone());
                sign_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Ext { command } => match command {
            ExtCommands::Install {
                name,
                config,
                verbose,
                force,
                extension,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = ExtInstallCommand::new(
                    name.or(extension),
                    config,
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Fetch {
                name,
                config,
                verbose,
                force,
                extension,
                target,
                container_args,
            } => {
                let fetch_cmd = ExtFetchCommand::new(
                    config,
                    name.or(extension),
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                fetch_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Build {
                name,
                extension,
                config,
                verbose,
                target,
                runtime,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                // Resolve runtime when caller asked for it (or env / default
                // runtime would resolve). Fall through to None for projects
                // without runtimes so legacy per-target builds keep working.
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let build_cmd = ExtBuildCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_runtime(resolved_runtime);
                build_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Checkout {
                name,
                config,
                verbose,
                extension,
                target,
                runtime,
                ext_path,
                src_path,
                container_tool,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let checkout_cmd = ExtCheckoutCommand::new(
                    extension,
                    ext_path,
                    src_path,
                    config,
                    verbose,
                    container_tool,
                    target.or(cli.target),
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_runtime(resolved_runtime);
                checkout_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::List { config, target: _ } => {
                let list_cmd = ExtListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Deps {
                name,
                config,
                extension,
                target,
            } => {
                let deps_cmd =
                    ExtDepsCommand::new(config, name.or(extension), target.or(cli.target));
                deps_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Dnf {
                config,
                verbose,
                extension,
                target,
                runtime,
                command,
                container_args,
                dnf_args,
            } => {
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let dnf_cmd = ExtDnfCommand::new(
                    config,
                    extension,
                    command,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_runtime(resolved_runtime);
                dnf_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Clean {
                name,
                extension,
                config,
                verbose,
                target,
                runtime,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let clean_cmd = ExtCleanCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_runtime(resolved_runtime);
                clean_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Image {
                name,
                extension,
                config,
                verbose,
                target,
                runtime,
                out_dir,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let image_cmd = ExtImageCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_output_dir(out_dir)
                .with_runtime(resolved_runtime);
                image_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Package {
                name,
                extension,
                target,
                runtime,
                config,
                verbose,
                output_dir,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                let resolved_runtime = resolve_runtime_at_path(&config, runtime.as_deref()).ok();
                let package_cmd = ExtPackageCommand::new(
                    config,
                    extension,
                    target,
                    output_dir,
                    verbose,
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_runtime(resolved_runtime);
                package_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Rootfs { command } => match command {
            RootfsCommands::Install {
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = RootfsInstallCommand::new(
                    config,
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
                Ok(())
            }
            RootfsCommands::Image {
                config,
                verbose,
                target,
                out_dir,
                container_args,
                dnf_args,
            } => {
                let image_cmd = RootfsImageCommand::new(
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_output_dir(out_dir);
                image_cmd.execute().await?;
                Ok(())
            }
            RootfsCommands::Clean {
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = RootfsCleanCommand::new(
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                clean_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Initramfs { command } => match command {
            InitramfsCommands::Install {
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = InitramfsInstallCommand::new(
                    config,
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
                Ok(())
            }
            InitramfsCommands::Image {
                config,
                verbose,
                target,
                out_dir,
                container_args,
                dnf_args,
            } => {
                let image_cmd = InitramfsImageCommand::new(
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_output_dir(out_dir);
                image_cmd.execute().await?;
                Ok(())
            }
            InitramfsCommands::Clean {
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = InitramfsCleanCommand::new(
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                clean_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Kernel { command } => match command {
            KernelCommands::Image {
                config,
                verbose,
                target,
                out_dir,
                container_args,
            } => {
                let image_cmd = KernelImageCommand::new(
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                )
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone())
                .with_output_dir(out_dir);
                image_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Config { command } => match command {
            ConfigCommands::Show {
                config,
                output,
                detail,
            } => {
                let cmd = ConfigShowCommand {
                    config_path: config,
                    output,
                    detail,
                };
                cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Vm { command } => match command {
            VmCommands::Start {
                vm_source,
                memory_mib,
                cpus,
                ssh_port,
                cmdline_extra,
                workspace,
                var_size,
                dns,
                watch,
                foreground,
            } => {
                let cmd = commands::vm::start::StartCommand {
                    vm_source,
                    memory_mib,
                    cpus,
                    ssh_port,
                    cmdline_extra,
                    workspace,
                    var_size,
                    dns,
                    watch,
                    foreground,
                };
                cmd.execute().await
            }
            VmCommands::Stop { force } => commands::vm::stop::StopCommand { force }.execute().await,
            #[cfg(unix)]
            VmCommands::Supervise {
                user_port,
                internal_port,
                infra_port,
                qmp_socket,
                idle_after_secs,
                pid_file,
                docker_socket,
                docker_socket_internal,
                docker_socket_stream,
                ssh_key,
                known_hosts,
            } => {
                commands::vm::supervise::SuperviseCommand {
                    user_port,
                    internal_port,
                    infra_port,
                    qmp_socket,
                    idle_after_secs,
                    pid_file,
                    docker_socket,
                    docker_socket_internal,
                    docker_socket_stream,
                    ssh_key,
                    known_hosts,
                }
                .execute()
                .await
            }
            VmCommands::Status => commands::vm::status::StatusCommand.execute().await,
            VmCommands::Shell { command } => {
                commands::vm::shell::ShellCommand { command }
                    .execute()
                    .await
            }
            VmCommands::Logs { follow } => {
                commands::vm::logs::LogsCommand { follow }.execute().await
            }
            VmCommands::Rebuild {
                vm_source,
                reset_data,
            } => {
                commands::vm::rebuild::RebuildCommand {
                    vm_source,
                    reset_data,
                }
                .execute()
                .await
            }
            VmCommands::Update {
                channel,
                check,
                yes,
                output,
            } => {
                commands::vm::update::UpdateCommand {
                    channel,
                    check_only: check,
                    assume_yes: yes,
                    output,
                }
                .execute()
                .await
            }
            VmCommands::Reset { yes } => {
                commands::vm::reset::ResetCommand { assume_yes: yes }
                    .execute()
                    .await
            }
            VmCommands::Config { command } => match command {
                VmConfigCommands::Get { key, output } => {
                    commands::vm::config::ConfigGetCommand { key, output }
                        .execute()
                        .await
                }
                VmConfigCommands::Set { key, values } => {
                    commands::vm::config::ConfigSetCommand { key, values }
                        .execute()
                        .await
                }
                VmConfigCommands::Unset { key } => {
                    commands::vm::config::ConfigUnsetCommand { key }
                        .execute()
                        .await
                }
                VmConfigCommands::List { output } => {
                    commands::vm::config::ConfigListCommand { output }
                        .execute()
                        .await
                }
            },
        },
        Commands::Hitl { command } => match command {
            HitlCommands::Server {
                config_path,
                extensions,
                container_args,
                dnf_args,
                target,
                verbose,
                port,
                no_stamps,
            } => {
                let hitl_cmd = HitlServerCommand {
                    config_path,
                    extensions,
                    container_args,
                    dnf_args,
                    target: target.or(cli.target),
                    verbose,
                    port,
                    no_stamps: no_stamps || cli.no_stamps,
                    sdk_arch: cli.sdk_arch.clone(),
                    composed_config: None,
                };
                hitl_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Sdk { command } => match command {
            SdkCommands::Install {
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let mut install_cmd = SdkInstallCommand::new(
                    config,
                    verbose,
                    force,
                    target.or(cli.target.clone()),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                install_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Run {
                config,
                target,
                name,
                detach,
                rm,
                interactive,
                verbose,
                env,
                extension,
                runtime,
                command,
                container_args,
                dnf_args,
                no_bootstrap,
            } => {
                // Validate runtime exists if provided
                validate_runtime_if_provided(&config, runtime.as_ref())?;

                let cmd = if command.is_empty() {
                    None
                } else {
                    Some(command)
                };
                let run_cmd = SdkRunCommand::new(
                    config,
                    name,
                    detach,
                    rm,
                    interactive,
                    verbose,
                    env,
                    extension,
                    runtime,
                    cmd,
                    target,
                    container_args,
                    dnf_args,
                    no_bootstrap,
                )
                .with_runs_on(cli.runs_on.clone(), cli.nfs_port)
                .with_sdk_arch(cli.sdk_arch.clone());
                run_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Deps {
                config,
                target: _,
                container_args: _,
                dnf_args: _,
            } => {
                let deps_cmd = SdkDepsCommand::new(config);
                deps_cmd.execute()?;
                Ok(())
            }
            SdkCommands::Compile {
                config,
                verbose,
                target,
                sections,
                container_args,
                dnf_args,
            } => {
                let compile_cmd = SdkCompileCommand::new(
                    config,
                    verbose,
                    sections,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone());
                compile_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Dnf {
                config,
                verbose,
                target,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = SdkDnfCommand::new(
                    config,
                    verbose,
                    command,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                dnf_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Clean {
                config,
                verbose,
                target,
                sections,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = SdkCleanCommand::new(
                    config,
                    verbose,
                    sections,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_sdk_arch(cli.sdk_arch.clone());
                clean_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Package {
                config,
                verbose,
                target,
                section,
                out_dir,
                container_args,
                dnf_args,
            } => {
                let package_cmd = SdkPackageCommand::new(
                    config,
                    verbose,
                    section,
                    out_dir,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                )
                .with_no_stamps(cli.no_stamps)
                .with_sdk_arch(cli.sdk_arch.clone());
                package_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Connect { command } => match command {
            ConnectCommands::Auth { command } => match command {
                ConnectAuthCommands::Login {
                    url,
                    profile,
                    token,
                    org,
                    output,
                } => {
                    let cmd = ConnectAuthLoginCommand::new(url, profile, token, org, output);
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectAuthCommands::Logout { profile, output } => {
                    let cmd = ConnectAuthLogoutCommand { profile, output };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectAuthCommands::Status { profile, output } => {
                    let cmd = ConnectAuthStatusCommand { profile, output };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Init {
                org,
                project,
                cohort,
                runtime,
                config,
                profile,
                output,
            } => {
                let cmd = ConnectInitCommand {
                    org,
                    project,
                    cohort,
                    runtime,
                    config_path: config,
                    profile,
                    output,
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::Clean {
                runtime,
                config,
                output,
            } => {
                let cmd = ConnectCleanCommand {
                    runtime,
                    config_path: config,
                    output,
                };
                cmd.execute()?;
                Ok(())
            }
            ConnectCommands::Orgs { command } => match command {
                ConnectOrgsCommands::List { profile, output } => {
                    let cmd = ConnectOrgsListCommand { profile, output };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Ext { command } => match command {
                ConnectExtCommands::Publish {
                    rpm,
                    org,
                    name,
                    version,
                    release,
                    arch,
                    targets,
                    config,
                    profile,
                } => {
                    let cmd = commands::connect::ext::ExtPublishCommand {
                        config,
                        org,
                        profile,
                        rpm,
                        name,
                        version,
                        release,
                        arch,
                        targets,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectExtCommands::Status {
                    id,
                    org,
                    config,
                    profile,
                } => {
                    let cmd = commands::connect::ext::ExtStatusCommand {
                        config,
                        org,
                        profile,
                        id,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectExtCommands::List {
                    name,
                    org,
                    config,
                    profile,
                } => {
                    let cmd = commands::connect::ext::ExtListCommand {
                        config,
                        org,
                        profile,
                        name,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Projects { command } => match command {
                ConnectProjectsCommands::List {
                    org,
                    config,
                    profile,
                    output,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectProjectsListCommand {
                        org: resolved_org,
                        profile,
                        output,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectProjectsCommands::Create {
                    org,
                    name,
                    description,
                    config,
                    profile,
                    output,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectProjectsCreateCommand {
                        org: resolved_org,
                        name,
                        description,
                        profile,
                        output,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectProjectsCommands::Delete {
                    org,
                    id,
                    yes,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectProjectsDeleteCommand {
                        org: resolved_org,
                        id,
                        yes,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Devices { command } => match command {
                ConnectDevicesCommands::List {
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectDevicesListCommand {
                        org: resolved_org,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectDevicesCommands::Create {
                    org,
                    name,
                    identifier,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectDevicesCreateCommand {
                        org: resolved_org,
                        name,
                        identifier,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectDevicesCommands::Delete {
                    org,
                    id,
                    yes,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectDevicesDeleteCommand {
                        org: resolved_org,
                        id,
                        yes,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectDevicesCommands::Reclaim { command } => match command {
                    ConnectDeviceReclaimCommands::List {
                        org,
                        status,
                        device_id,
                        config,
                        profile,
                    } => {
                        let profile_org =
                            commands::connect::profile_organization_id(profile.as_deref())?;
                        let resolved_org =
                            commands::connect::resolve_org(org, &config, profile_org)?;
                        let cmd = ConnectDeviceReclaimListCommand {
                            org: resolved_org,
                            status: status.as_str().to_string(),
                            device_id,
                            profile,
                        };
                        cmd.execute().await?;
                        Ok(())
                    }
                    ConnectDeviceReclaimCommands::Approve {
                        id,
                        org,
                        yes,
                        config,
                        profile,
                    } => {
                        let profile_org =
                            commands::connect::profile_organization_id(profile.as_deref())?;
                        let resolved_org =
                            commands::connect::resolve_org(org, &config, profile_org)?;
                        let cmd = ConnectDeviceReclaimApproveCommand {
                            org: resolved_org,
                            id,
                            yes,
                            profile,
                        };
                        cmd.execute().await?;
                        Ok(())
                    }
                    ConnectDeviceReclaimCommands::Deny {
                        id,
                        org,
                        reason,
                        yes,
                        config,
                        profile,
                    } => {
                        let profile_org =
                            commands::connect::profile_organization_id(profile.as_deref())?;
                        let resolved_org =
                            commands::connect::resolve_org(org, &config, profile_org)?;
                        let cmd = ConnectDeviceReclaimDenyCommand {
                            org: resolved_org,
                            id,
                            reason,
                            yes,
                            profile,
                        };
                        cmd.execute().await?;
                        Ok(())
                    }
                    ConnectDeviceReclaimCommands::Delete {
                        id,
                        org,
                        yes,
                        config,
                        profile,
                    } => {
                        let profile_org =
                            commands::connect::profile_organization_id(profile.as_deref())?;
                        let resolved_org =
                            commands::connect::resolve_org(org, &config, profile_org)?;
                        let cmd = ConnectDeviceReclaimDeleteCommand {
                            org: resolved_org,
                            id,
                            yes,
                            profile,
                        };
                        cmd.execute().await?;
                        Ok(())
                    }
                },
            },
            ConnectCommands::Cohorts { command } => match command {
                ConnectCohortsCommands::List {
                    org,
                    project,
                    config,
                    profile,
                    output,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(
                            org,
                            project,
                            &config,
                            profile_org,
                        )?;
                    let cmd = ConnectCohortsListCommand {
                        org: resolved_org,
                        project: resolved_project,
                        profile,
                        output,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectCohortsCommands::Create {
                    org,
                    project,
                    name,
                    description,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(
                            org,
                            project,
                            &config,
                            profile_org,
                        )?;
                    let cmd = ConnectCohortsCreateCommand {
                        org: resolved_org,
                        project: resolved_project,
                        name,
                        description,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectCohortsCommands::Delete {
                    org,
                    project,
                    id,
                    yes,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(
                            org,
                            project,
                            &config,
                            profile_org,
                        )?;
                    let cmd = ConnectCohortsDeleteCommand {
                        org: resolved_org,
                        project: resolved_project,
                        id,
                        yes,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Runtimes { command } => match command {
                ConnectRuntimesCommands::List {
                    org,
                    project,
                    config,
                    profile,
                    output,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(
                            org,
                            project,
                            &config,
                            profile_org,
                        )?;
                    let cmd = ConnectRuntimesListCommand {
                        org: resolved_org,
                        project: resolved_project,
                        profile,
                        output,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::ClaimTokens { command } => match command {
                ConnectClaimTokensCommands::List {
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectClaimTokensListCommand {
                        org: resolved_org,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectClaimTokensCommands::Create {
                    org,
                    project,
                    cohort,
                    name,
                    tag,
                    max_uses,
                    no_expiration,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectClaimTokensCreateCommand {
                        org: resolved_org,
                        project,
                        cohort,
                        name,
                        tags: tag,
                        max_uses,
                        no_expiration,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectClaimTokensCommands::Delete {
                    org,
                    id,
                    yes,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;
                    let cmd = ConnectClaimTokensDeleteCommand {
                        org: resolved_org,
                        id,
                        yes,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Upload {
                org,
                project,
                runtime,
                version,
                description,
                config,
                target,
                file,
                profile,
                publish,
                deploy_cohort,
                deploy_name,
                deploy_tag,
                deploy_activate,
                output,
            } => {
                let profile_org = commands::connect::profile_organization_id(profile.as_deref())?;
                let (resolved_org, resolved_project) = commands::connect::resolve_org_and_project(
                    org.clone(),
                    project.clone(),
                    &config,
                    profile_org,
                )?;

                let cmd = ConnectUploadCommand {
                    org: resolved_org.clone(),
                    project: resolved_project.clone(),
                    runtime,
                    version,
                    description,
                    config_path: config,
                    target: target.or(cli.target),
                    file,
                    profile: profile.clone(),
                    publish,
                    deploy_cohort,
                    deploy_name,
                    deploy_tags: deploy_tag,
                    deploy_activate,
                    output,
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::Deploy {
                org,
                project,
                runtime,
                cohort,
                name,
                description,
                tag,
                activate,
                config,
                profile,
                output,
            } => {
                let profile_org = commands::connect::profile_organization_id(profile.as_deref())?;
                let (resolved_org, resolved_project) =
                    commands::connect::resolve_org_and_project(org, project, &config, profile_org)?;
                let cmd = ConnectDeployCommand {
                    org: resolved_org,
                    project: resolved_project,
                    runtime,
                    cohort,
                    name,
                    description,
                    tags: tag,
                    activate,
                    profile,
                    output,
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::ServerKey {
                org,
                config,
                profile,
            } => {
                let profile_org = commands::connect::profile_organization_id(profile.as_deref())?;
                let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                let cmd = ConnectServerKeyCommand {
                    org: resolved_org,
                    profile,
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::Keys { command } => match command {
                ConnectKeysCommands::Register {
                    key_type,
                    key,
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectKeysRegisterCommand {
                        org: resolved_org,
                        key_name: key,
                        key_type,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectKeysCommands::Approve {
                    keyid,
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectKeysApproveCommand {
                        org: resolved_org,
                        keyid,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectKeysCommands::List {
                    org,
                    key_type,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectKeysListCommand {
                        org: resolved_org,
                        key_type,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectKeysCommands::Retire {
                    keyid,
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectKeysRetireCommand {
                        org: resolved_org,
                        keyid,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Trust { command } => match command {
                ConnectTrustCommands::Status {
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectTrustStatusCommand {
                        org: resolved_org,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectTrustCommands::PromoteRoot {
                    key,
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectTrustPromoteRootCommand {
                        key,
                        org: resolved_org,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectTrustCommands::RotateServerKey {
                    key,
                    org,
                    config,
                    profile,
                } => {
                    let profile_org =
                        commands::connect::profile_organization_id(profile.as_deref())?;
                    let resolved_org = commands::connect::resolve_org(org, &config, profile_org)?;

                    let cmd = ConnectTrustRotateServerKeyCommand {
                        key,
                        org: resolved_org,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
        },
    };

    if let Some(handle) = update_handle {
        if let Ok(Ok(Some(version))) =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle).await
        {
            // Suppress the banner when the user asked for JSON / NDJSON
            // output. The banner goes to stderr, but callers that
            // capture both streams (e.g. the avocado-desktop CLI runner
            // merges stdout + stderr in causal order) ended up feeding
            // the banner into a JSON parser, which then refused the
            // entire payload. Silent-when-machine-readable is the
            // safer contract.
            let json_mode = std::env::args().any(|a| a == "--output")
                && std::env::args()
                    .skip_while(|a| a != "--output")
                    .nth(1)
                    .as_deref()
                    == Some("json");
            if !json_mode {
                let upgrade_hint = match utils::install_method::current_install_method() {
                    utils::install_method::InstallMethod::Homebrew => {
                        "Run 'avocado upgrade' or 'brew upgrade avocado-cli' to update."
                    }
                    _ => "Run 'avocado upgrade' to update.",
                };
                eprintln!(
                    "\n\x1b[93m[UPDATE]\x1b[0m avocado {} is available (you have {}).\n         {}",
                    version,
                    env!("CARGO_PKG_VERSION"),
                    upgrade_hint
                );
            }
        }
    }

    result
}

#[derive(Subcommand)]
enum ExtCommands {
    /// Install dependencies into extension sysroots
    Install {
        /// Extension name (if not provided, installs all extensions)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Fetch remote extensions from repo, git, or path sources
    Fetch {
        /// Extension name (if not provided, fetches all remote extensions)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force re-fetch even if already installed
        #[arg(short, long)]
        force: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
    },
    /// Build sysext and/or confext extensions from configuration
    Build {
        /// Extension name (must be defined in config)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime to build the extension against (kernel/rootfs context).
        /// Required when the project has multiple runtimes. Resolves from
        /// AVOCADO_RUNTIME / default_runtime / sole-runtime when omitted.
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List extension names
    List {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// List dependencies for extensions
    Deps {
        /// Extension name (if not provided, shows all extensions)
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run DNF commands in an extension's context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to operate on
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime context for the dnf operation (scopes which extension
        /// sysroot tree dnf operates on).
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Clean an extension's sysroot
    Clean {
        /// Extension name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime context for the clean (resolves which extension sysroot
        /// tree to operate on). Falls through to legacy per-target behavior
        /// when no runtime resolves.
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Check out files from extension sysroot to source directory
    Checkout {
        /// Extension name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime context for the checkout (selects which sysroot tree the
        /// files come from).
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Path within the extension sysroot to checkout (e.g., /etc/config.json or /etc for directory)
        #[arg(long = "ext-path", required = true)]
        ext_path: String,
        /// Destination path in source directory (relative to src root)
        #[arg(long = "src-path", required = true)]
        src_path: String,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
    },
    /// Create squashfs image from system extension
    Image {
        /// Extension name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime to image the extension under (kernel/rootfs context).
        /// Same resolution rules as `ext build -r`.
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Output directory on host to copy the resulting image to
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Package extension sysroot into an RPM
    Package {
        /// Extension name
        name: Option<String>,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name (deprecated, use positional argument)
        #[arg(short = 'e', long = "extension", hide = true)]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Runtime context for the package operation.
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Output directory on host for the RPM package (relative or absolute path). If not specified, RPM stays in container at $AVOCADO_PREFIX/output/extensions
        #[arg(long = "out-dir")]
        output_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum RootfsCommands {
    /// Install rootfs sysroot packages via DNF
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build rootfs image from sysroot
    Image {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Output directory on host for the resulting image
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove rootfs sysroot
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum InitramfsCommands {
    /// Install initramfs sysroot packages via DNF
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build initramfs image from sysroot
    Image {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Output directory on host for the resulting image
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove initramfs sysroot
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum KernelCommands {
    /// Wrap the rootfs sysroot's kernel binary into a signed kos.layer.kernel KAB.
    /// Requires `avocado rootfs install` to have run first (the kernel-image-*
    /// package lands the binary in the rootfs sysroot's /boot dir).
    Image {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Output directory on host for the resulting image
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum VmCommands {
    /// Boot the avocado-vm (no-op if already running).
    Start {
        /// Directory containing `direct` profile output (manifest.json + artifacts).
        /// Resolution order when unset: $AVOCADO_VM_DIR → ~/.avocado/vm/install/
        /// (populated by `avocado vm update`) → last `vm start`/`vm rebuild`
        /// dir → error.
        #[arg(long)]
        vm_source: Option<std::path::PathBuf>,
        /// Memory in MiB. Resolution order: this flag → `runtime.memory_mib`
        /// in `~/.avocado/vm/config.yaml` (also written by Avocado.app's
        /// settings UI) → 4096. When passed, the value is persisted back to
        /// the config so the next flag-less `vm start` reuses it.
        #[arg(long)]
        memory_mib: Option<u32>,
        /// vCPU count. Same resolution + persistence as `--memory-mib`,
        /// falling back to `runtime.cpus` or 4.
        #[arg(long)]
        cpus: Option<u32>,
        /// Bind SSH on this host port (default: pick a free high port).
        #[arg(long)]
        ssh_port: Option<u16>,
        /// Extra kernel cmdline appended to the manifest's default.
        #[arg(long)]
        cmdline_extra: Option<String>,
        /// Host directory exposed to the VM as a 9p workspace (mounted at
        /// /mnt/workspace in the guest). Defaults to $AVOCADO_VM_WORKSPACE or $HOME.
        /// Every project the CLI operates on must live under this path.
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
        /// Persistent /var disk size (e.g. "50G", "100G"). Growable on each
        /// start; shrink requires `vm rebuild --reset-data`. The file is
        /// sparse, so the on-disk footprint only grows as data is written.
        /// Default 50G — comfortable for SDK image + several container images.
        #[arg(long)]
        var_size: Option<String>,
        /// One-shot DNS override applied to this start only. Repeatable —
        /// `--dns 1.1.1.1 --dns 8.8.8.8` sets both. Wins over any value
        /// persisted via `vm config set network.dns`; the persisted value
        /// is unchanged. Useful when a VPN's slirp DNS proxy is broken.
        #[arg(long = "dns")]
        dns: Vec<String>,
        /// Tail the serial log live while waiting for boot-sync. On failure,
        /// the tail of the log is printed automatically even without this flag.
        #[arg(short = 'w', long)]
        watch: bool,
        /// Stay in the foreground (does not yet implement live serial; placeholder).
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the avocado-vm (graceful; falls back to SIGKILL with --force).
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// Show running state + manifest metadata.
    Status,
    /// Open an SSH session into the running avocado-vm.
    Shell {
        /// Optional command + args to run instead of an interactive shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Print (or tail with -f) the QEMU serial console log.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Re-record the manifest from a fresh --vm-source. Preserves data disk
    /// unless --reset-data is given. VM must be stopped first.
    Rebuild {
        /// Falls back to $AVOCADO_VM_DIR if unset.
        #[arg(long)]
        vm_source: Option<std::path::PathBuf>,
        #[arg(long)]
        reset_data: bool,
    },
    /// Wipe the persistent `var.btrfs` and re-seed from the installed
    /// var artifact. Use this when you want a clean /var (Docker
    /// volumes, container caches, project work in /data, etc.).
    /// Doesn't change the VM image version — see `vm update` for that.
    Reset {
        /// Skip the interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Read/write persistent VM configuration at `~/.avocado/vm/config.yaml`.
    /// Same file the Avocado.app settings UI edits — every knob shipped in
    /// the desktop is reachable here.
    Config {
        #[command(subcommand)]
        command: VmConfigCommands,
    },
    /// Long-lived hibernation supervisor. Internal — spawned by `vm start`,
    /// not for direct use. Owns the user-facing SSH port AND docker
    /// socket, proxies to QEMU's internal hostfwd / SSH tunnel, and
    /// sends QMP stop/cont on the idle timeout. Unix-only because the
    /// docker socket path requires UnixListener.
    #[cfg(unix)]
    #[command(hide = true)]
    Supervise {
        #[arg(long)]
        user_port: u16,
        #[arg(long)]
        internal_port: u16,
        #[arg(long)]
        infra_port: u16,
        #[arg(long)]
        qmp_socket: std::path::PathBuf,
        #[arg(long)]
        idle_after_secs: u64,
        #[arg(long)]
        pid_file: std::path::PathBuf,
        #[arg(long)]
        docker_socket: std::path::PathBuf,
        #[arg(long)]
        docker_socket_internal: std::path::PathBuf,
        #[arg(long)]
        docker_socket_stream: std::path::PathBuf,
        #[arg(long)]
        ssh_key: std::path::PathBuf,
        #[arg(long)]
        known_hosts: std::path::PathBuf,
    },
    /// Check for and apply VM image updates from the release channel.
    /// Stops + restarts the VM if it was running. Preserves the existing
    /// `var` partition; use `vm reset` to wipe state.
    Update {
        /// Channel name (default: `~/.avocado/config.yaml [vm].channel`,
        /// or `stable` if unset).
        #[arg(long)]
        channel: Option<String>,
        /// Print availability + exit without downloading.
        #[arg(long)]
        check: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Output format (human prose or single JSON object).
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
}

#[derive(Subcommand)]
enum VmConfigCommands {
    /// Print the value of a dotted key (e.g. `network.dns`). Silent on
    /// missing keys; use `--output json` to disambiguate missing vs empty.
    Get {
        /// Dotted key path, e.g. `network.dns` or `network.dns_search`.
        key: String,
        /// Output format (human plain text or JSON `{key, value}`).
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
    /// Set a dotted key. Multiple values become a list (e.g.
    /// `vm config set network.dns 1.1.1.1 8.8.8.8`).
    Set {
        /// Dotted key path.
        key: String,
        /// One or more values. A single value is stored as a scalar; two
        /// or more are stored as a list. Use `vm config unset` to remove.
        #[arg(required = true, num_args = 1..)]
        values: Vec<String>,
    },
    /// Remove a dotted key. No-op if it doesn't exist.
    Unset {
        /// Dotted key path.
        key: String,
    },
    /// Print the entire config (YAML by default, JSON with `--output json`).
    /// The same JSON shape is what avocado-desktop reads to render its UI.
    List {
        #[arg(long, value_enum, default_value_t = crate::utils::output_format::OutputFormat::Human)]
        output: crate::utils::output_format::OutputFormat,
    },
}

#[derive(Subcommand)]
enum HitlCommands {
    /// Start a HITL server container with preconfigured settings
    Server {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config_path: String,
        /// Extensions to create NFS exports for
        #[arg(short, long = "extension")]
        extensions: Vec<String>,
        /// Additional container arguments
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Target to build for
        #[arg(short, long)]
        target: Option<String>,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// NFS port number to use
        #[arg(short, long)]
        port: Option<u16>,
        /// Disable stamp validation
        #[arg(long)]
        no_stamps: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_vars() {
        let env_args = vec![
            "KEY1=value1".to_string(),
            "KEY2=value2".to_string(),
            "COMPLEX_KEY=value with spaces".to_string(),
        ];

        let result = parse_env_vars(Some(&env_args)).unwrap();

        assert_eq!(result.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(result.get("KEY2"), Some(&"value2".to_string()));
        assert_eq!(
            result.get("COMPLEX_KEY"),
            Some(&"value with spaces".to_string())
        );
    }

    #[test]
    fn test_parse_env_vars_invalid_format() {
        let env_args = vec![
            "VALID_KEY=valid_value".to_string(),
            "INVALID_FORMAT".to_string(),
            "ANOTHER_VALID=another_value".to_string(),
        ];

        let result = parse_env_vars(Some(&env_args)).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("VALID_KEY"), Some(&"valid_value".to_string()));
        assert_eq!(
            result.get("ANOTHER_VALID"),
            Some(&"another_value".to_string())
        );
        assert_eq!(result.get("INVALID_FORMAT"), None);
    }

    #[test]
    fn test_parse_env_vars_empty() {
        let result = parse_env_vars(None);
        assert_eq!(result, None);

        let empty_vec = vec![];
        let result = parse_env_vars(Some(&empty_vec));
        assert_eq!(result, Some(HashMap::new()));
    }

    #[test]
    fn test_build_env_vars_with_provision_profile_only() {
        let result = build_env_vars(Some(&"production".to_string()), None).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get("AVOCADO_PROVISION_PROFILE"),
            Some(&"production".to_string())
        );
    }

    #[test]
    fn test_build_env_vars_with_env_args_only() {
        let env_args = vec!["CUSTOM_VAR=custom_value".to_string()];

        let result = build_env_vars(None, Some(&env_args)).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("CUSTOM_VAR"), Some(&"custom_value".to_string()));
    }

    #[test]
    fn test_build_env_vars_combined() {
        let env_args = vec![
            "AVOCADO_DEVICE_ID=device123".to_string(),
            "AVOCADO_DEVICE_CERT=cert_data".to_string(),
        ];

        let result = build_env_vars(Some(&"staging".to_string()), Some(&env_args)).unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(
            result.get("AVOCADO_PROVISION_PROFILE"),
            Some(&"staging".to_string())
        );
        assert_eq!(
            result.get("AVOCADO_DEVICE_ID"),
            Some(&"device123".to_string())
        );
        assert_eq!(
            result.get("AVOCADO_DEVICE_CERT"),
            Some(&"cert_data".to_string())
        );
    }

    #[test]
    fn test_build_env_vars_empty() {
        let result = build_env_vars(None, None);
        assert_eq!(result, None);

        let empty_vec = vec![];
        let result = build_env_vars(None, Some(&empty_vec));
        assert_eq!(result, None);
    }

    /// `connect upload` talks to Docker (prerequisite stamp check +
    /// in-container artifact discovery), so it must route to the avocado-vm.
    /// The other `connect` subcommands are pure Connect-API calls and must
    /// NOT route — routing can auto-start the VM, which is wrong for e.g.
    /// `connect auth`/`deploy`. Pins the Docker-socket fix against regressions
    /// (dropping the upload arm, or over-broadly routing all of `Connect`).
    #[test]
    fn needs_vm_routing_gates_connect_upload_only() {
        let cmd = |args: &[&str]| {
            Cli::try_parse_from(args)
                .expect("args should parse")
                .command
        };

        // connect upload → routes
        assert!(needs_vm_routing(&cmd(&[
            "avocado",
            "connect",
            "upload",
            "dev",
            "--version",
            "v0.0.1",
        ])));

        // other connect subcommands → do NOT route
        assert!(!needs_vm_routing(&cmd(&[
            "avocado",
            "connect",
            "deploy",
            "--runtime",
            "r",
            "--cohort",
            "c",
        ])));
        assert!(!needs_vm_routing(&cmd(&["avocado", "connect", "clean"])));

        // sanity: an existing docker command still routes
        assert!(needs_vm_routing(&cmd(&[
            "avocado",
            "build",
            "--target",
            "qemux86-64",
        ])));
    }
}
