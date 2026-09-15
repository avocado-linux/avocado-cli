
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

echo -n "sdk/x86_64/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/sdk/x86_64/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/sdk/x86_64/install.stamp"; echo; else echo "null"; fi
echo -n "sdk/x86_64/compile-deps.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/sdk/x86_64/compile-deps.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/sdk/x86_64/compile-deps.stamp"; echo; else echo "null"; fi
echo -n "rootfs/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/rootfs/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/rootfs/install.stamp"; echo; else echo "null"; fi
echo -n "initramfs/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/initramfs/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/initramfs/install.stamp"; echo; else echo "null"; fi
echo -n "runtime/dev/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/runtime/dev/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/runtime/dev/install.stamp"; echo; else echo "null"; fi
echo -n "ext/app/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/app/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/app/install.stamp"; echo; else echo "null"; fi
echo -n "ext/app/build.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/app/build.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/app/build.stamp"; echo; else echo "null"; fi
echo -n "ext/app/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/app/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/app/image.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-bsp-qemux86-64/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/install.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-bsp-qemux86-64/build.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/build.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/build.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-bsp-qemux86-64/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-bsp-qemux86-64/image.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-dev/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/install.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-dev/build.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/build.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/build.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-dev/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-dev/image.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-sshd-dev/install.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/install.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/install.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-sshd-dev/build.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/build.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/build.stamp"; echo; else echo "null"; fi
echo -n "ext/avocado-ext-sshd-dev/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/ext/avocado-ext-sshd-dev/image.stamp"; echo; else echo "null"; fi
echo -n "rootfs/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/rootfs/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/rootfs/image.stamp"; echo; else echo "null"; fi
echo -n "initramfs/image.stamp:::"; if [ -f "$AVOCADO_PREFIX/.stamps/initramfs/image.stamp" ]; then tr -d '\n' < "$AVOCADO_PREFIX/.stamps/initramfs/image.stamp"; echo; else echo "null"; fi