//! TUI renderer — silent by default, only shows status lines per task.
//!
//! Output is captured but never shown during execution. On failure the full
//! captured output is dumped so the user can diagnose what went wrong.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{cursor, execute, terminal, terminal::Clear, terminal::ClearType};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::task_state::{TaskId, TaskState, TaskStatus};

/// Rendering tick interval — 80ms for smooth spinner animation.
const TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Render mode based on environment detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Compact TUI with cursor manipulation (TTY detected).
    Tui,
    /// Simple line-by-line status output (non-TTY / CI).
    Passthrough,
}

/// Manages the display of task progress in the terminal.
///
/// In TUI mode only a single status line per running task is shown (no output).
/// Completed tasks collapse to a one-liner. Failed tasks expand their full
/// captured output at shutdown time.
/// Spinner frames (braille dots).
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct TaskRenderer {
    state: Arc<Mutex<Vec<TaskState>>>,
    notify: Arc<Notify>,
    mode: RenderMode,
    /// Track how many lines the TUI region currently occupies (for clearing).
    rendered_lines: Arc<Mutex<usize>>,
    /// Whether the renderer loop is running.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Spinner frame counter (incremented each render tick).
    spin: Arc<std::sync::atomic::AtomicUsize>,
    /// When the renderer was created (for total elapsed time).
    created_at: std::time::Instant,
}

impl TaskRenderer {
    /// Create a new renderer. Detects TTY and environment to choose mode.
    pub fn new(_verbose: bool) -> Self {
        let mode = if Self::should_use_tui() {
            RenderMode::Tui
        } else {
            RenderMode::Passthrough
        };

        Self {
            state: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
            mode,
            rendered_lines: Arc::new(Mutex::new(0)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            spin: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            created_at: std::time::Instant::now(),
        }
    }

    /// Check if TUI mode should be used.
    fn should_use_tui() -> bool {
        std::io::stderr().is_terminal()
            && std::env::var("AVOCADO_NO_TUI").is_err()
            && std::env::var("CI").is_err()
    }

    /// Get the render mode.
    #[allow(dead_code)]
    pub fn mode(&self) -> RenderMode {
        self.mode
    }

    // ------------------------------------------------------------------
    // State mutation (called by task executors)
    // ------------------------------------------------------------------

    /// Register a new task.
    pub fn register_task(&self, id: TaskId, label: String) {
        let mut state = self.state.lock().unwrap();
        if state.iter().any(|t| t.id == id) {
            return;
        }
        state.push(TaskState::new(id, label));
    }

    /// Append a new line of output to a task. Output is captured silently —
    /// it is only displayed on failure.
    pub fn append_output(&self, id: &TaskId, line: String) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.iter_mut().find(|t| &t.id == id) {
            task.append_line(line);
        }
        // No notify — we don't redraw for output, only for status changes.
    }

