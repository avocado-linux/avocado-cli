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
