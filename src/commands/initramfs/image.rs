//! Initramfs image build command and shared build script generation.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::{
    config::{get_ext_image_args, get_ext_image_type, get_post_install, Config},
    container::{RunConfig, SdkContainer},
    host_copy::copy_volume_path_to_host,
    kab_wrap::generate_kab_wrap_script,
    output::{print_error, print_info, print_success, OutputLevel},
    permissions::{mapping_from_hashmap, render_users_groups_script},
    runs_on::RunsOnContext,
    target::resolve_target_required,
};

use crate::commands::rootfs::image::{render_hook_block, resolve_install_hooks, NAMESPACE_UUID};

/// Default post-install commands for the initramfs build. Same shape as
/// `DEFAULT_ROOTFS_POST_INSTALL` but for `$INITRAMFS_WORK`, plus the
/// `/init` symlink the kernel needs to find the init binary.
///
/// Used as a fallback only when the user does NOT define `pre_install`
/// or `post_install` in the initramfs config. If the user defines
/// either, they take full control and this default list is skipped.
pub const DEFAULT_INITRAMFS_POST_INSTALL: &[&str] = &[
    // usrmerge symlinks.
    "ln -sfn usr/bin \"$INITRAMFS_WORK/bin\"",
    "ln -sfn usr/sbin \"$INITRAMFS_WORK/sbin\"",
    "ln -sfn usr/lib \"$INITRAMFS_WORK/lib\"",
    // Strip dirs that Yocto's avocado-image-initramfs.bb also stripped.
    "rm -rf \"$INITRAMFS_WORK/media\" \"$INITRAMFS_WORK/mnt\" \"$INITRAMFS_WORK/srv\"",
    "rm -rf \"$INITRAMFS_WORK/boot/\"*",
    "mkdir -p \"$INITRAMFS_WORK/sysroot\"",
    "mkdir -p \"$INITRAMFS_WORK/opt\"",
    // /init symlink so the kernel can find the init process.
    // (matches OE IMAGE_CMD:cpio in image_types.bbclass)
    "if [ ! -L \"$INITRAMFS_WORK/init\" ] && [ ! -e \"$INITRAMFS_WORK/init\" ]; then \
if [ -L \"$INITRAMFS_WORK/sbin/init\" ] || [ -e \"$INITRAMFS_WORK/sbin/init\" ]; then \
ln -sf /sbin/init \"$INITRAMFS_WORK/init\"; \
echo \"Created /init -> /sbin/init symlink\"; \
else echo \"WARNING: /sbin/init not found in initramfs — kernel may not find init\"; fi; fi",
];

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
///
/// `post_install` hook semantics are identical to
/// `generate_rootfs_build_script` — see that function's docs.
pub fn generate_initramfs_build_script(
    namespace_uuid: &str,
    initramfs_filesystem: &str,
    post_install: Option<&str>,
    permissions_section: &str,
) -> String {
    let post = resolve_install_hooks(post_install, DEFAULT_INITRAMFS_POST_INSTALL);
    let post_install_block = render_hook_block("post_install", &post);
    format!(
        r#"
# Build initramfs image from shared sysroot.
# These vars are `export`ed so the post_install script (which we invoke
# as a child `bash` process) inherits them.
export INITRAMFS_SYSROOT="$AVOCADO_PREFIX/initramfs"
if [ -d "$INITRAMFS_SYSROOT/usr" ]; then
    echo "Building initramfs image from packages..."

    export INITRAMFS_WORK="${{INITRAMFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/initramfs-work}}"
    # Standalone initramfs builds (no runtime build before this) leave
    # the parent runtimes/$RUNTIME_NAME dir uncreated; ensure it exists.
    mkdir -p "$(dirname "$INITRAMFS_WORK")"
    rm -rf "$INITRAMFS_WORK"
    cp -a "$INITRAMFS_SYSROOT" "$INITRAMFS_WORK"
{permissions_section}

{post_install_block}

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
        post_install_block = post_install_block,
        permissions_section = permissions_section,
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
        // Honor per-target `target-<name>:` overrides inside the `initramfs:`
        // section (e.g. a custom `--tag`). Apply the same override merge
        // extensions use, but on the already-composed value so path-based
        // sources (merge_path_based_image_sections) are preserved.
        let initramfs_merged = composed
            .merged_value
            .get("initramfs")
            .cloned()
            .map(|v| config.resolve_overrides_in_value(v, &target_arch, None, "initramfs"));
        let initramfs_node = initramfs_merged.as_ref();
        let post_install = get_post_install(initramfs_node);
        let permissions_section = config
            .initramfs_default()
            .and_then(|img| config.resolve_image_permissions(img))
            .map(|p| {
                let users = mapping_from_hashmap(p.users.as_ref());
                let groups = mapping_from_hashmap(p.groups.as_ref());
                render_users_groups_script(
                    users.as_ref(),
                    groups.as_ref(),
                    "$INITRAMFS_WORK/etc",
                    None,
                )
            })
            .unwrap_or_default();
        let build_section = generate_initramfs_build_script(
            NAMESPACE_UUID,
            &initramfs_filesystem,
            post_install.as_deref(),
            &permissions_section,
        );

        // Same kab-wrap pipeline as rootfs/image.rs — see comments
        // there for the design rationale.
        let image_type = initramfs_node
            .and_then(get_ext_image_type)
            .unwrap_or_else(|| "raw".to_string());
        let image_args = initramfs_node.and_then(get_ext_image_args);
        let wrap_kab = image_type == "kab";

        let kab_keyset_host_path: Option<String> = if wrap_kab {
            let p = std::env::var("KAB_KEYSET_FILE").map_err(|_| {
                anyhow::anyhow!(
                    "initramfs.image.type is `kab` but KAB_KEYSET_FILE is not set. \
                     Set it to the path of your KAB signing keyset."
                )
            })?;
            if !std::path::Path::new(&p).is_file() {
                return Err(anyhow::anyhow!(
                    "KAB_KEYSET_FILE points to '{}' but the file does not exist.",
                    p
                ));
            }
            Some(p)
        } else {
            None
        };

        let wrap_section = if wrap_kab {
            let args = image_args
                .as_deref()
                .context("initramfs.image.type is `kab` but initramfs.image.args is missing")?;
            generate_kab_wrap_script(
                "initramfs",
                "AVOCADO_INITRAMFS_IMAGE",
                args,
                "$RUNTIME_VERSION",
            )
        } else {
            String::new()
        };

        let internal_output_dir = "$AVOCADO_PREFIX/output/images";

        let script = format!(
            r#"set -euo pipefail
export TARGET_ARCH="{target_arch}"
export RUNTIME_NAME="${{AVOCADO_RUNTIME_NAME:-standalone}}"
export RUNTIME_VERSION="${{AVOCADO_RUNTIME_VERSION:-0.0.0}}"
export OUTPUT_DIR="{internal_output_dir}"
mkdir -p "$OUTPUT_DIR"
{build_section}
AVOCADO_OS_VERSION_ID=""
if [ -f "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" ]; then
    AVOCADO_OS_VERSION_ID=$(grep '^VERSION_ID=' "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" \
        | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//")
fi
export AVOCADO_OS_VERSION_ID
{wrap_section}
"#
        );

        let mut env_vars: HashMap<String, String> = HashMap::new();
        if wrap_kab {
            env_vars.insert("KAB_KEYSET_FILE".to_string(), "/tmp/kab.keyset".to_string());
        }

        let container_args_with_keyset = if let Some(ref host_path) = kab_keyset_host_path {
            let mut args = merged_container_args.clone().unwrap_or_default();
            args.push("-v".to_string());
            args.push(format!("{host_path}:/tmp/kab.keyset:ro"));
            Some(args)
        } else {
            merged_container_args.clone()
        };

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: container_args_with_keyset,
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            env_vars: if env_vars.is_empty() {
                None
            } else {
                Some(env_vars)
            },
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
        if !success {
            return Err(anyhow::anyhow!("Failed to build initramfs image."));
        }

        if let Some(ref out_dir) = self.out_dir {
            let cwd = std::env::current_dir().context("Failed to get current directory")?;
            let volume_manager =
                crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
            let volume_state = volume_manager
                .get_or_create_volume(&cwd)
                .await
                .context("Failed to resolve SDK volume for host copy")?;
            let volume_name = &volume_state.volume_name;

            let host_dir = if out_dir.starts_with('/') {
                PathBuf::from(out_dir)
            } else {
                cwd.join(out_dir)
            };
            std::fs::create_dir_all(&host_dir)
                .with_context(|| format!("Failed to mkdir -p {}", host_dir.display()))?;

            // /opt/_avocado/<target>/output/images/avocado-image-initramfs-<target>.<fs>
            // When wrapping, the kab is the final artifact — skip the
            // raw cpio (it's intermediate and stays in the volume).
            let raw_filename =
                format!("avocado-image-initramfs-{target_arch}.{initramfs_filesystem}");
            let (host_filename, container_path) = if wrap_kab {
                let kab_filename = format!("{raw_filename}.kab");
                (
                    kab_filename.clone(),
                    format!("/opt/_avocado/{target_arch}/output/images/{kab_filename}"),
                )
            } else {
                (
                    raw_filename.clone(),
                    format!("/opt/_avocado/{target_arch}/output/images/{raw_filename}"),
                )
            };
            copy_volume_path_to_host(
                &container_helper.container_tool,
                volume_name,
                &container_path,
                &host_dir.join(&host_filename),
            )
            .await
            .with_context(|| format!("Failed to copy {host_filename} to host"))?;
            print_info(
                &format!("Copied {} to {}", host_filename, host_dir.display()),
                OutputLevel::Normal,
            );
        }

        print_success("Built initramfs image.", OutputLevel::Normal);
        Ok(())
    }
}
