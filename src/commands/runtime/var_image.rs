//! The two halves of what used to be the tail of `runtime build`.
//!
//! Josh's rule for the split: **anything an OTA requires belongs at the tail of
//! `runtime build`; anything only provisioning consumes belongs at the start of
//! `provision`.** The first cut of this module moved all of it to provision,
//! which broke OTA on UKI platforms — the platform build hook *is* the kernel
//! and initramfs there, and `os-bundle.aos` is the OTA payload rather than a
//! provisioning artifact. Hardware-verified: `deploy` reported success, shipped
//! the extensions, and left the board on its old kernel.
//!
//! So:
//!
//! - [`render_ota_tail`] runs at the end of the build script: the stone include
//!   paths, device-tree overlays, the `avocado-build-<target>` hook, `stone
//!   bundle`, the `os_bundle` manifest patch and the re-sign after it. All of
//!   that produces or names OTA payload. It renders into the build script, so it
//!   inherits `RUNTIME_NAME`, `TARGET_ARCH`, `OUTPUT_DIR`, `VAR_DIR`,
//!   `AVOCADO_MANIFEST_PATH` and `sign_amf` from it.
//! - [`render_var_image`] runs at the start of `provision`: Docker priming and
//!   the var image. Nothing but `provision` reads either. It runs standalone, so
//!   it emits its own preamble.
//!
//! Both halves take the same [`VarImageContext`], which is deliberate: it is the
//! set of things a portable provisioning bundle has to carry, so a later
//! `avocado provision --bundle <path>` sources the context from a bundle rather
//! than a project.

use anyhow::Result;

use crate::utils::config::Config;

/// Everything the provisioning tail needs that is not derivable inside the
/// container. Each field is here because the rendered script reads it; a
/// portable bundle has to carry an equivalent of every one.
pub struct VarImageContext<'a> {
    pub runtime_name: &'a str,
    pub target_arch: &'a str,
    pub config: &'a Config,
    pub parsed: &'a serde_yaml::Value,
    pub merged_runtime: &'a serde_yaml::Value,
    /// Extensions in this runtime: the Docker priming section and the subvolume
    /// resolution both read per-extension config.
    pub ext_list: &'a [String],
}

/// Shared context unpacking and config-derived sections for both halves.
struct Rendered {
    docker_section: String,
    mkfs_flags: String,
    global_compress_flag: String,
    post_creation_section: String,
    device_tree_overlay_section: String,
    var_luks_room: &'static str,
}