    /// Replace the last line of output (progress bar / carriage-return updates).
    /// Captured silently like `append_output`.
    pub fn replace_last_output(&self, id: &TaskId, line: String) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.iter_mut().find(|t| &t.id == id) {
            task.replace_last_line(line);
        }
    }

    /// Update a task's status.
    pub fn set_status(&self, id: &TaskId, status: TaskStatus) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.iter_mut().find(|t| &t.id == id) {
            task.status = status;
            match status {
                TaskStatus::Running => {
                    // Only set started_at on the first Running transition —
                    // subsequent calls (e.g. from multiple container runs
                    // within the same task) must not reset the clock.
                    if task.started_at.is_none() {
                        task.started_at = Some(std::time::Instant::now());
                    }
                }
                TaskStatus::Success | TaskStatus::Failed | TaskStatus::Skipped => {
                    task.finished_at = Some(std::time::Instant::now());
                }
                _ => {}
            }
        }
        drop(state);

        if self.mode == RenderMode::Passthrough {
            self.passthrough_status(id, status);
        }

        self.notify.notify_one();
    }

    /// Print a message above the TUI region (scrolls normally).
    pub fn print_above(&self, message: &str) {
        if self.mode == RenderMode::Tui {
            let mut stderr = std::io::stderr();
            let rendered = *self.rendered_lines.lock().unwrap();
            if rendered > 0 {
                for _ in 0..rendered {
                    let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
                }
            }
            eprintln!("{message}");
            *self.rendered_lines.lock().unwrap() = 0;
        } else {
            eprintln!("{message}");
        }
    }

    /// Set error message for a task.
    #[allow(dead_code)]
    pub fn set_error(&self, id: &TaskId, message: String) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.iter_mut().find(|t| &t.id == id) {
            task.error_message = Some(message);
        }
    }

    // ------------------------------------------------------------------
    // Render loop
    // ------------------------------------------------------------------

    /// Start the rendering loop. Returns a handle to the spawned task.
    /// Performs an immediate first render so the full task checklist is
    /// visible before any work begins.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        self.running
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Render once immediately so all registered tasks are visible.
        if self.mode == RenderMode::Tui {
            self.render_tui();
        }

        let renderer = Arc::clone(self);
        tokio::spawn(async move {
            renderer.render_loop().await;
        })
    }

    /// Stop the rendering loop and print a final summary.
    ///
    /// Completed tasks show a single success line. Failed tasks dump their
    /// full captured output in red.
    pub fn shutdown(&self) {
        // Unregister from global so print_info etc. stop routing here.
        super::clear_active_renderer();

        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.notify.notify_one();

        let state = self.state.lock().unwrap();

        // Clear the live TUI region
        if self.mode == RenderMode::Tui {
            let mut stderr = std::io::stderr();
            let rendered = *self.rendered_lines.lock().unwrap();
            for _ in 0..rendered {
                let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
            }
            *self.rendered_lines.lock().unwrap() = 0;
            let _ = stderr.flush();
        }

        // Print final summary
        for task in state.iter() {
            match task.status {
                TaskStatus::Success => {
                    let elapsed = format_duration(task.elapsed());
                    eprintln!(
                        "\x1b[92m  \u{2713}\x1b[0m \x1b[2m{} {}\x1b[0m",
                        task.label, elapsed
                    );
                }
                TaskStatus::Failed => {
                    let elapsed = format_duration(task.elapsed());
                    let msg = task.error_message.as_deref().unwrap_or("failed");
                    eprintln!(
                        "\x1b[91m  \u{2717}\x1b[0m {} {} \x1b[91m({})\x1b[0m",
                        task.label, elapsed, msg
                    );
                    // Dump full captured output
                    for line in &task.full_output {
                        eprintln!("    \x1b[2m{}\x1b[0m", strip_ansi(line));
                    }
                }
                TaskStatus::Skipped => {
                    eprintln!("\x1b[2m  - {} (skipped)\x1b[0m", task.label);
                }
                TaskStatus::Pending => {
                    // Still pending at shutdown = never ran (e.g. stamps satisfied)
                    eprintln!("\x1b[2m  \u{2713} {} (up to date)\x1b[0m", task.label);
                }
                _ => {}
            }
        }

        // Total elapsed time
        let total = format_duration(Some(self.created_at.elapsed()));
        eprintln!("\x1b[2m  Total: {total}\x1b[0m");
    }

    /// The main rendering loop, run as a tokio task.
    async fn render_loop(&self) {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Wait for either a fixed tick or a status-change notification.
            tokio::select! {
                _ = ticker.tick() => {}
                _ = self.notify.notified() => {}
            }

            if !self.running.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }

            if self.mode == RenderMode::Tui {
                self.render_tui();
            }
        }
    }

    /// Render the TUI region — status lines with animated spinner.
    fn render_tui(&self) {
        let state = self.state.lock().unwrap();
        let mut stderr = std::io::stderr();
        let prev_rendered = *self.rendered_lines.lock().unwrap();

        let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);

        // Advance spinner
        let frame = self.spin.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let spinner = SPINNER[frame % SPINNER.len()];

        // Clear previous render
        if prev_rendered > 0 {
            for _ in 0..prev_rendered {
                let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
            }
        }

        let mut lines_written = 0;
        let mut showed_peek = false;

        // Show the peek for the longest-running task.  When it completes the
        // peek naturally jumps to the next longest, giving a stable display.
        let peek_task_id = state
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .max_by_key(|t| t.elapsed().unwrap_or_default())
            .map(|t| t.id.clone())
            .unwrap_or(TaskId::SdkInstall);

        // Show every task — pending ones are visible from the start as a
        // checklist so the user can see the full scope of work.
        for task in state.iter() {
            match task.status {
                TaskStatus::Success => {
                    let elapsed = format_duration(task.elapsed());
                    let _ = writeln!(
                        stderr,
                        "\x1b[92m  \u{2713}\x1b[0m \x1b[2m{} {}\x1b[0m",
                        task.label, elapsed
                    );
                }
                TaskStatus::Failed => {
                    let elapsed = format_duration(task.elapsed());
                    let msg = task.error_message.as_deref().unwrap_or("failed");
                    let _ = writeln!(
                        stderr,
                        "\x1b[91m  \u{2717}\x1b[0m {} {} \x1b[91m({})\x1b[0m",
                        task.label, elapsed, msg
                    );
                }
                TaskStatus::Running => {
                    let elapsed = format_duration(task.elapsed());
                    let is_peek_task = !showed_peek && task.id == peek_task_id;

                    // Try to show a progress counter on the status line
                    let progress = task
                        .output_ring
                        .back()
                        .and_then(|l| extract_progress(&strip_ansi(l)));
                    if let Some(ref p) = progress {
                        let _ = writeln!(
                            stderr,
                            "\x1b[94m  {spinner}\x1b[0m {} {} \x1b[2m{p}\x1b[0m",
                            task.label, elapsed
                        );
                    } else {
                        let _ = writeln!(
                            stderr,
                            "\x1b[94m  {spinner}\x1b[0m {} {}",
                            task.label, elapsed
                        );
                    }
                    lines_written += 1;

                    // Show the best non-noise line as a peek
                    if is_peek_task {
                        if let Some(peek) = best_peek_line(&task.output_ring) {
                            let trimmed = peek.trim().to_string();
                            if !trimmed.is_empty() {
                                let max_w = term_width.saturating_sub(6);
                                let display = truncate_with_ellipsis(&trimmed, max_w);
                                let _ = writeln!(stderr, "\x1b[2m    {display}\x1b[0m");
                                lines_written += 1;
                            }
                        }
                        showed_peek = true;
                    }
                    continue; // already counted
                }
                TaskStatus::Skipped => {
                    let _ = writeln!(stderr, "\x1b[2m  - {} (skipped)\x1b[0m", task.label);
                }
                TaskStatus::Pending => {
                    let _ = writeln!(stderr, "\x1b[2m  - {}\x1b[0m", task.label);
                }
                TaskStatus::WaitingForInput => {
                    let _ = writeln!(
                        stderr,
                        "\x1b[93m  ? {} (waiting for input)\x1b[0m",
                        task.label
                    );
                }
            }
            lines_written += 1;
        }

        let _ = stderr.flush();
        *self.rendered_lines.lock().unwrap() = lines_written;
    }

    /// Print a status change in passthrough mode (non-TTY / CI).
    fn passthrough_status(&self, id: &TaskId, status: TaskStatus) {
        match status {
            TaskStatus::Running => {
                eprintln!("\x1b[94m[INFO]\x1b[0m Starting {id}");
            }
            TaskStatus::Success => {
                let state = self.state.lock().unwrap();
                let elapsed = state
                    .iter()
                    .find(|t| &t.id == id)
                    .and_then(|t| t.elapsed())
                    .map(|d| format_duration(Some(d)))
                    .unwrap_or_default();
                eprintln!("\x1b[92m[SUCCESS]\x1b[0m {id} {elapsed}");
            }
            TaskStatus::Failed => {
                eprintln!("\x1b[91m[ERROR]\x1b[0m {id} failed");
                // Dump full output in passthrough mode too
                let state = self.state.lock().unwrap();
                if let Some(task) = state.iter().find(|t| &t.id == id) {
                    for line in &task.full_output {
                        eprintln!("    {}", strip_ansi(line));
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Query helpers (for scheduler / Phase 2)
    // ------------------------------------------------------------------

    /// Return the ID of the first currently-running task (if any).
    /// Used to attribute orphan container output to a task.
    pub fn first_running_task(&self) -> Option<TaskId> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.status == TaskStatus::Running)
            .map(|t| t.id.clone())
    }

    #[allow(dead_code)]
    pub fn get_task_status(&self, id: &TaskId) -> Option<TaskStatus> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.status)
    }

    #[allow(dead_code)]
    pub fn get_last_output_line(&self, id: &TaskId) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|t| &t.id == id)
            .and_then(|t| t.output_ring.back().cloned())
    }

    #[allow(dead_code)]
    pub fn get_full_output(&self, id: &TaskId) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.full_output.clone())
            .unwrap_or_default()
    }
}

