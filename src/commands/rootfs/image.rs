//! Rootfs image build command and shared build script generation.

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

/// Namespace UUID for deterministic OS build ID generation (shared with runtime build).
pub const NAMESPACE_UUID: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

/// Default post-install commands for the rootfs build. Run on the work
/// directory (`$ROOTFS_WORK`) after package install + overlay, before
/// the identity stamp and `mkfs.erofs`. These mirror what Yocto's
/// `ROOTFS_POSTPROCESS_COMMAND` + `image.bbclass` would do, in the
/// minimum form needed for a bootable avocado rootfs.
///
/// Used as a fallback only when the user does NOT define `pre_install`
/// or `post_install` in the rootfs config. If the user defines either,
/// they take full control and this default list is skipped entirely.
pub const DEFAULT_ROOTFS_POST_INSTALL: &[&str] = &[
    // usrmerge symlinks (Yocto image class does this, not any RPM package).
    "ln -sfn usr/bin \"$ROOTFS_WORK/bin\"",
    "ln -sfn usr/sbin \"$ROOTFS_WORK/sbin\"",
    "ln -sfn usr/lib \"$ROOTFS_WORK/lib\"",
    // Strip dirs that Yocto's avocado-image-rootfs.bb also stripped.
    "rm -rf \"$ROOTFS_WORK/media\" \"$ROOTFS_WORK/mnt\" \"$ROOTFS_WORK/srv\"",
    "rm -rf \"$ROOTFS_WORK/boot/\"*",
    "mkdir -p \"$ROOTFS_WORK/opt\"",
    // Empty /etc/machine-id for stateless systemd on read-only rootfs.
    "touch \"$ROOTFS_WORK/etc/machine-id\"",
    // systemd preset-all (matches image.bbclass systemd_preset_all).
    "if [ -e \"$ROOTFS_WORK/usr/lib/systemd/systemd\" ]; then \
\"$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/systemctl\" --root=\"$ROOTFS_WORK\" \
--preset-mode=enable-only preset-all 2>/dev/null || true; \
echo \"Applied systemd presets\"; fi",
    // ld.so.cache generation (matches Yocto ldconfig-native).
    "/usr/sbin/ldconfig -r \"$ROOTFS_WORK\" -c new -X 2>/dev/null || true",
    "echo \"Generated ld.so.cache\"",
];

/// Render a list of user-supplied shell commands as an indented block,
/// preceded by a one-line "Running … hooks" echo for log clarity. Empty
/// input returns an empty string so the surrounding script stays clean.
pub fn render_hook_block(name: &str, hooks: &[String]) -> String {
    if hooks.is_empty() {
        return String::new();
    }
    let header = format!(
        "    echo \"Running {} hooks ({} command(s))...\"",
        name,
        hooks.len()
    );
    let body = hooks
        .iter()
        .map(|h| format!("    {h}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n{body}")
}

/// Resolve a user-provided `post_install` script path into the shell
/// command(s) to splice into the build script.
///
/// - `Some(path)` → emit one guarded `bash /opt/src/<path>` invocation.
///   Defaults are skipped — the script takes full responsibility for
///   all post-install transformations. Mirrors the pattern upstream's
///   runtime `post_build` uses.
/// - `None` → fall back to `defaults`.
pub fn resolve_install_hooks(post_install_script: Option<&str>, defaults: &[&str]) -> Vec<String> {
    match post_install_script {
        Some(path) => vec![format!(
            "if [ -f '/opt/src/{path}' ]; then \
echo 'Running post_install script: {path}'; \
bash '/opt/src/{path}'; \
else \
echo 'post_install script /opt/src/{path} not found.'; \
exit 1; \
fi"
        )],
        None => defaults.iter().map(|s| s.to_string()).collect(),
    }
}

/// Work-relative paths pruned from the build-id tree hash: their bytes are
/// nondeterministic across builds and are either not part of the image identity
/// or already covered elsewhere.
/// - `var/lib/rpm`: the rpmdb sqlite embeds install timestamps — which is
///   exactly why package identity is hashed from the NEVRA set (`PKG_HASH`),
///   not these bytes.
/// - `var/cache`, `var/log`: dnf caches and logs, build-varying.
const BUILD_ID_TREE_PRUNES: &[&str] = &["./var/lib/rpm", "./var/cache", "./var/log"];

/// The differences between the rootfs and initramfs build-id derivations, fed to
/// [`render_build_id_block`].
pub struct BuildIdSpec<'a> {
    /// Namespace UUID for the uuid5 derivation.
    pub namespace_uuid: &'a str,
    /// Shell variable naming the assembled work dir (e.g. `ROOTFS_WORK`).
    pub work_var: &'a str,
    /// Shell variable naming the sysroot holding the rpmdb (e.g. `ROOTFS_SYSROOT`).
    pub sysroot_var: &'a str,
    /// Extra `rpm` args to locate the db — rootfs passes `--dbpath /var/lib/rpm`,
    /// initramfs the empty string (default path).
    pub rpm_args: &'a str,
    /// Shell variable to assign the derived id (e.g. `OS_BUILD_ID`).
    pub id_var: &'a str,
    /// Work-relative identity files to canonicalize before hashing (strip any
    /// AVOCADO_* fields a prior build injected), e.g. `usr/lib/os-release`.
    pub identity_files: &'a [&'a str],
}

