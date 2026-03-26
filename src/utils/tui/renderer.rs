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
    /// Sticky peek task — once chosen, stays until it completes.
    sticky_peek: Arc<Mutex<Option<TaskId>>>,
    /// Messages queued by print_above() for the render loop to drain.
    above_queue: Arc<Mutex<Vec<String>>>,
    /// Set to true by the render loop when it has fully stopped.
    loop_stopped: Arc<std::sync::atomic::AtomicBool>,
    /// Tasks whose final status has been permanently emitted (won't be redrawn).
    finalized: Arc<Mutex<std::collections::HashSet<TaskId>>>,
    /// Injectable output sink. When set, all output goes here instead of stderr.
    /// Used by tests to capture and verify output.
    #[cfg(test)]
    test_output: Arc<Mutex<Vec<String>>>,
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
            sticky_peek: Arc::new(Mutex::new(None)),
            above_queue: Arc::new(Mutex::new(Vec::new())),
            loop_stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finalized: Arc::new(Mutex::new(std::collections::HashSet::new())),
            #[cfg(test)]
            test_output: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create a renderer in Passthrough mode for testing.
    /// Output is captured via `test_output` instead of stderr.
    #[cfg(test)]
    pub(crate) fn new_test() -> Self {
        Self {
            state: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
            mode: RenderMode::Passthrough,
            rendered_lines: Arc::new(Mutex::new(0)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            spin: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            created_at: std::time::Instant::now(),
            sticky_peek: Arc::new(Mutex::new(None)),
            above_queue: Arc::new(Mutex::new(Vec::new())),
            loop_stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finalized: Arc::new(Mutex::new(std::collections::HashSet::new())),
            test_output: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Check if TUI mode should be used.
    fn should_use_tui() -> bool {
        std::io::stderr().is_terminal()
            && std::env::var("AVOCADO_NO_TUI").is_err()
            && std::env::var("CI").is_err()
    }

    /// Write a line to the output sink. In tests, appends to test_output.
    /// In production, writes to stderr.
    fn emit_line(&self, line: &str) {
        #[cfg(test)]
        {
            self.test_output.lock().unwrap().push(line.to_string());
        }
        #[cfg(not(test))]
        {
            eprintln!("{line}");
        }
    }

    /// Get the captured test output lines.
    #[cfg(test)]
    pub fn get_test_output(&self) -> Vec<String> {
        self.test_output.lock().unwrap().clone()
    }

    /// Check if a task has been finalized (permanently emitted) in the TUI.
    #[cfg(test)]
    pub(crate) fn is_finalized(&self, task_id: &TaskId) -> bool {
        self.finalized.lock().unwrap().contains(task_id)
    }

    /// Get the status and error message of a task (for testing).
    #[cfg(test)]
    pub(crate) fn get_task_state(&self, task_id: &TaskId) -> Option<(TaskStatus, Option<String>)> {
        let state = self.state.lock().unwrap();
        state
            .iter()
            .find(|t| t.id == *task_id)
            .map(|t| (t.status, t.error_message.clone()))
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
    ///
    /// In TUI mode, messages are queued and drained by the render loop on
    /// the next tick so they never race with cursor manipulation.
    pub fn print_above(&self, message: &str) {
        if self.mode == RenderMode::Tui && self.running.load(std::sync::atomic::Ordering::Relaxed) {
            self.above_queue.lock().unwrap().push(message.to_string());
            self.notify.notify_one();
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

        // Wait for the render loop to fully stop so it won't write to
        // stderr after we clear the TUI region and print the summary.
        // Skip the wait if the loop was never started (Passthrough mode
        // or start() was never called).
        if self.mode == RenderMode::Tui {
            while !self.loop_stopped.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        let state = self.state.lock().unwrap();

        // Clear the live TUI region
        if self.mode == RenderMode::Tui {
            let mut stderr = std::io::stderr();
            let rendered = *self.rendered_lines.lock().unwrap();
            for _ in 0..rendered {
                let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
            }
            *self.rendered_lines.lock().unwrap() = 0;

            // Drain any remaining queued messages
            for msg in self.above_queue.lock().unwrap().drain(..) {
                let _ = writeln!(stderr, "{msg}");
            }

            let _ = stderr.flush();
        }

        // Print final summary — only tasks not already finalized by the
        // render loop (which emits permanent lines for completed tasks).
        let finalized = self.finalized.lock().unwrap();
        for task in state.iter() {
            if finalized.contains(&task.id) {
                continue; // Already emitted by render loop
            }
            match task.status {
                TaskStatus::Success => {
                    let elapsed = format_duration(task.elapsed());
                    self.emit_line(&format!(
                        "\x1b[92m  \u{2713}\x1b[0m \x1b[2m{} {}\x1b[0m",
                        task.label, elapsed
                    ));
                }
                TaskStatus::Failed => {
                    let elapsed = format_duration(task.elapsed());
                    if let Some(ref msg) = task.error_message {
                        self.emit_line(&format!(
                            "\x1b[91m  \u{2717}\x1b[0m {} {} \x1b[91m({})\x1b[0m",
                            task.label, elapsed, msg
                        ));
                    } else {
                        self.emit_line(&format!(
                            "\x1b[91m  \u{2717}\x1b[0m {} {}",
                            task.label, elapsed
                        ));
                    }
                }
                TaskStatus::Skipped => {
                    self.emit_line(&format!("\x1b[2m  - {} (skipped)\x1b[0m", task.label));
                }
                TaskStatus::Pending => {
                    self.emit_line(&format!(
                        "\x1b[2m  \u{2713} {} (up to date)\x1b[0m",
                        task.label
                    ));
                }
                _ => {}
            }
        }
        drop(finalized);

        // Total elapsed time
        let total = format_duration(Some(self.created_at.elapsed()));
        self.emit_line(&format!("\x1b[2m  Total: {total}\x1b[0m"));

        // Dump failed task output AFTER the task list with a header per task.
        // Include tasks with captured output OR an error message (a task can
        // fail before running any container, so full_output may be empty).
        let failed_tasks: Vec<_> = state
            .iter()
            .filter(|t| {
                t.status == TaskStatus::Failed
                    && (!t.full_output.is_empty() || t.error_message.is_some())
            })
            .collect();
        if !failed_tasks.is_empty() {
            self.emit_line(""); // blank separator
            for task in &failed_tasks {
                self.emit_line(&format!("\x1b[91m--- {} ---\x1b[0m", task.label));
                if let Some(ref msg) = task.error_message {
                    self.emit_line(&format!("\x1b[91m  {msg}\x1b[0m"));
                }
                for line in &task.full_output {
                    self.emit_line(&format!("  {}", strip_ansi(line)));
                }
            }
        }
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

        // Signal that the render loop has fully stopped — shutdown() waits on this.
        self.loop_stopped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Render the TUI region — status lines with animated spinner.
    ///
    /// Completed/failed tasks are "finalized" — a permanent line is emitted
    /// once and they are excluded from the clearable TUI region.  Only
    /// running/pending tasks are redrawn each tick, keeping scrollback clean.
    fn render_tui(&self) {
        let state = self.state.lock().unwrap();
        let mut finalized = self.finalized.lock().unwrap();
        let mut stderr = std::io::stderr();
        let prev_rendered = *self.rendered_lines.lock().unwrap();

        let (tw, _th) = terminal::size().unwrap_or((80, 24));
        let term_width = tw as usize;

        // Advance spinner
        let frame = self.spin.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let spinner = SPINNER[frame % SPINNER.len()];

        // Clear the mutable TUI region (running/pending tasks only)
        if prev_rendered > 0 {
            for _ in 0..prev_rendered {
                let _ = execute!(stderr, cursor::MoveUp(1), Clear(ClearType::CurrentLine));
            }
        }

        // Drain any messages queued by print_above()
        {
            let mut queue = self.above_queue.lock().unwrap();
            for msg in queue.drain(..) {
                let _ = writeln!(stderr, "{msg}");
            }
        }

        // Emit permanent lines for completed/failed/skipped tasks in
        // registration order.  A task is only finalized once all tasks
        // before it (in registration order) have already been finalized.
        // This keeps the output ordered consistently regardless of which
        // parallel tasks finish first.
        for task in state.iter() {
            if finalized.contains(&task.id) {
                continue;
            }
            match task.status {
                TaskStatus::Success => {
                    let elapsed = format_duration(task.elapsed());
                    let _ = writeln!(
                        stderr,
                        "\x1b[92m  \u{2713}\x1b[0m \x1b[2m{} {}\x1b[0m",
                        task.label, elapsed
                    );
                    finalized.insert(task.id.clone());
                }
                TaskStatus::Failed => {
                    let elapsed = format_duration(task.elapsed());
                    let _ = writeln!(
                        stderr,
                        "\x1b[91m  \u{2717}\x1b[0m {} {}",
                        task.label, elapsed
                    );
                    finalized.insert(task.id.clone());
                }
                TaskStatus::Skipped => {
                    let _ = writeln!(stderr, "\x1b[2m  - {} (skipped)\x1b[0m", task.label);
                    finalized.insert(task.id.clone());
                }
                _ => {
                    // This task is still running/pending — stop finalizing.
                    // Later tasks cannot be emitted until this one completes.
                    break;
                }
            }
        }

        // Sticky peek selection
        let peek_task_id = {
            let mut sticky = self.sticky_peek.lock().unwrap();
            let still_running = sticky.as_ref().is_some_and(|id| {
                state
                    .iter()
                    .any(|t| &t.id == id && t.status == TaskStatus::Running)
            });
            if still_running {
                sticky.clone().unwrap()
            } else {
                let new_pick = state
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .max_by_key(|t| t.elapsed().unwrap_or_default())
                    .map(|t| t.id.clone());
                *sticky = new_pick.clone();
                new_pick.unwrap_or(TaskId::SdkInstall)
            }
        };

        // Draw the mutable region: non-finalized tasks shown in place.
        // Completed tasks that can't be finalized yet (a predecessor is still
        // running) are rendered with their final status indicator so they
        // appear to turn into checkmarks in place rather than disappearing.
        let mut lines_written = 0;
        let mut showed_peek = false;

        for task in state.iter() {
            if finalized.contains(&task.id) {
                continue;
            }
            match task.status {
                TaskStatus::Running => {
                    let elapsed = format_duration(task.elapsed());
                    let is_peek_task = !showed_peek && task.id == peek_task_id;

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
                }
                TaskStatus::Pending => {
                    let _ = writeln!(stderr, "\x1b[2m  - {}\x1b[0m", task.label);
                    lines_written += 1;
                }
                TaskStatus::WaitingForInput => {
                    let _ = writeln!(
                        stderr,
                        "\x1b[93m  ? {} (waiting for input)\x1b[0m",
                        task.label
                    );
                    lines_written += 1;
                }
                TaskStatus::Success => {
                    // Completed but waiting for predecessors to finalize —
                    // show checkmark in the mutable region (redrawn each tick).
                    let elapsed = format_duration(task.elapsed());
                    let _ = writeln!(
                        stderr,
                        "\x1b[92m  \u{2713}\x1b[0m \x1b[2m{} {}\x1b[0m",
                        task.label, elapsed
                    );
                    lines_written += 1;
                }
                TaskStatus::Failed => {
                    let elapsed = format_duration(task.elapsed());
                    let _ = writeln!(
                        stderr,
                        "\x1b[91m  \u{2717}\x1b[0m {} {}",
                        task.label, elapsed
                    );
                    lines_written += 1;
                }
                TaskStatus::Skipped => {
                    let _ = writeln!(stderr, "\x1b[2m  - {} (skipped)\x1b[0m", task.label);
                    lines_written += 1;
                }
            }
        }

        let _ = stderr.flush();
        *self.rendered_lines.lock().unwrap() = lines_written;
    }

    /// Print a status change in passthrough mode (non-TTY / CI).
    fn passthrough_status(&self, id: &TaskId, status: TaskStatus) {
        match status {
            TaskStatus::Running => {
                self.emit_line(&format!("\x1b[94m[INFO]\x1b[0m Starting {id}"));
            }
            TaskStatus::Success => {
                let state = self.state.lock().unwrap();
                let elapsed = state
                    .iter()
                    .find(|t| &t.id == id)
                    .and_then(|t| t.elapsed())
                    .map(|d| format_duration(Some(d)))
                    .unwrap_or_default();
                self.emit_line(&format!("\x1b[92m[SUCCESS]\x1b[0m {id} {elapsed}"));
            }
            TaskStatus::Failed => {
                self.emit_line(&format!("\x1b[91m[ERROR]\x1b[0m {id} failed"));
                // Dump full output in passthrough mode too
                let state = self.state.lock().unwrap();
                if let Some(task) = state.iter().find(|t| &t.id == id) {
                    for line in &task.full_output {
                        self.emit_line(&format!("    {}", strip_ansi(line)));
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

    /// Create a renderer in Passthrough mode for testing.
    /// Output is captured via test_output instead of stderr.
    fn test_renderer() -> Arc<TaskRenderer> {
        Arc::new(TaskRenderer {
            state: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
            mode: RenderMode::Passthrough,
            rendered_lines: Arc::new(Mutex::new(0)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            spin: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            created_at: std::time::Instant::now(),
            sticky_peek: Arc::new(Mutex::new(None)),
            above_queue: Arc::new(Mutex::new(Vec::new())),
            loop_stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finalized: Arc::new(Mutex::new(std::collections::HashSet::new())),
            test_output: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Strip ANSI codes from test output lines for easier assertions.
    fn clean_output(r: &TaskRenderer) -> Vec<String> {
        r.get_test_output().iter().map(|l| strip_ansi(l)).collect()
    }

    #[test]
    fn test_failed_task_captures_output() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        r.set_status(&TaskId::SdkInstall, TaskStatus::Running);
        r.append_output(&TaskId::SdkInstall, "Installing packages...".to_string());
        r.append_output(&TaskId::SdkInstall, "Error: package not found".to_string());
        r.set_error(&TaskId::SdkInstall, "package not found".to_string());
        r.set_status(&TaskId::SdkInstall, TaskStatus::Failed);

        let state = r.state.lock().unwrap();
        let task = state.iter().find(|t| t.id == TaskId::SdkInstall).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.full_output.len(), 2);
        assert_eq!(task.full_output[0], "Installing packages...");
        assert_eq!(task.full_output[1], "Error: package not found");
        assert!(task.error_message.as_deref() == Some("package not found"));
    }

    #[test]
    fn test_register_task_idempotent() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        let state = r.state.lock().unwrap();
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn test_set_status_on_missing_task_is_noop() {
        let r = test_renderer();
        // Should not panic
        r.set_status(&TaskId::TargetDevInstall, TaskStatus::Success);
        let state = r.state.lock().unwrap();
        assert!(state.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shutdown_stops_render_loop() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        let _handle = r.start();

        // The render loop should be running
        assert!(r.running.load(std::sync::atomic::Ordering::Relaxed));

        // Small delay to let the loop start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        r.shutdown();

        // After shutdown, the render loop should have stopped
        assert!(!r.running.load(std::sync::atomic::Ordering::Relaxed));
        // In passthrough mode, loop_stopped won't be set (shutdown
        // doesn't wait for it), but the loop should still exit.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shutdown_output_shows_failed_task_with_output() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        r.register_task(
            TaskId::ExtInstall("foo".to_string()),
            "ext install foo".to_string(),
        );
        r.register_task(
            TaskId::RuntimeInstall("dev".to_string()),
            "runtime install dev".to_string(),
        );
        let _handle = r.start();

        // SDK succeeds
        r.set_status(&TaskId::SdkInstall, TaskStatus::Running);
        r.set_status(&TaskId::SdkInstall, TaskStatus::Success);

        // Ext install fails with captured output
        r.set_status(&TaskId::ExtInstall("foo".to_string()), TaskStatus::Running);
        r.append_output(
            &TaskId::ExtInstall("foo".to_string()),
            "Downloading packages...".to_string(),
        );
        r.append_output(
            &TaskId::ExtInstall("foo".to_string()),
            "Error: package avocado-bsp not found".to_string(),
        );
        r.set_error(
            &TaskId::ExtInstall("foo".to_string()),
            "package not found".to_string(),
        );
        r.set_status(&TaskId::ExtInstall("foo".to_string()), TaskStatus::Failed);

        // Runtime skipped (blocked by ext failure)
        r.set_status(
            &TaskId::RuntimeInstall("dev".to_string()),
            TaskStatus::Skipped,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        r.shutdown();

        let output = clean_output(&r);

        // Verify: success task appears once
        let sdk_lines: Vec<_> = output
            .iter()
            .filter(|l| l.contains("sdk bootstrap"))
            .collect();
        assert_eq!(
            sdk_lines.len(),
            1,
            "sdk bootstrap should appear exactly once in shutdown output, got: {sdk_lines:?}"
        );
        assert!(
            sdk_lines[0].contains("\u{2713}"),
            "sdk bootstrap should have checkmark"
        );

        // Verify: failed task appears with error indicator
        // In passthrough mode, both passthrough_status and shutdown emit lines.
        let ext_lines: Vec<_> = output
            .iter()
            .filter(|l| l.contains("ext install foo"))
            .collect();
        assert!(
            !ext_lines.is_empty(),
            "ext install foo should appear in output. Full: {output:?}"
        );
        let has_failure = ext_lines
            .iter()
            .any(|l| l.contains("\u{2717}") || l.contains("[ERROR]") || l.contains("failed"));
        assert!(has_failure, "should indicate failure. Lines: {ext_lines:?}");

        // Verify: failed task's captured output is dumped AFTER the task list
        // (in a separate section with a header)
        let output_section_start = output
            .iter()
            .position(|l| l.contains("--- ext install foo ---"));
        assert!(
            output_section_start.is_some(),
            "failed task should have error section header. Got: {output:?}"
        );
        let after_header: Vec<_> = output[output_section_start.unwrap()..].to_vec();
        assert!(
            after_header
                .iter()
                .any(|l| l.contains("Error: package avocado-bsp not found")),
            "failed task output should be dumped after header. Got: {after_header:?}"
        );
        // Error output should NOT be dimmed
        let raw_output = r.get_test_output();
        let error_output_lines: Vec<_> = raw_output
            .iter()
            .filter(|l| l.contains("package avocado-bsp not found") && !l.contains("---"))
            .collect();
        assert!(
            !error_output_lines.is_empty(),
            "should have error output lines"
        );
        for line in &error_output_lines {
            assert!(
                !line.contains("\x1b[2m"),
                "error output should NOT be dimmed: {line}"
            );
        }

        // Verify: Total appears BEFORE the error section
        let total_pos = output.iter().position(|l| l.contains("Total:"));
        assert!(total_pos.is_some(), "Total should appear");
        assert!(
            total_pos.unwrap() < output_section_start.unwrap(),
            "Total should appear before error output section"
        );

        // Verify: skipped task appears
        assert!(
            output
                .iter()
                .any(|l| l.contains("runtime install dev") && l.contains("skipped")),
            "skipped task should appear"
        );

        // Verify: no task label is spammed (the original bug)
        for task_label in &["sdk bootstrap", "ext install foo", "runtime install dev"] {
            let count = output.iter().filter(|l| l.contains(task_label)).count();
            assert!(
                count <= 5,
                "task '{task_label}' should not be spammed ({count} times). Full output: {output:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_no_output_after_shutdown() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        let _handle = r.start();

        r.set_status(&TaskId::SdkInstall, TaskStatus::Running);
        r.set_status(&TaskId::SdkInstall, TaskStatus::Success);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        r.shutdown();

        let count_at_shutdown = r.get_test_output().len();

        // Wait a bit — if the render loop is still running, it would add more lines
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let count_after_wait = r.get_test_output().len();
        assert_eq!(
            count_at_shutdown, count_after_wait,
            "no output should be written after shutdown"
        );
    }

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

    #[test]
    fn test_finalized_output_preserves_registration_order() {
        // Tasks A, B, C registered in that order.
        // B completes first, then A, then C.
        // The render_tui finalization should only finalize tasks when all
        // preceding tasks are already finalized (preserving registration order).
        let r = test_renderer();
        let id_a = TaskId::ExtBuild("a".to_string());
        let id_b = TaskId::ExtBuild("b".to_string());
        let id_c = TaskId::ExtBuild("c".to_string());

        r.register_task(id_a.clone(), "ext build a".to_string());
        r.register_task(id_b.clone(), "ext build b".to_string());
        r.register_task(id_c.clone(), "ext build c".to_string());

        // Directly set all tasks to Running
        {
            let mut state = r.state.lock().unwrap();
            for task in state.iter_mut() {
                task.status = TaskStatus::Running;
                task.started_at = Some(std::time::Instant::now());
            }
        }

        // B completes first
        {
            let mut state = r.state.lock().unwrap();
            state.iter_mut().find(|t| t.id == id_b).unwrap().status = TaskStatus::Success;
        }
        r.render_tui();

        // B should NOT be finalized yet because A (before it) is still running
        assert!(
            !r.is_finalized(&id_b),
            "B should NOT be finalized while A is still running"
        );

        // A completes — both A and B should now be finalized
        {
            let mut state = r.state.lock().unwrap();
            state.iter_mut().find(|t| t.id == id_a).unwrap().status = TaskStatus::Success;
        }
        r.render_tui();

        assert!(r.is_finalized(&id_a), "A should be finalized");
        assert!(
            r.is_finalized(&id_b),
            "B should be finalized (all predecessors done)"
        );
        assert!(!r.is_finalized(&id_c), "C should NOT be finalized yet");

        // C completes last — all three should be finalized
        {
            let mut state = r.state.lock().unwrap();
            state.iter_mut().find(|t| t.id == id_c).unwrap().status = TaskStatus::Success;
        }
        r.render_tui();

        assert!(r.is_finalized(&id_a), "A finalized");
        assert!(r.is_finalized(&id_b), "B finalized");
        assert!(r.is_finalized(&id_c), "C finalized");
    }
}
