//! Task state types for TUI rendering.

use std::collections::VecDeque;
use std::time::Instant;

/// Identifies a task in the build/install pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskId {
    SdkInstall,
    ExtInstall(String),
    ExtBuild(String),
    ExtImage(String),
    RuntimeInstall(String),
    RuntimeBuild(String),
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskId::SdkInstall => write!(f, "sdk install"),
            TaskId::ExtInstall(name) => write!(f, "ext install {name}"),
            TaskId::ExtBuild(name) => write!(f, "ext build {name}"),
            TaskId::ExtImage(name) => write!(f, "ext image {name}"),
            TaskId::RuntimeInstall(name) => write!(f, "runtime install {name}"),
            TaskId::RuntimeBuild(name) => write!(f, "runtime build {name}"),
        }
    }
}

/// Current status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TaskStatus {
    Pending,
    Running,
    Success,
    Failed,
    WaitingForInput,
    Skipped,
}

/// The default number of output lines to keep visible in the rolling window.
pub const DEFAULT_RING_SIZE: usize = 5;

/// State of a single task, shared between the executor and renderer.
pub struct TaskState {
    pub id: TaskId,
    /// Human-readable label shown in the TUI (e.g. "ext build avocado-wifi").
    pub label: String,
    pub status: TaskStatus,
    /// Rolling window of recent output lines (newest at back).
    pub output_ring: VecDeque<String>,
    /// Maximum size of the output ring.
    pub ring_capacity: usize,
    /// All output captured (for verbose mode or post-failure dump).
    pub full_output: Vec<String>,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub error_message: Option<String>,
}

impl TaskState {
    pub fn new(id: TaskId, label: String) -> Self {
        Self {
            id,
            label,
            status: TaskStatus::Pending,
            output_ring: VecDeque::with_capacity(DEFAULT_RING_SIZE),
            ring_capacity: DEFAULT_RING_SIZE,
            full_output: Vec::new(),
            started_at: None,
            finished_at: None,
            error_message: None,
        }
    }

    /// Append a new line to the output ring (newline-terminated output).
    pub fn append_line(&mut self, line: String) {
        self.full_output.push(line.clone());
        if self.output_ring.len() >= self.ring_capacity {
            self.output_ring.pop_front();
        }
        self.output_ring.push_back(line);
    }

    /// Replace the last line in the output ring (carriage-return output, e.g. progress bars).
    pub fn replace_last_line(&mut self, line: String) {
        // Also update the full output — replace last entry if it exists
        if let Some(last) = self.full_output.last_mut() {
            *last = line.clone();
        } else {
            self.full_output.push(line.clone());
        }

        if let Some(last) = self.output_ring.back_mut() {
            *last = line;
        } else {
            self.output_ring.push_back(line);
        }
    }

    /// Elapsed duration since the task started, if applicable.
    pub fn elapsed(&self) -> Option<std::time::Duration> {
        self.started_at.map(|start| {
            self.finished_at
                .unwrap_or_else(Instant::now)
                .duration_since(start)
        })
    }
}