/// Render the shell that derives a deterministic build id into `spec.id_var`.
///
/// The id is `uuid5(namespace, "$PKG_HASH:$TREE_HASH")`:
/// - `PKG_HASH` is the sorted NEVRA set — a stable package identity that does
///   not depend on the nondeterministic rpmdb *bytes* (so a version-only bump
///   with an identical file payload still moves the id).
/// - `TREE_HASH` is a content hash of the assembled work tree, so *any*
///   rootfs-affecting change — `permissions:`, `post_install`, `overlay:`,
///   anything future — moves the id and therefore OTAs. It hashes exactly what
///   the image carries: sorted path, type, mode, symlink target, and file
///   content. It excludes what the image build normalizes away (`mkfs.erofs -T`
///   mtimes, `--all-root` ownership) and what is fs-dependent (directory
///   sizes), so it can't churn on noise the image doesn't hold.
///   [`BUILD_ID_TREE_PRUNES`] are excluded.
///
/// Must be invoked AFTER the permissions and post_install steps and BEFORE the
/// os-release identity lines are appended: the derivation strips `AVOCADO_*`
/// from the identity files first, so the hash can't depend on a prior build's
/// id (self-reference) or the possibly-unpinned runtime version.
pub fn render_build_id_block(spec: &BuildIdSpec) -> String {
    let BuildIdSpec {
        namespace_uuid,
        work_var,
        sysroot_var,
        rpm_args,
        id_var,
        identity_files,
    } = *spec;

    // Strip any AVOCADO_* fields a prior build injected into the identity files
    // (the work copy inherits them from the sysroot) so the tree hash is stable.
    let strip = identity_files
        .iter()
        .map(|f| {
            format!(
                "    if [ -f \"${work_var}/{f}\" ]; then\n        \
                 sed -i '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' \"${work_var}/{f}\"\n    fi"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prune = BUILD_ID_TREE_PRUNES
        .iter()
        .map(|p| format!("-path {p}"))
        .collect::<Vec<_>>()
        .join(" -o ");

    format!(
        r#"    # Canonicalize the identity files before hashing (see render_build_id_block).
{strip}

    # Deterministic package identity from the NEVRA set — independent of the
    # rpmdb *bytes*, which embed install timestamps (hence var/lib/rpm is pruned
    # from the tree hash below). LC_ALL=C so collation can't reorder it.
    PKG_NEVRA=$(rpm {rpm_args} -qa --queryformat '%{{NEVRA}}\n' --root "${sysroot_var}" | LC_ALL=C sort)
    PKG_HASH=$(echo "$PKG_NEVRA" | sha256sum | awk '{{print $1}}')

    # Content hash of the assembled work tree: the id must move iff the image
    # bytes move. Hash only what the image carries — sorted path, type, mode,
    # symlink target, file content — excluding what the image build normalizes
    # out (mtime, ownership) or what is fs-dependent (directory sizes). %m is the
    # octal mode, %l the symlink target (empty for non-links).
    BUILD_ID_META=$(cd "${work_var}" && find . \( {prune} \) -prune -o -printf '%y %m %P\t%l\n' | LC_ALL=C sort)
    BUILD_ID_CONTENT=$(cd "${work_var}" && find . \( {prune} \) -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
    TREE_HASH=$(printf '%s\n%s\n' "$BUILD_ID_META" "$BUILD_ID_CONTENT" | sha256sum | awk '{{print $1}}')

    {id_var}=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('{namespace_uuid}'), '$PKG_HASH:$TREE_HASH'))")"#
    )
}

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
///
/// `post_install` is a project-relative script path (resolved against
/// `/opt/src` inside the SDK container). When set, the build splices
/// one guarded `bash /opt/src/<path>` invocation in place of the
/// default post-install commands. When `None`, the defaults run
/// (usrmerge symlinks, /mnt /media /srv cleanup, /etc/machine-id,
/// systemd preset, ld.so.cache — see `DEFAULT_ROOTFS_POST_INSTALL`).
///
/// Identity stamping (build_id + os-release injection) and `mkfs.erofs`
/// are always run as internal mechanics.
pub fn generate_rootfs_build_script(
    namespace_uuid: &str,
    rootfs_filesystem: &str,
    post_install: Option<&str>,
    permissions_section: &str,
) -> String {
    let post = resolve_install_hooks(post_install, DEFAULT_ROOTFS_POST_INSTALL);
    let post_install_block = render_hook_block("post_install", &post);
    format!(
        r#"
# Build rootfs image from shared sysroot.
# These vars are `export`ed so the post_install script (which we invoke
# as a child `bash` process) inherits them.
export ROOTFS_SYSROOT="$AVOCADO_PREFIX/rootfs"
if [ -d "$ROOTFS_SYSROOT/usr" ]; then
    echo "Building rootfs image from packages..."

    # Work on a copy so we don't mutate the shared sysroot used for extension priming
    export ROOTFS_WORK="${{ROOTFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/rootfs-work}}"
    # Standalone rootfs builds (no runtime build before this) leave the
    # parent runtimes/$RUNTIME_NAME dir uncreated; ensure it exists.
    mkdir -p "$(dirname "$ROOTFS_WORK")"
    rm -rf "$ROOTFS_WORK"
    cp -a "$ROOTFS_SYSROOT" "$ROOTFS_WORK"

    # A fully-installed sysroot always ships /etc/passwd. If it is absent
    # the build volume is half-populated or stale (e.g. a prior install
    # was interrupted, or the project dir was deleted without `avocado
    # clean`). Fail here with an actionable message rather than letting
    # the user-creation step below emit a cryptic
    # `grep: .../etc/passwd: No such file`.
    if [ ! -f "$ROOTFS_WORK/etc/passwd" ]; then
        echo "ERROR: rootfs staging at $ROOTFS_WORK is missing /etc/passwd. The build volume looks half-populated or stale." >&2
        echo "Reset the build state with 'avocado clean' and 'avocado prune', then re-run 'avocado install -f' and 'avocado build'." >&2
        exit 1
    fi
{permissions_section}

{post_install_block}

{build_id_block}

    # Inject identity into os-release (work copy for the image, sysroot for stone).
    # The work copy was canonicalized (AVOCADO_* stripped) during id derivation,
    # so these appends land in a clean file.
    echo "AVOCADO_OS_BUILD_ID=$OS_BUILD_ID" >> "$ROOTFS_WORK/usr/lib/os-release"
    echo "AVOCADO_RUNTIME_NAME=$RUNTIME_NAME" >> "$ROOTFS_WORK/usr/lib/os-release"
    echo "AVOCADO_RUNTIME_VERSION=$RUNTIME_VERSION" >> "$ROOTFS_WORK/usr/lib/os-release"

    # Also write AVOCADO_OS_BUILD_ID to the sysroot so stone bundle can read it
    sed -i '/^AVOCADO_OS_BUILD_ID=/d' "$ROOTFS_SYSROOT/usr/lib/os-release"
    echo "AVOCADO_OS_BUILD_ID=$OS_BUILD_ID" >> "$ROOTFS_SYSROOT/usr/lib/os-release"

    # Build rootfs image using configured filesystem format
    ROOTFS_FS="{rootfs_filesystem}"
    ROOTFS_OUTPUT="$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.$ROOTFS_FS"
    echo "Building rootfs image: $ROOTFS_FS"
    case "$ROOTFS_FS" in
        erofs-zst)
            mkfs.erofs \
                -T "${{SOURCE_DATE_EPOCH:-0}}" \
                -U 00000000-0000-0000-0000-000000000000 \
                -x -1 \
                --all-root \
                -z zstd \
                "$ROOTFS_OUTPUT" \
                "$ROOTFS_WORK"
            ;;
        erofs-lz4)
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
        rootfs_filesystem = rootfs_filesystem,
        post_install_block = post_install_block,
        permissions_section = permissions_section,
        build_id_block = render_build_id_block(&BuildIdSpec {
            namespace_uuid,
            work_var: "ROOTFS_WORK",
            sysroot_var: "ROOTFS_SYSROOT",
            rpm_args: "--dbpath /var/lib/rpm",
            id_var: "OS_BUILD_ID",
            identity_files: &["usr/lib/os-release"],
        }),
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
        // Honor per-target `target-<name>:` overrides inside the `rootfs:`
        // section (e.g. a custom `--tag`). Resolved on the already-composed
        // value so path-based rootfs sources (merge_path_based_image_sections)
        // are preserved.
        let rootfs_merged =
            config.resolve_image_section(&composed.merged_value, "rootfs", &target_arch);
        let rootfs_node = rootfs_merged.as_ref();
        let post_install = get_post_install(rootfs_node);
        let permissions_section = config
            .rootfs_default()
            .and_then(|img| config.resolve_image_permissions(img))
            .map(|p| {
                let users = mapping_from_map(p.users.as_ref());
                let groups = mapping_from_map(p.groups.as_ref());
                render_users_groups_script(
                    users.as_ref(),
                    groups.as_ref(),
                    "$ROOTFS_WORK/etc",
                    None,
                )
            })
            .unwrap_or_default();
        let build_section = generate_rootfs_build_script(
            NAMESPACE_UUID,
            &rootfs_filesystem,
            post_install.as_deref(),
            &permissions_section,
        );

        // If the avocado.yaml asks for a kab-wrapped rootfs, validate the
        // keyset on the host, append the wrap step to the script, and
        // bind-mount the keyset into the container at /tmp/kab.keyset.
        // Same plumbing as runtime/build.rs.
        let image_type = rootfs_node
            .and_then(get_ext_image_type)
            .unwrap_or_else(|| "raw".to_string());
        let image_args = rootfs_node.and_then(get_ext_image_args);
        let wrap_kab = image_type == "kab";

        let kab_keyset_host_path: Option<String> = if wrap_kab {
            let p = std::env::var("KAB_KEYSET_FILE").map_err(|_| {
                anyhow::anyhow!(
                    "rootfs.image.type is `kab` but KAB_KEYSET_FILE is not set. \
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
                .context("rootfs.image.type is `kab` but rootfs.image.args is missing")?;
            generate_kab_wrap_script("rootfs", "AVOCADO_ROOTFS_IMAGE", args, "$RUNTIME_VERSION")
        } else {
            String::new()
        };

        // Always produce inside the SDK volume; the user-facing --out
        // is treated as a host destination and gets `docker cp`'d to
        // after the container exits. This lets standalone callers see
        // the artifact on the host rather than leaving it stranded in
        // the container's overlay (the prior behavior).
        let internal_output_dir = "$AVOCADO_PREFIX/output/images";

        // After the rootfs build, expose AVOCADO_OS_VERSION_ID so kab
        // args can interpolate it (e.g. `-v "$AVOCADO_OS_VERSION_ID"`).
        // Runtime build does the same exact thing — keep the recipe
        // identical so callers can use the same args either way.
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

        // Bind-mount the keyset into the container as a single -v arg
        // appended to whatever the user / config already has.
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
        if !success {
            return Err(anyhow::anyhow!("Failed to build rootfs image."));
        }

        // Copy outputs to host if --out was given. The SDK volume is
        // shared with `avocado ext image` & friends — same naming.
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

            // Filenames are deterministic from filesystem + target.
            // The volume layout is /opt/_avocado/<target>/output/images/...
            // ($AVOCADO_PREFIX = /opt/_avocado/<target>).
            //
            // When the kab wrap is configured, the kab is the final
            // artifact — the raw fs image is just an intermediate that
            // stays inside the volume. Only copy the kab in that case.
            let raw_filename = format!("avocado-image-rootfs-{target_arch}.{rootfs_filesystem}");
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

        print_success("Built rootfs image.", OutputLevel::Normal);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rootfs_script_guards_against_half_populated_sysroot() {
        // A stale or interrupted build volume can leave the sysroot with
        // /usr present but /etc/passwd missing, which used to surface as
        // a cryptic `grep: .../etc/passwd: No such file` from the
        // user-creation step. The generated script must instead fail
        // fast with an actionable message pointing at `avocado clean`.
        let script = generate_rootfs_build_script(
            "00000000-0000-0000-0000-000000000000",
            "erofs-lz4",
            None,
            "# permissions placeholder\n",
        );

        assert!(
            script.contains("$ROOTFS_WORK/etc/passwd"),
            "script must check for the base rootfs /etc/passwd before user creation"
        );
        assert!(
            script.contains("avocado clean"),
            "script must tell the user to run `avocado clean` on a stale volume"
        );
        assert!(
            script.contains("avocado prune"),
            "script must also point at `avocado prune`, since clean alone does not \
             clear abandoned volumes that can shadow a build"
        );
    }

    #[test]
    fn test_rootfs_passwd_guard_precedes_permissions_section() {
        // The guard only helps if it runs before the user-creation
        // (permissions) step it protects.
        let marker = "# PERMISSIONS_MARKER_FOR_TEST";
        let script = generate_rootfs_build_script(
            "00000000-0000-0000-0000-000000000000",
            "erofs-lz4",
            None,
            marker,
        );

        let guard_pos = script
            .find("$ROOTFS_WORK/etc/passwd")
            .expect("guard present");
        let perms_pos = script.find(marker).expect("permissions section present");
        assert!(
            guard_pos < perms_pos,
            "the /etc/passwd guard must run before the permissions/user-creation section"
        );
    }

    fn rootfs_script() -> String {
        generate_rootfs_build_script(
            "00000000-0000-0000-0000-000000000000",
            "erofs-lz4",
            None,
            "",
        )
    }

    #[test]
    fn test_build_id_is_pkg_hash_plus_tree_hash() {
        // ENG-2441: the id folds a content hash of the assembled work tree in
        // beside the NEVRA package hash, so any rootfs-affecting change moves it.
        let s = rootfs_script();
        assert!(s.contains("PKG_HASH="), "package identity retained");
        assert!(s.contains("TREE_HASH="), "work-tree content hash present");
        assert!(
            s.contains("uuid.uuid5(uuid.UUID('00000000-0000-0000-0000-000000000000'), '$PKG_HASH:$TREE_HASH')"),
            "id must derive from both the package hash and the tree hash"
        );
    }

    #[test]
    fn test_tree_hash_prunes_nondeterministic_paths_and_excludes_metadata() {
        // The rpmdb (install timestamps) and dnf caches/logs would churn the id
        // every build, and mtime/ownership are normalized out of the image — so
        // none may feed the hash. It hashes type, mode (%m), path (%P) and
        // symlink target (%l) only.
        let s = rootfs_script();
        for pruned in [
            "-path ./var/lib/rpm",
            "-path ./var/cache",
            "-path ./var/log",
        ] {
            assert!(s.contains(pruned), "tree hash must prune {pruned}");
        }
        assert!(
            s.contains(r"-printf '%y %m %P\t%l\n'"),
            "hash covers type/mode/path/link"
        );
        // No mtime (%T*), user (%u/%U) or group (%g/%G) directives leak in.
        for meta in ["%T", "%u", "%U", "%g", "%G"] {
            assert!(!s.contains(meta), "tree hash must not depend on {meta}");
        }
    }

    #[test]
    fn test_build_id_computed_before_identity_is_injected() {
        // Taking the hash after the os-release identity append would make the id
        // depend on itself (and on the possibly-unpinned runtime version), so the
        // derivation — including the strip that canonicalizes os-release — must
        // finish before anything is appended.
        let s = rootfs_script();
        let strip = s
            .find("sed -i '/^AVOCADO_OS_BUILD_ID=/d")
            .expect("identity strip present");
        let tree = s.find("TREE_HASH=").expect("tree hash present");
        let derive = s
            .find("OS_BUILD_ID=$(python3")
            .expect("id derivation present");
        let append = s
            .find(r#"echo "AVOCADO_OS_BUILD_ID=$OS_BUILD_ID" >>"#)
            .expect("identity append present");
        assert!(
            strip < tree && tree < derive && derive < append,
            "order must be: strip identity -> tree hash -> derive id -> append identity"
        );
    }

    #[test]
    fn test_render_build_id_block_shape() {
        // Direct contract check on the shared helper both images use.
        let block = render_build_id_block(&BuildIdSpec {
            namespace_uuid: "11111111-1111-1111-1111-111111111111",
            work_var: "WK",
            sysroot_var: "SR",
            rpm_args: "--dbpath /var/lib/rpm",
            id_var: "MY_ID",
            identity_files: &["usr/lib/os-release"],
        });
        assert!(block.contains(r#"[ -f "$WK/usr/lib/os-release" ]"#));
        assert!(block.contains(r#"--root "$SR""#));
        assert!(block.contains(r#"rpm --dbpath /var/lib/rpm -qa"#));
        assert!(block.contains(r#"cd "$WK" && find ."#));
        assert!(block.contains("MY_ID=$(python3"));
        assert!(block.contains("'$PKG_HASH:$TREE_HASH'"));
    }
}
