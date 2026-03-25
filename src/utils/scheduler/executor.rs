//! Async task executor with semaphore-gated parallelism.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Semaphore;

use super::dag::TaskGraph;
use crate::utils::tui::{TaskId, TaskRenderer, TaskStatus};

/// Runs tasks from a `TaskGraph` with bounded parallelism.
pub struct TaskScheduler {
    graph: TaskGraph,
    renderer: Arc<TaskRenderer>,
    max_parallel: usize,
}

impl TaskScheduler {
    pub fn new(graph: TaskGraph, renderer: Arc<TaskRenderer>, max_parallel: usize) -> Self {
        Self {
            graph,
            renderer,
            max_parallel: max_parallel.max(1),
        }
    }

    /// Execute all tasks in the graph, respecting dependencies and the
    /// concurrency limit.
    ///
    /// `spawn_task` is called for each ready task — it should return a future
    /// that performs the actual work (e.g. running `ExtBuildCommand::execute`).
    pub async fn run<F>(&mut self, spawn_task: F) -> Result<()>
    where
        F: Fn(TaskId) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync + 'static,
    {
        let semaphore = Arc::new(Semaphore::new(self.max_parallel));
        let spawn_task = Arc::new(spawn_task);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(TaskId, Result<()>)>();

        // Track which tasks are currently in flight so we don't spawn them twice.
        let mut in_flight: HashSet<TaskId> = HashSet::new();
        let mut interrupted = false;

        loop {
            // Don't spawn new tasks if interrupted — just drain in-flight.
            if !interrupted {
                // Spawn all ready tasks that aren't already in flight.
                let ready = self.graph.ready_tasks();
                for task_id in ready {
                    if in_flight.contains(&task_id) {
                        continue;
                    }
                    in_flight.insert(task_id.clone());

                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    let tx = tx.clone();
                    let renderer = self.renderer.clone();
                    let spawn_task = spawn_task.clone();
                    let id = task_id.clone();

                    renderer.set_status(&id, TaskStatus::Running);

                    tokio::spawn(async move {
                        let result = spawn_task(id.clone()).await;
                        drop(permit); // release semaphore slot
                        let _ = tx.send((id, result));
                        drop(renderer); // prevent ref from keeping renderer alive
                    });
                }
            }

            // If nothing is in flight and nothing is remaining, we're done.
            if in_flight.is_empty() {
                break;
            }

            // Wait for the next task to finish OR Ctrl-C.
            tokio::select! {
                result = rx.recv() => {
                    if let Some((id, result)) = result {
                        in_flight.remove(&id);

                        match result {
                            Ok(()) => {
                                self.renderer.set_status(&id, TaskStatus::Success);
                                self.graph.mark_complete(&id);
                            }
                            Err(e) => {
                                self.renderer.set_error(&id, format!("{e:#}"));
                                self.renderer.set_status(&id, TaskStatus::Failed);
                                self.graph.mark_failed(&id);

                                // Mark tasks blocked by this failure as skipped.
                                for blocked in self.graph.blocked_by_failure() {
                                    self.renderer.set_status(&blocked, TaskStatus::Skipped);
                                }
                            }
                        }
                    } else {
                        // Channel closed unexpectedly — all senders dropped.
                        break;
                    }
                }
                _ = tokio::signal::ctrl_c(), if !interrupted => {
                    interrupted = true;
                    // Mark all pending (not yet started) tasks as skipped.
                    for task in self.graph.all_pending() {
                        self.renderer.set_status(&task, TaskStatus::Skipped);
                    }
                    // In-flight tasks will be killed by their own ctrl_c handlers
                    // in execute_container_command_with_tui. We just wait for them
                    // to report back.
                }
            }
        }

        if interrupted {
            Err(anyhow::anyhow!("Interrupted"))
        } else if self.graph.has_failures() {
            Err(anyhow::anyhow!("One or more tasks failed"))
        } else {
            Ok(())
        }
    }
}