impl Drop for TaskRenderer {
    fn drop(&mut self) {
        if self.mode == RenderMode::Tui {
            let mut stderr = std::io::stderr();
            let rendered = *self.rendered_lines.lock().unwrap();
            for _ in 0..rendered {
                let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
            }
            let _ = execute!(stderr, cursor::Show);
            let _ = stderr.flush();
        }
    }
}

/// Format a duration as "(3s)" or "(1m 23s)".  Shows at least "<1s" for
/// sub-second durations so completed tasks never show "(0s)".
fn format_duration(duration: Option<Duration>) -> String {
    match duration {
        Some(d) => {
            let secs = d.as_secs();
            if secs >= 60 {
                format!("({}m {}s)", secs / 60, secs % 60)
            } else if secs == 0 {
                "(<1s)".to_string()
            } else {
                format!("({}s)", secs)
            }
        }
        None => String::new(),
    }
}

/// Strip ANSI escape sequences from a string.
/// Lines matching these prefixes are noise from container scriptlets — skip them.
const NOISE_PREFIXES: &[&str] = &[
    "%post(",
    "%prein(",
    "%preun(",
    "%posttrans(",
    "%triggerin(",
    "update-alternatives:",
    "+ set -e",
    "+ '[' ",
    "+ test ",
    "+ echo ",
    "+ true",
    "+ false",
    "NOTE: ",
    "Running groupadd",
    "Running useradd",
    "Running groupmems",
    "+ perform_groupadd",
    "+ perform_useradd",
    "+ local ",
    "++ echo ",
    "++ grep ",
    "++ cut ",
    "++ sed ",
    "++ awk ",
    "++ tr ",
    "+ bbnote ",
    "+ break",
    "+ opts=",
    "+ remaining=",
    "+ OPT=",
    "+ SYSROOT=",
    "+ GROUPADD_PARAM=",
    "+ USERADD_PARAM=",
    "+ GROUPMEMS_PARAM=",
    "waitpid(",
];

