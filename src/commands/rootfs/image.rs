//! Rootfs image build command and shared build script generation.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    target::resolve_target_required,
};

/// Namespace UUID for deterministic OS build ID generation (shared with runtime build).
pub const NAMESPACE_UUID: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// Generate the shell script fragment that builds a rootfs image from the shared sysroot.
///
/// The generated script expects these shell variables to be set:
/// - `$AVOCADO_PREFIX` — SDK prefix (container volume)
/// - `$AVOCADO_SDK_PREFIX` — SDK tools prefix
/// - `$OUTPUT_DIR` — directory for output image
/// - `$TARGET_ARCH` — target architecture string
/// - `$RUNTIME_NAME` — runtime name (for os-release injection)
/// - `$RUNTIME_VERSION` — runtime version (for os-release injection)
///
/// Exports on success:
/// - `$AVOCADO_ROOTFS_IMAGE` — path to built image
/// - `$AVOCADO_ROOTFS_FILESYSTEM` — filesystem format used
/// - `$AVOCADO_OS_BUILD_ID` — deterministic build ID
pub fn generate_rootfs_build_script(namespace_uuid: &str, rootfs_filesystem: &str) -> String {
    format!(
        r#"
# Build rootfs image from shared sysroot
ROOTFS_SYSROOT="$AVOCADO_PREFIX/rootfs"
if [ -d "$ROOTFS_SYSROOT/usr" ]; then
    echo "Building rootfs image from packages..."

    # Work on a copy so we don't mutate the shared sysroot used for extension priming
    ROOTFS_WORK="${{ROOTFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/rootfs-work}}"
    rm -rf "$ROOTFS_WORK"
    cp -a "$ROOTFS_SYSROOT" "$ROOTFS_WORK"

    # Create usrmerge symlinks (Yocto image class does this, not any RPM package)
    ln -sfn usr/bin "$ROOTFS_WORK/bin"
    ln -sfn usr/sbin "$ROOTFS_WORK/sbin"
    ln -sfn usr/lib "$ROOTFS_WORK/lib"

    # Post-processing (matches Yocto avocado-image-rootfs.bb)
    rm -rf "$ROOTFS_WORK/media" "$ROOTFS_WORK/mnt" "$ROOTFS_WORK/srv"
    rm -rf "$ROOTFS_WORK/boot/"*
    mkdir -p "$ROOTFS_WORK/opt"

    # Create empty /etc/machine-id for stateless systemd on read-only rootfs.
    # systemd will bind-mount a transient machine-id at boot.
    # (matches OE read_only_rootfs_hook in rootfs-postcommands.bbclass)
    touch "$ROOTFS_WORK/etc/machine-id"

    # Enable systemd service units via preset files.
    # (matches OE systemd_preset_all in image.bbclass)
    if [ -e "$ROOTFS_WORK/usr/lib/systemd/systemd" ]; then
        "$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/systemctl" --root="$ROOTFS_WORK" --preset-mode=enable-only preset-all 2>/dev/null || true
        echo "Applied systemd presets"
    fi

    # Generate ld.so.cache so the read-only rootfs has a working linker cache.
    # Uses the container's host-native ldconfig with -r (chroot flag).
    # ldconfig -r does chroot() but continues as the host binary — no binfmt needed.
    # This matches Yocto's ldconfig-native approach.
    /usr/sbin/ldconfig -r "$ROOTFS_WORK" -c new -X 2>/dev/null || true
    echo "Generated ld.so.cache"

    # Compute deterministic AVOCADO_OS_BUILD_ID from installed packages
    PKG_NEVRA=$(rpm -qa --queryformat '%{{NEVRA}}\n' --root "$ROOTFS_SYSROOT" | sort)
    PKG_HASH=$(echo "$PKG_NEVRA" | sha256sum | awk '{{print $1}}')
    OS_BUILD_ID=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('{namespace_uuid}'), '$PKG_HASH'))")

    # Inject identity into os-release (in the work copy only)
    echo "AVOCADO_OS_BUILD_ID=$OS_BUILD_ID" >> "$ROOTFS_WORK/usr/lib/os-release"
    echo "AVOCADO_RUNTIME_NAME=$RUNTIME_NAME" >> "$ROOTFS_WORK/usr/lib/os-release"
    echo "AVOCADO_RUNTIME_VERSION=$RUNTIME_VERSION" >> "$ROOTFS_WORK/usr/lib/os-release"

    # Build rootfs image using configured filesystem format
    ROOTFS_FS="{rootfs_filesystem}"
    ROOTFS_OUTPUT="$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.$ROOTFS_FS"
    echo "Building rootfs image: $ROOTFS_FS"
    case "$ROOTFS_FS" in
        erofs.zst)
            mkfs.erofs \
                -T "${{SOURCE_DATE_EPOCH:-0}}" \
                -U 00000000-0000-0000-0000-000000000000 \
                -x -1 \
                --all-root \
                -z zstd \
                "$ROOTFS_OUTPUT" \
                "$ROOTFS_WORK"
            ;;
        erofs.lz4)
            mkfs.erofs \
                -T "${{SOURCE_DATE_EPOCH:-0}}" \
                -U 00000000-0000-0000-0000-000000000000 \
                -x -1 \
                --all-root \
                -z lz4hc \
                "$ROOTFS_OUTPUT" \
                "$ROOTFS_WORK"
            ;;
        *)
            echo "ERROR: unsupported rootfs filesystem format: $ROOTFS_FS"
            exit 1
            ;;
    esac

    rm -rf "$ROOTFS_WORK"
    export AVOCADO_ROOTFS_IMAGE="$ROOTFS_OUTPUT"
    export AVOCADO_ROOTFS_FILESYSTEM="$ROOTFS_FS"
    export AVOCADO_OS_BUILD_ID="$OS_BUILD_ID"
    echo "Built rootfs: $ROOTFS_OUTPUT (AVOCADO_OS_BUILD_ID=$OS_BUILD_ID)"
