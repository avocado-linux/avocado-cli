//! Guard: which commands run the feed preflight, and where.
//!
//! The preflight probes the configured feeds before the first download so a
//! dead feed fails fast with one clear error instead of mid-fetch. Deleting the
//! call from an installing command leaves the unit suite green, and moving it
//! after `materialize_feeds` makes the Connect token mint happen before the
//! probe. Passthrough commands (dnf, run) and `update` must stay free of it.
//!
//! The scan lives here rather than in a `mod tests` inside those files because
//! the needle would then appear in the file it scans (same reason as
//! `source_date_epoch_wiring.rs`).
//!
//! ponytail: pins the call's spelling and order, not its effect. The effect is
//! covered by the `preflight_feeds_once` unit tests in `utils::config`.

use std::fs;
use std::path::PathBuf;

const CALL: &str = "preflight_feeds_once(";
const MINT: &str = "materialize_feeds";

/// (module, must_call, must_precede_materialize). Extend this table when a
/// command gains or loses the preflight.
const TABLE: &[(&str, bool, bool)] = &[
    ("src/commands/ext/install.rs", true, true),
    ("src/commands/runtime/install.rs", true, true),
    ("src/commands/rootfs/install.rs", true, true),
    ("src/commands/initramfs/install.rs", true, true),
    ("src/commands/install.rs", true, false),
    ("src/commands/sdk/install.rs", true, false),
    ("src/commands/fetch.rs", true, true),
    ("src/commands/ext/fetch.rs", true, true),
    ("src/commands/sdk/dnf.rs", false, false),
    ("src/commands/runtime/dnf.rs", false, false),
    ("src/commands/ext/dnf.rs", false, false),
    ("src/commands/sdk/run.rs", false, false),
    ("src/commands/update.rs", false, false),
];

/// The module's code with comment lines removed, so a needle inside a comment
/// (a commented-out call, a doc line naming `materialize_feeds`) cannot satisfy
/// or move the checks below.
fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn preflight_wiring_matches_table() {
    for (module, must_call, before_mint) in TABLE {
        let src = source(module);
        let call = src.find(CALL);
        if *must_call {
            let call = call.unwrap_or_else(|| panic!("{module} must call {CALL}"));
            if *before_mint {
                let mint = src
                    .find(MINT)
                    .unwrap_or_else(|| panic!("{module} must call {MINT}"));
                assert!(
                    call < mint,
                    "{module}: {CALL} must come before {MINT} so the probe runs before the Connect mint"
                );
            }
        } else {
            assert!(call.is_none(), "{module} must not call {CALL}");
        }
    }
}
