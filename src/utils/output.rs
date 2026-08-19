//! Output utilities for Avocado CLI.
//!
//! When a TUI renderer is active (registered via `tui::set_active_renderer`),
//! all print functions automatically route through `renderer.print_above()` so
//! they don't corrupt the TUI display region.

use std::io::{self, IsTerminal, Write};

/// Output level for controlling verbosity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutputLevel {
    Normal,
    Verbose,
    Debug,
}

/// When a TUI renderer is active, info/success/warning/plain messages are
/// suppressed — the TUI task status lines are the progress indicator.
/// Errors always print (via `print_above`) so they're visible immediately.
///
/// Also returns true when JSON output mode is active: prose would interleave
/// with the NDJSON event stream and confuse consumers, so we treat JSON
/// mode the same as "TUI on" for prose-suppression purposes.
pub fn tui_is_active() -> bool {
    crate::utils::tui::get_active_renderer().is_some()
        || crate::utils::output_format::is_json_output_active()
}

/// Print an error message to stderr with red color.
/// Suppressed when TUI is active — the error is already captured in task
/// state and shown in the post-task-list error section at shutdown.
pub fn print_error(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        eprintln!("\x1b[91m[ERROR]\x1b[0m {message}");
    }
}

/// Print a success message with green color.
/// Suppressed when TUI is active (task ✓ lines show success).
pub fn print_success(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        println!("\x1b[92m[SUCCESS]\x1b[0m {message}");
    }
}

/// Print an info message with blue color.
/// Suppressed when TUI is active (task status lines show progress).
pub fn print_info(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        println!("\x1b[94m[INFO]\x1b[0m {message}");
    }
}

/// Print a warning message with yellow color.
/// Suppressed when TUI is active.
#[allow(dead_code)]
pub fn print_warning(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        println!("\x1b[93m[WARNING]\x1b[0m {message}");
    }
}

/// Print a warning that an active renderer cannot swallow.
///
/// [`print_warning`] early-returns on [`tui_is_active`], which is also true for
/// `--json`, so on the interactive renderer path and every JSON invocation it
/// prints nothing at all. That is fine for progress chatter the task lines
/// already convey, but not for a notice that a safety check was skipped: those
/// are exactly the paths where the user would otherwise see an unqualified
/// green run.
///
/// Routes through the renderer's `print_above` when one is active so the notice
/// lands above the task list instead of being overwritten by it. With no
/// renderer but JSON output active, goes to stderr - stdout carries the NDJSON
/// stream and prose would corrupt it.
pub fn print_warning_above(message: &str) {
    // stdout carries the NDJSON stream, so the human line goes to stderr under
    // --json (see print_notice_above). Without an event too, a consumer reading
    // only the stream sees nothing at all and renders an unqualified green run -
    // which is the failure this whole notice exists to prevent, just moved.
    if crate::utils::output_format::is_json_output_active() {
        crate::utils::output_format::emit_json_event(
            &serde_json::json!({"event": "warning", "message": message}),
        );
    }
    print_notice_above(&format!("\x1b[93m[WARNING]\x1b[0m {message}"), |line| {
        println!("{line}")
    });
}

/// Emit `formatted` where neither an active renderer nor the NDJSON stream can
/// swallow it.
///
/// The three-way routing had been open-coded three times (here,
/// [`crate::utils::container::print_failure_notice`], and
/// `commands/runtime/build.rs`), each copy re-deriving the same policy. This is
/// the one implementation; [`warning_sink`] stays separate as the pure,
/// testable statement of that policy.
///
/// `plain` is the emit for the neither-active case and is a parameter because
/// severity genuinely differs there: a warning belongs on stdout, an error on
/// stderr. Everything above that case is identical, which is the part worth
/// sharing.
///
/// The renderer `Option` is bound once and matched alongside the sink rather
/// than looked up a second time behind an `expect`. `get_active_renderer`
/// returning `None` on the second call is reachable - `clear_active_renderer`
/// is `pub` and is the first statement of `TaskRenderer::shutdown` - so the
/// old shape could abort the process while printing an informational notice.
/// Pairing the two makes that branch fall through to stderr instead of
/// depending on the interleaving being impossible.
pub fn print_notice_above(formatted: &str, plain: impl FnOnce(&str)) {
    let renderer = crate::utils::tui::get_active_renderer();
    let json_active = crate::utils::output_format::is_json_output_active();
    match (warning_sink(renderer.is_some(), json_active), renderer) {
        (WarningSink::Renderer, Some(active)) => active.print_above(formatted),
        (WarningSink::Renderer, None) | (WarningSink::Stderr, _) => eprintln!("{formatted}"),
        (WarningSink::Stdout, _) => plain(formatted),
    }
}

/// Where a [`print_warning_above`] notice goes.
///
/// Split out from the emit so the routing policy is testable: the property that
/// matters is that there is no fourth, suppressed variant, which is precisely
/// what `print_warning` has and what made the skipped-check notice invisible on
/// the default paths.
#[derive(Debug, PartialEq, Eq)]
pub enum WarningSink {
    Renderer,
    Stderr,
    Stdout,
}

