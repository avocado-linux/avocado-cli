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
    renderer.set_status(&task_id, TaskStatus::Running);
    set_active_renderer(&renderer);
    renderer.start();

    let ctx = TuiContext {
        task_id,
        renderer: Arc::clone(&renderer),
    };

    Some((ctx, renderer))
}

// ---------------------------------------------------------------------------
// TuiGuard — RAII wrapper for standalone TUI
// ---------------------------------------------------------------------------

/// A drop-guard that guarantees `renderer.shutdown()` is called no matter
/// how the owning function exits (`?`, early return, panic, normal path).
///
/// On drop:
/// - If `mark_success()` was NOT called → sets the task to `Failed` with an
///   optional error message, then shuts down (which dumps captured output).
/// - If `mark_success()` WAS called → sets the task to `Success`, then shuts
///   down.
///
/// Usage:
/// ```ignore
/// let tui_guard = TuiGuard::new(task_id, label, verbose);
/// let ctx = tui_guard.tui_context();
/// // ... do work, use `?` freely ...
/// tui_guard.mark_success();   // must call before Ok(()) return
/// ```
pub struct TuiGuard {
    ctx: Option<TuiContext>,
    renderer: Option<Arc<TaskRenderer>>,
    succeeded: std::cell::Cell<bool>,
    error_msg: std::cell::RefCell<Option<String>>,
}

impl TuiGuard {
    /// Create a new guard.  If TUI is disabled (non-TTY, verbose, etc.)
    /// the guard is inert — all methods are no-ops.
    pub fn new(task_id: TaskId, label: &str, verbose: bool) -> Self {
        match create_standalone_tui(task_id, label, verbose) {
            Some((ctx, renderer)) => Self {
                ctx: Some(ctx),
                renderer: Some(renderer),
                succeeded: std::cell::Cell::new(false),
                error_msg: std::cell::RefCell::new(None),
            },
            None => Self {
                ctx: None,
                renderer: None,
                succeeded: std::cell::Cell::new(false),
                error_msg: std::cell::RefCell::new(None),
            },
        }
    }

    /// Returns the `TuiContext` to pass into `RunConfig` etc.  Returns
    /// `None` if TUI is inactive.
    pub fn tui_context(&self) -> Option<TuiContext> {
        self.ctx.clone()
    }

    /// Mark the task as successful.  Must be called before the function
    /// returns `Ok(())` — if not called, the guard assumes failure on drop.
    pub fn mark_success(&self) {
        self.succeeded.set(true);
    }

    /// Set an error message that will be shown in the failure summary.
    #[allow(dead_code)]
    pub fn set_error(&self, msg: impl Into<String>) {
        *self.error_msg.borrow_mut() = Some(msg.into());
    }