fn prepare(ctx: &VarImageContext) -> Result<Rendered> {
    let VarImageContext {
        runtime_name,
        target_arch,
        config,
        parsed,
        merged_runtime,
        ext_list,
    } = *ctx;

    let ext_list: Vec<&str> = ext_list.iter().map(|s| s.as_str()).collect();

    let device_tree_overlay_section = {
        let overlays =
            crate::utils::device_tree_overlay::collect_for_runtime(merged_runtime, parsed)?;
        crate::utils::device_tree_overlay::render_build_section(&overlays)?
    };

    let (resolved_subvolumes, _subvol_warnings) =
        crate::utils::config::resolve_subvolumes(&ext_list, parsed, merged_runtime)?;
    let has_ro_subvolumes = resolved_subvolumes.iter().any(|s| !s.writable);
    let subvol_flags: Vec<String> = resolved_subvolumes
        .iter()
        .map(|s| format!("    --subvol rw:{}", s.path))
        .collect();
    let mkfs_flags = subvol_flags.join(" \\\n");

    let global_compress_flag = {
        // Use the runtime-level var.compression as a global --compress flag
        let var_compression = merged_runtime
            .get("var")
            .and_then(|v| v.get("compression"))
            .and_then(|v| v.as_str());
        match var_compression {
            Some(c) if c != "no" => format!("    --compress {c}"),
            _ => String::new(),
        }
    };

    let needs_post_creation = has_ro_subvolumes
        || resolved_subvolumes
            .iter()
            .any(|s| s.nodatacow || s.quota.is_some() || s.compression.is_some());

    let post_creation_section = if needs_post_creation {
        let mut commands = vec![
            "# Post-creation: apply per-subvolume properties via loop mount".to_string(),
            "echo \"Applying subvolume properties...\"".to_string(),
            "LOOP_DEV=$(losetup --find --show \"$VAR_IMAGE\")".to_string(),
            "mkdir -p /tmp/btrfs-var-setup".to_string(),
            "mount -t btrfs \"$LOOP_DEV\" /tmp/btrfs-var-setup".to_string(),
        ];

        // nodatacow via chattr +C (requires e2fsprogs in SDK)
        for s in &resolved_subvolumes {
            if s.nodatacow {
                commands.push(format!("chattr +C /tmp/btrfs-var-setup/{}", s.path));
                commands.push(format!("echo \"  {}: nodatacow\"", s.path));
            }
        }

        // Per-subvolume compression properties
        // Skip subvolumes with nodatacow -- NOCOW and compression are mutually
        // exclusive on btrfs (COW is required for transparent compression).
        for s in &resolved_subvolumes {
            if s.nodatacow {
                continue;
            }
            if let Some(ref comp) = s.compression {
                if comp != "no" {
                    commands.push(format!(
                        "btrfs property set /tmp/btrfs-var-setup/{} compression {}",
                        s.path, comp
                    ));
                    commands.push(format!("echo \"  {}: compression={}\"", s.path, comp));
                }
            }
        }

        // Quotas
        let has_quotas = resolved_subvolumes.iter().any(|s| s.quota.is_some());
        if has_quotas {
            commands.push("btrfs quota enable /tmp/btrfs-var-setup".to_string());
            for s in &resolved_subvolumes {
                if let Some(ref quota) = s.quota {
                    if quota != "none" {
                        commands.push(format!(
                            "btrfs qgroup limit {} /tmp/btrfs-var-setup/{}",
                            quota, s.path
                        ));
                        commands.push(format!("echo \"  {}: quota={}\"", s.path, quota));
                    }
                }
            }
        }

        // Flip read-only subvolumes to ro (created as rw so properties could be set first)
        for s in &resolved_subvolumes {
            if !s.writable {
                commands.push(format!(
                    "btrfs property set /tmp/btrfs-var-setup/{} ro true",
                    s.path
                ));
                commands.push(format!("echo \"  {}: read-only\"", s.path));
            }
        }

        commands.push("umount /tmp/btrfs-var-setup".to_string());
        commands.push("losetup -d \"$LOOP_DEV\"".to_string());
        commands.join("\n")
    } else {
        String::new()
    };

    let docker_section = {
        let docker_images: Vec<crate::utils::config::DockerImageRef> = ext_list
            .iter()
            .flat_map(|ext_name| {
                parsed
                    .get("extensions")
                    .and_then(|e| e.get(*ext_name))
                    .map(crate::utils::config::get_docker_images)
                    .unwrap_or_default()
            })
            .collect();
        if docker_images.is_empty() {
            "# No Docker images to prime".to_string()
        } else {
            let pull_commands: Vec<String> = docker_images
                .iter()
                .map(|img| {
                    format!(
                        r#"docker --host unix:///tmp/avocado-dockerd.sock pull --platform "linux/$DOCKER_ARCH" "{image}:{tag}"
echo "  Primed: {image}:{tag}""#,
                        image = img.image,
                        tag = img.tag
                    )
                })
                .collect();

            format!(
                r#"# Prime Docker image cache on var partition
echo "Priming Docker images on var partition..."
mkdir -p "$VAR_DIR/lib/docker"

# Verify dockerd is available
if ! command -v dockerd >/dev/null 2>&1; then
echo "ERROR: dockerd not found in SDK container. Docker image priming requires dockerd, containerd, runc, and docker CLI."
exit 1
fi

# Map target arch to Docker platform
# Use OECORE_TARGET_ARCH (CPU arch like x86_64/aarch64) from SDK environment
DOCKER_TARGET_ARCH="${{OECORE_TARGET_ARCH:-$TARGET_ARCH}}"
case "$DOCKER_TARGET_ARCH" in
aarch64) DOCKER_ARCH="arm64" ;;
x86_64) DOCKER_ARCH="amd64" ;;
*) echo "WARNING: Unknown target architecture '$DOCKER_TARGET_ARCH' for Docker platform mapping, defaulting to amd64"; DOCKER_ARCH="amd64" ;;
esac

# The SDK container may have the host's /sys bind-mounted (-v /sys:/sys),
# and --privileged gives write access even without that flag.
# Make the /sys/fs/cgroup mount private so the inner dockerd's mount
# events do not propagate to the host.  A bind+private mount preserves
# the existing cgroup controllers (required by dockerd) while isolating
# mount propagation.
_AVOCADO_CGROUP_PRIVATE=0
if mount --bind /sys/fs/cgroup /sys/fs/cgroup 2>/dev/null \
   && mount --make-private /sys/fs/cgroup 2>/dev/null; then
_AVOCADO_CGROUP_PRIVATE=1
else
echo "WARNING: Could not make /sys/fs/cgroup private — inner dockerd may leave stale cgroup entries on the host."
fi

# When the SDK container uses --network=host the inner dockerd shares the
# host network namespace and may delete the host's docker0 bridge on exit.
# Save its address now so we can restore it if needed.
_DOCKER0_ADDR=""
if ip link show docker0 >/dev/null 2>&1; then
_DOCKER0_ADDR=$(ip -4 addr show docker0 2>/dev/null | awk '/inet /{{print $2}}' | head -1)
fi

_avocado_docker_cleanup() {{
kill $DOCKERD_PID 2>/dev/null || true
wait $DOCKERD_PID 2>/dev/null || true
rm -f /tmp/avocado-dockerd.sock /tmp/avocado-dockerd.pid /tmp/avocado-dockerd.log
[ "$_AVOCADO_CGROUP_PRIVATE" = "1" ] && umount /sys/fs/cgroup 2>/dev/null || true
# Restore docker0 if the inner dockerd removed it from the host network namespace
if [ -n "$_DOCKER0_ADDR" ] && ! ip link show docker0 >/dev/null 2>&1; then
    echo "NOTE: inner dockerd removed host docker0 — restoring."
    ip link add name docker0 type bridge 2>/dev/null || true
    ip addr add "$_DOCKER0_ADDR" dev docker0 2>/dev/null || true
    ip link set docker0 up 2>/dev/null || true
fi
}}
trap _avocado_docker_cleanup EXIT

# Start temporary dockerd with data-root pointing at var staging.
# cgroupdriver=cgroupfs avoids systemd-cgroup interaction inside the container.
dockerd --data-root "$VAR_DIR/lib/docker" \
--host unix:///tmp/avocado-dockerd.sock \
--exec-opt native.cgroupdriver=cgroupfs \
--iptables=false --ip-masq=false \
--bridge=none \
--exec-root /tmp/avocado-dockerd \
--pidfile /tmp/avocado-dockerd.pid \
>/tmp/avocado-dockerd.log 2>&1 &
DOCKERD_PID=$!

# Wait for dockerd to be ready
echo "Waiting for temporary dockerd..."
for i in $(seq 1 30); do
if docker --host unix:///tmp/avocado-dockerd.sock info >/dev/null 2>&1; then
    break
fi
if ! kill -0 $DOCKERD_PID 2>/dev/null; then
    echo "ERROR: dockerd exited unexpectedly. Check /tmp/avocado-dockerd.log"
    cat /tmp/avocado-dockerd.log
    exit 1
fi
sleep 1
done

if ! docker --host unix:///tmp/avocado-dockerd.sock info >/dev/null 2>&1; then
echo "ERROR: dockerd failed to start within 30 seconds"
cat /tmp/avocado-dockerd.log
exit 1
fi

echo "Pulling Docker images for platform linux/$DOCKER_ARCH..."
{pull_commands}

trap - EXIT
_avocado_docker_cleanup
echo "Docker image priming complete.""#,
                pull_commands = pull_commands.join("\n")
            )
        }
    };

    let var_encrypt = |target: &str| {
        config
            .var_encrypt_runtimes(Some(parsed), target)
            .iter()
            .any(|n| n == runtime_name)
    };

    Ok(Rendered {
        docker_section,
        mkfs_flags,
        global_compress_flag: if global_compress_flag.is_empty() {
            String::new()
        } else {
            format!("{global_compress_flag} \\\n")
        },
        post_creation_section,
        device_tree_overlay_section,
        var_luks_room: if var_encrypt(target_arch) { "1" } else { "0" },
    })
}

