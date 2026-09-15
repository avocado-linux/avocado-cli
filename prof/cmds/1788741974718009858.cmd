
set -e

# Set up signal handlers to forward signals to all child processes
# This ensures that when the container receives SIGTERM/SIGINT,
# the compile script also gets terminated immediately
cleanup() {
    # Kill all child processes
    pkill -P $$ 2>/dev/null || true
    exit $1
}
trap 'cleanup 143' TERM
trap 'cleanup 130' INT

# Remount source directory with permission translation via bindfs
# This maps host UID/GID to root inside the container for seamless file access
mkdir -p /opt/src

# Check if bindfs is available
if ! command -v bindfs >/dev/null 2>&1; then
    echo "[ERROR] bindfs is not installed in this container image." >&2
    echo "" >&2
    echo "To resolve this, update the SDK container by running one of the following:" >&2
    echo "" >&2
    echo "  avocado fetch" >&2
    echo "  docker pull $AVOCADO_SDK_IMAGE" >&2
    echo "" >&2
    exit 1
fi

if [ -n "$AVOCADO_HOST_UID" ] && [ -n "$AVOCADO_HOST_GID" ]; then
    # If host user is already root (UID 0), no mapping needed - just bind mount
    if [ "$AVOCADO_HOST_UID" = "0" ] && [ "$AVOCADO_HOST_GID" = "0" ]; then
        mount --bind /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (host is root, no mapping needed)" >&2; fi
    else
        # Use --map with colon-separated user and group mappings
        # Maps host UID -> 0 (root) and host GID -> 0 (root group)
        # Format: --map=uid1/uid2:@gid1/@gid2
        bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src with UID/GID mapping ($AVOCADO_HOST_UID:$AVOCADO_HOST_GID -> 0:0)" >&2; fi
    fi
else
    # Fallback: simple bind mount without permission translation
    mount --bind /mnt/src /opt/src
    if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (no UID/GID mapping)" >&2; fi
fi

# Mount extension source paths with bindfs (for path-based remote extensions)
# These are mounted at /mnt/ext/<ext_name> and need to be bindfs'd to $AVOCADO_PREFIX/includes/<ext_name>
if [ -n "$AVOCADO_EXT_PATH_MOUNTS" ]; then
    # AVOCADO_PREFIX must be set before this - use the target from environment
    EXT_PREFIX="/opt/_avocado/${AVOCADO_TARGET}/includes"
    for ext_name in $AVOCADO_EXT_PATH_MOUNTS; do
        mnt_path="/mnt/ext/$ext_name"
        target_path="$EXT_PREFIX/$ext_name"

        if [ -d "$mnt_path" ]; then
            # Create target if it doesn't exist. Avoid rm -rf because parallel
            # containers share this volume and would race on the same paths.
            # Stale package-sourced files are harmless — bindfs overlays them.
            mkdir -p "$target_path"
            if [ -n "$AVOCADO_HOST_UID" ] && [ -n "$AVOCADO_HOST_GID" ]; then
                if [ "$AVOCADO_HOST_UID" = "0" ] && [ "$AVOCADO_HOST_GID" = "0" ]; then
                    mount --bind "$mnt_path" "$target_path"
                    if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted extension '$ext_name': $mnt_path -> $target_path (host is root)" >&2; fi
                else
                    bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 "$mnt_path" "$target_path"
                    if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted extension '$ext_name': $mnt_path -> $target_path with UID/GID mapping" >&2; fi
                fi
            else
                mount --bind "$mnt_path" "$target_path"
                if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted extension '$ext_name': $mnt_path -> $target_path (no UID/GID mapping)" >&2; fi
            fi
        else
            echo "[WARNING] Extension mount path not found: $mnt_path" >&2
        fi
    done
fi

# Repo URL is always supplied by the CLI env-builder (Config::DEFAULT_REPO_URL
# when unset), so there is no literal default to drift here.
REPO_URL="$AVOCADO_SDK_REPO_URL"

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo URL: '$REPO_URL'" >&2; fi

# Get repo release from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_RELEASE" ]; then
    REPO_RELEASE="$AVOCADO_SDK_REPO_RELEASE"
else
    REPO_RELEASE="https://repo.avocadolinux.org"

    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        REPO_RELEASE=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    REPO_RELEASE=${REPO_RELEASE:-dev}
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo release: '$REPO_RELEASE'" >&2; fi

