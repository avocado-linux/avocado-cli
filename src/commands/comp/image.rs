//! `avocado comp image <name>` — wrap an already-built component payload in a
//! KAB, signed with `KAB_KEYSET_FILE`. Outputs a `.kab` file to `--out`.
//!
//! This is the standalone counterpart to the inline component-wrapping step
//! in `avocado runtime build`. It assumes the role-appropriate payload has
//! already been built by the existing primitives:
//!
//! - `role: basefs`     → `avocado rootfs install` + `avocado rootfs image`
//! - `role: initramfs`  → `avocado initramfs install` + `avocado initramfs image`
//! - `role: kernel`     → not yet wired (the kernel-binary path convention
//!                        still needs to be decided; see kernel_amf.md).
//!
//! The standalone build script is thus a short sequence:
//!
//!     avocado sdk install
//!     avocado rootfs install
//!     avocado rootfs image
//!     avocado comp image avocado-comp-rootfs --out ./build
//!
//! Mirroring the `ext build_kab.sh` pattern but without replicating the full
//! install/build pipelines separately for components.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::{
    config::{
        get_comp_role, get_ext_image_args, get_ext_image_type, ComponentRole, ComposedConfig,
        Config,
    },
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target_required,
};

pub struct CompImageCommand {
    name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    out_dir: Option<String>,
    container_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    composed_config: Option<Arc<ComposedConfig>>,
}

