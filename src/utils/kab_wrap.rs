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

/// Build a bash fragment that, when sourced, wraps the file at
/// `$<env_var>` into a signed `.kab` using `kabtool` with `image_args`.
///
/// `label` shows up in echo lines and the embedded `descriptor.json`.
/// `source_id_expr` is a bash expression interpolated into the
/// descriptor's `kos.build.source` field — e.g. `"$AVOCADO_RUNTIME_VERSION"`
/// for runtime builds, or just `"standalone-$(date +%s)"` for one-off
/// invocations.
pub fn generate_kab_wrap_script(
    label: &str,
    env_var: &str,
    image_args: &str,
    source_id_expr: &str,
) -> String {
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
    (cd "$KAB_TMPDIR" && zip -Z store tmp.zip layer.img descriptor.json)
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