export AVOCADO_PREFIX="/opt/_avocado/${AVOCADO_TARGET}"
export AVOCADO_SDK_ARCH="$(uname -m)"
export AVOCADO_SDK_PREFIX="${AVOCADO_PREFIX}/sdk/${AVOCADO_SDK_ARCH}"
# When the CLI passes AVOCADO_RUNTIME, scope the extension sysroot tree to
# that runtime so kernel pin changes can produce fresh extension state per
# runtime without leaking across them. Also maintain a compat symlink at
# the legacy `$AVOCADO_PREFIX/extensions` location pointing to the runtime
# tree, so ext-touching commands that haven't been wired to pass
# AVOCADO_RUNTIME yet (build, image, clean, runtime build, fetch, hitl)
# transparently resolve to the same content. The symlink is only created
# when nothing already exists at the legacy path or when an existing
# symlink is found — never clobbers a real directory.
if [ -n "${AVOCADO_RUNTIME:-}" ]; then
    export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/runtimes/${AVOCADO_RUNTIME}/extensions"
    mkdir -p "${AVOCADO_EXT_SYSROOTS}"
    # Maintain a compat symlink at the legacy location pointing to the
    # runtime-scoped tree. Replace an existing real directory only when it
    # has no actual installed packages (just the rootfs rpm-db copies that
    # `setup_command` populates). Refuses to clobber a directory that
    # contains real package state — that would be data loss.
    legacy_dir="${AVOCADO_PREFIX}/extensions"
    if [ -L "$legacy_dir" ] || [ ! -e "$legacy_dir" ]; then
        ln -sfn "${AVOCADO_EXT_SYSROOTS}" "$legacy_dir"
    elif [ -d "$legacy_dir" ]; then
        # Real directory: safe to replace if every per-ext rpm db is empty
        # (no package install happened against the legacy path). Empty here
        # means rpm -qa returns nothing for every per-ext sysroot.
        all_empty=true
        for ext_dir in "$legacy_dir"/*; do
            [ -d "$ext_dir" ] || continue
            if rpm --root="$ext_dir" -qa 2>/dev/null | grep -q '.'; then
                all_empty=false
                break
            fi
        done
        if [ "$all_empty" = "true" ]; then
            rm -rf "$legacy_dir"
            ln -sfn "${AVOCADO_EXT_SYSROOTS}" "$legacy_dir"
        fi
    fi
else
    export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/extensions"
fi
export DNF_SDK_HOST_PREFIX="${AVOCADO_SDK_PREFIX}"
export DNF_SDK_TARGET_PREFIX="${AVOCADO_PREFIX}/sdk/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
--setopt=check_config_file_age=0 \
${AVOCADO_DNF_ARGS:-} \
"

export DNF_NO_SCRIPTS="--setopt=tsflags=noscripts"
export SSL_CERT_FILE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${DNF_SDK_HOST_PREFIX}/var/cache \
--setopt=logdir=${DNF_SDK_HOST_PREFIX}/var/log \
--setopt=persistdir=${DNF_SDK_HOST_PREFIX}/var/lib/dnf \
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

# Combined repo config for SDK package installations (nativesdk packages).
# Uses arch-specific varsdir for correct architecture filtering, but includes
# BOTH repo directories: arch-specific SDK repos (base repos from container)
# and target-repoconf repos (target-specific repos like qemux86-64-sdk).
# This ensures correct arch selection when running --runs-on with cross-arch targets.
export DNF_SDK_COMBINED_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d,${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

mkdir -p /etc/dnf/vars
mkdir -p ${AVOCADO_SDK_PREFIX}/etc/dnf/vars
mkdir -p ${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars

echo "${REPO_URL}" > /etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_HOST_PREFIX}/etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars/repo_url

# Re-apply machine-scoped SDK arch and rpm platform to override any package scriptlet
# resets. Package post-install scripts (via update-alternatives) may register a generic
# (host-arch-only) alternative. Without this repair the rpm platform stays at
# x86_64_avocadosdk and RPM rejects qemux86_64_x86_64_avocadosdk packages as
# "intended for a different architecture" during the transaction check.
_SDK_MACHINE_US=$(echo "${AVOCADO_TARGET}" | tr '-' '_')
_SDK_HOST_US=$(uname -m | tr '-' '_')
_SDKIMGARCH="${_SDK_MACHINE_US}_${_SDK_HOST_US}_avocadosdk"
if [ -f "${AVOCADO_SDK_PREFIX}/etc/dnf/vars/arch" ]; then
    _ARCH_FILE="${AVOCADO_SDK_PREFIX}/etc/dnf/vars/arch"
    _ARCH_CURRENT=$(cat "${_ARCH_FILE}" 2>/dev/null || echo "")
    _ARCH_FIRST=$(echo "${_ARCH_CURRENT}" | cut -d: -f1)
    if [ -n "${_ARCH_CURRENT}" ] && [ "${_ARCH_FIRST}" != "${_SDKIMGARCH}" ]; then
        rm -f "${_ARCH_FILE}"
        echo "${_SDKIMGARCH}:${_ARCH_CURRENT}" > "${_ARCH_FILE}"
    fi
fi
if [ -f "${AVOCADO_SDK_PREFIX}/etc/rpm/platform" ]; then
    _PLATFORM_FILE="${AVOCADO_SDK_PREFIX}/etc/rpm/platform"
    _PLATFORM_CURRENT=$(cat "${_PLATFORM_FILE}" 2>/dev/null || echo "")
    if [ "${_PLATFORM_CURRENT}" != "${_SDKIMGARCH}-avocado-linux" ]; then
        rm -f "${_PLATFORM_FILE}"
        echo "${_SDKIMGARCH}-avocado-linux" > "${_PLATFORM_FILE}"
    fi
fi

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

cd /opt/src

# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"

    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
fi

# --- custom repo CA / insecure TLS (AVOCADO_REPO_CA / AVOCADO_REPO_INSECURE) ---
if [ -n "${AVOCADO_REPO_CA_B64:-}" ]; then
    _avocado_ca_bundle="${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt"
    mkdir -p "$(dirname "$_avocado_ca_bundle")"
    if ! grep -q "BEGIN AVOCADO_REPO_CA" "$_avocado_ca_bundle" 2>/dev/null; then
        { echo "# BEGIN AVOCADO_REPO_CA"; printf '%s' "$AVOCADO_REPO_CA_B64" | base64 -d; echo; echo "# END AVOCADO_REPO_CA"; } >> "$_avocado_ca_bundle"
        echo "[INFO] Added custom repo CA to the SDK trust bundle." >&2
    fi
fi
if [ "${AVOCADO_REPO_INSECURE:-}" = "1" ]; then
    export DNF_SDK_HOST="${DNF_SDK_HOST} --setopt=sslverify=0"
    echo "[WARN] AVOCADO_REPO_INSECURE=1: TLS verification DISABLED for all dnf operations." >&2
fi


# Set common variables. `export`ed so any post_install script (which we
# invoke as a child `bash` process from the build_section below)
# inherits them.
export RUNTIME_NAME="dev"
export TARGET_ARCH="qemux86-64"
export RUNTIME_VERSION="b837d0f7"

VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging
mkdir -p "$VAR_DIR/lib/avocado/images"
mkdir -p "$VAR_DIR/lib/avocado/runtimes"
mkdir -p "$VAR_DIR/lib/avocado"


export OUTPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
mkdir -p $OUTPUT_DIR

# Create runtime-specific extensions directory (staging area for image ID computation)
RUNTIME_EXT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/extensions"
mkdir -p "$RUNTIME_EXT_DIR"

# Clean up stale extensions to ensure fresh copies
echo "Cleaning up stale extensions..."
rm -f "$RUNTIME_EXT_DIR"/*.raw "$RUNTIME_EXT_DIR"/*.kab 2>/dev/null || true
# Re-keyed bootloader outputs from an earlier build (the feed's re-key script writes
# them here, where stone resolves them ahead of the SDK's copies). A build that
# does not re-key must not ship a bootloader closed to the previous key.
rm -f "$OUTPUT_DIR"/imx-boot-*.bin-* "$OUTPUT_DIR"/imx-boot "$OUTPUT_DIR"/u-boot-*.dtb.keyed 2>/dev/null || true

# Copy required extension images from global output/extensions to runtime-specific location
echo "Copying required extension images to runtime-specific directory..."

if [ -f "$AVOCADO_PREFIX/output/extensions/avocado-ext-dev-0.1.0.raw" ]; then
    cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-ext-dev-0.1.0.raw" "$RUNTIME_EXT_DIR/avocado-ext-dev-0.1.0.raw"
    echo "  Copied: avocado-ext-dev-0.1.0.raw"
    # dm-verity sidecars (image.verity: true): hash tree + root hash travel with
    # the image; stale ones from an earlier build never survive here.
    for sc in verity roothash; do
        rm -f "$RUNTIME_EXT_DIR/avocado-ext-dev-0.1.0.$sc"
        [ -f "$AVOCADO_PREFIX/output/extensions/avocado-ext-dev-0.1.0.$sc" ] && cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-ext-dev-0.1.0.$sc" "$RUNTIME_EXT_DIR/avocado-ext-dev-0.1.0.$sc"
    done
fi

if [ -f "$AVOCADO_PREFIX/output/extensions/avocado-ext-sshd-dev-0.1.0.raw" ]; then
    cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-ext-sshd-dev-0.1.0.raw" "$RUNTIME_EXT_DIR/avocado-ext-sshd-dev-0.1.0.raw"
    echo "  Copied: avocado-ext-sshd-dev-0.1.0.raw"
    # dm-verity sidecars (image.verity: true): hash tree + root hash travel with
    # the image; stale ones from an earlier build never survive here.
    for sc in verity roothash; do
        rm -f "$RUNTIME_EXT_DIR/avocado-ext-sshd-dev-0.1.0.$sc"
        [ -f "$AVOCADO_PREFIX/output/extensions/avocado-ext-sshd-dev-0.1.0.$sc" ] && cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-ext-sshd-dev-0.1.0.$sc" "$RUNTIME_EXT_DIR/avocado-ext-sshd-dev-0.1.0.$sc"
    done
fi

if [ -f "$AVOCADO_PREFIX/output/extensions/app-0.1.0.raw" ]; then
    cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/app-0.1.0.raw" "$RUNTIME_EXT_DIR/app-0.1.0.raw"
    echo "  Copied: app-0.1.0.raw"
    # dm-verity sidecars (image.verity: true): hash tree + root hash travel with
    # the image; stale ones from an earlier build never survive here.
    for sc in verity roothash; do
        rm -f "$RUNTIME_EXT_DIR/app-0.1.0.$sc"
        [ -f "$AVOCADO_PREFIX/output/extensions/app-0.1.0.$sc" ] && cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/app-0.1.0.$sc" "$RUNTIME_EXT_DIR/app-0.1.0.$sc"
    done
fi

if [ -f "$AVOCADO_PREFIX/output/extensions/avocado-bsp-qemux86-64-0.1.0.raw" ]; then
    cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-bsp-qemux86-64-0.1.0.raw" "$RUNTIME_EXT_DIR/avocado-bsp-qemux86-64-0.1.0.raw"
    echo "  Copied: avocado-bsp-qemux86-64-0.1.0.raw"
    # dm-verity sidecars (image.verity: true): hash tree + root hash travel with
    # the image; stale ones from an earlier build never survive here.
    for sc in verity roothash; do
        rm -f "$RUNTIME_EXT_DIR/avocado-bsp-qemux86-64-0.1.0.$sc"
        [ -f "$AVOCADO_PREFIX/output/extensions/avocado-bsp-qemux86-64-0.1.0.$sc" ] && cp -f --reflink=auto "$AVOCADO_PREFIX/output/extensions/avocado-bsp-qemux86-64-0.1.0.$sc" "$RUNTIME_EXT_DIR/avocado-bsp-qemux86-64-0.1.0.$sc"
    done
fi

# Build rootfs and initramfs images from package sysroots

ROOTFS_IMAGE_REUSE="1"
if [ "$ROOTFS_IMAGE_REUSE" = "1" ] && [ -f "$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.erofs-lz4" ] && [ -f "$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.erofs-lz4.exports" ]; then
    echo "rootfs image is up to date; reusing $OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.erofs-lz4"
    . "$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.erofs-lz4.exports"
else

# Build rootfs image from shared sysroot.
# These vars are `export`ed so the post_install script (which we invoke
# as a child `bash` process) inherits them.
export ROOTFS_SYSROOT="$AVOCADO_PREFIX/rootfs"
if [ -d "$ROOTFS_SYSROOT/usr" ]; then
    echo "Building rootfs image from packages..."

    # Work on a copy so we don't mutate the shared sysroot used for extension priming
    export ROOTFS_WORK="${ROOTFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/rootfs-work}"
    # Standalone rootfs builds (no runtime build before this) leave the
    # parent runtimes/$RUNTIME_NAME dir uncreated; ensure it exists.
    mkdir -p "$(dirname "$ROOTFS_WORK")"
    rm -rf "$ROOTFS_WORK"
    # --reflink=auto: a CoW clone where the filesystem supports one (btrfs, xfs),
    # a plain copy elsewhere. The work copy is mutated below, so it must not be
    # a hardlink; reflink gives the isolation of a copy without paying for one.
    cp -a --reflink=auto "$ROOTFS_SYSROOT" "$ROOTFS_WORK"

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

# Copy and manage user authentication files# Auto-incrementing counters for uid/gid
CURRENT_UID=1000
CURRENT_GID=1000

# Create and configure users
# Create user 'root'
echo "Creating user 'root'"
echo "[WARNING] User 'root' will be able to login with NO PASSWORD"
if ! grep -q "^root:" "$ROOTFS_WORK/etc/passwd"; then
    echo "root:x:$CURRENT_UID:$CURRENT_UID:root:/home/root:/bin/sh" >> "$ROOTFS_WORK/etc/passwd"
    echo "User 'root' created with UID $CURRENT_UID, GID $CURRENT_UID, home '/home/root', shell '/bin/sh'"

    if [ "$CURRENT_UID" = "$CURRENT_UID" ]; then
        CURRENT_UID=$((CURRENT_UID + 1))
    fi
else
    echo "User 'root' already exists, updating attributes"
fi
# Set password and shadow attributes for user 'root'
echo "Setting password and aging policy for user 'root'"
if grep -q "^root:" "$ROOTFS_WORK/etc/shadow"; then
    sed -i "s|^root:.*$|root::19000:0:99999:7:::|" "$ROOTFS_WORK/etc/shadow"
    echo "Updated shadow entry for existing user 'root'"
else
    echo "root::19000:0:99999:7:::" >> "$ROOTFS_WORK/etc/shadow"
    echo "Added new user 'root' to shadow file"
fi
# Set proper ownership and permissions for authentication files
chown root:root "$ROOTFS_WORK/etc/passwd" "$ROOTFS_WORK/etc/shadow" "$ROOTFS_WORK/etc/group"
chmod 644 "$ROOTFS_WORK/etc/passwd"
chmod 640 "$ROOTFS_WORK/etc/shadow"
chmod 644 "$ROOTFS_WORK/etc/group"
echo "Set proper permissions on authentication files"

    echo "Running post_install hooks (10 command(s))..."
    ln -sfn usr/bin "$ROOTFS_WORK/bin"
    ln -sfn usr/sbin "$ROOTFS_WORK/sbin"
    ln -sfn usr/lib "$ROOTFS_WORK/lib"
    rm -rf "$ROOTFS_WORK/media" "$ROOTFS_WORK/mnt" "$ROOTFS_WORK/srv"
    rm -rf "$ROOTFS_WORK/boot/"*
    mkdir -p "$ROOTFS_WORK/opt"
    touch "$ROOTFS_WORK/etc/machine-id"
    if [ -e "$ROOTFS_WORK/usr/lib/systemd/systemd" ]; then "$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/systemctl" --root="$ROOTFS_WORK" --preset-mode=enable-only preset-all 2>/dev/null || true; echo "Applied systemd presets"; fi
    /usr/sbin/ldconfig -r "$ROOTFS_WORK" -c new -X 2>/dev/null || true
    echo "Generated ld.so.cache"

    # Purge build-time state from the work copy before imaging. The paths come
    # from BUILD_STATE_PATHS — the same list the build-id tree hash prunes — so
    # the two can never disagree about what ships.
    #
    # Runs BEFORE the build-id derivation so the tree hash covers exactly what
    # ships: this state is both nondeterministic and absent from the image, so
    # it must be gone before the id is taken.
    #
    # dnf installs into the sysroot leave ~13MB of state behind (measured on a
    # qemux86-64 rootfs: 2.0M rpmdb, 6.4M var/cache/dnf). Nothing on target
    # consumes it — there is no runtime package manager — and it is what keeps
    # the image from being reproducible: the rpmdb records INSTALLTIME and
    # INSTALLTID per package, var/lib/dnf/history.sqlite records the
    # transaction, and var/cache/dnf holds generated repodata plus solvfiles.
    # While those are in the tree, two installs of the same package set never
    # produce identical image bytes.
    #
    # ldconfig's aux-cache is on the list for the same reason: post_install
    # runs ldconfig, which caches per-library dev/ino/ctime under
    # var/cache/ldconfig, and the work copy is cp -a'd fresh every build so
    # those inodes never repeat. /etc/ld.so.cache is the file the loader reads;
    # it is content-derived, so it stays in the image and in the hash.
    #
    # var/log is deliberately on neither list. By default it is a symlink to
    # volatile/log, so there is nothing to remove and the symlink entry hashes
    # deterministically; a post_install that turns it into a real directory
    # ships what it writes there, and that content hashes too. dnf never writes
    # logs into an installroot anyway: only cachedir and persistdir get
    # prepend_installroot (dnf/cli/cli.py), so its logs land in the SDK prefix.
    #
    # Removing all of this is necessary for a reproducible image but not
    # sufficient: the archive's own mtime handling is a separate problem, and
    # the removal itself restamps the directories it empties. That is #199's
    # half, not this one's.
    #
    # Runs after post_install so state left by a hook's own dnf call is caught
    # too. Safe for identity and for extension priming: the build ID above
    # queries $ROOTFS_SYSROOT, and the installroot seeding in `ext install` /
    # `runtime install` copies from $AVOCADO_PREFIX/rootfs — all the pristine
    # sysroot, never this work copy.
    echo "Purging package-manager state from rootfs image"
    rm -rf "$ROOTFS_WORK/var/lib/rpm" "$ROOTFS_WORK/var/lib/dnf" "$ROOTFS_WORK/var/cache/dnf" "$ROOTFS_WORK/var/cache/ldconfig"

    # Canonicalize the identity files before hashing (see render_build_id_block).
    if [ -f "$ROOTFS_WORK/usr/lib/os-release" ]; then
        sed -i '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' "$ROOTFS_WORK/usr/lib/os-release"
    fi

    # Deterministic package identity from the NEVRA set — independent of the
    # rpmdb *bytes*, which embed install timestamps (hence var/lib/rpm is pruned
    # from the tree hash below). LC_ALL=C so collation can't reorder it.
    PKG_NEVRA=$(rpm --dbpath /var/lib/rpm -qa --queryformat '%{NEVRA}\n' --root "$ROOTFS_SYSROOT" | LC_ALL=C sort)
    PKG_HASH=$(echo "$PKG_NEVRA" | sha256sum | awk '{print $1}')

    # Content hash of the assembled work tree: the id must move iff the image
    # bytes move. Hash only what the image carries — sorted path, type, mode,
    # symlink target, file content, and uid/gid where the image format keeps it
    # — excluding what the image build normalizes out (mtime, and ownership on
    # the erofs side) or what is fs-dependent (directory sizes). %m is the octal
    # mode, %l the symlink target (empty for non-links).
    BUILD_ID_META=$(cd "$ROOTFS_WORK" && find . \( -path ./var/lib/rpm -o -path ./var/lib/dnf -o -path ./var/cache/dnf -o -path ./var/cache/ldconfig \) -prune -o -printf '%y %m %P\t%l\n' | LC_ALL=C sort)
    BUILD_ID_CONTENT=$(cd "$ROOTFS_WORK" && find . \( -path ./var/lib/rpm -o -path ./var/lib/dnf -o -path ./var/cache/dnf -o -path ./var/cache/ldconfig \) -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
    TREE_HASH=$(printf '%s\n%s\n' "$BUILD_ID_META" "$BUILD_ID_CONTENT" | sha256sum | awk '{print $1}')

    OS_BUILD_ID=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('6ba7b810-9dad-11d1-80b4-00c04fd430c8'), '$PKG_HASH:$TREE_HASH'))")

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
    ROOTFS_FS="erofs-lz4"
    ROOTFS_OUTPUT="$OUTPUT_DIR/avocado-image-rootfs-$TARGET_ARCH.$ROOTFS_FS"
    echo "Building rootfs image: $ROOTFS_FS"
    case "$ROOTFS_FS" in
        erofs-zst)
            mkfs.erofs \
                -T "${SOURCE_DATE_EPOCH:-0}" \
                -U 00000000-0000-0000-0000-000000000000 \
                -x -1 \
                --all-root \
                -z zstd \
                "$ROOTFS_OUTPUT" \
                "$ROOTFS_WORK"
            ;;
        erofs-lz4)
            mkfs.erofs \
                -T "${SOURCE_DATE_EPOCH:-0}" \
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

    rm -f "${ROOTFS_OUTPUT%.*}.verity" "${ROOTFS_OUTPUT%.*}.roothash"
    export AVOCADO_ROOTFS_FILESYSTEM="$ROOTFS_FS"
    export AVOCADO_OS_BUILD_ID="$OS_BUILD_ID"
    : > "$ROOTFS_OUTPUT.exports"
    for _v in AVOCADO_ROOTFS_IMAGE AVOCADO_ROOTFS_FILESYSTEM AVOCADO_OS_BUILD_ID AVOCADO_ROOTFS_ROOTHASH AVOCADO_ROOTFS_VERITY; do
        if [ -n "${!_v+x}" ]; then printf 'export %s=%q\n' "$_v" "${!_v}" >> "$ROOTFS_OUTPUT.exports"; fi
    done
    echo "Built rootfs: $ROOTFS_OUTPUT (AVOCADO_OS_BUILD_ID=$OS_BUILD_ID)"
else
    echo "No rootfs sysroot found — skipping rootfs image build."
fi
fi

INITRAMFS_IMAGE_REUSE="1"
if [ "$INITRAMFS_IMAGE_REUSE" = "1" ] && [ -f "$OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.cpio.zst" ] && [ -f "$OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.cpio.zst.exports" ]; then
    echo "initramfs image is up to date; reusing $OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.cpio.zst"
    . "$OUTPUT_DIR/avocado-image-initramfs-$TARGET_ARCH.cpio.zst.exports"
else

# Build initramfs image from shared sysroot.
# These vars are `export`ed so the post_install script (which we invoke
# as a child `bash` process) inherits them.
export INITRAMFS_SYSROOT="$AVOCADO_PREFIX/initramfs"
if [ -d "$INITRAMFS_SYSROOT/usr" ]; then
    echo "Building initramfs image from packages..."

    export INITRAMFS_WORK="${INITRAMFS_WORK_DIR:-$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/initramfs-work}"
    # Standalone initramfs builds (no runtime build before this) leave
    # the parent runtimes/$RUNTIME_NAME dir uncreated; ensure it exists.
    mkdir -p "$(dirname "$INITRAMFS_WORK")"
    rm -rf "$INITRAMFS_WORK"
    # --reflink=auto: a CoW clone where the filesystem supports one (btrfs, xfs),
    # a plain copy elsewhere. The work copy is mutated below, so it must not be
    # a hardlink; reflink gives the isolation of a copy without paying for one.
    cp -a --reflink=auto "$INITRAMFS_SYSROOT" "$INITRAMFS_WORK"

# Copy and manage user authentication files# Auto-incrementing counters for uid/gid
CURRENT_UID=1000
CURRENT_GID=1000

# Create and configure users
# Create user 'root'
echo "Creating user 'root'"
echo "[WARNING] User 'root' will be able to login with NO PASSWORD"
if ! grep -q "^root:" "$INITRAMFS_WORK/etc/passwd"; then
    echo "root:x:$CURRENT_UID:$CURRENT_UID:root:/home/root:/bin/sh" >> "$INITRAMFS_WORK/etc/passwd"
    echo "User 'root' created with UID $CURRENT_UID, GID $CURRENT_UID, home '/home/root', shell '/bin/sh'"

    if [ "$CURRENT_UID" = "$CURRENT_UID" ]; then
        CURRENT_UID=$((CURRENT_UID + 1))
    fi
else
    echo "User 'root' already exists, updating attributes"
fi
# Set password and shadow attributes for user 'root'
echo "Setting password and aging policy for user 'root'"
if grep -q "^root:" "$INITRAMFS_WORK/etc/shadow"; then
    sed -i "s|^root:.*$|root::19000:0:99999:7:::|" "$INITRAMFS_WORK/etc/shadow"
    echo "Updated shadow entry for existing user 'root'"
else
    echo "root::19000:0:99999:7:::" >> "$INITRAMFS_WORK/etc/shadow"
    echo "Added new user 'root' to shadow file"
fi
# Set proper ownership and permissions for authentication files
chown root:root "$INITRAMFS_WORK/etc/passwd" "$INITRAMFS_WORK/etc/shadow" "$INITRAMFS_WORK/etc/group"
chmod 644 "$INITRAMFS_WORK/etc/passwd"
chmod 640 "$INITRAMFS_WORK/etc/shadow"
chmod 644 "$INITRAMFS_WORK/etc/group"
echo "Set proper permissions on authentication files"

    echo "Running post_install hooks (8 command(s))..."
    ln -sfn usr/bin "$INITRAMFS_WORK/bin"
    ln -sfn usr/sbin "$INITRAMFS_WORK/sbin"
    ln -sfn usr/lib "$INITRAMFS_WORK/lib"
    rm -rf "$INITRAMFS_WORK/media" "$INITRAMFS_WORK/mnt" "$INITRAMFS_WORK/srv"
    rm -rf "$INITRAMFS_WORK/boot/"*
    mkdir -p "$INITRAMFS_WORK/sysroot"
    mkdir -p "$INITRAMFS_WORK/opt"
    if [ ! -L "$INITRAMFS_WORK/init" ] && [ ! -e "$INITRAMFS_WORK/init" ]; then if [ -L "$INITRAMFS_WORK/sbin/init" ] || [ -e "$INITRAMFS_WORK/sbin/init" ]; then ln -sf /sbin/init "$INITRAMFS_WORK/init"; echo "Created /init -> /sbin/init symlink"; else echo "WARNING: /sbin/init not found in initramfs — kernel may not find init"; fi; fi

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
    rm -rf "$INITRAMFS_WORK/var/lib/rpm" "$INITRAMFS_WORK/var/lib/dnf" "$INITRAMFS_WORK/var/cache/dnf" "$INITRAMFS_WORK/var/cache/ldconfig"

    # Canonicalize the identity files before hashing (see render_build_id_block).
    if [ -f "$INITRAMFS_WORK/usr/lib/initrd-release" ]; then
        sed -i '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' "$INITRAMFS_WORK/usr/lib/initrd-release"
    fi
    if [ -f "$INITRAMFS_WORK/usr/lib/os-release-initrd" ]; then
        sed -i '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' "$INITRAMFS_WORK/usr/lib/os-release-initrd"
    fi
    if [ -f "$INITRAMFS_WORK/usr/lib/os-release" ]; then
        sed -i '/^AVOCADO_OS_BUILD_ID=/d;/^AVOCADO_RUNTIME_NAME=/d;/^AVOCADO_RUNTIME_VERSION=/d' "$INITRAMFS_WORK/usr/lib/os-release"
    fi

    # Deterministic package identity from the NEVRA set — independent of the
    # rpmdb *bytes*, which embed install timestamps (hence var/lib/rpm is pruned
    # from the tree hash below). LC_ALL=C so collation can't reorder it.
    PKG_NEVRA=$(rpm  -qa --queryformat '%{NEVRA}\n' --root "$INITRAMFS_SYSROOT" | LC_ALL=C sort)
    PKG_HASH=$(echo "$PKG_NEVRA" | sha256sum | awk '{print $1}')

    # Content hash of the assembled work tree: the id must move iff the image
    # bytes move. Hash only what the image carries — sorted path, type, mode,
    # symlink target, file content, and uid/gid where the image format keeps it
    # — excluding what the image build normalizes out (mtime, and ownership on
    # the erofs side) or what is fs-dependent (directory sizes). %m is the octal
    # mode, %l the symlink target (empty for non-links).
    BUILD_ID_META=$(cd "$INITRAMFS_WORK" && find . \( -path ./var/lib/rpm -o -path ./var/lib/dnf -o -path ./var/cache/dnf -o -path ./var/cache/ldconfig \) -prune -o -printf '%y %m %U %G %P\t%l\n' | LC_ALL=C sort)
    BUILD_ID_CONTENT=$(cd "$INITRAMFS_WORK" && find . \( -path ./var/lib/rpm -o -path ./var/lib/dnf -o -path ./var/cache/dnf -o -path ./var/cache/ldconfig \) -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
    TREE_HASH=$(printf '%s\n%s\n' "$BUILD_ID_META" "$BUILD_ID_CONTENT" | sha256sum | awk '{print $1}')

    INITRAMFS_BUILD_ID=$(python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('6ba7b810-9dad-11d1-80b4-00c04fd430c8'), '$PKG_HASH:$TREE_HASH'))")

    # Inject identity into the initrd's release files (see render_identity_injection).
    # readlink -f canonicalizes, so the allowlist has to be canonical too: a
    # relative work dir, or one reached through a symlinked component, would
    # otherwise never match its own resolved paths and fail the build below.
    _avocado_work=$(readlink -f "$INITRAMFS_WORK" 2>/dev/null || printf '%s' "$INITRAMFS_WORK")
    _avocado_allowed=":$_avocado_work/usr/lib/initrd-release:$_avocado_work/usr/lib/os-release-initrd:$_avocado_work/usr/lib/os-release:"
    _avocado_identity_written=0
    for _avocado_f in "$_avocado_work/usr/lib/initrd-release" "$_avocado_work/usr/lib/os-release-initrd" "$_avocado_work/usr/lib/os-release" "$_avocado_work/etc/initrd-release"; do
        [ -e "$_avocado_f" ] || continue
        _avocado_t=$(readlink -f "$_avocado_f") || continue
        case "$_avocado_allowed" in
            *":$_avocado_t:"*) ;;
            *) continue ;;
        esac
        grep -q '^AVOCADO_OS_BUILD_ID=' "$_avocado_t" && continue
        echo "AVOCADO_OS_BUILD_ID=$INITRAMFS_BUILD_ID" >> "$_avocado_t" || exit 1
        _avocado_identity_written=1
    done
    if [ "$_avocado_identity_written" -eq 0 ]; then
        echo "ERROR: no release file in the initramfs can carry AVOCADO_OS_BUILD_ID (looked in $_avocado_allowed and $_avocado_work/etc/initrd-release). The image would ship with no identity: both the boot-time initramfs verification and /run/avocado/initramfs-build-id read it back from there." >&2
        exit 1
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
    echo "Normalizing initramfs mtimes to SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}"
    find "$INITRAMFS_WORK" -print0 \
        | xargs -0r touch -h -d "@${SOURCE_DATE_EPOCH:-0}"

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
    INITRAMFS_FS="cpio.zst"
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
    : > "$INITRAMFS_OUTPUT.exports"
    for _v in AVOCADO_INITRAMFS_IMAGE AVOCADO_INITRAMFS_FILESYSTEM AVOCADO_INITRAMFS_BUILD_ID; do
        if [ -n "${!_v+x}" ]; then printf 'export %s=%q\n' "$_v" "${!_v}" >> "$INITRAMFS_OUTPUT.exports"; fi
    done
    echo "Built initramfs: $INITRAMFS_OUTPUT"
else
    echo "No initramfs sysroot found — skipping initramfs image build."
fi
fi

# Boot FIT: rebuild the feed's fitImage with this runtime's initramfs (and the
# rootfs root hash when verity is on). Only on machines whose feed ships the
# fit-image.its template, and only when the result can be signed
# (AVOCADO_FIT_KEY_DIR) or an unsigned FIT is explicitly asked for
# (AVOCADO_FIT_UNSIGNED=1): a distro built with verified-boot embeds its key in
# U-Boot, and an unsigned FIT would not boot there. Without either, the feed's
# FIT is left alone - which also means this runtime's initramfs is not in it.
FIT_ITS="$OUTPUT_DIR/fit-image.its"
if [ -f "$FIT_ITS" ] && [ -f "$OUTPUT_DIR/linux.bin" ] && [ -n "${AVOCADO_INITRAMFS_IMAGE:-}" ]; then
    if [ -z "${AVOCADO_FIT_KEY_DIR:-}" ] && [ "${AVOCADO_FIT_UNSIGNED:-0}" != "1" ]; then
        if [ -n "${AVOCADO_ROOTFS_ROOTHASH:-}" ]; then
            echo "ERROR: rootfs.image.verity is on, which needs the boot FIT rebuilt with the root hash, but no FIT signing key is configured. Set runtimes.<name>.signing.fit_key to an RSA key in the signing-key registry, or signing.fit_unsigned: true if this machine's U-Boot enforces no key." >&2
            exit 1
        fi
        echo "WARNING: boot FIT not rebuilt (no signing.fit_key, signing.fit_unsigned not set): the feed's fitImage, with the feed's initramfs, will be used." >&2
    else
        echo "Assembling boot FIT from $FIT_ITS..."
        FIT_WORK_ITS="$OUTPUT_DIR/fit-image.project.its"
        # The template's ramdisk node points at the distro build's initramfs by an
        # absolute path that does not exist here; point it at ours, and make
        # sure the rewrite actually matched.
        sed -E "s#/incbin/\(\"[^\"]*initramfs[^\"]*\"\)#/incbin/(\"$AVOCADO_INITRAMFS_IMAGE\")#" "$FIT_ITS" > "$FIT_WORK_ITS"
        grep -qF "/incbin/(\"$AVOCADO_INITRAMFS_IMAGE\")" "$FIT_WORK_ITS" \
            || { echo "ERROR: could not find the initramfs node to replace in $FIT_ITS" >&2; exit 1; }
        if [ -n "${AVOCADO_ROOTFS_ROOTHASH:-}" ]; then
            # One property per configuration node, right after its opening line.
            sed -i -E "s#^([[:space:]]*conf-[^[:space:]]+ \{)\$#\1\n\t\t\tavocado,roothash = \"$AVOCADO_ROOTFS_ROOTHASH\";#" "$FIT_WORK_ITS"
            grep -qF "avocado,roothash = \"$AVOCADO_ROOTFS_ROOTHASH\"" "$FIT_WORK_ITS" \
                || { echo "ERROR: no conf-* configuration node in $FIT_ITS to carry the rootfs root hash" >&2; exit 1; }
        fi
        FIT_SIGN_ARGS=""
        if [ -n "${AVOCADO_FIT_KEY_DIR:-}" ]; then
            FIT_SIGN_ARGS="-k $AVOCADO_FIT_KEY_DIR -r"
            # A feed built without verified-boot ships a template whose
            # configuration nodes carry no signature-* subnode, and mkimage -r
            # then signs nothing without complaint. Give every configuration
            # one (algo, key-name-hint "FIT", sign-images = the image
            # properties that configuration actually names) unless the
            # template already has them. The result is checked below.
            if ! grep -qE '^[[:space:]]*signature-[0-9]+ \{' "$FIT_WORK_ITS"; then
                awk -v algo="${AVOCADO_FIT_ALGO:-sha256,rsa2048}" '
                    /^[[:space:]]*conf-[^[:space:]]+ \{$/ { inconf=1; imgs=""; depth=0 }
                    inconf && /^[[:space:]]*(kernel|fdt|ramdisk|loadables) = / {
                        p=$1; imgs = imgs (imgs==""?"":", ") "\"" p "\""
                    }
                    inconf && /\{[[:space:]]*$/ { depth++ }
                    inconf && /^[[:space:]]*\};/ {
                        depth--
                        if (depth==0) {
                            print "\t\t\tsignature-1 {"
                            print "\t\t\t\talgo = \"" algo "\";"
                            print "\t\t\t\tkey-name-hint = \"FIT\";"
                            print "\t\t\t\tsign-images = " imgs ";"
                            print "\t\t\t};"
                            inconf=0
                        }
                    }
                    { print }
                ' "$FIT_WORK_ITS" > "$FIT_WORK_ITS.signed" && mv "$FIT_WORK_ITS.signed" "$FIT_WORK_ITS"
                grep -qE '^[[:space:]]*signature-1 \{' "$FIT_WORK_ITS" \
                    || { echo "ERROR: could not add signature nodes to the FIT configurations in $FIT_ITS" >&2; exit 1; }
            fi
        else
            # Explicitly unsigned: strip the signature nodes so mkimage does not look for a key.
            sed -i -E '/^[[:space:]]*signature-[0-9]+ \{/,/^[[:space:]]*\};/d' "$FIT_WORK_ITS"
        fi
        rm -f "$OUTPUT_DIR/fitImage"
        (cd "$OUTPUT_DIR" && mkimage -f "$FIT_WORK_ITS" $FIT_SIGN_ARGS "$OUTPUT_DIR/fitImage" > /dev/null) \
            || { echo "ERROR: mkimage failed to assemble the boot FIT" >&2; exit 1; }
        [ -s "$OUTPUT_DIR/fitImage" ] || { echo "ERROR: boot FIT was not produced" >&2; exit 1; }
        mkimage -l "$OUTPUT_DIR/fitImage" | grep -E 'Default Configuration|Sign algo' | head -2 | sed 's/^/  /'
        if [ -n "${AVOCADO_FIT_KEY_DIR:-}" ]; then
            # Prove the signature exists rather than trust mkimage's silence.
            mkimage -l "$OUTPUT_DIR/fitImage" | grep -q 'Sign algo' \
                || { echo "ERROR: boot FIT was built but carries no configuration signature" >&2; exit 1; }
        fi
        echo "Built boot FIT: $OUTPUT_DIR/fitImage${AVOCADO_FIT_KEY_DIR:+ (signed)}${AVOCADO_ROOTFS_ROOTHASH:+ (rootfs root hash embedded)}"
        # Make the bootloader enforce that key. The feed ships the procedure and
        # its inputs (imx-boot-tools/rekey-imx-boot.sh + rekey.env, i.MX8M); the
        # re-packed images take the feed's file names, so stone's imx_boot* image
        # keys resolve to them ahead of the SDK's copies. A feed without the
        # tooling is an error, not a silent distro bootloader: a project that
        # asked for its key in the bootloader must not ship one that ignores it.
        if [ "${AVOCADO_FIT_KEY_IN_BOOTLOADER:-0}" = "1" ]; then
            REKEY="$OUTPUT_DIR/imx-boot-tools/rekey-imx-boot.sh"
            if [ ! -x "$REKEY" ]; then
                echo "ERROR: signing.fit_key_in_bootloader is on but this feed ships no imx-boot-tools/rekey-imx-boot.sh for $TARGET_ARCH. Set signing.fit_key_in_bootloader: false to keep the distro bootloader." >&2
                exit 1
            fi
            echo "Rebuilding the bootloader to enforce the FIT key..."
            "$REKEY" "$OUTPUT_DIR/imx-boot-tools" "$AVOCADO_FIT_KEY_DIR" "$OUTPUT_DIR" "${AVOCADO_FIT_ALGO:-sha256,rsa2048}" \
                || { echo "ERROR: bootloader re-key failed" >&2; exit 1; }
            # The keyed control DTB is what U-Boot will verify with; run that
            # verification here so a mismatch fails the build, not the boot.
            KEYED_DTB=$(ls "$OUTPUT_DIR"/u-boot-*.dtb.keyed 2>/dev/null | head -1)
            if [ -n "$KEYED_DTB" ] && command -v fit_check_sign >/dev/null 2>&1; then
                fit_check_sign -f "$OUTPUT_DIR/fitImage" -k "$KEYED_DTB" >/dev/null 2>&1 \
                    || { echo "ERROR: the re-keyed bootloader does not verify this runtime's boot FIT" >&2; exit 1; }
                echo "Bootloader re-keyed: fitImage verifies against $(basename "$KEYED_DTB")"
            fi
        fi
    fi
fi


# Resolve kernel image staged by `rootfs install` at $AVOCADO_PREFIX/kernel/<kver>/Image.
# Done in shell so the optional KAB wrap below can rewrite the env var
# uniformly across rootfs / initramfs / kernel.
KERNEL_IMAGE_GLOB=$(ls "$AVOCADO_PREFIX"/kernel/*/Image 2>/dev/null | head -1)
if [ -n "$KERNEL_IMAGE_GLOB" ]; then
    export AVOCADO_KERNEL_IMAGE="$KERNEL_IMAGE_GLOB"