impl CompImageCommand {
    pub fn new(
        name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            name,
            config_path,
            verbose,
            target,
            out_dir: None,
            container_args,
            sdk_arch: None,
            composed_config: None,
        }
    }

    pub fn with_output_dir(mut self, out_dir: Option<String>) -> Self {
        self.out_dir = out_dir;
        self
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // --- Resolve component + role + KAB args ---
        let comp_val = parsed
            .get("components")
            .and_then(|c| c.get(&self.name))
            .with_context(|| {
                format!("Component '{}' not found in config.", self.name)
            })?;

        let role = get_comp_role(comp_val).with_context(|| {
            format!(
                "Component '{}' has no valid role (expected one of: basefs, initramfs, kernel, other)",
                self.name
            )
        })?;

        let image_type = get_ext_image_type(comp_val).unwrap_or_default();
        if image_type != "kab" {
            return Err(anyhow::anyhow!(
                "Component '{}' has image.type '{}'; `comp image` only wraps KAB images",
                self.name,
                if image_type.is_empty() { "<unset>" } else { &image_type }
            ));
        }

        let image_args = get_ext_image_args(comp_val).unwrap_or_default();
        let version = comp_val
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();

        // --- Decide which already-built payload to wrap ---
        //
        // For basefs/initramfs the payload is the output of a prior
        // `rootfs image` / `initramfs image` step. For kernel, the payload
        // is a plain kernel binary dropped into the rootfs sysroot by the
        // `kernel-image-*` package (via `avocado rootfs install`) — no
        // separate image-build step is needed. The glob list tries the
        // common aarch64/x86 filenames in order.
        let (payload_glob, role_str, prereq_hint) = match role {
            ComponentRole::Basefs => (
                "$AVOCADO_PREFIX/output/images/avocado-image-rootfs-$TARGET_ARCH.erofs-*",
                "basefs",
                "run 'avocado rootfs install' + 'avocado rootfs image' first",
            ),
            ComponentRole::Initramfs => (
                "$AVOCADO_PREFIX/output/images/avocado-image-initramfs-$TARGET_ARCH.cpio*",
                "initramfs",
                "run 'avocado initramfs install' + 'avocado initramfs image' first",
            ),
            ComponentRole::Kernel => (
                // Common Yocto kernel install paths; first alphabetical match wins.
                // `ls` sorts across arguments, so on aarch64 `/boot/Image` comes
                // ahead of `vmlinuz`/`zImage` when multiple exist.
                "$AVOCADO_PREFIX/rootfs/boot/Image* $AVOCADO_PREFIX/rootfs/boot/vmlinuz* $AVOCADO_PREFIX/rootfs/boot/zImage*",
                "kernel",
                "run 'avocado rootfs install' with a kernel-image-* package in rootfs.packages first",
            ),
            ComponentRole::Other => {
                return Err(anyhow::anyhow!(
                    "`comp image` for role=other has no standard payload",
                ));
            }
        };

        // --- Require KAB_KEYSET_FILE on host + bind-mount it into the container ---
        let keyset_host_path = std::env::var("KAB_KEYSET_FILE").map_err(|_| {
            anyhow::anyhow!(
                "Component '{}' has image.type: kab but KAB_KEYSET_FILE is not set. \
                 Export it to the path of your KAB signing keyset.",
                self.name,
            )
        })?;
        if !std::path::Path::new(&keyset_host_path).is_file() {
            return Err(anyhow::anyhow!(
                "KAB_KEYSET_FILE points to '{}' but the file does not exist.",
                keyset_host_path,
            ));
        }

        // --- Resolve --out directory on host (create if missing) ---
        let out_spec = self
            .out_dir
            .as_ref()
            .context("`--out <dir>` is required for `comp image`")?;
        let host_out = if out_spec.starts_with('/') {
            PathBuf::from(out_spec)
        } else {
            std::env::current_dir()?.join(out_spec)
        };
        std::fs::create_dir_all(&host_out)
            .with_context(|| format!("Failed to create output dir: {}", host_out.display()))?;
        let host_out_str = host_out
            .to_str()
            .context("--out dir path is not valid UTF-8")?
            .to_string();

        // --- Container image + helper ---
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // --- Container args: base + keyset + out dir bind-mounts ---
        let mut merged_container_args = config
            .merge_sdk_container_args(self.container_args.as_ref())
            .unwrap_or_default();
        merged_container_args.push("-v".to_string());
        merged_container_args.push(format!("{keyset_host_path}:/tmp/kab.keyset:ro"));
        merged_container_args.push("-v".to_string());
        merged_container_args.push(format!("{host_out_str}:/tmp/comp-out:rw"));

        let mut env_vars = std::collections::HashMap::new();
        env_vars.insert(
            "KAB_KEYSET_FILE".to_string(),
            "/tmp/kab.keyset".to_string(),
        );

        let kab_filename = format!("{}-{}.kab", self.name, version);

        // --- Shell script: find payload → wrap in KAB → copy to /tmp/comp-out ---
        // image_args is passed verbatim (user-authored, template-resolved by load_composed).
        let script = format!(
            r#"set -euo pipefail
TARGET_ARCH="{target_arch}"

PAYLOAD=$(ls {payload_glob} 2>/dev/null | head -1 || true)
if [ -z "${{PAYLOAD}}" ] || [ ! -f "${{PAYLOAD}}" ]; then
    echo "ERROR: No {role_str} payload found. Looked at: {payload_glob}" >&2
    echo "       Hint: {prereq_hint}." >&2
    exit 1
fi
echo "Wrapping component '{name}' (role={role_str}) from ${{PAYLOAD}}"

KAB_TMPDIR=$(mktemp -d)
trap 'rm -rf "$KAB_TMPDIR"' EXIT

cp "${{PAYLOAD}}" "$KAB_TMPDIR/layer.img"
cat > "$KAB_TMPDIR/descriptor.json" << 'DESCEOF'
{{"kos":{{"build":{{"source":"{name}-{version}"}}}}}}
DESCEOF

(cd "$KAB_TMPDIR" && zip -Z store tmp.zip layer.img descriptor.json) > /dev/null

kabtool {image_args} -k "$KAB_KEYSET_FILE" -z "$KAB_TMPDIR/tmp.zip" "$KAB_TMPDIR/output.kab"

mkdir -p /tmp/comp-out
cp "$KAB_TMPDIR/output.kab" "/tmp/comp-out/{kab_filename}"
echo "Created component KAB: /tmp/comp-out/{kab_filename}"
"#,
            name = self.name,
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
            container_args: Some(merged_container_args),
            dnf_args: None,
            env_vars: Some(env_vars),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        print_info(
            &format!("Building component KAB '{}'.", self.name),
            OutputLevel::Normal,
        );

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to build component KAB for '{}'",
                self.name
            ));
        }

        print_success(
            &format!(
                "Wrote {}/{}",
                host_out.display(),
                kab_filename,
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
}
