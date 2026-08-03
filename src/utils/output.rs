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
    let formatted = format!("\x1b[93m[WARNING]\x1b[0m {message}");
    match warning_sink(
        crate::utils::tui::get_active_renderer().is_some(),
        crate::utils::output_format::is_json_output_active(),
    ) {
        WarningSink::Renderer => {
            // Unwrap is sound: the sink is Renderer only when one is active, and
            // nothing clears it mid-call.
            crate::utils::tui::get_active_renderer()
                .expect("renderer active")
                .print_above(&formatted)
        }
        WarningSink::Stderr => eprintln!("{formatted}"),
        WarningSink::Stdout => println!("{formatted}"),
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
        for (renderer, json) in [(true, true), (true, false), (false, true), (false, false)] {
            let sink = warning_sink(renderer, json);
            assert!(
                matches!(
                    sink,
                    WarningSink::Renderer | WarningSink::Stderr | WarningSink::Stdout
                ),
                "renderer={renderer} json={json} produced {sink:?}"
            );
        }
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
    fn warning_above_keeps_json_stdout_clean() {
        // stdout carries the NDJSON stream; prose on it would break consumers
        // parsing line by line.
        assert_eq!(warning_sink(false, true), WarningSink::Stderr);
        assert_eq!(warning_sink(false, false), WarningSink::Stdout);
    }
}
