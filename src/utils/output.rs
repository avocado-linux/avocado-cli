//! Output utilities for Avocado CLI.

use std::io::{self, Write};

/// Print an error message to stderr with red color
pub fn print_error(message: &str) {
    eprintln!("\x1b[31mERROR:\x1b[0m {}", message);
}

/// Print a success message to stdout with green color
pub fn print_success(message: &str) {
    println!("\x1b[32mSUCCESS:\x1b[0m {}", message);
}

/// Print an info message to stdout with blue color
pub fn print_info(message: &str) {
    println!("\x1b[34mINFO:\x1b[0m {}", message);
}

/// Print a warning message to stdout with yellow color
#[allow(dead_code)]
pub fn print_warning(message: &str) {
    println!("\x1b[33mWARNING:\x1b[0m {}", message);
}

/// Print a message without any color formatting
#[allow(dead_code)]
pub fn print_plain(message: &str) {
    println!("{}", message);
}

/// Print a debug message to stderr with gray color (only in debug builds)
#[allow(dead_code)]
pub fn print_debug(message: &str) {
    #[cfg(debug_assertions)]
    eprintln!("\x1b[90mDEBUG:\x1b[0m {}", message);
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
        print_error("Test error");
        print_success("Test success");
        print_info("Test info");
        print_warning("Test warning");
        print_plain("Test plain");
        print_debug("Test debug");
        flush_stdout();
        flush_stderr();
    }
}
