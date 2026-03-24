//! Heuristic detection of interactive prompts in container output.
//!
//! These functions are used by Phase 2 (parallel scheduler) for detecting
//! when a container process is waiting for user input.

use std::time::Duration;

/// Known prompt patterns that indicate a process is waiting for user input.
#[allow(dead_code)]
const PROMPT_PATTERNS: &[&str] = &[
    "[y/N]",
    "[Y/n]",
    "[y/n]",
    "Is this ok",
    "is this ok",
    "(yes/no)",
    "password:",
    "Password:",
    "Proceed",
    "Continue",
    "[Y/N]",
];

/// How long to wait after last output before considering a process stalled.
#[allow(dead_code)]
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Check if a line of output looks like an interactive prompt.
#[allow(dead_code)]
pub fn looks_like_prompt(last_line: &str) -> bool {
    let trimmed = last_line.trim();
    if trimmed.is_empty() {
        return false;
    }
    PROMPT_PATTERNS
        .iter()
        .any(|pattern| trimmed.contains(pattern))
}

/// Check if enough time has passed since last output to consider the process stalled.
#[allow(dead_code)]
pub fn is_stalled(time_since_last_output: Duration) -> bool {
    time_since_last_output >= STALL_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_prompt() {
        assert!(looks_like_prompt("Is this ok [y/N]: "));
        assert!(looks_like_prompt("Proceed? [Y/n]"));
        assert!(looks_like_prompt("Password: "));
        assert!(looks_like_prompt("is this ok [y/N]: "));
        assert!(!looks_like_prompt("Installing packages..."));
        assert!(!looks_like_prompt(""));
        assert!(!looks_like_prompt("   "));
    }

    #[test]
    fn test_is_stalled() {
        assert!(!is_stalled(Duration::from_secs(1)));
        assert!(!is_stalled(Duration::from_secs(4)));
        assert!(is_stalled(Duration::from_secs(5)));
        assert!(is_stalled(Duration::from_secs(10)));
    }
}
