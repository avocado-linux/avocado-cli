//! Guard: the image runs actually inject `SOURCE_DATE_EPOCH`.
//!
//! Both halves of this feature are unit-tested — `inject_source_date_epoch`'s
//! three-way `Option` behavior in `utils::container`, and the rootfs script's
//! `mkfs.erofs -T "${SOURCE_DATE_EPOCH:-0}"` in `rootfs::image` — but the call
//! that connects them was covered by neither. Deleting it leaves the whole
//! suite green while a configured epoch silently stops reaching the container,
//! which is the exact regression the feature exists to prevent.
//!
//! The check lives here rather than in a `mod tests` inside those files
//! because the needle would then appear in the file it scans, and the
//! assertion would hold with the real call site deleted. (Confirmed the hard
//! way.) Same reason `no_hand_rolled_stdio_flags.rs` sits out here.
//!
//! ponytail: pins the call's spelling, not its effect. Testing the effect
//! means lifting env-map construction out of the async run path in both
//! commands; worth doing when a third image type wants the same stamp.

use std::fs;
use std::path::PathBuf;

/// The call every image-building run has to make.
const INJECTION: &str = "inject_source_date_epoch(&mut env_vars, config.source_date_epoch)";

fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn rootfs_image_run_injects_source_date_epoch() {
    assert!(
        source("src/commands/rootfs/image.rs").contains(INJECTION),
        "the rootfs image run must inject SOURCE_DATE_EPOCH into its container env"
    );
}

#[test]
fn initramfs_image_run_injects_source_date_epoch() {
    // Inert on this base — the initramfs script has no reader until the
    // mtime-normalization pass lands — so a deletion here would be entirely
    // invisible without this.
    assert!(
        source("src/commands/initramfs/image.rs").contains(INJECTION),
        "the initramfs image run must inject SOURCE_DATE_EPOCH into its container env"
    );
}
