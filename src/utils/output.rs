//! Output utilities for Avocado CLI.

use std::io::{self, Write};

/// Output level for controlling verbosity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutputLevel {
    Normal,
    Verbose,
    Debug,
}

/// Print an error message to stderr with red color
pub fn print_error(message: &str, _level: OutputLevel) {
    eprintln!("\x1b[91m[ERROR]\x1b[0m {message}");
}

/// Print a success message to stdout with green color
pub fn print_success(message: &str, _level: OutputLevel) {
    println!("\x1b[92m[SUCCESS]\x1b[0m {message}");
}

/// Print an info message to stdout with blue color
pub fn print_info(message: &str, _level: OutputLevel) {
    println!("\x1b[94m[INFO]\x1b[0m {message}");
}

/// Print a warning message to stdout with yellow color
#[allow(dead_code)]
pub fn print_warning(message: &str, _level: OutputLevel) {
    println!("\x1b[93m[WARNING]\x1b[0m {message}");
}

/// Print a message without any color formatting
#[allow(dead_code)]
pub fn print_plain(message: &str, _level: OutputLevel) {
    println!("{message}");
}

/// Print a debug message to stderr with gray color (only in debug builds)
pub fn print_debug(_message: &str, _level: OutputLevel) {
    #[cfg(debug_assertions)]
    eprintln!("\x1b[90m[DEBUG]\x1b[0m {_message}");
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