pub fn warning_sink(renderer_active: bool, json_active: bool) -> WarningSink {
    match (renderer_active, json_active) {
        (true, _) => WarningSink::Renderer,
        (false, true) => WarningSink::Stderr,
        (false, false) => WarningSink::Stdout,
    }
}

/// Print a diagnostic about the environment the command runs in, on stderr.
///
/// [`print_warning`] and [`print_info`] are `println!`, which is right for
/// progress commentary that shares stdout with nothing else. It is wrong for a
/// notice emitted before the command even dispatches, because the command that
/// follows may own stdout: `avocado sbom > sbom.json` writes an SPDX document
/// there, and a `[WARNING]` line ahead of it leaves a file no consumer can
/// parse. These notices describe the environment rather than the result, so
/// stderr is where they belong whichever command comes next.
///
/// Prose on stderr on a human-output run; nothing at all under `--output json`.
///
/// Stderr alone does not make a line safe. The avocado-desktop CLI runner merges
/// stdout and stderr in causal order and parses the result, which is why the
/// upgrade banner in `main` is suppressed under `--output json`. Same trade
/// here, and the same cost: under JSON these notices are lost outright, which
/// for the non-fatal failures in `lifecycle` means a caller can read a clean
/// success over a VM whose SSH proxy never came up.
///
/// An earlier version emitted an NDJSON `{"event":"warning"}` record under JSON
/// instead of going quiet, on the theory that a record joins a stream
/// harmlessly. For most callers it did: the pre-dispatch notices fire for every
/// `needs_vm_routing` command that declares `--output`, and `build`, `install`
/// and `provision` all answer in NDJSON, where one more record is well-formed.
/// `vm update` is the case that lost most by the removal — the `lifecycle`
/// auto-start hiccups are the diagnostics it exists to report, and its
/// `download_*` events are a stream too. It is the weakest of those
/// beneficiaries today: the apply path in `commands/vm/update.rs` still has
/// ungated `println!` prose between those events, so the stream a record would
/// have joined is not clean under `--output json` yet regardless.
///
/// `sbom` is the one caller the record was wrong for, and the earlier note here
/// overstated how: the record originates in `ensure_routed_for_process`, which
/// runs before dispatch, so it could not land in the *middle* of the document.
/// (`SbomCommand::execute` does reach this module — `print_summary` calls
/// `print_info` and `print_success` — but only on the `output_path.is_some()`
/// arm, where the document went to a file rather than to stdout.) It would have
/// been **prepended** — one `{"event":"warning"}` from
/// `ensure_routed_for_process` ahead of the single SPDX object — two objects
/// where the contract says one.
/// Latent rather than observed, and Docker-Desktop-only: every route.rs emitter
/// sits behind `RoutingMode::Apply`.
///
/// So this is not "silence is right for every caller". It is one rule that
/// cannot corrupt a one-object contract, bought by dropping a record the stream
/// commands could have carried. Keeping both needs each command to declare
/// stream-versus-object; nothing carries that yet.
///
/// The predicate is
/// [`crate::utils::output_format::json_requested_on_command_line`] and not the
/// `is_json_output_active` flag, because five of the ten callers sit in
/// `ensure_routed_for_process`, which runs before the dispatch match and so
/// before any guard exists.
///
/// Returns the line it emitted, or `None` when it stayed quiet. `eprintln!` is
/// not observable from a test, so the return value is how a test sees which
/// predicate fed the decision.
fn print_stderr_notice(formatted: &str) -> Option<&str> {
    let line = stderr_notice_line(
        formatted,
        crate::utils::output_format::json_requested_on_command_line(),
    )?;
    print_notice_above(line, |l| eprintln!("{l}"));
    Some(line)
}

/// The line [`print_stderr_notice`] emits, or `None` when it stays quiet.
///
/// Split from the emit for the same reason as [`warning_sink`]: the policy is
/// the part worth pinning. Taking `json_requested` as an argument is what lets a
/// test drive the quiet side at all — the real predicate reads `argv`, which an
/// in-process test cannot change.
fn stderr_notice_line(formatted: &str, json_requested: bool) -> Option<&str> {
    (!json_requested).then_some(formatted)
}

/// The warning severity of [`print_stderr_notice`], which documents the routing.
pub fn print_warning_stderr(message: &str) {
    let _ = print_stderr_notice(&format!("\x1b[93m[WARNING]\x1b[0m {message}"));
}

/// The informational counterpart of [`print_warning_stderr`], same reasoning.
pub fn print_info_stderr(message: &str) {
    let _ = print_stderr_notice(&format!("\x1b[94m[INFO]\x1b[0m {message}"));
}

/// Print a message without any color formatting.
/// Suppressed when TUI is active.
#[allow(dead_code)]
pub fn print_plain(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        eprintln!("{message}");
    }
}

