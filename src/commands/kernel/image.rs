//! Kernel image build command.
//!
//! Unlike rootfs / initramfs there is no filesystem to assemble — the
//! kernel binary is just a file dropped into the rootfs sysroot by the
//! `kernel-image-*` package. This command:
//!
//!   1. Locates the uncompressed kernel binary under
//!      `$AVOCADO_PREFIX/rootfs/boot/` (preferring `Image-<kver>` over
//!      compressed `*.gz`, skipping the metadata `System.map-*` and
//!      `config-*` siblings).
//!   2. When `kernel.image.type: kab` is set in avocado.yaml, wraps it
//!      into a signed `.kab` using the SDK's `kabtool` — same recipe
//!      `runtime build` uses, via the shared helper in `utils::kab_wrap`.
//!   3. `docker cp`s the produced artifact (the `.kab` when wrapping,
//!      otherwise the raw kernel binary) to the host directory passed
//!      via `--out`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::{
    config::{get_ext_image_args, get_ext_image_type, Config},
    container::{RunConfig, SdkContainer},
    host_copy::copy_volume_path_to_host,
    kab_wrap::generate_kab_wrap_script,
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    target::resolve_target_required,
};

pub struct KernelImageCommand {
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    out_dir: Option<String>,
}

impl KernelImageCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            target,
            container_args,
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

        print_info("Building kernel image.", OutputLevel::Normal);

        // Honor per-target `target-<name>:` overrides inside the `kernel:`
        // section (e.g. a custom `--tag`). Apply the same override merge
        // extensions use, but on the already-composed value so path-based
        // sources (merge_path_based_image_sections) are preserved.
        let kernel_merged = composed
            .merged_value
            .get("kernel")
            .cloned()
            .map(|v| config.resolve_overrides_in_value(v, &target_arch, None, "kernel"));
        let kernel_node = kernel_merged.as_ref();
        let image_type = kernel_node
            .and_then(get_ext_image_type)
            .unwrap_or_else(|| "raw".to_string());
        let image_args = kernel_node.and_then(get_ext_image_args);
        let wrap_kab = image_type == "kab";

        let kab_keyset_host_path: Option<String> = if wrap_kab {
            let p = std::env::var("KAB_KEYSET_FILE").map_err(|_| {
                anyhow::anyhow!(
                    "kernel.image.type is `kab` but KAB_KEYSET_FILE is not set. \
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
                .context("kernel.image.type is `kab` but kernel.image.args is missing")?;
            generate_kab_wrap_script("kernel", "AVOCADO_KERNEL_IMAGE", args, "$RUNTIME_VERSION")
        } else {
            String::new()
        };

        // Locate the uncompressed kernel binary in the rootfs sysroot
        // boot directory and copy it into $OUTPUT_DIR. Skip System.map /
        // config metadata + .gz variants. Mirrors how runtime/build.rs
        // computes AVOCADO_KERNEL_IMAGE for the wrap step.
        let internal_output_dir = "$AVOCADO_PREFIX/output/images";
        let locate_section = r#"
ROOTFS_BOOT="$AVOCADO_PREFIX/rootfs/boot"
if [ ! -d "$ROOTFS_BOOT" ]; then
    echo "ERROR: rootfs sysroot has no /boot — run 'avocado rootfs install' first" >&2
    exit 1
fi
KERNEL_SRC=""
for f in "$ROOTFS_BOOT"/Image "$ROOTFS_BOOT"/Image-*; do
    [ -f "$f" ] || continue
    case "$f" in
        *.gz|*/System.map-*|*/config-*) continue ;;
    esac
    KERNEL_SRC="$f"
    break
done
if [ -z "$KERNEL_SRC" ]; then
    echo "ERROR: no uncompressed kernel image found in $ROOTFS_BOOT" >&2
    exit 1
fi
KERNEL_BASENAME=$(basename "$KERNEL_SRC")
AVOCADO_KERNEL_IMAGE="$OUTPUT_DIR/$KERNEL_BASENAME"
cp -f "$KERNEL_SRC" "$AVOCADO_KERNEL_IMAGE"
export AVOCADO_KERNEL_IMAGE
echo "Staged kernel image: $AVOCADO_KERNEL_IMAGE"
"#;

        let script = format!(
            r#"set -euo pipefail
TARGET_ARCH="{target_arch}"
RUNTIME_NAME="${{AVOCADO_RUNTIME_NAME:-standalone}}"
RUNTIME_VERSION="${{AVOCADO_RUNTIME_VERSION:-0.0.0}}"
OUTPUT_DIR="{internal_output_dir}"
mkdir -p "$OUTPUT_DIR"
{locate_section}
AVOCADO_OS_VERSION_ID=""
if [ -f "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" ]; then
    AVOCADO_OS_VERSION_ID=$(grep '^VERSION_ID=' "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" \
        | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//")
fi
export AVOCADO_OS_VERSION_ID
{wrap_section}
echo "KERNEL_BASENAME=$KERNEL_BASENAME" > "$OUTPUT_DIR/.kernel-basename"
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
            return Err(anyhow::anyhow!("Failed to build kernel image."));
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

            // Read the kernel basename the in-container script wrote.
            let basename_marker =
                format!("/opt/_avocado/{target_arch}/output/images/.kernel-basename");
            let basename_local = host_dir.join(".kernel-basename");
            copy_volume_path_to_host(
                &container_helper.container_tool,
                volume_name,
                &basename_marker,
                &basename_local,
            )
            .await
            .context("Failed to read kernel basename marker")?;
            let kernel_basename = std::fs::read_to_string(&basename_local)
                .context("Failed to read kernel basename marker file")?
                .trim()
                .strip_prefix("KERNEL_BASENAME=")
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("Kernel basename marker has unexpected shape"))?;
            let _ = std::fs::remove_file(&basename_local);

            // When wrapping, the kab is the final artifact — skip the
            // raw kernel binary (intermediate, stays in the volume).
            let (host_filename, container_path) = if wrap_kab {
                let kab_filename = format!("{kernel_basename}.kab");
                (
                    kab_filename.clone(),
                    format!("/opt/_avocado/{target_arch}/output/images/{kab_filename}"),
                )
            } else {
                (
                    kernel_basename.clone(),
                    format!("/opt/_avocado/{target_arch}/output/images/{kernel_basename}"),
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

        print_success("Built kernel image.", OutputLevel::Normal);
        Ok(())
    }
}
