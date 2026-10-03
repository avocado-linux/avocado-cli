//! Shell-fragment generator for wrapping a built layer image into a
//! signed `.kab` using the SDK's nativesdk-kabtool.
//!
//! Used by:
//!   * `rootfs image`     — wraps the erofs into a kos.layer.basefs kab
//!   * `initramfs image`  — wraps the cpio into a kos.layer.initramfs kab
//!   * `kernel image`     — wraps the kernel binary into a kos.layer.kernel kab
//!   * `runtime build`    — wraps each role-relevant artifact during a runtime build
//!
//! The fragment expects these env vars to be set at execution time:
//!   * `$<env_var>`        — host path of the layer image to wrap (e.g. `$AVOCADO_ROOTFS_IMAGE`)
//!   * `$KAB_KEYSET_FILE`  — path to the keyset (bind-mounted into the container)
//!   * `$OUTPUT_DIR`       — directory the resulting `.kab` lands in
//!
//! It re-exports `$<env_var>` to point at the produced `.kab` so downstream
//! steps in the same script can pick up the wrapped artifact directly.

/// Where an image command bind-mounts the SBOM document it built on the host.
///
/// Keyed by label because `runtime build` wraps rootfs, initramfs and kernel
/// in one container run and each carries its own document; a single fixed
/// path would have the last mount win and ship one component's inventory
/// inside another's kab.
pub fn sbom_mount_path(label: &str) -> String {
    format!("/tmp/avocado-sbom-{label}.json")
}