/// The OTA half: everything an over-the-air update needs, rendered into the tail
/// of the build script.
///
/// Emits no preamble — it is spliced into `create_build_script`, which has
/// already defined every variable it reads and the `sign_amf` helper it calls.
/// That coupling is why this belongs in the build script rather than being run
/// separately: `stone bundle` resolves its inputs out of the runtime directory
/// the build just populated.
pub fn render_ota_tail(ctx: &VarImageContext) -> Result<String> {
    let r = prepare(ctx)?;
    Ok(format!(
        r#"
# stone requires a size for the `var` partition because every platform manifest
# declares it `expand: "true"` with no size — that is exactly what
# `--partition-size` overrides, and stone fails hard without it
# ("partition 'var' omits size; no --partition-size override was supplied").
#
# The var image itself is provisioning-only and does not exist yet, so this is
# the *declared* size, derived from the staged tree with the same headroom the
# image would carry. Its accuracy does not matter to what consumes this bundle:
# the bundle is OTA payload, an OTA never repartitions, and the partition is
# `expand: "true"` so its physical size is settled at provision time. Verified
# that no provisioning script reads the bundle — `avocado-provision-<target>`
# and the UFS flow both inject raw images. If a provision-from-bundle path is
# ever added, this becomes a real number and must come from the var image.
STONE_VAR_STAGED=$(du -sb "$VAR_DIR" 2>/dev/null | awk '{{print $1}}')
STONE_VAR_STAGED=${{STONE_VAR_STAGED:-0}}
# btrfs metadata measured at ~10% over the staged tree; round up generously
# rather than under-declare, plus the LUKS headroom when var is encrypted.
STONE_VAR_SIZE=$(( STONE_VAR_STAGED + STONE_VAR_STAGED / 5 + 67108864 ))

# Build OS bundle (.aos) — needs rootfs + initramfs + kernel + var (all built above)
STONE_MANIFEST="${{AVOCADO_STONE_MANIFEST:-$AVOCADO_SDK_PREFIX/stone/stone-$TARGET_ARCH.json}}"
STONE_INPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
# NOT $OUTPUT_DIR. `$AVOCADO_PREFIX/output/runtimes/<rt>` and
# `$AVOCADO_PREFIX/runtimes/<rt>` are two different trees: the former is where
# the SDK's stone tooling looks (`$AVOCADO_STONE_DATA_DIR`), where the
# device-tree overlay staging goes, and where provision keeps its state. A
# review flagged these as duplicating `OUTPUT_DIR` and "easy to drift"; they are
# not a duplicate, and repointing them left `provision` unpacking a stale
# bootfiles tarball from the old location — which flashed an old GPT and old
# firmware alongside a new system image. Caught on hardware, not here.
STONE_BUILD_DIR="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/stone"
# Clean previous stone build artifacts to prevent stale image reuse
rm -rf "$STONE_BUILD_DIR"
STONE_AOS_OUTPUT="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/os-bundle.aos"
export STONE_AOS_OUTPUT

# Build include path flags from AVOCADO_STONE_INCLUDE_PATHS
STONE_INCLUDE_FLAGS=""
if [ -n "${{AVOCADO_STONE_INCLUDE_PATHS:-}}" ]; then
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
{device_tree_overlay_section}
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
    --partition-size "var=$STONE_VAR_SIZE" \
    -o "$STONE_AOS_OUTPUT" \
    --build-dir "$STONE_BUILD_DIR"

# Patch manifest in var-staging to add os_bundle reference (for connect upload)
# The btrfs image for provisioning doesn't need os_bundle — initial flash doesn't OTA.
# Connect upload reads from var-staging directly, so it sees this update.
python3 << 'PYEOF'
{link_or_copy}
import json, hashlib, uuid, os, shutil

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
"#,
        device_tree_overlay_section = r.device_tree_overlay_section,
        link_or_copy = crate::commands::runtime::build::LINK_OR_COPY_PY,
    ))
}