fi

# Read VERSION_ID from rootfs os-release for use as a stable version
# identifier in kab-wrap args (matches the version field that the AMF
# manifest will carry for rootfs/initramfs/kernel entries — i.e.
# whatever ends up in `manifest.<X>.version` is what the kab's signed
# -v value will be, so users can write
#   args: '-b -t kos.layer.kernel -v "$AVOCADO_OS_VERSION_ID" ...'
# without risk of drift between the kab and the manifest entry).
AVOCADO_OS_VERSION_ID=""
if [ -f "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" ]; then
    AVOCADO_OS_VERSION_ID=$(grep '^VERSION_ID=' "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" \
        | head -1 | cut -d= -f2- | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//")
fi
export AVOCADO_OS_VERSION_ID

# Optional KAB wrapping for rootfs / initramfs / kernel. Each block is
# emitted only when the corresponding `image.type: kab` is set in the
# config. Each rewrites $AVOCADO_<NAME>_IMAGE in place to point at the
# wrapped .kab so the manifest section reads the wrapped artifact.

# Assemble var partition content and build var image
# No extension var_files to apply
# No runtime var_files to apply

# Generate Avocado Runtime Manifest with content-addressable image IDs
IMAGES_DIR="$VAR_DIR/lib/avocado/images"
mkdir -p "$IMAGES_DIR"
BUILD_ID="b837d0f7-2f43-41af-8366-4ece9e65b12f"
BUILT_AT="2026-09-07T00:46:12Z"
RUNTIME_VERSION="b837d0f7"
# Clean stale runtime manifests from previous builds
rm -rf "$VAR_DIR/lib/avocado/runtimes"
MANIFEST_DIR="$VAR_DIR/lib/avocado/runtimes/$BUILD_ID"
mkdir -p "$MANIFEST_DIR"

export AVOCADO_NS_UUID="7488fa35-6390-425b-bbbf-b156cfe1eed2"
export AVOCADO_RT_EXT_DIR="$RUNTIME_EXT_DIR"
export AVOCADO_IMAGES_DIR="$IMAGES_DIR"
export AVOCADO_MANIFEST_PATH="$MANIFEST_DIR/manifest.json"
export AVOCADO_SPOT_HASHES_PATH="$MANIFEST_DIR/spot_hashes.json"
export AVOCADO_BUILD_ID="$BUILD_ID"
export AVOCADO_BUILT_AT="$BUILT_AT"
export AVOCADO_RUNTIME_NAME="dev"
export AVOCADO_RUNTIME_VERSION="$RUNTIME_VERSION"
export AVOCADO_EXT_PAIRS="avocado-ext-dev:0.1.0:raw:plain avocado-ext-sshd-dev:0.1.0:raw:plain avocado-bsp-qemux86-64:0.1.0:raw:plain app:0.1.0:raw:plain"
export AVOCADO_EXT_DISABLED=""
export AVOCADO_ROOTFS_IMAGE_TYPE="raw"
export AVOCADO_INITRAMFS_IMAGE_TYPE="raw"
export AVOCADO_KERNEL_IMAGE_TYPE="raw"

# sign_amf <manifest-path>: sign the AMF at the given path with the
# KAB_KEYSET_FILE-pointed keyset. Idempotent — replaces any existing
# kos.auth block, preserving every other kos.* field. No-ops when
# AVOCADO_AMF_KOS != "1" (non-kos runtimes) or the keyset is
# unavailable. Called twice: once after the manifest is written (so
# the btrfs image flashed onto fresh devices carries a signature), and
# again after the os_bundle patch (so the var-staging copy used for
# OTA upload covers the mutated state).
sign_amf() {
    AMF_SIGN_PATH="$1" python3 << 'SIGNEOF'
import json, os, base64, tempfile, subprocess, shutil

if os.environ.get("AVOCADO_AMF_KOS") != "1":
    raise SystemExit(0)

manifest_path = os.environ["AMF_SIGN_PATH"]
keyset_path = os.environ.get("KAB_KEYSET_FILE", "")
if not keyset_path or not os.path.isfile(keyset_path):
    print("AMF signing: KAB_KEYSET_FILE unset or missing; emitting unsigned manifest.")
    raise SystemExit(0)

workdir = tempfile.mkdtemp(prefix="amf-sign-")
try:
    # kabtool -x writes privateKey.der + certPath.p7b into cwd.
    subprocess.run(["kabtool", "-x", keyset_path], cwd=workdir, check=True)
    key_path = os.path.join(workdir, "privateKey.der")
    chain_path = os.path.join(workdir, "certPath.p7b")

    # certPath.p7b is a Java PkiPath SEQUENCE OF Certificate, NOT a
    # PKCS#7 SignedData (so openssl pkcs7 cannot read it). Split the
    # outer SEQUENCE into individual DER certs. PkiPath order is
    # root-adjacent first, leaf last; AMF convention places leaf first,
    # so we reverse.
    data = open(chain_path, "rb").read()
    def _tl(buf, off):
        tag, ln = buf[off], buf[off + 1]
        if ln & 0x80:
            n = ln & 0x7f
            ln = int.from_bytes(buf[off + 2:off + 2 + n], "big")
            return tag, ln, off + 2 + n
        return tag, ln, off + 2
    _, outer_len, body = _tl(data, 0)
    certs_der = []
    i = body
    while i < body + outer_len:
        _, clen, cbody = _tl(data, i)
        certs_der.append(data[i:cbody + clen])
        i = cbody + clen
    if not certs_der:
        raise RuntimeError("certPath.p7b contained no certificates")

    # Canonical form = manifest JSON with kos.auth removed; the "kos"
    # object itself is preserved (possibly empty) so the verifier and
    # signer compute identical bytes. Compact, insertion-order
    # preserved. An on-device verifier reproduces this form by stripping
    # kos.auth and recomputing the same bytes.
    with open(manifest_path, "r") as f:
        manifest = json.load(f)
    # Materialize "kos" so the canonical shape matches what the verifier
    # sees after stripping kos.auth from a signed manifest, even when no
    # other kos.* fields exist yet.
    manifest.setdefault("kos", {})
    canonical_manifest = dict(manifest)
    canonical_manifest["kos"] = {
        kk: vv for kk, vv in manifest["kos"].items() if kk != "auth"
    }
    canonical = json.dumps(canonical_manifest, separators=(",", ":"))
    canon_path = os.path.join(workdir, "canonical.json")
    with open(canon_path, "w") as f:
        f.write(canonical)

    # SHA256 + RSA PKCS#1 v1.5 — matches KAB signing.
    sig_path = os.path.join(workdir, "sig.bin")
    subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", key_path,
         "-out", sig_path, canon_path],
        check=True,
    )
    sig_b64 = base64.b64encode(open(sig_path, "rb").read()).decode("ascii")
    certs_b64 = [base64.b64encode(c).decode("ascii") for c in reversed(certs_der)]

    # Mutate kos in place so any other kos.* fields the build set
    # earlier are preserved.
    manifest["kos"]["auth"] = {
        "signature": sig_b64,
        "certificates": certs_b64,
    }
    with open(manifest_path, "w") as f:
        json.dump(manifest, f, indent=2)
    print("Signed AMF at " + manifest_path + ": leaf + " + str(len(certs_b64) - 1) + " intermediate cert(s).")