/// Build a bash fragment that, when sourced, wraps the file at
/// `$<env_var>` into a signed `.kab` using `kabtool` with `image_args`.
///
/// `label` shows up in echo lines and the embedded `descriptor.json`.
/// `source_id_expr` is a bash expression interpolated into the
/// descriptor's `kos.build.source` field — e.g. `"$AVOCADO_RUNTIME_VERSION"`
/// for runtime builds, or just `"standalone-$(date +%s)"` for one-off
/// invocations.
///
/// `sbom_path` is the in-container path of an SPDX document to carry inside
/// the payload as `sbom.json`, from `image: { sbom: true }`. The caller
/// bind-mounts it; this fragment only copies it in and names it in the zip.
/// It is added as a third entry rather than written into the layer, so the
/// layer image stays byte-for-byte what it was — its dm-verity tree, its
/// root hash and every content-derived id are unaffected by shipping an
/// inventory alongside it. Entry order is fixed (`layer.img`,
/// `descriptor.json`, `sbom.json`): it decides the zip's bytes, and two
/// builds of one component have to produce one payload.
pub fn generate_kab_wrap_script(
    label: &str,
    env_var: &str,
    image_args: &str,
    source_id_expr: &str,
    sbom_path: Option<&str>,
) -> String {
    // `cp` rather than a mount straight into the tmpdir: the tmpdir is made
    // per run by `mktemp -d`, and the zip must hold a regular file.
    let (sbom_copy, sbom_entry) = match sbom_path {
        Some(path) => (
            format!("    cp \"{path}\" \"$KAB_TMPDIR/sbom.json\"\n"),
            " sbom.json",
        ),
        None => (String::new(), ""),
    };
    format!(
        r#"
# --- KAB wrap: {label} ---
if [ -n "${env_var}" ] && [ -f "${env_var}" ]; then
    echo "Wrapping {label} as KAB..."
    KAB_TMPDIR=$(mktemp -d)
    cp "${env_var}" "$KAB_TMPDIR/layer.img"
    cat > "$KAB_TMPDIR/descriptor.json" << DESCEOF
{{"kos":{{"build":{{"source":"{label}-{source_id_expr}"}}}}}}
DESCEOF
{sbom_copy}    (cd "$KAB_TMPDIR" && zip -Z store tmp.zip layer.img descriptor.json{sbom_entry})
    KAB_OUTPUT="$OUTPUT_DIR/$(basename "${env_var}").kab"
    rm -f "$KAB_OUTPUT"
    kabtool {image_args} \
        -k "$KAB_KEYSET_FILE" \
        -z "$KAB_TMPDIR/tmp.zip" "$KAB_TMPDIR/output.kab"
    cp "$KAB_TMPDIR/output.kab" "$KAB_OUTPUT"
    rm -rf "$KAB_TMPDIR"
    export {env_var}="$KAB_OUTPUT"
    echo "Wrapped {label} -> $KAB_OUTPUT"
fi
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nobody who has not opted in may get different bytes out of this.
    /// Pinned against a verbatim copy of the fragment as it stood before
    /// `sbom_path` existed, because the option is threaded through the
    /// middle of the template: a stray newline or a lost indent would
    /// re-sign every `kernel` kab — which is never offered the option — and
    /// every rootfs and initramfs that left it off.
    #[test]
    fn the_fragment_is_unchanged_for_callers_that_pass_no_sbom() {
        let before = r#"
# --- KAB wrap: kernel ---
if [ -n "$AVOCADO_KERNEL_IMAGE" ] && [ -f "$AVOCADO_KERNEL_IMAGE" ]; then
    echo "Wrapping kernel as KAB..."
    KAB_TMPDIR=$(mktemp -d)
    cp "$AVOCADO_KERNEL_IMAGE" "$KAB_TMPDIR/layer.img"
    cat > "$KAB_TMPDIR/descriptor.json" << DESCEOF
{"kos":{"build":{"source":"kernel-$RUNTIME_VERSION"}}}
DESCEOF
    (cd "$KAB_TMPDIR" && zip -Z store tmp.zip layer.img descriptor.json)
    KAB_OUTPUT="$OUTPUT_DIR/$(basename "$AVOCADO_KERNEL_IMAGE").kab"
    rm -f "$KAB_OUTPUT"
    kabtool -b -t kos.layer.kernel \
        -k "$KAB_KEYSET_FILE" \
        -z "$KAB_TMPDIR/tmp.zip" "$KAB_TMPDIR/output.kab"
    cp "$KAB_TMPDIR/output.kab" "$KAB_OUTPUT"
    rm -rf "$KAB_TMPDIR"
    export AVOCADO_KERNEL_IMAGE="$KAB_OUTPUT"
    echo "Wrapped kernel -> $KAB_OUTPUT"
fi
"#;
        assert_eq!(
            generate_kab_wrap_script(
                "kernel",
                "AVOCADO_KERNEL_IMAGE",
                "-b -t kos.layer.kernel",
                "$RUNTIME_VERSION",
                None,
            ),
            before
        );
    }

    #[test]
    fn the_sbom_is_a_third_payload_entry_only_when_one_was_staged() {
        let without = generate_kab_wrap_script("rootfs", "AVOCADO_ROOTFS_IMAGE", "-b", "$V", None);
        assert!(without.contains("zip -Z store tmp.zip layer.img descriptor.json)"));
        assert!(!without.contains("sbom.json"));

        let path = sbom_mount_path("rootfs");
        let with =
            generate_kab_wrap_script("rootfs", "AVOCADO_ROOTFS_IMAGE", "-b", "$V", Some(&path));
        assert!(with.contains(&format!("cp \"{path}\" \"$KAB_TMPDIR/sbom.json\"")));
        // Order is part of the payload's bytes: two builds of one component
        // have to produce one zip, so the entry goes last and stays there.
        assert!(with.contains("zip -Z store tmp.zip layer.img descriptor.json sbom.json)"));
        // And it is copied before the zip is made, not after.
        assert!(with.find("sbom.json\"").unwrap() < with.find("zip -Z store").unwrap());
    }

    #[test]
    fn each_component_mounts_its_own_document() {
        // One runtime build wraps rootfs, initramfs and kernel in a single
        // container run. A shared path would have the last mount win and put
        // one component's inventory inside another's kab.
        let paths = ["rootfs", "initramfs", "kernel"].map(sbom_mount_path);
        let mut sorted = paths.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "got: {paths:?}");
    }
}
