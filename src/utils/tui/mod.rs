//! TUI module for silent task output capture and status display.
//!
//! Output is captured but never shown during execution. On failure the full
//! captured output is dumped.
//!
//! A **global active renderer** is registered so that all `print_info` /
//! `print_success` / etc. calls throughout the codebase automatically route
//! through `renderer.print_above()` instead of writing directly to stderr.

pub mod input_detect;
pub mod renderer;
pub mod task_state;

pub use renderer::TaskRenderer;
pub use task_state::{TaskId, TaskStatus};

use crate::utils::container::TuiContext;
use std::sync::{Arc, Mutex, Weak};

// ---------------------------------------------------------------------------
// Global active renderer
// ---------------------------------------------------------------------------

static ACTIVE_RENDERER: Mutex<Option<Weak<TaskRenderer>>> = Mutex::new(None);

/// Register a renderer as the globally active one.  All `print_info` (etc.)
/// calls will route through it until it is unregistered or dropped.
pub fn set_active_renderer(renderer: &Arc<TaskRenderer>) {
    let mut guard = ACTIVE_RENDERER.lock().unwrap();
    *guard = Some(Arc::downgrade(renderer));
}

/// Unregister the global renderer (called by `shutdown()`).
pub fn clear_active_renderer() {
    let mut guard = ACTIVE_RENDERER.lock().unwrap();
    *guard = None;
}

/// If a TUI renderer is active, return a strong reference to it.
pub fn get_active_renderer() -> Option<Arc<TaskRenderer>> {
    let guard = ACTIVE_RENDERER.lock().unwrap();
    guard.as_ref().and_then(Weak::upgrade)
}

// ---------------------------------------------------------------------------
// Standalone TUI helper
// ---------------------------------------------------------------------------

/// Create a self-contained TUI for a single command invocation.
///
/// Returns `(TuiContext, Arc<TaskRenderer>)` if TUI is active, or `None` if
/// TUI is disabled (non-TTY, verbose mode, `AVOCADO_NO_TUI`, etc.).
///
/// The renderer is automatically registered as the global active renderer.
/// Call `renderer.shutdown()` when the command finishes.
pub fn create_standalone_tui(
    task_id: TaskId,
    label: &str,
    verbose: bool,
) -> Option<(TuiContext, Arc<TaskRenderer>)> {
    if verbose || !crate::utils::output::should_use_tui() {
        return None;
    }

    let renderer = Arc::new(TaskRenderer::new(false));
    renderer.register_task(task_id.clone(), label.to_string());
    set_active_renderer(&renderer);
    renderer.start();

    let ctx = TuiContext {
        task_id,
        renderer: Arc::clone(&renderer),
    };

    Some((ctx, renderer))
}