finally:
    shutil.rmtree(workdir, ignore_errors=True)
SIGNEOF
}

echo "Computing content-addressable image IDs..."
python3 << 'PYEOF'
import json, hashlib, uuid, os, shutil, struct, sys
def link_or_copy(src, dst):
    # Same name means same bytes by construction, but an interrupted earlier
    # build can leave a short file under the right name; replace, never trust.
    if os.path.lexists(dst):
        os.remove(dst)
    # Link the file, not a symlink to it. The kernel is staged as
    # kernel/<kver>/Image -> bzImage-<kver>, a relative symlink; whether os.link
    # follows that depends on the platform's python, and a hard link to the
    # symlink lands in images/ as a dangling relative link. copy2 followed it;
    # so must this.
    real = os.path.realpath(src)
    try:
        os.link(real, dst)
    except OSError:
        shutil.copy2(real, dst)


namespace = uuid.UUID(os.environ["AVOCADO_NS_UUID"])
runtime_ext_dir = os.environ["AVOCADO_RT_EXT_DIR"]
images_dir = os.environ["AVOCADO_IMAGES_DIR"]
manifest_path = os.environ["AVOCADO_MANIFEST_PATH"]
build_id = os.environ["AVOCADO_BUILD_ID"]
built_at = os.environ["AVOCADO_BUILT_AT"]
runtime_name = os.environ["AVOCADO_RUNTIME_NAME"]
runtime_version = os.environ["AVOCADO_RUNTIME_VERSION"]
ext_pairs_str = os.environ.get("AVOCADO_EXT_PAIRS", "")