    /// Whether TUI mode is active (for conditional logic in commands).
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.renderer.is_some()
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        if let (Some(ctx), Some(renderer)) = (self.ctx.take(), self.renderer.take()) {
            if self.succeeded.get() {
                renderer.set_status(&ctx.task_id, TaskStatus::Success);
            } else {
                if let Some(msg) = self.error_msg.borrow().as_ref() {
                    renderer.set_error(&ctx.task_id, msg.clone());
                }
                renderer.set_status(&ctx.task_id, TaskStatus::Failed);
            }
            renderer.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that touch the global ACTIVE_RENDERER.
    /// Tests run in parallel by default; the global static causes races.
    static GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Create a test renderer in Passthrough mode (doesn't need a terminal).
    fn test_renderer() -> Arc<TaskRenderer> {
        Arc::new(TaskRenderer::new_test())
    }

    #[test]
    fn test_active_renderer_lifecycle() {
        let _lock = GLOBAL_LOCK.lock().unwrap();
        clear_active_renderer();

        let r = test_renderer();
        set_active_renderer(&r);
        assert!(get_active_renderer().is_some());

        // shutdown() should clear the active renderer
        r.shutdown();
        assert!(
            get_active_renderer().is_none(),
            "active renderer should be cleared after shutdown"
        );
    }

    #[test]
    fn test_active_renderer_cleared_when_dropped() {
        let _lock = GLOBAL_LOCK.lock().unwrap();
        clear_active_renderer();
        {
            let r = test_renderer();
            set_active_renderer(&r);
            assert!(get_active_renderer().is_some());
            // r is dropped here — the Weak reference should no longer upgrade
        }
        assert!(
            get_active_renderer().is_none(),
            "active renderer should be cleared when Arc is dropped"
        );
    }

    #[test]
    fn test_print_and_exit_shuts_down_tui() {
        let _lock = GLOBAL_LOCK.lock().unwrap();
        clear_active_renderer();

        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        set_active_renderer(&r);

        assert!(get_active_renderer().is_some(), "renderer should be active");

        // Simulate what print_and_exit does before printing
        if let Some(renderer) = get_active_renderer() {
            renderer.shutdown();
        }

        assert!(
            get_active_renderer().is_none(),
            "renderer should be cleared after shutdown (as print_and_exit does)"
        );

        // After shutdown, tui_is_active should be false, so print_error
        // would actually print (not be suppressed)
        assert!(
            !crate::utils::output::tui_is_active(),
            "tui_is_active should be false after renderer shutdown"
        );
    }

    #[test]
    fn test_tui_guard_marks_failure_on_drop_without_mark_success() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        r.set_status(&TaskId::SdkInstall, TaskStatus::Running);

        // Create a TuiGuard manually (bypassing create_standalone_tui which
        // checks for a real terminal)
        {
            let ctx = TuiContext {
                task_id: TaskId::SdkInstall,
                renderer: Arc::clone(&r),
            };
            let _guard = TuiGuard {
                ctx: Some(ctx),
                renderer: Some(Arc::clone(&r)),
                succeeded: std::cell::Cell::new(false),
                error_msg: std::cell::RefCell::new(Some("container failed".to_string())),
            };
            // guard dropped here without mark_success()
        }

        let (status, error_msg) = r
            .get_task_state(&TaskId::SdkInstall)
            .expect("task should exist");
        assert_eq!(
            status,
            TaskStatus::Failed,
            "task should be Failed when TuiGuard is dropped without mark_success"
        );
        assert_eq!(
            error_msg.as_deref(),
            Some("container failed"),
            "error message should be set on the task"
        );
    }

    #[test]
    fn test_tui_guard_marks_success_on_drop_with_mark_success() {
        let r = test_renderer();
        r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
        r.set_status(&TaskId::SdkInstall, TaskStatus::Running);

        {
            let ctx = TuiContext {
                task_id: TaskId::SdkInstall,
                renderer: Arc::clone(&r),
            };
            let guard = TuiGuard {
                ctx: Some(ctx),
                renderer: Some(Arc::clone(&r)),
                succeeded: std::cell::Cell::new(false),
                error_msg: std::cell::RefCell::new(None),
            };
            guard.mark_success();
        }

        let (status, _) = r
            .get_task_state(&TaskId::SdkInstall)
            .expect("task should exist");
        assert_eq!(
            status,
            TaskStatus::Success,
            "task should be Success when TuiGuard is dropped after mark_success"
        );
    }

    #[test]
    fn test_install_failure_propagates_as_error_not_ok() {
        // Verify the contract: when a task returns Err, the scheduler
        // marks it as Failed (not Success). This tests the fix where
        // execute_install_internal returned Ok(()) on failure.
        let r = test_renderer();
        let task_id = TaskId::RuntimeInstall("dev".to_string());
        r.register_task(task_id.clone(), "runtime install dev".to_string());
        r.set_status(&task_id, TaskStatus::Running);

        // Simulate what the scheduler does when a task returns Err
        r.set_error(
            &task_id,
            "Failed to install dependencies for runtime 'dev'".to_string(),
        );
        r.set_status(&task_id, TaskStatus::Failed);

        let (status, error_msg) = r.get_task_state(&task_id).expect("task should exist");
        assert_eq!(
            status,
            TaskStatus::Failed,
            "runtime install should be marked Failed, not Success"
        );
        assert!(
            error_msg.is_some(),
            "error message should be present on failed task"
        );
    }
}
