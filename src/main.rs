use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;

mod commands;
mod utils;

use utils::config::Config;

use commands::build::BuildCommand;
use commands::clean::CleanCommand;
use commands::connect::auth::{
    ConnectAuthLoginCommand, ConnectAuthLogoutCommand, ConnectAuthStatusCommand,
};
use commands::connect::claim_tokens::{
    ConnectClaimTokensCreateCommand, ConnectClaimTokensDeleteCommand, ConnectClaimTokensListCommand,
};
use commands::connect::clean::ConnectCleanCommand;
use commands::connect::cohorts::{
    ConnectCohortsCreateCommand, ConnectCohortsDeleteCommand, ConnectCohortsListCommand,
};
use commands::connect::deploy::ConnectDeployCommand;
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
use commands::connect::server_key::ConnectServerKeyCommand;
use commands::connect::trust::{
    ConnectTrustPromoteRootCommand, ConnectTrustRotateServerKeyCommand, ConnectTrustStatusCommand,
};
use commands::connect::upload::ConnectUploadCommand;
use commands::comp::{CompImageCommand, CompListCommand};
use commands::ext::{
    ExtBuildCommand, ExtCheckoutCommand, ExtCleanCommand, ExtDepsCommand, ExtDnfCommand,
    ExtFetchCommand, ExtImageCommand, ExtInstallCommand, ExtListCommand, ExtPackageCommand,
};
use commands::fetch::FetchCommand;
use commands::hitl::HitlServerCommand;
use commands::init::InitCommand;
use commands::initramfs::{InitramfsCleanCommand, InitramfsImageCommand, InitramfsInstallCommand};
use commands::install::InstallCommand;
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
    /// Component related commands (basefs / initramfs / kernel KABs)
    Comp {
        #[command(subcommand)]
        command: CompCommands,
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
    /// Initialize a new avocado project
    Init {
        /// Directory to initialize (defaults to current directory)
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
    },
    /// Remove connect configuration (connect section, connect-config extension, and device config overlay)
    Clean {
        /// Runtime to remove connect-config extension from (default: dev)
        #[arg(short, long, default_value = "dev")]
        runtime: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
    },
    /// Manage organizations
    Orgs {
        #[command(subcommand)]
        command: ConnectOrgsCommands,
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
        /// Version string (defaults to runtime name-version from manifest)
        #[arg(long)]
        version: Option<String>,
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
        #[arg(long)]
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
    },
    /// Logout from the Connect platform
    Logout {
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Show current auth status
    Status {
        /// Profile name (defaults to the active default profile)
        #[arg(long)]
        profile: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set AVOCADO_NO_TUI env var so all should_use_tui() calls respect --no-tui
    if cli.no_tui {
        std::env::set_var("AVOCADO_NO_TUI", "1");
    }

    let is_upgrade = matches!(cli.command, Commands::Upgrade { .. });
    let update_handle = if !is_upgrade {
        Some(tokio::spawn(utils::update_check::check_for_update()))
    } else {
        None
    };

    let result = match cli.command {
        Commands::Init {
            directory,
            target,
            reference,
            reference_branch,
            reference_commit,
            reference_repo,
        } => {
            let init_cmd = InitCommand::new(
                target.or(cli.target),
                directory,
                reference,
                reference_branch,
                reference_commit,
                reference_repo,
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
        } => {
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
        } => {
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
        } => {
            let runtime = name
                .or(runtime)
                .context("runtime name is required (provide as positional or -r/--runtime)")?;

            // Validate runtime exists (required argument)
            validate_runtime_required(&config, &runtime)?;

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
        } => {
            let runtime = name
                .or(runtime)
                .context("runtime name is required (provide as positional or -r/--runtime)")?;

            // Validate runtime exists (required argument)
            validate_runtime_required(&config, &runtime)?;

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
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
            } => {
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
                let runtime = name
                    .or(runtime)
                    .context("runtime name is required (provide as positional or -r/--runtime)")?;
                // Validate runtime exists (required argument)
                validate_runtime_required(&config, &runtime)?;

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
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
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
                .with_sdk_arch(cli.sdk_arch.clone());
                build_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Checkout {
                name,
                config,
                verbose,
                extension,
                target,
                ext_path,
                src_path,
                container_tool,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
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
                .with_sdk_arch(cli.sdk_arch.clone());
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
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = ExtDnfCommand::new(
                    config,
                    extension,
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
            ExtCommands::Clean {
                name,
                extension,
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
                let clean_cmd = ExtCleanCommand::new(
                    extension,
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
            ExtCommands::Image {
                name,
                extension,
                config,
                verbose,
                target,
                out_dir,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
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
                .with_output_dir(out_dir);
                image_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Package {
                name,
                extension,
                target,
                config,
                verbose,
                output_dir,
                container_args,
                dnf_args,
            } => {
                let extension = name.or(extension).context(
                    "extension name is required (provide as positional or -e/--extension)",
                )?;
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
                .with_sdk_arch(cli.sdk_arch.clone());
                package_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Comp { command } => match command {
            CompCommands::List { config, target: _ } => {
                let list_cmd = CompListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            CompCommands::Image {
                name,
                config,
                verbose,
                target,
                out_dir,
                container_args,
            } => {
                let image_cmd = CompImageCommand::new(
                    name,
                    config,
                    verbose,
                    target.or(cli.target.clone()),
                    container_args,
                )
                .with_output_dir(out_dir)
                .with_sdk_arch(cli.sdk_arch.clone());
                image_cmd.execute().await?;
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
                } => {
                    let cmd = ConnectAuthLoginCommand::new(url, profile, token);
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectAuthCommands::Logout { profile } => {
                    let cmd = ConnectAuthLogoutCommand { profile };
                    cmd.execute().await?;
                    Ok(())
                }
                ConnectAuthCommands::Status { profile } => {
                    let cmd = ConnectAuthStatusCommand { profile };
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
            } => {
                let cmd = ConnectInitCommand {
                    org,
                    project,
                    cohort,
                    runtime,
                    config_path: config,
                    profile,
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::Clean { runtime, config } => {
                let cmd = ConnectCleanCommand {
                    runtime,
                    config_path: config,
                };
                cmd.execute()?;
                Ok(())
            }
            ConnectCommands::Orgs { command } => match command {
                ConnectOrgsCommands::List { profile } => {
                    let cmd = ConnectOrgsListCommand { profile };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Projects { command } => match command {
                ConnectProjectsCommands::List {
                    org,
                    config,
                    profile,
                } => {
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
                    let cmd = ConnectProjectsListCommand {
                        org: resolved_org,
                        profile,
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
                } => {
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
                    let cmd = ConnectProjectsCreateCommand {
                        org: resolved_org,
                        name,
                        description,
                        profile,
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
                    let cmd = ConnectDevicesDeleteCommand {
                        org: resolved_org,
                        id,
                        yes,
                        profile,
                    };
                    cmd.execute().await?;
                    Ok(())
                }
            },
            ConnectCommands::Cohorts { command } => match command {
                ConnectCohortsCommands::List {
                    org,
                    project,
                    config,
                    profile,
                } => {
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(org, project, &config)?;
                    let cmd = ConnectCohortsListCommand {
                        org: resolved_org,
                        project: resolved_project,
                        profile,
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
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(org, project, &config)?;
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
                    let (resolved_org, resolved_project) =
                        commands::connect::resolve_org_and_project(org, project, &config)?;
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
            ConnectCommands::ClaimTokens { command } => match command {
                ConnectClaimTokensCommands::List {
                    org,
                    config,
                    profile,
                } => {
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;
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
            } => {
                let (resolved_org, resolved_project) = commands::connect::resolve_org_and_project(
                    org.clone(),
                    project.clone(),
                    &config,
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
            } => {
                let (resolved_org, resolved_project) =
                    commands::connect::resolve_org_and_project(org, project, &config)?;
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
                };
                cmd.execute().await?;
                Ok(())
            }
            ConnectCommands::ServerKey {
                org,
                config,
                profile,
            } => {
                let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
                    let resolved_org = commands::connect::resolve_org(org, &config)?;

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
            eprintln!(
                "\n\x1b[93m[UPDATE]\x1b[0m avocado {} is available (you have {}).\n         Run 'avocado upgrade' to update.",
                version,
                env!("CARGO_PKG_VERSION")
            );
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
enum CompCommands {
    /// List component names (with their role)
    List {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Wrap an already-built component payload in a signed KAB.
    ///
    /// For role=basefs the payload is the rootfs erofs produced by
    /// `avocado rootfs image`; for role=initramfs it's the cpio.zst from
    /// `avocado initramfs image`. Requires KAB_KEYSET_FILE to be set.
    Image {
        /// Component name
        name: String,
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Output directory on host to copy the resulting KAB to
        #[arg(long = "out")]
        out_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
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
}
