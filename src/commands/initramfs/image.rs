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
    permissions::{mapping_from_map, render_users_groups_script},
    runs_on::RunsOnContext,
    target::resolve_target_required,
};

use crate::commands::rootfs::image::{
    render_build_id_block, render_build_state_purge, render_hook_block, resolve_install_hooks,
    BuildIdSpec, NAMESPACE_UUID,
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

/// Release files that may carry the initramfs identity.
///
/// The injection and the build-id strip must both use this list: a file that
/// can be written but is not stripped puts the previous build's id into the
/// tree hash, and the "deterministic" id then moves on every build.
///
/// `usr/lib/os-release` is here because `/etc/initrd-release` is a symlink to
/// it; the dedicated files need not exist.
const INITRAMFS_IDENTITY_FILES: &[&str] = &[
    "usr/lib/initrd-release",
    "usr/lib/os-release-initrd",
    "usr/lib/os-release",
];

/// Shell that writes `AVOCADO_OS_BUILD_ID` into the initrd's release files.
///
/// Follows `/etc/initrd-release` when it is a symlink, but writes only a target
/// that is itself one of [`INITRAMFS_IDENTITY_FILES`] resolved inside the work
/// tree: an absolute symlink must not reach the SDK's own os-release, and a
/// file the strip does not cover must not reach the hash.
///
/// Fails the build when nothing could be written, because an image with no
/// identity is unreadable to everything that reads the id back.
fn render_identity_injection(work_var: &str, id_var: &str) -> String {
    let loop_list = INITRAMFS_IDENTITY_FILES
        .iter()
        .map(|f| format!("\"$_avocado_work/{f}\""))
        .collect::<Vec<_>>()
        .join(" ");
    // Colon-delimited so the membership test is a plain glob; none of these
    // paths can contain a colon.
    let allowed = INITRAMFS_IDENTITY_FILES
        .iter()
        .map(|f| format!("$_avocado_work/{f}"))
        .collect::<Vec<_>>()
        .join(":");
    format!(
        r#"    # Inject identity into the initrd's release files (see render_identity_injection).
    # readlink -f canonicalizes, so the allowlist has to be canonical too: a
    # relative work dir, or one reached through a symlinked component, would
    # otherwise never match its own resolved paths and fail the build below.
    _avocado_work=$(readlink -f "${work_var}" 2>/dev/null || printf '%s' "${work_var}")
    _avocado_allowed=":{allowed}:"
    _avocado_identity_written=0
    for _avocado_f in {loop_list} "$_avocado_work/etc/initrd-release"; do
        [ -e "$_avocado_f" ] || continue
        _avocado_t=$(readlink -f "$_avocado_f") || continue
        case "$_avocado_allowed" in
            *":$_avocado_t:"*) ;;
            *) continue ;;
        esac
        grep -q '^AVOCADO_OS_BUILD_ID=' "$_avocado_t" && continue
        echo "AVOCADO_OS_BUILD_ID=${id_var}" >> "$_avocado_t" || exit 1
        _avocado_identity_written=1
    done
    if [ "$_avocado_identity_written" -eq 0 ]; then
        echo "ERROR: no release file in the initramfs can carry AVOCADO_OS_BUILD_ID (looked in $_avocado_allowed and $_avocado_work/etc/initrd-release). The image would ship with no identity: both the boot-time initramfs verification and /run/avocado/initramfs-build-id read it back from there." >&2
        exit 1
    fi
"#
    )
}

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
    var_encrypt: bool,
) -> String {
    let post = resolve_install_hooks(post_install, DEFAULT_INITRAMFS_POST_INSTALL);
    let post_install_block = render_hook_block("post_install", &post);
    // Encrypted /var is a per-runtime opt-in, but the initramfs sysroot is
    // shared across runtimes (cryptsetup-var is in all of them once one opts
    // in). This marker, written into THIS runtime's work copy, is what the
    // initrd (avocado-tegra-init & co.) keys on. Deliberately not
    // /etc/avocado-security-capabilities: that file states what the image was
    // built to support and is owned by the feed.
    //
    // Emitted AFTER post_install so a hook that rebuilds $INITRAMFS_WORK/etc
    // cannot drop it, and BEFORE the build id so the opt-in moves the id.
    let var_encrypt_block = if var_encrypt {
        r#"
    # Encrypted /var opt-in (avocado.yaml runtimes.<name>.var.encrypt).
    mkdir -p "$INITRAMFS_WORK/etc/avocado"
    echo "luks2" > "$INITRAMFS_WORK/etc/avocado/var-encrypt"
"#
    } else {
        ""
    };
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
{var_encrypt_block}
    # Compute the deterministic build id from the assembled work tree (see
    # render_build_id_block). Taken before the identity injection below so the
    # hash can't depend on the id it is about to write. LC_ALL=C throughout for
    # the same reason as the cpio pipeline: collation must not reorder the hash
    # inputs (the id lands in initrd-release / os-release-initrd inside the
    # archive, so a shift would change the archive for an unchanged tree).
    # Purge build-time state from the work copy before archiving, from the same
    # BUILD_STATE_PATHS list the build-id tree hash prunes.
    #
    # Runs BEFORE the build-id derivation so the tree hash covers exactly what
    # ships: this state is both nondeterministic and absent from the image, so
    # it must be gone before the id is taken.
    #
    # Same reasoning as the rootfs image — see the comment in
    # `generate_rootfs_build_script`, including why var/log is on neither list
    # and why this is necessary but not sufficient for reproducibility.
    # Measured 14MB of a 123MB qemux86-64 initramfs (2.5M rpmdb, 4.3M
    # var/lib/dnf, 6.4M var/cache/dnf), none of it read by anything in an
    # initrd. The default initramfs post_install runs no ldconfig, so
    # var/cache/ldconfig is normally absent here — it is still purged so a
    # custom post_install that does run ldconfig cannot ship an unhashed
    # aux-cache.
    #
    # Before the mtime normalization below on purpose: the purge restamps the
    # directories it empties, and the mtime pass is what makes that not matter.
    echo "Purging package-manager state from initramfs image"
    rm -rf {purge_paths}

{build_id_block}

{identity_injection}

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
        initramfs_filesystem = initramfs_filesystem,
        post_install_block = post_install_block,
        permissions_section = permissions_section,
        var_encrypt_block = var_encrypt_block,
        purge_paths = render_build_state_purge("INITRAMFS_WORK"),
        identity_injection = render_identity_injection("INITRAMFS_WORK", "INITRAMFS_BUILD_ID"),
        build_id_block = render_build_id_block(&BuildIdSpec {
            namespace_uuid,
            work_var: "INITRAMFS_WORK",
            sysroot_var: "INITRAMFS_SYSROOT",
            rpm_args: "",
            id_var: "INITRAMFS_BUILD_ID",
            identity_files: INITRAMFS_IDENTITY_FILES,
            // Unlike erofs, cpio has no --all-root: every newc header carries
            // uid/gid and nothing in this script chowns the tree, so ownership
            // reaches the image and must reach the id with it.
            hash_ownership: true,
        }),
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
                let users = mapping_from_map(p.users.as_ref());
                let groups = mapping_from_map(p.groups.as_ref());
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
            // Standalone image builds have no runtime to opt in.
            false,
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
            false,
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

    /// Regression: the staged initramfs carried 14MB of dnf/rpm state (2.5M
    /// rpmdb, 4.3M var/lib/dnf, 6.4M var/cache/dnf) into the cpio. Nothing in
    /// an initrd reads it, and it made the archive unreproducible — the rpmdb
    /// records INSTALLTIME/INSTALLTID per package and var/cache/dnf holds
    /// generated repodata, so the same package set produced different bytes.
    #[test]
    fn test_initramfs_purges_package_manager_state_before_archiving() {
        let script = generate_initramfs_build_script(
            "00000000-0000-0000-0000-000000000000",
            "cpio.zst",
            None,
            "",
            false,
        );

        // The initramfs shares the rootfs list, so purge and prune cannot
        // drift apart here either. Both sides are pinned as whole rendered
        // expressions — a per-path `contains` is only a subset check, so it
        // would pass on a list that gained an extra entry.
        let paths = crate::commands::rootfs::image::BUILD_STATE_PATHS;
        let prunes = paths
            .iter()
            .map(|p| format!("-path ./{p}"))
            .collect::<Vec<_>>()
            .join(" -o ");
        assert!(
            script.contains(&format!("find . \\( {prunes} \\) -prune")),
            "the tree hash must prune exactly BUILD_STATE_PATHS"
        );
        let purge = paths
            .iter()
            .map(|p| format!("\"$INITRAMFS_WORK/{p}\""))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            script.contains(&format!("rm -rf {purge}\n")),
            "the purge must remove exactly BUILD_STATE_PATHS"
        );

        // The purge only helps if it runs before the archive is created.
        let purge_at = script
            .find("Purging package-manager state")
            .expect("purge step present");
        let cpio_at = script
            .find("cpio --reproducible")
            .expect("cpio step present");
        assert!(purge_at < cpio_at, "purge must precede cpio creation");

        // And before the build-id derivation, so the tree hash covers exactly
        // what ships rather than dnf/rpm state that is about to be deleted.
        let tree_at = script.find("TREE_HASH=").expect("tree hash present");
        assert!(
            purge_at < tree_at,
            "purge must precede the build-id tree hash"
        );

        // dnf never writes its logs into an installroot (logdir is not
        // prefixed by prepend_installroot), so a log purge here would be a
        // no-op that reads as coverage. Pin its absence.
        assert!(
            !script.contains("var/log/dnf.log"),
            "the script must not claim to purge dnf logs that are never staged"
        );
    }

    /// Archive order is byte order, not locale collation. Entry order *is*
    /// archive order, and with `--renumber-inodes` it also decides the inode
    /// numbers, so a collation change would rewrite the whole archive.
    #[test]
    fn test_cpio_entry_order_is_locale_independent() {
        for fs in ["cpio", "cpio.zst", "cpio.lz4", "cpio.gz"] {
            let script = generate_initramfs_build_script("ns", fs, None, "", false);
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
        let script = generate_initramfs_build_script("ns", "cpio.zst", None, "", false);
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

    /// Initramfs derives its id through the same render_build_id_block as rootfs
    /// (NEVRA package hash + work-tree content hash, rpmdb pruned), so the two
    /// images can't diverge on the correctness-critical id logic.
    #[test]
    fn test_build_id_uses_shared_tree_hash() {
        let s = generate_initramfs_build_script("ns", "cpio.zst", None, "", false);
        assert!(s.contains("TREE_HASH="), "work-tree content hash present");
        assert!(s.contains("-path ./var/lib/rpm"), "rpmdb pruned");
        assert!(
            s.contains("INITRAMFS_BUILD_ID=$(python3"),
            "id assigned to the initramfs var"
        );
        assert!(
            s.contains("'$PKG_HASH:$TREE_HASH'"),
            "package + tree hash folded"
        );
        // Both initramfs identity files are canonicalized before hashing.
        assert!(s.contains(r#"[ -f "$INITRAMFS_WORK/usr/lib/initrd-release" ]"#));
        assert!(s.contains(r#"[ -f "$INITRAMFS_WORK/usr/lib/os-release-initrd" ]"#));
    }

    /// The initramfs hash covers uid/gid; the rootfs hash must not. The two
    /// derivations share `render_build_id_block` on purpose, so the exclusion
    /// was shared too — and it was only ever right for one of them. Measured:
    /// two trees differing only in one file's owner produce identical erofs
    /// bytes (`--all-root` flattens it) but different cpio bytes, because
    /// `cpio --reproducible` is just `--ignore-devno --ignore-dirnlink
    /// --renumber-inodes` and every newc header still carries uid/gid. So the
    /// initramfs shipped a changed image under an unchanged id, and the OTA
    /// pipeline — which compares ids — never offered it.
    ///
    /// Pinned as a pairing rather than two independent asserts: what makes
    /// either side correct is that it matches its own image format. Flipping
    /// one without the other silently reintroduces the bug in one direction or
    /// spurious every-build OTAs in the other.
    #[test]
    fn test_ownership_is_hashed_iff_the_image_records_it() {
        let initramfs = generate_initramfs_build_script("ns", "cpio.zst", None, "", false);
        let rootfs = crate::commands::rootfs::image::generate_rootfs_build_script(
            "ns",
            "erofs-lz4",
            None,
            "",
            false,
        );

        assert!(
            initramfs.contains(r"-printf '%y %m %U %G %P\t%l\n'"),
            "cpio stores uid/gid, so the initramfs tree hash must cover it"
        );
        assert!(
            !initramfs.contains("--all-root") && !initramfs.contains("--owner"),
            "nothing normalizes initramfs ownership — if that changes, stop hashing it"
        );

        assert!(
            rootfs.contains(r"-printf '%y %m %P\t%l\n'"),
            "mkfs.erofs --all-root flattens ownership, so the rootfs must not hash it"
        );
        assert_eq!(
            rootfs.matches("--all-root").count(),
            2,
            "the rootfs exclusion is only correct while both mkfs.erofs branches normalize"
        );
    }

    /// gzip already omits MTIME/FNAME when reading stdin, but `-n` keeps that
    /// true if the pipeline is ever changed to compress a file in place.
    #[test]
    fn test_gzip_omits_timestamp() {
        let script = generate_initramfs_build_script("ns", "cpio.gz", None, "", false);
        assert!(script.contains("gzip -9 -n"));
    }

    /// A `permissions:`-only change must move the initramfs build id. Under
    /// the tree-hash derivation the auth files are ordinary tree content, so
    /// the guarantee holds iff the tree hash is
    /// computed after the permissions section runs and `/etc` is never on the
    /// prune list.
    /// The var-encrypt marker is the initrd's switch, so it must be the last
    /// word: after a user post_install (which may rebuild /etc) and before the
    /// build id (so opting in moves the id).
    #[test]
    fn test_var_encrypt_marker_follows_post_install_and_precedes_build_id() {
        let hook = "rm -rf \"$INITRAMFS_WORK/etc\" # hook placeholder";
        let script = generate_initramfs_build_script("ns", "cpio.zst", Some(hook), "", true);
        let marker = "echo \"luks2\" > \"$INITRAMFS_WORK/etc/avocado/var-encrypt\"";
        let hook_at = script.find(hook).expect("post_install present");
        let marker_at = script.find(marker).expect("marker present");
        let tree_at = script.find("TREE_HASH=").expect("tree hash present");
        assert!(
            hook_at < marker_at,
            "marker must be written after post_install"
        );
        assert!(
            marker_at < tree_at,
            "marker must be hashed into the build id"
        );

        let off = generate_initramfs_build_script("ns", "cpio.zst", Some(hook), "", false);
        assert!(!off.contains("var-encrypt"), "unset writes no marker");
    }

    #[test]
    fn test_build_id_sees_permissions_changes() {
        let marker = "# permissions placeholder";
        let script = generate_initramfs_build_script("ns", "cpio.zst", None, marker, false);

        let perms_pos = script.find(marker).expect("permissions section present");
        let tree_pos = script
            .find("INITRAMFS_TREE_HASH=")
            .or_else(|| script.find("TREE_HASH="))
            .expect("tree hash present");
        assert!(
            perms_pos < tree_pos,
            "tree hash must be computed after the permissions section runs"
        );
        assert!(
            !script.contains("-path ./etc"),
            "/etc must never be pruned from the tree hash — it is what carries permissions changes into the id"
        );
    }

    /// Every compressed variant goes through the same normalized tree.
    #[test]
    fn test_all_cpio_formats_get_normalized_tree() {
        for fs in ["cpio", "cpio.zst", "cpio.lz4", "cpio.gz"] {
            let script = generate_initramfs_build_script("ns", fs, None, "", false);
            let touch_at = script.find("touch -h").expect("normalization present");
            let cpio_at = script
                .find("find . | LC_ALL=C sort | cpio --reproducible")
                .expect("cpio present");
            assert!(touch_at < cpio_at, "{fs}: normalization must precede cpio");
        }
    }
}

#[cfg(all(test, unix))]
mod identity_injection_tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// Run the rendered injection against a fake work tree and report what the
    /// release files ended up containing.
    fn run_injection(build: impl Fn(&std::path::Path)) -> (bool, String, Vec<(String, String)>) {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        fs::create_dir_all(work.join("usr/lib")).unwrap();
        fs::create_dir_all(work.join("etc")).unwrap();
        build(&work);
        let script = format!(
            "set -u\nINITRAMFS_WORK={}\nINITRAMFS_BUILD_ID=initramfs-xyz\n{}",
            work.display(),
            render_identity_injection("INITRAMFS_WORK", "INITRAMFS_BUILD_ID")
        );
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        let contents = INITRAMFS_IDENTITY_FILES
            .iter()
            .filter_map(|f| {
                fs::read_to_string(work.join(f))
                    .ok()
                    .map(|c| (f.to_string(), c))
            })
            .collect();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            contents,
        )
    }

    #[test]
    fn the_id_reaches_the_file_etc_initrd_release_points_at() {
        // The layout that actually ships: no usr/lib/initrd-release, and
        // /etc/initrd-release is a symlink to the initrd's own os-release. The
        // guarded appends this replaces wrote nothing here, silently.
        let (ok, stderr, files) = run_injection(|work| {
            fs::write(work.join("usr/lib/os-release"), "ID=avocado\n").unwrap();
            std::os::unix::fs::symlink("../usr/lib/os-release", work.join("etc/initrd-release"))
                .unwrap();
        });
        assert!(ok, "injection failed: {stderr}");
        let os_release = files
            .iter()
            .find(|(f, _)| f == "usr/lib/os-release")
            .expect("os-release still present");
        assert!(
            os_release.1.contains("AVOCADO_OS_BUILD_ID=initramfs-xyz"),
            "id did not reach the symlink target: {:?}",
            os_release.1
        );
    }

    #[test]
    fn a_dedicated_initrd_release_still_gets_the_id_exactly_once() {
        let (ok, stderr, files) = run_injection(|work| {
            fs::write(work.join("usr/lib/initrd-release"), "ID=avocado\n").unwrap();
            std::os::unix::fs::symlink(
                "../usr/lib/initrd-release",
                work.join("etc/initrd-release"),
            )
            .unwrap();
        });
        assert!(ok, "injection failed: {stderr}");
        let c = &files
            .iter()
            .find(|(f, _)| f == "usr/lib/initrd-release")
            .unwrap()
            .1;
        assert_eq!(
            c.matches("AVOCADO_OS_BUILD_ID=").count(),
            1,
            "the symlink and its target must not both append: {c:?}"
        );
    }

    #[test]
    fn a_work_root_reached_through_a_symlink_still_matches() {
        // readlink -f canonicalizes the target, so an allowlist built from a
        // non-canonical work root would never match and every build would fail
        // with "no release file". Reproduces a symlinked build directory.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real-work");
        fs::create_dir_all(real.join("usr/lib")).unwrap();
        fs::create_dir_all(real.join("etc")).unwrap();
        fs::write(real.join("usr/lib/os-release"), "ID=avocado\n").unwrap();
        std::os::unix::fs::symlink("../usr/lib/os-release", real.join("etc/initrd-release"))
            .unwrap();
        let via_link = tmp.path().join("work-link");
        std::os::unix::fs::symlink(&real, &via_link).unwrap();

        let script = format!(
            "set -u\nINITRAMFS_WORK={}\nINITRAMFS_BUILD_ID=initramfs-xyz\n{}",
            via_link.display(),
            render_identity_injection("INITRAMFS_WORK", "INITRAMFS_BUILD_ID")
        );
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(
            out.status.success(),
            "symlinked work root rejected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(fs::read_to_string(real.join("usr/lib/os-release"))
            .unwrap()
            .contains("AVOCADO_OS_BUILD_ID=initramfs-xyz"));
    }

    #[test]
    fn an_absolute_symlink_out_of_the_tree_is_refused() {
        // Appending to the SDK's own /usr/lib/os-release would corrupt the
        // build container, not the image.
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("os-release");
        fs::write(&victim, "ID=sdk\n").unwrap();
        let (ok, stderr, _) = run_injection(|work| {
            std::os::unix::fs::symlink(&victim, work.join("etc/initrd-release")).unwrap();
        });
        assert!(!ok, "a build with nowhere safe to write must fail closed");
        assert!(stderr.contains("no release file"), "{stderr}");
        assert!(
            !fs::read_to_string(&victim)
                .unwrap()
                .contains("AVOCADO_OS_BUILD_ID"),
            "wrote outside the work tree"
        );
    }

    #[test]
    fn no_release_file_at_all_fails_the_build() {
        let (ok, stderr, _) = run_injection(|_| {});
        assert!(!ok, "shipping an initramfs with no identity must not pass");
        assert!(stderr.contains("AVOCADO_OS_BUILD_ID"), "{stderr}");
    }

    #[test]
    fn every_injected_file_is_also_stripped_before_hashing() {
        // If the injection can write a file the strip does not clear, the next
        // build hashes this build's id and the id moves on every rebuild.
        //
        // The expectation is read out of the INJECTION's own rendered allowlist
        // rather than from INITRAMFS_IDENTITY_FILES: both sides derive from that
        // const today, so a test that iterates it cannot fail. What can actually
        // break is the two drifting apart - render_build_id_block being handed a
        // different list - and that is what this catches.
        let script = generate_initramfs_build_script("ns", "cpio.zst", None, "", false);
        let allowed_line = script
            .lines()
            .find(|l| l.trim_start().starts_with("_avocado_allowed="))
            .expect("the injection renders an allowlist");
        let injectable: Vec<String> = allowed_line
            .trim()
            .trim_start_matches("_avocado_allowed=\"")
            .trim_end_matches('"')
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_start_matches("$_avocado_work/").to_string())
            .collect();
        assert!(
            !injectable.is_empty(),
            "parsed no injectable paths from: {allowed_line}"
        );

        let stanza = |f: &str| {
            format!(
                "if [ -f \"$INITRAMFS_WORK/{f}\" ]; then\n        sed -i \
                 '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' \
                 \"$INITRAMFS_WORK/{f}\"\n    fi"
            )
        };
        for f in &injectable {
            assert!(
                script.contains(&stanza(f)),
                "{f} can be injected but is not stripped before the hash"
            );
        }
        // The assertion must be capable of failing.
        assert!(
            !script.contains(&stanza("usr/lib/not-an-identity-file")),
            "the stanza check matches a file that is not in the strip list"
        );
    }
}
