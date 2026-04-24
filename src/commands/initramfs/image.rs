//! Initramfs image build command and shared build script generation.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    target::resolve_target_required,
};

use crate::commands::rootfs::image::NAMESPACE_UUID;

/// Generate the shell script fragment that builds an initramfs image from the shared sysroot.
///
/// The generated script expects these shell variables to be set:
/// - `$AVOCADO_PREFIX` — SDK prefix (container volume)
/// - `$OUTPUT_DIR` — directory for output image
/// - `$TARGET_ARCH` — target architecture string
/// - `$RUNTIME_NAME` — runtime name (for work dir path)
///
/// Exports on success:
/// - `$AVOCADO_INITRAMFS_IMAGE` — path to built image
/// - `$AVOCADO_INITRAMFS_FILESYSTEM` — filesystem format used
/// - `$AVOCADO_INITRAMFS_BUILD_ID` — deterministic build ID
pub fn generate_initramfs_build_script(namespace_uuid: &str, initramfs_filesystem: &str) -> String {
    format!(
        r#"
# Build initramfs image from shared sysroot
INITRAMFS_SYSROOT="$AVOCADO_PREFIX/initramfs"
if [ -d "$INITRAMFS_SYSROOT/usr" ]; then
    echo "Building initramfs image from packages..."

    INITRAMFS_WORK="${{INITRAMFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/initramfs-work}}"
    rm -rf "$INITRAMFS_WORK"

    echo "[DEBUG] INITRAMFS_SYSROOT=$INITRAMFS_SYSROOT"
    echo "[DEBUG] sysroot module count: $(find "$INITRAMFS_SYSROOT/usr/lib/modules" -name '*.ko' 2>/dev/null | wc -l)"
    echo "[DEBUG] sysroot has nvme.ko: $(ls "$INITRAMFS_SYSROOT/usr/lib/modules"/*/kernel/drivers/nvme/host/nvme.ko 2>/dev/null || echo NO)"

    cp -a "$INITRAMFS_SYSROOT" "$INITRAMFS_WORK"

    echo "[DEBUG] INITRAMFS_WORK=$INITRAMFS_WORK"
    echo "[DEBUG] work module count after cp: $(find "$INITRAMFS_WORK/usr/lib/modules" -name '*.ko' 2>/dev/null | wc -l)"
    echo "[DEBUG] work has nvme.ko: $(ls "$INITRAMFS_WORK/usr/lib/modules"/*/kernel/drivers/nvme/host/nvme.ko 2>/dev/null || echo NO)"

    # Create usrmerge symlinks (Yocto image class does this, not any RPM package)
    ln -sfn usr/bin "$INITRAMFS_WORK/bin"
    ln -sfn usr/sbin "$INITRAMFS_WORK/sbin"
    ln -sfn usr/lib "$INITRAMFS_WORK/lib"

    # Post-processing (matches Yocto avocado-image-initramfs.bb)
    rm -rf "$INITRAMFS_WORK/media" "$INITRAMFS_WORK/mnt" "$INITRAMFS_WORK/srv"
    rm -rf "$INITRAMFS_WORK/boot/"*
    mkdir -p "$INITRAMFS_WORK/sysroot"
    mkdir -p "$INITRAMFS_WORK/opt"

    # Compute deterministic build ID for initramfs
    INITRAMFS_PKG_NEVRA=$(rpm -qa --queryformat '%{{NEVRA}}\n' --root "$INITRAMFS_SYSROOT" | sort)
    INITRAMFS_PKG_HASH=$(echo "$INITRAMFS_PKG_NEVRA" | sha256sum | awk '{{print $1}}')
    INITRAMFS_BUILD_ID=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('{namespace_uuid}'), '$INITRAMFS_PKG_HASH'))")

    # Inject identity into initrd-release and os-release-initrd
    if [ -f "$INITRAMFS_WORK/usr/lib/initrd-release" ]; then
        echo "AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID" >> "$INITRAMFS_WORK/usr/lib/initrd-release"
    fi
    if [ -f "$INITRAMFS_WORK/usr/lib/os-release-initrd" ]; then
        echo "AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID" >> "$INITRAMFS_WORK/usr/lib/os-release-initrd"
    fi

    # Create /init symlink so the kernel can find the init process in the initramfs.
    # (matches OE IMAGE_CMD:cpio in image_types.bbclass — creates /init -> /sbin/init)
    if [ ! -L "$INITRAMFS_WORK/init" ] && [ ! -e "$INITRAMFS_WORK/init" ]; then
        if [ -L "$INITRAMFS_WORK/sbin/init" ] || [ -e "$INITRAMFS_WORK/sbin/init" ]; then
            ln -sf /sbin/init "$INITRAMFS_WORK/init"
            echo "Created /init -> /sbin/init symlink"
        else
            echo "WARNING: /sbin/init not found in initramfs — kernel may not find init"
        fi
    fi

    # Build initramfs image using configured filesystem format
    INITRAMFS_FS="{initramfs_filesystem}"
    INITRAMFS_OUTPUT="$OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.$INITRAMFS_FS"
    echo "Building initramfs image: $INITRAMFS_FS"
    case "$INITRAMFS_FS" in
        cpio)
            (cd "$INITRAMFS_WORK" && find . | sort | cpio --reproducible -o -H newc --quiet > "$INITRAMFS_OUTPUT")
            ;;
        cpio.zst)
            (cd "$INITRAMFS_WORK" && find . | sort | cpio --reproducible -o -H newc --quiet | zstd -3 -f -o "$INITRAMFS_OUTPUT")
            ;;
        cpio.lz4)
            (cd "$INITRAMFS_WORK" && find . | sort | cpio --reproducible -o -H newc --quiet | lz4 -l -f - "$INITRAMFS_OUTPUT")
            ;;
        cpio.gz)
            (cd "$INITRAMFS_WORK" && find . | sort | cpio --reproducible -o -H newc --quiet | gzip -9 > "$INITRAMFS_OUTPUT")
            ;;
        *)
            echo "ERROR: unsupported initramfs filesystem format: $INITRAMFS_FS"
            exit 1
            ;;
    esac

    rm -rf "$INITRAMFS_WORK"
    export AVOCADO_INITRAMFS_IMAGE="$INITRAMFS_OUTPUT"
    export AVOCADO_INITRAMFS_FILESYSTEM="$INITRAMFS_FS"
    export AVOCADO_INITRAMFS_BUILD_ID="$INITRAMFS_BUILD_ID"
    echo "Built initramfs: $INITRAMFS_OUTPUT"
else
    echo "No initramfs sysroot found — skipping initramfs image build."
fi"#,
        namespace_uuid = namespace_uuid,
        initramfs_filesystem = initramfs_filesystem,
    )
}

/// Implementation of the 'initramfs image' command.
pub struct InitramfsImageCommand {
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

impl InitramfsImageCommand {
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

        print_info("Building initramfs image.", OutputLevel::Normal);

        let initramfs_filesystem = config.get_initramfs_filesystem();
        let build_section = generate_initramfs_build_script(NAMESPACE_UUID, &initramfs_filesystem);

        let out_dir_setup = if let Some(ref out) = self.out_dir {
            format!(r#"OUTPUT_DIR="{out}""#)
        } else {
            r#"OUTPUT_DIR="$AVOCADO_PREFIX/output/images""#.to_string()
        };

        let script = format!(
            r#"set -euo pipefail
TARGET_ARCH="{target_arch}"
RUNTIME_NAME="${{AVOCADO_RUNTIME_NAME:-standalone}}"
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
            print_success("Built initramfs image.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to build initramfs image."));
        }

        Ok(())
    }
}