/// The provisioning half: Docker priming and the var image.
///
/// Runs standalone from `provision`, so it emits its own preamble and resolves
/// the manifest the build produced.
pub fn render_var_image(ctx: &VarImageContext) -> Result<String> {
    let VarImageContext {
        runtime_name,
        target_arch,
        ..
    } = *ctx;
    let r = prepare(ctx)?;
    Ok(format!(
        r#"
# --- provisioning tail: var image and OS bundle -------------------------
# Rendered by `avocado provision`, ahead of the stone hook. `avocado build`
# does not produce any of this.
RUNTIME_NAME="{runtime_name}"
TARGET_ARCH="{target_arch}"
export OUTPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
VAR_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
export AVOCADO_IMAGES_DIR="$VAR_DIR/lib/avocado/images"
export AVOCADO_NS_UUID="{namespace_uuid}"

# Resolve the manifest this build produced. `provision` does not know the
# BUILD_ID that names its directory, so it resolves it the way the hash
# collection does: the `active` symlink, with a search as a fallback.
ACTIVE_LINK="$VAR_DIR/lib/avocado/active"
MANIFEST_FILE=""
if [ -L "$ACTIVE_LINK" ]; then
    MANIFEST_FILE="$VAR_DIR/lib/avocado/$(readlink "$ACTIVE_LINK")/manifest.json"
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_DIR/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    echo "ERROR: no manifest under $VAR_DIR/lib/avocado/runtimes/ — run \`avocado build\` first" >&2
    exit 1
fi
export AVOCADO_MANIFEST_PATH="$MANIFEST_FILE"
{docker_section}

VAR_IMAGE="$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"
VAR_INPUT_SIZE=$(du -sb "$VAR_DIR" 2>/dev/null | awk '{{print $1}}')
VAR_INPUT_MB=$(( VAR_INPUT_SIZE / 1048576 ))
echo "Building var image (${{VAR_INPUT_MB}}MB source)..."

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
{mkfs_flags} \
{global_compress_flag}    -f "$VAR_IMAGE"

# var.encrypt: the first boot converts this filesystem to LUKS2 in place, which
# needs 32 MiB in front of the data (cryptsetup reencrypt --reduce-device-size)
# that cryptsetup-var obtains by shrinking the filesystem. `mkfs.btrfs -r`
# packs its chunks to the content, so a tight image has nothing to shrink into;
# rebuild it at the tight size plus 64 MiB so the room is inside the filesystem
# the runtime declared it needs. The partition stays exactly the image size.
if [ "{var_luks_room}" = "1" ]; then
    VAR_TIGHT_SIZE=$(stat -c%s "$VAR_IMAGE")
    mkfs.btrfs -r "$VAR_DIR" \
{mkfs_flags} \
{global_compress_flag}    -b $(( VAR_TIGHT_SIZE + 67108864 )) -f "$VAR_IMAGE"
fi

kill $_PROGRESS_PID 2>/dev/null; wait $_PROGRESS_PID 2>/dev/null || true

{post_creation_section}
FINAL_SIZE=$(stat -c%s "$VAR_IMAGE" 2>/dev/null || echo 0)
FINAL_MB=$(( FINAL_SIZE / 1048576 ))
echo ""
echo "Built var image: ${{FINAL_MB}}MB"
"#,
        runtime_name = runtime_name,
        target_arch = target_arch,
        namespace_uuid = crate::commands::rootfs::image::NAMESPACE_UUID,
        docker_section = r.docker_section,
        mkfs_flags = r.mkfs_flags,
        global_compress_flag = r.global_compress_flag,
        post_creation_section = r.post_creation_section,
        var_luks_room = r.var_luks_room,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BASE: &str = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  test-runtime:
    target: "x86_64"
"#;

    fn ctx_for<'a>(
        config: &'a Config,
        parsed: &'a serde_yaml::Value,
        merged: &'a serde_yaml::Value,
        exts: &'a [String],
    ) -> VarImageContext<'a> {
        VarImageContext {
            runtime_name: "test-runtime",
            target_arch: "x86_64",
            config,
            parsed,
            merged_runtime: merged,
            ext_list: exts,
        }
    }

    fn render_both(content: &str, exts: &[String]) -> (String, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("avocado.yaml");
        std::fs::write(&path, content).unwrap();
        let config = Config::load(path.to_str().unwrap()).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(content).unwrap();
        let merged = config
            .get_merged_runtime_config("test-runtime", "x86_64", path.to_str().unwrap())
            .unwrap()
            .unwrap_or_default();
        let ctx = ctx_for(&config, &parsed, &merged, exts);
        (
            render_var_image(&ctx).unwrap(),
            render_ota_tail(&ctx).unwrap(),
        )
    }

    fn var_half(content: &str) -> String {
        render_both(content, &[]).0
    }
    fn ota_half(content: &str) -> String {
        render_both(content, &[]).1
    }

    /// The split itself, which is the whole point of this module: what an OTA
    /// needs is in the build tail, what only provisioning consumes is not.
    /// Getting this backwards is not a build failure — it is a `deploy` that
    /// reports success and leaves the device on its old OS.
    #[test]
    fn the_halves_split_ota_payload_from_provisioning_work() {
        let (var, ota) = render_both(BASE, &[]);

        // OTA payload: the platform build hook (on UKI platforms this *is* the
        // kernel and initramfs), the bundle, and the manifest patch that names
        // it. None of this may live in the provisioning half.
        for needle in [
            "$AVOCADO_SDK_PREFIX/usr/bin/avocado-build-$TARGET_ARCH",
            "stone bundle",
            "os_bundle",
            "sign_amf \"$AVOCADO_MANIFEST_PATH\"",
        ] {
            assert!(ota.contains(needle), "OTA half must contain {needle}");
            assert!(
                !var.contains(needle),
                "provisioning half must not contain {needle}"
            );
        }

        // Provisioning-only: the var image and the Docker priming that fills it.
        for needle in ["mkfs.btrfs -r \"$VAR_DIR\"", "VAR_IMAGE="] {
            assert!(var.contains(needle), "var half must contain {needle}");
            assert!(!ota.contains(needle), "OTA half must not contain {needle}");
        }
    }

    /// `stone bundle` fails hard without a var partition size, because every
    /// platform manifest declares `var` as `expand: "true"` with no size — the
    /// exact case `--partition-size` exists to override. The var image does not
    /// exist at build time, so the size is declared from the staged tree.
    #[test]
    fn the_bundle_declares_a_var_partition_size_without_a_var_image() {
        let ota = ota_half(BASE);
        assert!(ota.contains("--partition-size \"var=$STONE_VAR_SIZE\""));
        assert!(
            ota.contains("du -sb \"$VAR_DIR\""),
            "the size comes from the staged tree, not from an image that does not exist yet"
        );
        assert!(
            !ota.contains("$FINAL_SIZE"),
            "FINAL_SIZE is a property of the built var image and is not available here"
        );
    }

    /// `$AVOCADO_PREFIX/output/runtimes/<rt>` and `$AVOCADO_PREFIX/runtimes/<rt>`
    /// are two different trees and stone's are in the first one. The SDK's stone
    /// tooling reads that tree (`$AVOCADO_STONE_DATA_DIR`), the device-tree
    /// overlay staging writes there, and `provision` keeps its state there.
    ///
    /// This exists because a review read them as duplicating `OUTPUT_DIR` and I
    /// repointed them without checking. `provision` then unpacked a *stale*
    /// bootfiles tarball still sitting at the old path and flashed an old GPT
    /// and old firmware alongside a newly built system image — a board that
    /// booted to initrd emergency for a reason nowhere near the symptom. It
    /// cost hardware debugging time, so it is pinned.
    #[test]
    fn stone_writes_into_the_tree_the_sdk_reads_not_output_dir() {
        let ota = ota_half(BASE);
        assert!(ota
            .contains(r#"STONE_BUILD_DIR="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/stone""#));
        assert!(ota.contains(
            r#"STONE_AOS_OUTPUT="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/os-bundle.aos""#
        ));
        assert!(
            !ota.contains(r#"STONE_BUILD_DIR="$OUTPUT_DIR"#),
            "OUTPUT_DIR is a different tree; stone's data dir is not under it"
        );
        assert!(
            !ota.contains(r#"STONE_AOS_OUTPUT="$OUTPUT_DIR"#),
            "OUTPUT_DIR is a different tree; the bundle is not under it"
        );
    }

    /// The provisioning half runs standalone, so it must define everything it
    /// reads that the entrypoint prologue does not supply. A bundle carries no
    /// build script.
    #[test]
    fn the_var_half_is_self_contained() {
        let var = var_half(BASE);
        for v in [
            "RUNTIME_NAME=",
            "TARGET_ARCH=",
            "export OUTPUT_DIR=",
            "VAR_DIR=",
            "export AVOCADO_MANIFEST_PATH=",
        ] {
            assert!(var.contains(v), "var half must define {v}");
        }
        // Paths must match what `build` actually wrote, not what looks plausible.
        assert!(var.contains(r#"VAR_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging""#));
        assert!(var.contains(r#"ACTIVE_LINK="$VAR_DIR/lib/avocado/active""#));
        assert!(!var.contains("$AVOCADO_PREFIX/output"), "no such directory");
    }

    /// The OTA half is spliced into the build script and deliberately inherits
    /// its variables and its `sign_amf` helper, so it must NOT redeclare them —
    /// a second `RUNTIME_NAME=` would shadow the build's own.
    #[test]
    fn the_ota_half_inherits_the_build_script_and_redeclares_nothing() {
        let ota = ota_half(BASE);
        for v in ["RUNTIME_NAME=", "TARGET_ARCH=", "VAR_DIR=", "sign_amf()"] {
            assert!(
                !ota.contains(v),
                "the OTA half inherits {v} from the build script"
            );
        }
    }

    /// var.encrypt leaves room for the LUKS header to be added in place on first
    /// boot; a plaintext runtime keeps the tight image.
    #[test]
    fn var_encrypt_leaves_room_for_the_luks_header() {
        let on = BASE.to_string() + "    var:\n      encrypt: true\n";
        let s = var_half(&on);
        assert!(
            s.contains("if [ \"1\" = \"1\" ]; then"),
            "second mkfs pass is armed"
        );
        assert!(s.contains("-b $(( VAR_TIGHT_SIZE + 67108864 ))"));

        let off = on.replace("encrypt: true", "encrypt: false");
        assert!(
            var_half(&off).contains("if [ \"0\" = \"1\" ]; then"),
            "plaintext runtime keeps the tight image"
        );
    }

    /// Device-tree overlays feed `stone --overlay`, so they belong to the OTA
    /// half with the bundle.
    #[test]
    fn device_tree_overlays_reach_stone() {
        let none = ota_half(BASE);
        assert!(!none.contains("device-tree overlays"));
        assert!(none.contains("STONE_OVERLAY_FLAG=\"\""));

        let with = BASE.to_string()
            + r#"    extensions:
      - test-ext

extensions:
  test-ext:
    version: "1.0.0"
    types:
      - sysext
    device_tree_overlays:
      - name: my-spi
        src: overlays/my-spi.dtso
"#;
        let ota = render_both(&with, &["test-ext".to_string()]).1;
        assert!(ota.contains("device-tree overlays"));
        assert!(ota.contains("device-tree-overlay-deliver"));
        assert!(ota.contains("STONE_OVERLAY_FLAG=\"--overlay $DTO_FRAGMENT\""));
    }

    /// Both halves must parse. Cutting one script into two is exactly how an
    /// unbalanced `if` or a stranded heredoc ships.
    #[test]
    fn both_halves_are_valid_bash() {
        let dir = TempDir::new().unwrap();
        let encrypted = BASE.to_string() + "    var:\n      encrypt: true\n";
        for (name, script) in [
            ("var", var_half(BASE)),
            ("var-encrypted", var_half(&encrypted)),
            ("ota", ota_half(BASE)),
        ] {
            let p = dir.path().join(format!("{name}.sh"));
            std::fs::write(&p, &script).unwrap();
            let out = std::process::Command::new("bash")
                .arg("-n")
                .arg(&p)
                .output()
                .expect("bash -n");
            assert!(
                out.status.success(),
                "{name} is not valid bash: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