/// Print a debug message to stderr with gray color (only in debug builds).
pub fn print_debug(_message: &str, _level: OutputLevel) {
    #[cfg(debug_assertions)]
    if !tui_is_active() {
        eprintln!("\x1b[90m[DEBUG]\x1b[0m {_message}");
    }
}

/// Flush stdout to ensure immediate output
#[allow(dead_code)]
pub fn flush_stdout() {
    let _ = io::stdout().flush();
}

/// Flush stderr to ensure immediate output
#[allow(dead_code)]
pub fn flush_stderr() {
    let _ = io::stderr().flush();
}

/// Check if TUI mode should be used (TTY + no CI + no explicit opt-out
/// + no active JSON output mode).
pub fn should_use_tui() -> bool {
    !crate::utils::output_format::is_json_output_active()
        && io::stderr().is_terminal()
        && std::env::var("AVOCADO_NO_TUI").is_err()
        && std::env::var("CI").is_err()
}

/// Whether the orchestrator should construct a `TaskRenderer` for this
/// run. Distinct from `should_use_tui()` — JSON mode wants a renderer
/// so `register_task` / `set_status` calls fire (which our JSON sink
/// taps into for the desktop app's step list), but doesn't want the
/// renderer to paint a TUI to the terminal. The renderer's own mode
/// (Tui vs Passthrough) plus a JSON-mode check inside its mutators
/// handle the "no painting" half.
pub fn should_create_renderer() -> bool {
    should_use_tui() || crate::utils::output_format::is_json_output_active()
}

/// Wrap a message in dim ANSI formatting.
#[allow(dead_code)]
pub fn format_dimmed(message: &str) -> String {
    format!("\x1b[2m{message}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_print_functions() {
        // These tests mainly ensure the functions compile and don't panic
        print_error("Test error", OutputLevel::Normal);
        print_success("Test success", OutputLevel::Normal);
        print_info("Test info", OutputLevel::Normal);
        print_warning("Test warning", OutputLevel::Normal);
        print_plain("Test plain", OutputLevel::Normal);
        print_debug("Test debug", OutputLevel::Normal);
        flush_stdout();
        flush_stderr();
    }

    #[test]
    fn warning_above_is_never_suppressed() {
        // `tui_is_active()` is true for BOTH an active renderer and `--json`,
        // and those are the two states `print_warning` drops the message in -
        // which is every interactive run and every JSON run. Each of the four
        // combinations has to reach a real sink.
        //
        // Asserting the exact sink per combination, not merely that the result
        // is one of the three variants: a three-variant enum makes that weaker
        // form unfalsifiable, so it stayed green under a `warning_sink` forced
        // to return a single arm.
        assert_eq!(warning_sink(true, true), WarningSink::Renderer);
        assert_eq!(warning_sink(true, false), WarningSink::Renderer);
        assert_eq!(warning_sink(false, true), WarningSink::Stderr);
        assert_eq!(warning_sink(false, false), WarningSink::Stdout);
    }

    #[test]
    fn warning_above_goes_through_the_renderer_when_one_is_active() {
        // A plain println would be painted over by the task list on the next
        // frame, so an active renderer has to take precedence over the JSON
        // check rather than the other way round.
        assert_eq!(warning_sink(true, false), WarningSink::Renderer);
        assert_eq!(warning_sink(true, true), WarningSink::Renderer);
    }

    #[test]
    fn an_environment_notice_goes_quiet_only_under_json() {
        // Both directions matter and they fail differently. Emitting under
        // JSON puts ANSI prose in a parsed stream; suppressing without it
        // loses the routing warning on every ordinary run, which is the whole
        // reason these printers exist.
        assert_eq!(stderr_notice_line("[WARNING] x", true), None);
        assert_eq!(
            stderr_notice_line("[WARNING] x", false),
            Some("[WARNING] x")
        );
    }

    #[test]
    fn an_environment_notice_reads_argv_not_the_process_flag() {
        // Which predicate feeds the policy, pinned rather than asserted in a doc
        // comment. Five of the ten callers run inside
        // `ensure_routed_for_process`, before the dispatch match and so before
        // any `JsonOutputGuard` exists — `is_json_output_active()` is false
        // there on a `--output json` run, which is the bug that put prose in a
        // parsed stream in the first place.
        //
        // Holding a guard is what makes the two predicates disagree: the flag is
        // now true while this test binary's `argv` carries no `--output json`.
        // Only the argv predicate lets the notice through, so swapping the
        // callsite to the flag turns this `Some` into `None`.
        let _json = crate::utils::output_format::JsonOutputGuard::enable();
        assert_eq!(print_stderr_notice("[WARNING] x"), Some("[WARNING] x"));
    }

    #[test]
    fn warning_above_keeps_json_stdout_clean() {
        // stdout carries the NDJSON stream; prose on it would break consumers
        // parsing line by line.
        assert_eq!(warning_sink(false, true), WarningSink::Stderr);
        assert_eq!(warning_sink(false, false), WarningSink::Stdout);
    }
}