# kos runtimes get extra fields for now
kos_amf = os.environ.get("AVOCADO_AMF_KOS") == "1"

# Drift-detect minihash carried per-component on kos AMFs. The leading
# digit identifies the protocol version so consumers can refuse hashes
# they don't know how to recompute.
#
# Protocol 1 mini_hash implementation:
#     head = file[0 .. min(BLOCK, size)]
#     tail = file[size - min(BLOCK, size) .. size]   # overlaps head when size <= BLOCK
#     size32 = (size & 0xFFFFFFFF) as little-endian 4 bytes
#     digest = SHA256(head || tail || size32)
#     mini_hash = "1" + hex(digest)
#
MINI_HASH_PROTO = "1"
MINI_HASH_BLOCK = 4096
# Streaming SHA256. Images can be tens of GB (customer extensions in
# particular); the previous hashlib.sha256(f.read()) form OOM-killed the
# python3 child on a 20 GB extension. Chunk size is a balance: large
# enough to keep syscall overhead negligible, small enough that resident
# set stays flat regardless of file size.
HASH_CHUNK = 1024 * 1024
def sha256_file(filepath):
    h = hashlib.sha256()
    with open(filepath, "rb") as f:
        for chunk in iter(lambda: f.read(HASH_CHUNK), b""):
            h.update(chunk)
    return h.hexdigest()