/// Check if a line is just noise that should be skipped in the peek.
fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// Try to extract a progress counter like "(45/120)" from a dnf-style line.
/// Returns `Some("45/120")` if found.
fn extract_progress(line: &str) -> Option<String> {
    // Match patterns like "  Installing : foo  45/120" or "(45/120)" or "[45/120]"
    // DNF format: "  Installing       : pkg-name   N/M"
    let trimmed = line.trim();

    // Look for N/M pattern at the end of the line (dnf progress)
    if let Some(pos) = trimmed.rfind(|c: char| c.is_ascii_whitespace()) {
        let tail = trimmed[pos..].trim();
        if let Some((a, b)) = tail.split_once('/') {
            if a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok() {
                return Some(tail.to_string());
            }
        }
    }
    None
}

/// Find the best line to show as the peek from the ring buffer.
/// Skips noise lines and returns the last meaningful one.
fn best_peek_line(ring: &std::collections::VecDeque<String>) -> Option<String> {
    // Walk backwards to find the last non-noise line
    for line in ring.iter().rev() {
        let clean = strip_ansi(line);
        if !is_noise(&clean) {
            return Some(clean);
        }
    }
    // Fall back to the very last line if everything is noise
    ring.back().map(|l| strip_ansi(l))
}

/// Truncate a string to fit within `max_width` visible characters, adding
/// "..." if truncated.
fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if max_width < 4 {
        return s.chars().take(max_width).collect();
    }
    if s.len() <= max_width {
        return s.to_string();
    }
    // Count visible chars (rough — doesn't handle wide chars)
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_width {
        return s.to_string();
    }
    let mut result: String = chars[..max_width - 3].iter().collect();
    result.push_str("...");
    result
}

pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(None), "");
        assert_eq!(format_duration(Some(Duration::from_millis(500))), "(<1s)");
        assert_eq!(format_duration(Some(Duration::from_secs(3))), "(3s)");
        assert_eq!(format_duration(Some(Duration::from_secs(83))), "(1m 23s)");
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[91mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("no ansi here"), "no ansi here");
        assert_eq!(strip_ansi("\x1b[2m\x1b[94mblue\x1b[0m"), "blue");
    }

    #[test]
    fn test_is_noise() {
        assert!(is_noise(""));
        assert!(is_noise("   "));
        assert!(is_noise(
            "%post(kmod-31-r0.0.core2_64): waitpid(7857) rc 7857 status 0"
        ));
        assert!(is_noise("+ set -e"));
        assert!(is_noise("update-alternatives: Linking /opt/foo to /bar"));
        assert!(is_noise("  + '[' x = x ']'"));
        assert!(is_noise("NOTE: systemd: group render already exists"));
        assert!(is_noise("Running groupadd commands..."));
        assert!(!is_noise("Installing : kmod-31-r0.0.core2_64  45/240"));
        assert!(!is_noise("Complete!"));
        assert!(!is_noise("Dependencies resolved."));
    }

    #[test]
    fn test_extract_progress() {
        assert_eq!(
            extract_progress("  Installing : kmod-31-r0.0.core2_64                     45/240"),
            Some("45/240".to_string())
        );
        assert_eq!(
            extract_progress("  Preparing  :                                            1/1"),
            Some("1/1".to_string())
        );
        assert_eq!(extract_progress("Complete!"), None);
        assert_eq!(extract_progress("Dependencies resolved."), None);
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("short", 80), "short");
        assert_eq!(
            truncate_with_ellipsis("this is a very long line that should be truncated", 20),
            "this is a very lo..."
        );
        assert_eq!(truncate_with_ellipsis("exact", 5), "exact");
    }

    #[test]
    fn test_best_peek_line() {
        use std::collections::VecDeque;

        let mut ring = VecDeque::new();
        ring.push_back("+ set -e".to_string());
        ring.push_back("%post(foo): waitpid(123) rc 123 status 0".to_string());
        ring.push_back("  Installing : kmod-31  45/240".to_string());
        ring.push_back("+ '[' x = x ']'".to_string());

        // Should skip the noise and find the Installing line
        let peek = best_peek_line(&ring);
        assert_eq!(peek, Some("  Installing : kmod-31  45/240".to_string()));
    }
}
