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

use crate::commands::rootfs::image::{
    render_auth_files_hash, render_hook_block, resolve_install_hooks, NAMESPACE_UUID,
};

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

    # Compute deterministic build ID for initramfs.
    #
    # LC_ALL=C for the same reason as the cpio pipeline below: this hash becomes
    # $INITRAMFS_BUILD_ID, which is appended to initrd-release and
    # os-release-initrd *inside* the archive. Collation drift would reorder the
    # NEVRA list, change the hash, and so change the archive contents for an
    # otherwise unchanged package set.
    INITRAMFS_PKG_NEVRA=$(rpm -qa --queryformat '%{{NEVRA}}\n' --root "$INITRAMFS_SYSROOT" | LC_ALL=C sort)
    INITRAMFS_PKG_HASH=$(echo "$INITRAMFS_PKG_NEVRA" | sha256sum | awk '{{print $1}}')

    # Fold the assembled auth files into the build id so a `permissions:`-only
    # change — which the NEVRA set above is blind to — still moves the id and
    # OTAs (ENG-2437). See render_auth_files_hash.
{auth_hash_block}

    INITRAMFS_BUILD_ID=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('{namespace_uuid}'), '$INITRAMFS_PKG_HASH:$INITRAMFS_AUTH_HASH'))")

    # Inject identity into initrd-release and os-release-initrd
    if [ -f "$INITRAMFS_WORK/usr/lib/initrd-release" ]; then
        echo "AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID" >> "$INITRAMFS_WORK/usr/lib/initrd-release"
    fi
    if [ -f "$INITRAMFS_WORK/usr/lib/os-release-initrd" ]; then
        echo "AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID" >> "$INITRAMFS_WORK/usr/lib/os-release-initrd"
    fi

    # Normalize mtimes across the staged tree so the cpio is reproducible.
    #
    # `cpio --reproducible` is only --ignore-devno --ignore-dirnlink
    # --renumber-inodes; it passes mtime straight through from the
    # filesystem. Most file mtimes come from RPM payloads and are already
    # stable, but everything *created* during install is stamped with
    # wall-clock time: every directory, the `usr/lib/opkg/alternatives/*`
    # links, depmod's `modules.*` output, systemd preset `*.wants/*` links,
    # plus the usrmerge symlinks and the release files this script appends
    # to above. A runtime build and a standalone build install at different
    # times, so those mtimes differ and the same package set produces a
    # different cpio — which kos_boot then reports as initramfs drift.
    #
    # -h stamps symlinks themselves instead of dereferencing to their
    # targets. touch on an existing entry doesn't perturb its parent
    # directory's mtime, so a single unordered pass is sufficient.
    echo "Normalizing initramfs mtimes to SOURCE_DATE_EPOCH=${{SOURCE_DATE_EPOCH:-0}}"
    find "$INITRAMFS_WORK" -print0 \
        | xargs -0r touch -h -d "@${{SOURCE_DATE_EPOCH:-0}}"

    # Build initramfs image using configured filesystem format.
    #
    # Reproducibility notes for the pipeline below:
    #   * `LC_ALL=C sort` — entry order is archive order (and, with
    #     --renumber-inodes, decides the inode numbers), so collation must not
    #     drift. Today's SDK ships only the C/POSIX locales, which makes this a
    #     no-op, but it stops the archive from changing if the image ever gains
    #     real locales or the CLI starts forwarding the host's LC_* vars.
    #   * `gzip -n` — belt-and-braces. gzip already writes MTIME=0 and no FNAME
    #     when it reads stdin (there is no input file to take them from), so
    #     this only matters if the pipeline is ever refactored to compress a
    #     file in place.
    #   * zstd/lz4 embed no timestamp, and both run single-threaded here.
    INITRAMFS_FS="{initramfs_filesystem}"
    INITRAMFS_OUTPUT="$OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.$INITRAMFS_FS"
    echo "Building initramfs image: $INITRAMFS_FS"
    case "$INITRAMFS_FS" in
        cpio)
            (cd "$INITRAMFS_WORK" && find . | LC_ALL=C sort | cpio --reproducible -o -H newc --quiet > "$INITRAMFS_OUTPUT")
            ;;
        cpio.zst)
            (cd "$INITRAMFS_WORK" && find . | LC_ALL=C sort | cpio --reproducible -o -H newc --quiet | zstd -3 -f -o "$INITRAMFS_OUTPUT")
            ;;
        cpio.lz4)
            (cd "$INITRAMFS_WORK" && find . | LC_ALL=C sort | cpio --reproducible -o -H newc --quiet | lz4 -l -f - "$INITRAMFS_OUTPUT")
            ;;
        cpio.gz)
            (cd "$INITRAMFS_WORK" && find . | LC_ALL=C sort | cpio --reproducible -o -H newc --quiet | gzip -9 -n > "$INITRAMFS_OUTPUT")
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
        auth_hash_block = render_auth_files_hash("INITRAMFS_WORK", "INITRAMFS_AUTH_HASH"),
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
        // section (e.g. a custom `--tag`). Resolved on the already-composed
        // value so path-based sources (merge_path_based_image_sections) are
        // preserved.
        let initramfs_merged =
            config.resolve_image_section(&composed.merged_value, "initramfs", &target_arch);
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
        // Reproducibility stamp. Inert for now — the generated initramfs script
        // has no reader yet, since the mtime-normalization step that consumes
        // the epoch lands separately. Set anyway so every image-building run
        // carries the same env.
        crate::utils::container::inject_source_date_epoch(&mut env_vars, config.source_date_epoch);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The staged tree's mtimes must be normalized before the cpio is created.
    ///
    /// Regression: `cpio --reproducible` only covers devno / dirnlink / inode
    /// renumbering — it passes mtime through from the filesystem. Entries created
    /// during install (directories, `opkg/alternatives` links, depmod output,
    /// systemd preset links, the usrmerge symlinks) carry wall-clock mtimes, so a
    /// runtime build and a standalone build of the same package set produced
    /// byte-different cpios and kos_boot reported initramfs drift.
    #[test]
    fn test_initramfs_mtimes_normalized_before_cpio() {
        let script = generate_initramfs_build_script(
            "7488fa35-6390-425b-bbbf-b156cfe1eed2",
            "cpio.zst",
            None,
            "",
        );

        // Normalization is emitted, pinned to SOURCE_DATE_EPOCH (default 0 so it
        // is stable when the caller doesn't set one).
        assert!(script.contains(r#"touch -h -d "@${SOURCE_DATE_EPOCH:-0}""#));
        // -h so symlinks are stamped rather than their targets.
        assert!(script.contains("touch -h"));

        // It must run BEFORE the archive is created, or it normalizes nothing.
        let touch_at = script.find("touch -h").expect("normalization step present");
        let cpio_at = script
            .find("find . | LC_ALL=C sort | cpio --reproducible")
            .expect("cpio step present");
        assert!(
            touch_at < cpio_at,
            "mtime normalization must precede cpio creation"
        );

        // And after the release-file injection, so those appends are covered too.
        let inject_at = script
            .find("AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID")
            .expect("build-id injection present");
        assert!(
            inject_at < touch_at,
            "normalization must follow the release-file appends"
        );
    }

    /// Archive order is byte order, not locale collation. Entry order *is*
    /// archive order, and with `--renumber-inodes` it also decides the inode
    /// numbers, so a collation change would rewrite the whole archive.
    #[test]
    fn test_cpio_entry_order_is_locale_independent() {
        for fs in ["cpio", "cpio.zst", "cpio.lz4", "cpio.gz"] {
            let script = generate_initramfs_build_script("ns", fs, None, "");
            assert!(
                script.contains("find . | LC_ALL=C sort | cpio --reproducible"),
                "{fs}: sort must be pinned to the C locale"
            );
            assert!(
                !script.contains("find . | sort |"),
                "{fs}: unpinned sort left in the pipeline"
            );
        }
    }

    /// The build-ID hash is derived from a sorted NEVRA list, and the resulting
    /// $INITRAMFS_BUILD_ID is appended to release files *inside* the archive —
    /// so that sort needs the same C-locale pin as the cpio pipeline. Pinning
    /// only the cpio sort would leave the archive contents locale-sensitive.
    #[test]
    fn test_build_id_nevra_sort_is_locale_independent() {
        let script = generate_initramfs_build_script("ns", "cpio.zst", None, "");
        assert!(
            script.contains(r#"--root "$INITRAMFS_SYSROOT" | LC_ALL=C sort)"#),
            "the NEVRA sort feeding INITRAMFS_BUILD_ID must be pinned to LC_ALL=C"
        );
        // No unpinned `| sort` anywhere in the generated script.
        assert!(
            !script.contains("| sort)") && !script.contains("| sort |"),
            "an unpinned sort remains in the generated initramfs script"
        );
    }

    /// gzip already omits MTIME/FNAME when reading stdin, but `-n` keeps that
    /// true if the pipeline is ever changed to compress a file in place.
    #[test]
    fn test_gzip_omits_timestamp() {
        let script = generate_initramfs_build_script("ns", "cpio.gz", None, "");
        assert!(script.contains("gzip -9 -n"));
    }

    /// ENG-2437: a `permissions:`-only change must move the initramfs build
    /// id too, so the id has to fold the auth-file hash into its uuid5 input —
    /// the same way the rootfs build id does.
    #[test]
    fn test_build_id_folds_auth_files() {
        let marker = "# permissions placeholder";
        let script = generate_initramfs_build_script("ns", "cpio.zst", None, marker);

        assert!(
            script.contains("INITRAMFS_AUTH_HASH="),
            "build id must incorporate a hash of the auth files"
        );
        for f in ["passwd", "shadow", "group", "gshadow"] {
            assert!(
                script.contains(&format!("$INITRAMFS_WORK/etc/{f}")),
                "auth hash must cover /etc/{f}"
            );
        }
        assert!(
            script.contains("'$INITRAMFS_PKG_HASH:$INITRAMFS_AUTH_HASH'"),
            "uuid5 input must combine the package hash and the auth hash"
        );

        // The auth hash must be computed after the permissions section runs.
        let perms_pos = script.find(marker).expect("permissions section present");
        let auth_pos = script
            .find("INITRAMFS_AUTH_HASH=")
            .expect("auth hash present");
        assert!(
            perms_pos < auth_pos,
            "auth hash must be computed after the permissions section runs"
        );
    }

    /// Every compressed variant goes through the same normalized tree.
    #[test]
    fn test_all_cpio_formats_get_normalized_tree() {
        for fs in ["cpio", "cpio.zst", "cpio.lz4", "cpio.gz"] {
            let script = generate_initramfs_build_script("ns", fs, None, "");
            let touch_at = script.find("touch -h").expect("normalization present");
            let cpio_at = script
                .find("find . | LC_ALL=C sort | cpio --reproducible")
                .expect("cpio present");
            assert!(touch_at < cpio_at, "{fs}: normalization must precede cpio");
        }
    }
}