def mini_hash(filepath):
    file_size = os.path.getsize(filepath)
    n = min(MINI_HASH_BLOCK, file_size)
    h = hashlib.sha256()
    with open(filepath, "rb") as f:
        if n > 0:
            head = f.read(n)
            h.update(head)
            # When file_size <= BLOCK, tail starts at 0 and is identical
            # to head — the reference impl hashes the body twice in that
            # regime; we match that exactly.
            f.seek(file_size - n)
            tail = f.read(n)
            h.update(tail)
    h.update(struct.pack("<I", file_size & 0xFFFFFFFF))
    return MINI_HASH_PROTO + h.hexdigest()

ext_pairs = ext_pairs_str.split() if ext_pairs_str else []

# Names the runtime config marked `enabled: false`. Emitted into the
# manifest entry as `"enabled": false` (omitted when default-true) so
# avocadoctl skips activation at refresh time.
ext_disabled = set(os.environ.get("AVOCADO_EXT_DISABLED", "").split())

extensions = []
for pair in ext_pairs:
    parts = pair.split(":", 3)
    name, version = parts[0], parts[1]
    image_type = parts[2] if len(parts) > 2 else "raw"
    verity = len(parts) > 3 and parts[3] == "verity"
    ext_suffix = ".kab" if image_type == "kab" else ".raw"
    img_file = os.path.join(runtime_ext_dir, name + "-" + version + ext_suffix)
    if not os.path.isfile(img_file):
        print("WARNING: Extension image not found: " + img_file)
        continue
    sha256 = sha256_file(img_file)
    size = os.path.getsize(img_file)
    image_id = str(uuid.uuid5(namespace, sha256))
    dest = os.path.join(images_dir, image_id + ext_suffix)
    link_or_copy(img_file, dest)
    print("  Image: " + name + "-" + version + ext_suffix + " -> " + image_id + ext_suffix)
    entry = dict(name=name, version=version, image_id=image_id, sha256=sha256)
    # dm-verity, driven by the configured flag (never by sidecar presence): the
    # hash tree lands as <image_id>.verity, exactly where avocadoctl's
    # ManifestExtension::resolve_verity_path looks (image path with the
    # extension swapped), and the root hash goes in the manifest entry. Only
    # the manifest carries the hash, so a tampered sidecar cannot vouch for a
    # tampered image. With the flag set both sidecars are required - shipping
    # an unverified entry for an extension that asked for verity is the failure
    # this guards against; without it, sidecars are ignored.
    base = img_file[: -len(ext_suffix)]
    if verity:
        if not (os.path.isfile(base + ".verity") and os.path.isfile(base + ".roothash")):
            sys.exit("ERROR: extension " + name + "-" + version + " has image.verity: true but its hash tree "
                     "(.verity/.roothash) is missing next to " + img_file + " - rebuild it with `avocado ext image`")
        link_or_copy(base + ".verity", os.path.join(images_dir, image_id + ".verity"))
        with open(base + ".roothash") as rf:
            entry["root_hash"] = rf.read().strip()
        print("  Verity: " + name + "-" + version + " root hash " + entry["root_hash"][:16] + "...")
    if image_type != "raw":
        entry["image_type"] = image_type
    if name in ext_disabled:
        entry["enabled"] = False
    if kos_amf:
        entry["size"] = size
        entry["mini_hash"] = mini_hash(img_file)
    extensions.append(entry)

