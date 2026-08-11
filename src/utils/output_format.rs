//! Shared `--output` flag value type for commands that support
//! machine-readable output alongside the default human prose.
//!
//! Originally introduced for the `connect auth` subcommands; promoted
//! here so `init`, `config show`, `install`, `build`, and `provision`
//! can all share one type and consistent JSON conventions.
//!
//! Also exposes a process-wide "JSON output active" flag. Long-running
//! commands (install / build / provision) flip it on at the start of
//! their `execute()` so that the TUI subsystem skips painting and the
//! task scheduler emits NDJSON events instead — that way the desktop
//! app can drive its own progress UI from a clean event stream while
//! the human-output paths behave as they did before.

use clap::ValueEnum;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Output format selector. Default is human-readable prose;
/// `Json` switches the command to emit machine-readable output:
/// a single JSON object for single-shot commands, or NDJSON (one
/// JSON object per line, flushed after each) for long-running ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

impl OutputFormat {
    pub fn is_json(self) -> bool {
        matches!(self, OutputFormat::Json)
    }
}

/// Emit one JSON value followed by a newline, flushing stdout
/// afterwards. Use for NDJSON event streams during long-running
/// commands — the flush matters because stdout is block-buffered
/// when piped, and consumers (the desktop app) need each line
/// promptly to react in real time.
pub fn emit_json_event(value: &serde_json::Value) {
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Emit one JSON object as the entire stdout output of a
/// single-shot command. Identical wire format to `emit_json_event`;
/// kept as a separate function so callsites read at-a-glance as
/// "this command emits exactly one object" vs "this command
/// emits an event stream".
pub fn emit_json_object(value: &serde_json::Value) {
    emit_json_event(value)
}

// ---------------------------------------------------------------------------
// Step-event helpers — the same NDJSON vocabulary the task renderer emits for
// `build`/`install` (`task_registered` / `step` / `step_error`), so the
// desktop's per-step strip works identically for imperative commands
// (`connect upload`, `connect deploy`) that don't run the task scheduler.
// All are no-ops outside JSON output mode.

/// Register a step up front so the desktop shows it (pending) before it runs.
pub fn emit_task_registered(name: &str, label: &str) {
    if is_json_output_active() {
        emit_json_event(&serde_json::json!({
            "event": "task_registered",
            "name": name,
            "label": label,
        }));
    }
}

/// Transition a step to `pending` / `running` / `success` / `failed` / `skipped`.
pub fn emit_step(name: &str, status: &str) {
    if is_json_output_active() {
        emit_json_event(&serde_json::json!({
            "event": "step",
            "name": name,
            "status": status,
        }));
    }
}

/// Attach an error message to a step so the desktop can show it inline.
pub fn emit_step_error(name: &str, message: &str) {
    if is_json_output_active() {
        emit_json_event(&serde_json::json!({
            "event": "step_error",
            "name": name,
            "message": message,
        }));
    }
}

// ---------------------------------------------------------------------------
// Process-wide "JSON output active" flag.
//
// Long-running commands that wrap a task scheduler / TUI flip this on at the
// start of their execute(). The TUI subsystem reads it in `should_use_tui()`
// (skip painting), and the TaskRenderer state-mutators tap it to emit NDJSON
// events the desktop app can consume. Use the guard rather than the raw
// setters so the flag clears on early-return / panic / completion.

static JSON_OUTPUT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True iff the current process is in JSON output mode.
pub fn is_json_output_active() -> bool {
    JSON_OUTPUT_ACTIVE.load(Ordering::Relaxed)
}

/// Whether the command line asks for JSON output, read from `argv` rather than
/// from [`is_json_output_active`].
///
/// Every `JsonOutputGuard::enable` site is inside a command's `execute()`, so
/// the flag is still false for anything that runs before the dispatch `match` —
/// `ensure_routed_for_process` (main.rs) most of all. Code there that asks
/// `is_json_output_active()` gets `false` on a `--output json` run and takes the
/// human-output branch, which is the opposite of what it wanted. `argv` is
/// already parsed by the time the process starts, so it answers correctly at
/// any point.
///
/// Deliberately not clap: this is consulted both before clap runs and after the
/// parsed `Cli` has been partially moved into the dispatch match, and a second
/// parse could fail on input the real one accepts.
pub fn json_requested_on_command_line() -> bool {
    json_requested_in(std::env::args())
}

/// Subcommand paths that open a `trailing_var_arg` pass-through: `sdk run`,
/// `sdk dnf`, `runtime dnf`, `ext dnf`. Everything after one of these belongs
/// to the wrapped tool, and none of the four takes an `--output` of its own, so
/// a format flag past this point is the tool's, not ours.
///
/// Matched as a `(parent, child)` pair, not on the bare child token. `run` and
/// `dnf` are ordinary *values* elsewhere on the line: `avocado install dnf
/// --output json` installs the `dnf` RPM and does mean that JSON. A bare-token
/// cutoff stopped there, read the run as human output, and let a raw
/// `[WARNING]` from `ensure_routed_for_process` into the stream avocado-desktop
/// parses — the corruption this scan exists to prevent.
///
/// What the pair still cannot separate is a pass-through name sitting where its
/// own parent's name precedes it as a value, `avocado install sdk dnf`. That
/// needs the arg spec, which is clap's job and not a scan's.
const PASS_THROUGH: [(&str, &str); 4] = [
    ("sdk", "run"),
    ("sdk", "dnf"),
    ("runtime", "dnf"),
    ("ext", "dnf"),
];

/// The scan itself, over an injected `argv` so it can be tested.
///
/// Three things the obvious `skip_while(|a| a != "--output")` gets wrong, all of
/// which flip the answer rather than merely blur it:
///
/// - **Last occurrence wins.** `--output` is `ArgAction::Set`, so clap resolves
///   `--output plain --output json` to json. Stopping at the first match reports
///   human, and a wrapper appending `--output json` to a line that already
///   carries a format is exactly how that arises.
/// - **Nothing inside a pass-through is ours.** Four subcommands declare
///   `trailing_var_arg = true, allow_hyphen_values = true` and have no
///   `--output` of their own: `sdk run` (main.rs), `sdk dnf`, `runtime dnf`
///   and `ext dnf`. A `--` separator stops the scan, but clap does not require
///   one — it absorbs everything from the first unmatched token — so
///   `sdk run mytool --output json` needs stopping too. Hence [`PASS_THROUGH`].
/// - **Both spellings.** `--output=json` is valid clap input.
///
/// Known bound: a value-taking flag with `allow_hyphen_values` can swallow the
/// literal `--output` (`--dnf-arg --output json`), which this reads as a format
/// request. The pair misses in the other direction too: `--runs-on`,
/// `--nfs-port` and `--sdk-arch` are `global = true`, so a value of one sitting
/// between parent and child makes `previous` the value rather than the parent —
/// `sdk --runs-on user@host dnf install foo` never matches `("sdk", "dnf")`, the
/// cutoff never fires, and the wrapped tool's args get scanned. Both bounds fail
/// the same safe way: at worst a `--output json` that is not ours reads as one,
/// and the notices go quiet instead of landing on a stream. Resolving either
/// needs the full arg spec — clap's job, not a scan's.
fn json_requested_in(args: impl Iterator<Item = String>) -> bool {
    let mut requested = false;
    let mut previous = String::new();
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--" || PASS_THROUGH.contains(&(previous.as_str(), arg.as_str())) {
            break;
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            requested = value == "json";
        } else if arg == "--output" {
            requested = args.next().as_deref() == Some("json");
        }
        // `--output`'s consumed value is deliberately not the next `previous`:
        // the pair looks for a subcommand path, and a flag's value is not one.
        previous = arg;
    }
    requested
}