else
    echo "No rootfs sysroot found — skipping rootfs image build."
fi"#,
        namespace_uuid = namespace_uuid,
        rootfs_filesystem = rootfs_filesystem,
    )
}

/// Implementation of the 'rootfs image' command.
pub struct RootfsImageCommand {
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    out_dir: Option<String>,
}

impl RootfsImageCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            sdk_arch: None,
            runs_on: None,
            nfs_port: None,
            out_dir: None,
        }
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    pub fn with_runs_on(mut self, runs_on: Option<String>, nfs_port: Option<u16>) -> Self {
        self.runs_on = runs_on;
        self.nfs_port = nfs_port;
        self
    }

    pub fn with_output_dir(mut self, out_dir: Option<String>) -> Self {
        self.out_dir = out_dir;
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = Arc::new(
            Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                || format!("Failed to load composed config from {}", self.config_path),
            )?,
        );
        let config = &composed.config;
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        let mut runs_on_context: Option<RunsOnContext> = if let Some(ref runs_on) = self.runs_on {
            Some(
                container_helper
                    .create_runs_on_context(runs_on, self.nfs_port, container_image, self.verbose)
                    .await?,
            )
        } else {
            None
        };

        print_info("Building rootfs image.", OutputLevel::Normal);

        let rootfs_filesystem = config.get_rootfs_filesystem();
        let build_section = generate_rootfs_build_script(NAMESPACE_UUID, &rootfs_filesystem);

        // Wrap the build script with variable setup for standalone execution
        let out_dir_setup = if let Some(ref out) = self.out_dir {
            format!(r#"OUTPUT_DIR="{out}""#)
        } else {
            r#"OUTPUT_DIR="$AVOCADO_PREFIX/output/images""#.to_string()
        };

        let script = format!(
            r#"set -euo pipefail
TARGET_ARCH="{target_arch}"
RUNTIME_NAME="${{AVOCADO_RUNTIME_NAME:-standalone}}"
RUNTIME_VERSION="${{AVOCADO_RUNTIME_VERSION:-0.0.0}}"
{out_dir_setup}
mkdir -p "$OUTPUT_DIR"
{build_section}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let result = if let Some(ref context) = runs_on_context {
            container_helper
                .run_in_container_with_context(&run_config, context)
                .await
        } else {
            container_helper.run_in_container(run_config).await
        };

        // Teardown runs_on context
        if let Some(ref mut context) = runs_on_context {
            if let Err(e) = context.teardown().await {
                print_error(
                    &format!("Warning: Failed to cleanup remote resources: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        let success = result?;
        if success {
            print_success("Built rootfs image.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to build rootfs image."));
        }

        Ok(())
    }
}