# rootfs / initramfs / kernel entries. Same content-addressing pattern
# as extensions: sha256 of the on-disk image -> UUIDv5 image_id, copied
# into images_dir. Suffix follows image_type: ".kab" when the artifact
# was wrapped + signed by kabtool earlier in the build, ".raw"
# otherwise. Skipped when the corresponding image isn't built.
def add_image_entry(img_path, version, image_type):
    if not (img_path and os.path.isfile(img_path)):
        return None
    sha256 = sha256_file(img_path)
    size = os.path.getsize(img_path)
    image_id = str(uuid.uuid5(namespace, sha256))
    suffix = ".kab" if image_type == "kab" else ".raw"
    dest = os.path.join(images_dir, image_id + suffix)
    link_or_copy(img_path, dest)
    print("  Image: " + os.path.basename(img_path) + " -> " + image_id + suffix)
    entry = dict(version=version, image_id=image_id, sha256=sha256)
    if image_type == "kab":
        entry["image_type"] = "kab"
    if kos_amf:
        entry["size"] = size
        entry["mini_hash"] = mini_hash(img_path)
    return entry

# rootfs, initramfs, and kernel all ship from the same avocado distro
# release, so VERSION_ID is identical between their os-release files.
# Read once from the rootfs os-release (already present and used by the
# os_bundle patch later in the build). Apply to all three.
version_id = ""
os_release_path = os.path.join(os.environ.get("AVOCADO_PREFIX", ""), "rootfs/usr/lib/os-release")
if os.path.isfile(os_release_path):
    with open(os_release_path) as f:
        for line in f:
            if line.startswith("VERSION_ID="):
                version_id = line.strip().split("=", 1)[1].strip('"').strip("'")
                break

# All three image paths arrive via env vars set by shell (rootfs /
# initramfs by their build sub-scripts; kernel by a shell-side glob
# of $AVOCADO_PREFIX/kernel/*/Image). When `image.type: kab` is set
# in the runtime config, an earlier shell section has already
# rewritten the env var to point at the wrapped .kab.
rootfs_entry = add_image_entry(
    os.environ.get("AVOCADO_ROOTFS_IMAGE", ""),
    version_id,
    os.environ.get("AVOCADO_ROOTFS_IMAGE_TYPE", "raw"),
)
# rootfs.image.verity: record the root hash the signed FIT carries, so the
# manifest states what the device is expected to have verified at boot.
if rootfs_entry and os.environ.get("AVOCADO_ROOTFS_ROOTHASH"):
    rootfs_entry["root_hash"] = os.environ["AVOCADO_ROOTFS_ROOTHASH"]
initramfs_entry = add_image_entry(
    os.environ.get("AVOCADO_INITRAMFS_IMAGE", ""),
    version_id,
    os.environ.get("AVOCADO_INITRAMFS_IMAGE_TYPE", "raw"),
)
kernel_entry = add_image_entry(
    os.environ.get("AVOCADO_KERNEL_IMAGE", ""),
    version_id,
    os.environ.get("AVOCADO_KERNEL_IMAGE_TYPE", "raw"),
)

manifest = dict(
    manifest_version=2,
    id=build_id,
    built_at=built_at,
    runtime=dict(name=runtime_name, version=runtime_version),
)
if rootfs_entry:
    manifest["rootfs"] = rootfs_entry
if initramfs_entry:
    manifest["initramfs"] = initramfs_entry
if kernel_entry:
    manifest["kernel"] = kernel_entry
manifest["extensions"] = extensions

with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
print("Created runtime manifest with " + str(len(extensions)) + " extension(s)")

# Clean up stale extension images (os_bundle cleanup happens after stone bundle)
current_image_files = set()
for ext in extensions:
    suffix = ".kab" if ext.get("image_type") == "kab" else ".raw"
    current_image_files.add(ext["image_id"] + suffix)
    if ext.get("root_hash"):
        current_image_files.add(ext["image_id"] + ".verity")
for entry in (rootfs_entry, initramfs_entry, kernel_entry):
    if entry:
        sfx = ".kab" if entry.get("image_type") == "kab" else ".raw"
        current_image_files.add(entry["image_id"] + sfx)
for fname in os.listdir(images_dir):
    if fname.endswith((".raw", ".kab", ".verity")) and fname not in current_image_files:
        stale_path = os.path.join(images_dir, fname)
        os.remove(stale_path)
        print("  Removed stale image: " + fname)

# Generate spot_hashes.json for fast integrity checking at merge time.
# Hashes file_size (8 LE bytes) + first N bytes + last N bytes of each image.
spot_check_bytes = int(os.environ.get("AVOCADO_SPOT_CHECK_BYTES", "4096"))
spot_hashes_path = os.environ.get("AVOCADO_SPOT_HASHES_PATH", "")

def compute_spot_hash(filepath, spot_size):
    import struct
    file_size = os.path.getsize(filepath)
    h = hashlib.sha256()
    h.update(struct.pack("<Q", file_size))
    with open(filepath, "rb") as f:
        if file_size == 0:
            pass
        elif file_size <= spot_size * 2:
            h.update(f.read())
        else:
            h.update(f.read(spot_size))
            f.seek(-spot_size, 2)
            h.update(f.read(spot_size))
    return h.hexdigest()

if spot_hashes_path:
    spot_hashes = {}
    for ext in extensions:
        suffix = ".kab" if ext.get("image_type") == "kab" else ".raw"
        fname = ext["image_id"] + suffix
        fpath = os.path.join(images_dir, fname)
        if os.path.isfile(fpath):
            spot_hashes[fname] = compute_spot_hash(fpath, spot_check_bytes)
    cache = dict(version=1, spot_check_bytes=spot_check_bytes, hashes=spot_hashes)
    with open(spot_hashes_path, "w") as f:
        json.dump(cache, f, indent=2)
    print("Created spot hash cache with " + str(len(spot_hashes)) + " image(s)")
PYEOF

# Sign the just-written manifest BEFORE mkfs.btrfs runs so the btrfs
# image flashed onto fresh devices already contains the signature.
# Skipped (no-op) when this isn't a `type: kos` runtime.
sign_amf "$AVOCADO_MANIFEST_PATH"

ln -sfn "runtimes/$BUILD_ID" "$VAR_DIR/lib/avocado/active"
echo "Created runtime manifest: runtimes/$BUILD_ID/manifest.json"
echo "Set active runtime -> runtimes/$BUILD_ID"

# Provision update authority (trust anchor for verified updates)
mkdir -p "$VAR_DIR/lib/avocado/metadata"

cat > "$VAR_DIR/lib/avocado/metadata/root.json" <<'ROOT_EOF'
{
  "signatures": [
    {
      "keyid": "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b",
      "sig": "af1baa2fc5663f7646609439905344ca1ce8a59e7993dc387b5069ef32b197f1dfb5241da1faaa6c72d2f97630831269644bb4ac33b4206509a299142e2c1307"
    }
  ],
  "signed": {
    "_type": "root",
    "consistent_snapshot": false,
    "expires": "2027-09-07T00:46:12Z",
    "keys": {
      "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b": {
        "keytype": "ed25519",
        "keyval": {
          "public": "54abcfae37a3de20c18179bda444e4f69ddae301ec26bda2c433c064b5d410a4"
        },
        "scheme": "ed25519"
      }
    },
    "roles": {
      "root": {
        "keyids": [
          "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b"
        ],
        "threshold": 1
      },
      "snapshot": {
        "keyids": [
          "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b"
        ],
        "threshold": 1
      },
      "targets": {
        "keyids": [
          "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b"
        ],
        "threshold": 1
      },
      "timestamp": {
        "keyids": [
          "6b4efcc7cb32bb02bbbc9393e22fc5bf09b15380940ba37d9841a3e82715e98b"
        ],
        "threshold": 1
      }
    },
    "spec_version": "1.0.0",
    "version": 1
  }
}
ROOT_EOF

cp "$VAR_DIR/lib/avocado/metadata/root.json" "$VAR_DIR/lib/avocado/metadata/1.root.json"
echo "Provisioned update authority: metadata/root.json"

# --ota: everything from here to the closing `fi` is provisioning-only. Docker
# priming, the var image, stone's OS bundle and the os_bundle manifest patch all
# feed the flash image; `deploy` and `connect upload` read only the var-staging
# directory assembled above, which is complete at this point.
BUILD_VAR_IMAGE="1"
if [ "$BUILD_VAR_IMAGE" = "1" ]; then
# No Docker images to prime

VAR_IMAGE="$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"
VAR_INPUT_SIZE=$(du -sb "$VAR_DIR" 2>/dev/null | awk '{print $1}')
VAR_INPUT_MB=$(( VAR_INPUT_SIZE / 1048576 ))
echo "Building var image (${VAR_INPUT_MB}MB source)..."

# Background progress reporter — prints size and estimated % every 5s
(
    while [ ! -f "$VAR_IMAGE" ]; do sleep 1; done
    while kill -0 $$ 2>/dev/null; do
        CUR=$(stat -c%s "$VAR_IMAGE" 2>/dev/null || echo 0)
        CUR_MB=$(( CUR / 1048576 ))
        if [ "$VAR_INPUT_SIZE" -gt 0 ] 2>/dev/null; then
            PCT=$(( CUR * 100 / VAR_INPUT_SIZE ))
            [ "$PCT" -gt 99 ] && PCT=99
            printf "\r  var image: %dMB written (~%d%%)" "$CUR_MB" "$PCT"
        else
            printf "\r  var image: %dMB written" "$CUR_MB"
        fi
        sleep 5
    done
) &
_PROGRESS_PID=$!

mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/avocado \
    -f "$VAR_IMAGE"

# var.encrypt: the first boot converts this filesystem to LUKS2 in place, which
# needs 32 MiB in front of the data (cryptsetup reencrypt --reduce-device-size)
# that cryptsetup-var obtains by shrinking the filesystem. `mkfs.btrfs -r`
# packs its chunks to the content, so a tight image has nothing to shrink into;
# rebuild it at the tight size plus 64 MiB so the room is inside the filesystem
# the runtime declared it needs. The partition stays exactly the image size.
if [ "0" = "1" ]; then
    VAR_TIGHT_SIZE=$(stat -c%s "$VAR_IMAGE")
    mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/avocado \
    -b $(( VAR_TIGHT_SIZE + 67108864 )) -f "$VAR_IMAGE"
fi

kill $_PROGRESS_PID 2>/dev/null; wait $_PROGRESS_PID 2>/dev/null || true


FINAL_SIZE=$(stat -c%s "$VAR_IMAGE" 2>/dev/null || echo 0)
FINAL_MB=$(( FINAL_SIZE / 1048576 ))
echo ""
echo "Built var image: ${FINAL_MB}MB"

# Build OS bundle (.aos) — needs rootfs + initramfs + kernel + var (all built above)
STONE_MANIFEST="${AVOCADO_STONE_MANIFEST:-$AVOCADO_SDK_PREFIX/stone/stone-$TARGET_ARCH.json}"
STONE_INPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
STONE_BUILD_DIR="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/stone"
# Clean previous stone build artifacts to prevent stale image reuse
rm -rf "$STONE_BUILD_DIR"
STONE_AOS_OUTPUT="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/os-bundle.aos"
export STONE_AOS_OUTPUT

# Build include path flags from AVOCADO_STONE_INCLUDE_PATHS
STONE_INCLUDE_FLAGS=""
if [ -n "${AVOCADO_STONE_INCLUDE_PATHS:-}" ]; then
    for path in $AVOCADO_STONE_INCLUDE_PATHS; do
        STONE_INCLUDE_FLAGS="$STONE_INCLUDE_FLAGS -i $path"
    done
fi
STONE_INCLUDE_FLAGS="$STONE_INCLUDE_FLAGS -i $STONE_INPUT_DIR"
# Also search the SDK's stone dir. A BSP's stone-<arch>.json references boot
# artifacts it does not build here - u-boot.bin, bootfiles/ - and the runtime
# input dir only ever holds what this build produced (rootfs, kernel, initramfs,
# var), so a manifest naming any of them fails resolution with "not found in any
# input directory".
#
# This is the consumer half of a two-part change and is inert without the
# other: today meta-avocado's avocado-sdk-target installs only the stone JSON
# into this directory, so nothing new resolves from it yet. It is the right
# half to land regardless - stone resolves first-match-wins across -i dirs, so
# an empty extra input changes no current behaviour - but the finalize failure
# it targets stays until the BSP recipe stages those artifacts here.
STONE_INCLUDE_FLAGS="$STONE_INCLUDE_FLAGS -i $AVOCADO_SDK_PREFIX/stone"

STONE_OVERLAY_FLAG=""

# Platform build hook. A BSP uses this to stage artifacts the manifest
# references but the generic pipeline cannot build, because they are specific to
# the SoC rather than to Avocado -- on Tegra, the Android boot image that backs
# the kernel A/B partitions, which has to wrap whichever kernel this project
# pinned and so cannot be built in Yocto either. Runs before stone bundle so
# whatever it writes into the runtime input dir resolves as an input.
#
# Optional: absent or a no-op on every target that needs nothing extra. Invoked
# by absolute path with the SDK's bin on PATH because the hook shells out to
# nativesdk tools (jq, mkbootimg) that are not otherwise on PATH here.
AVOCADO_BUILD_HOOK="$AVOCADO_SDK_PREFIX/usr/bin/avocado-build-$TARGET_ARCH"
if [ -x "$AVOCADO_BUILD_HOOK" ]; then
    echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '$RUNTIME_NAME'."
    PATH="$AVOCADO_SDK_PREFIX/usr/bin:$PATH" "$AVOCADO_BUILD_HOOK" "$RUNTIME_NAME"
fi

echo -e "\033[94m[INFO]\033[0m Running stone bundle."
echo -e "  Manifest:  $STONE_MANIFEST"
echo -e "  Output:    $STONE_AOS_OUTPUT"
echo -e "  Build dir: $STONE_BUILD_DIR"

STONE_INITRD_FLAG=""
INITRD_OS_RELEASE="$AVOCADO_PREFIX/initramfs/usr/lib/os-release-initrd"
if [ -f "$INITRD_OS_RELEASE" ]; then
    STONE_INITRD_FLAG="--os-release-initrd $INITRD_OS_RELEASE"
fi

stone bundle \
    --os-release "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" \
    $STONE_INITRD_FLAG \
    -m "$STONE_MANIFEST" \
    $STONE_INCLUDE_FLAGS \
    $STONE_OVERLAY_FLAG \
    --partition-size "var=$FINAL_SIZE" \
    -o "$STONE_AOS_OUTPUT" \
    --build-dir "$STONE_BUILD_DIR"

# Patch manifest in var-staging to add os_bundle reference (for connect upload)
# The btrfs image for provisioning doesn't need os_bundle — initial flash doesn't OTA.
# Connect upload reads from var-staging directly, so it sees this update.
python3 << 'PYEOF'
import json, hashlib, uuid, os, shutil
def link_or_copy(src, dst):
    # Same name means same bytes by construction, but an interrupted earlier
    # build can leave a short file under the right name; replace, never trust.
    if os.path.lexists(dst):
        os.remove(dst)
    # Link the file, not a symlink to it. The kernel is staged as
    # kernel/<kver>/Image -> bzImage-<kver>, a relative symlink; whether os.link
    # follows that depends on the platform's python, and a hard link to the
    # symlink lands in images/ as a dangling relative link. copy2 followed it;
    # so must this.
    real = os.path.realpath(src)
    try:
        os.link(real, dst)
    except OSError:
        shutil.copy2(real, dst)

aos_path = os.environ.get("STONE_AOS_OUTPUT", "")
if not (aos_path and os.path.isfile(aos_path)):
    print("No .aos file found, skipping os_bundle manifest patch.")
    exit(0)

namespace = uuid.UUID(os.environ["AVOCADO_NS_UUID"])
images_dir = os.environ["AVOCADO_IMAGES_DIR"]
manifest_path = os.environ["AVOCADO_MANIFEST_PATH"]

# Streaming SHA256 — see HASH_CHUNK rationale in the manifest builder
# above. .aos bundles routinely exceed available RAM on builders.
HASH_CHUNK = 1024 * 1024
aos_h = hashlib.sha256()
with open(aos_path, "rb") as f:
    for chunk in iter(lambda: f.read(HASH_CHUNK), b""):
        aos_h.update(chunk)
aos_sha256 = aos_h.hexdigest()
aos_image_id = str(uuid.uuid5(namespace, aos_sha256))
dest = os.path.join(images_dir, aos_image_id + ".raw")
link_or_copy(aos_path, dest)
print("  OS bundle: os-bundle.aos -> " + aos_image_id + ".raw")

with open(manifest_path, "r") as f:
    manifest = json.load(f)
os_build_id = None
os_release_path = os.path.join(os.environ.get("AVOCADO_PREFIX", ""), "rootfs/usr/lib/os-release")
if os.path.isfile(os_release_path):
    with open(os_release_path) as f:
        for line in f:
            if line.startswith("AVOCADO_OS_BUILD_ID="):
                os_build_id = line.strip().split("=", 1)[1]
                break

initramfs_build_id = os.environ.get("AVOCADO_INITRAMFS_BUILD_ID")

os_bundle = dict(image_id=aos_image_id, sha256=aos_sha256)
if os_build_id:
    os_bundle["os_build_id"] = os_build_id
if initramfs_build_id:
    os_bundle["initramfs_build_id"] = initramfs_build_id
manifest["os_bundle"] = os_bundle
with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
print("Patched manifest with os_bundle reference.")

# Clean up stale os_bundle images
current_image_files = set()
for ext in manifest.get("extensions", []):
    current_image_files.add(ext["image_id"] + ".raw")
for key in ("rootfs", "initramfs", "kernel"):
    block = manifest.get(key)
    if block:
        sfx = ".kab" if block.get("image_type") == "kab" else ".raw"
        current_image_files.add(block["image_id"] + sfx)
current_image_files.add(aos_image_id + ".raw")
for fname in os.listdir(images_dir):
    if fname.endswith(".raw") and fname not in current_image_files:
        os.remove(os.path.join(images_dir, fname))
        print("  Removed stale image: " + fname)
PYEOF

# Re-sign the var-staging manifest after the os_bundle patch — the
# earlier pre-mkfs.btrfs signature is now invalid for this mutated
# content. The btrfs image flashed onto fresh devices already carries
# the pre-patch signature; this second signature covers the
# var-staging manifest that OTA upload / Studio publishing consumes.
sign_amf "$AVOCADO_MANIFEST_PATH"
else
    # A previous full build's var image and OS bundle describe an older
    # extension set than the manifest just written. Remove them so a later
    # `provision` fails on a missing input instead of flashing stale bytes.
    echo "Skipping var image and OS bundle (--ota)."
    rm -f "$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs" \
          "$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/os-bundle.aos"
fi