/// RAII guard: enables JSON output mode for the lifetime of the guard,
/// disables it on drop (including on unwind). Multiple commands shouldn't
/// nest, but if they do, the flag is reference-counted via a depth
/// counter so an inner guard exiting doesn't disable mode for the outer.
pub struct JsonOutputGuard {
    _private: (),
}

impl JsonOutputGuard {
    pub fn enable() -> Self {
        JSON_OUTPUT_ACTIVE.store(true, Ordering::Relaxed);
        Self { _private: () }
    }
}

impl Drop for JsonOutputGuard {
    fn drop(&mut self) {
        JSON_OUTPUT_ACTIVE.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> impl Iterator<Item = String> {
        args.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn json_is_recognised_in_both_spellings_clap_accepts() {
        // The separated form is what main's upgrade banner checked. The
        // attached form is equally valid input and was silently reading as a
        // human-output run, which is the case that leaks prose into a merged
        // stream.
        assert!(json_requested_in(argv(&[
            "avocado", "sbom", "--output", "json"
        ])));
        assert!(json_requested_in(argv(&[
            "avocado",
            "sbom",
            "--output=json"
        ])));
    }

    #[test]
    fn a_non_json_output_format_is_not_json() {
        assert!(!json_requested_in(argv(&["avocado", "sbom"])));
        assert!(!json_requested_in(argv(&[
            "avocado", "sbom", "--output", "plain"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado",
            "sbom",
            "--output=plain"
        ])));
    }

    #[test]
    fn a_trailing_output_flag_does_not_panic_or_claim_json() {
        // clap would reject this, but the scan runs before clap does.
        assert!(!json_requested_in(argv(&["avocado", "sbom", "--output"])));
    }

    #[test]
    fn json_as_some_other_flags_value_is_not_an_output_format() {
        // `nth(1)` after the first `--output`-looking token only. A bare
        // `json` elsewhere on the line is a subcommand arg, not a format.
        assert!(!json_requested_in(argv(&["avocado", "sbom", "-o", "json"])));
        assert!(!json_requested_in(argv(&["avocado", "run", "json"])));
    }

    #[test]
    fn the_last_output_flag_wins_the_way_clap_resolves_it() {
        // `--output` is ArgAction::Set. A wrapper appending `--output json` to
        // a line that already carries a format is the case that matters: clap
        // emits JSON, and a first-match scan reported human, so prose went out
        // on a stream something was parsing.
        assert!(json_requested_in(argv(&[
            "avocado", "sbom", "--output", "plain", "--output", "json"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado", "sbom", "--output", "json", "--output", "plain"
        ])));
    }

    #[test]
    fn a_pass_through_arg_after_a_bare_dash_dash_is_not_ours() {
        // `sdk run`, `sdk dnf` and `runtime dnf` take trailing_var_arg
        // pass-throughs and have no --output of their own, so this is a
        // human-output run whose argv happens to contain the flag. Reading it
        // as JSON silences the routing warning on a run that wanted it.
        assert!(!json_requested_in(argv(&[
            "avocado", "sdk", "run", "--", "mytool", "--output", "json"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado",
            "sdk",
            "run",
            "--",
            "mytool",
            "--output=json"
        ])));
    }

    #[test]
    fn a_pass_through_needs_no_separator_to_swallow_the_flag() {
        // clap's trailing_var_arg absorbs from the first unmatched token, so
        // these parse fine and are human-output runs. Reading them as JSON
        // silences the routing warning on exactly the runs that want it.
        assert!(!json_requested_in(argv(&[
            "avocado", "sdk", "run", "mytool", "--output", "json"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado", "sdk", "run", "--output", "json", "mytool"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado", "sdk", "dnf", "install", "--output", "json"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado", "ext", "dnf", "install", "--output", "json"
        ])));
        assert!(!json_requested_in(argv(&[
            "avocado", "runtime", "dnf", "install", "--output", "json"
        ])));
    }

    #[test]
    fn a_pass_through_name_used_as_a_value_is_not_a_pass_through() {
        // `install` takes positional packages, so clap resolves each of these to
        // `Install { packages: [..], output: Json }` — a real JSON run. A cutoff
        // on the bare `run`/`dnf` token read them as human output, and
        // `Commands::Install` is in `needs_vm_routing`, so route.rs's ANSI
        // `[WARNING]` went into the stream avocado-desktop parses.
        assert!(json_requested_in(argv(&[
            "avocado", "install", "dnf", "--output", "json"
        ])));
        assert!(json_requested_in(argv(&[
            "avocado", "install", "run", "--output", "json"
        ])));
        // Same token one level deeper, and as another flag's value.
        assert!(json_requested_in(argv(&[
            "avocado", "sdk", "compile", "run", "--output", "json"
        ])));
        assert!(json_requested_in(argv(&[
            "avocado",
            "install",
            "--container-arg",
            "run",
            "--output",
            "json"
        ])));
    }

    #[test]
    fn our_own_flag_before_a_pass_through_still_counts() {
        // The cutoff must not swallow a format the user really did ask for on
        // the near side of the subcommand.
        assert!(json_requested_in(argv(&[
            "avocado", "--output", "json", "sdk", "run", "mytool"
        ])));
    }

    #[test]
    fn our_own_flag_before_a_bare_dash_dash_still_counts() {
        // The `--` cutoff must not swallow a format the user really did ask
        // for on the near side of it. Spelled on a command that has its own
        // `--output`: `sdk run` would stop the scan at `run` regardless, which
        // would make this pass for the wrong reason.
        assert!(json_requested_in(argv(&[
            "avocado", "sbom", "--output", "json", "--", "trailing"
        ])));
    }

    #[test]
    fn the_flag_is_found_wherever_it_sits_on_the_line() {
        // Global flags can precede the subcommand, so a scan anchored to a
        // fixed position would miss them.
        assert!(json_requested_in(argv(&[
            "avocado", "--output", "json", "sbom"
        ])));
    }
}
