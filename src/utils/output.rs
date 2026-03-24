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
fn tui_is_active() -> bool {
    crate::utils::tui::get_active_renderer().is_some()
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
        eprintln!("\x1b[92m[SUCCESS]\x1b[0m {message}");
    }
}

/// Print an info message with blue color.
/// Suppressed when TUI is active (task status lines show progress).
pub fn print_info(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        eprintln!("\x1b[94m[INFO]\x1b[0m {message}");
    }
}

/// Print a warning message with yellow color.
/// Suppressed when TUI is active.
#[allow(dead_code)]
pub fn print_warning(message: &str, _level: OutputLevel) {
    if !tui_is_active() {
        eprintln!("\x1b[93m[WARNING]\x1b[0m {message}");
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

/// Check if TUI mode should be used (TTY + no CI + no explicit opt-out).
pub fn should_use_tui() -> bool {
    io::stderr().is_terminal()
        && std::env::var("AVOCADO_NO_TUI").is_err()
        && std::env::var("CI").is_err()
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
}
