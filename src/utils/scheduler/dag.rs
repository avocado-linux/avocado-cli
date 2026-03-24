//! Simple DAG (directed acyclic graph) for task dependency tracking.

use std::collections::{HashMap, HashSet};

use crate::utils::tui::TaskId;

/// A node in the task graph.
struct TaskNode {
    /// Tasks that must complete before this one can start.
    dependencies: Vec<TaskId>,
}

/// A dependency graph of tasks.  Tasks become "ready" once all their
/// dependencies have completed successfully.
#[derive(Default)]
pub struct TaskGraph {
    tasks: HashMap<TaskId, TaskNode>,
    completed: HashSet<TaskId>,
    failed: HashSet<TaskId>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            completed: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// Register a task with its dependencies.
    pub fn add_task(&mut self, id: TaskId, depends_on: Vec<TaskId>) {
        self.tasks.insert(
            id,
            TaskNode {
                dependencies: depends_on,
            },
        );
    }

    /// Return tasks whose dependencies have all completed successfully
    /// and that haven't been started, completed, or failed yet.
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|(id, node)| {
                // Not already done or failed
                !self.completed.contains(id)
                    && !self.failed.contains(id)
                    // All deps satisfied
                    && node
                        .dependencies
                        .iter()
                        .all(|dep| self.completed.contains(dep))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Mark a task as successfully completed.  May unlock dependents.
    pub fn mark_complete(&mut self, id: &TaskId) {
        self.completed.insert(id.clone());
    }

    /// Mark a task as failed.
    pub fn mark_failed(&mut self, id: &TaskId) {
        self.failed.insert(id.clone());
    }

    /// True while there are tasks that haven't completed or failed.
    #[allow(dead_code)]
    pub fn has_remaining(&self) -> bool {
        self.tasks
            .keys()
            .any(|id| !self.completed.contains(id) && !self.failed.contains(id))
    }

    /// True if any task has failed — dependents should be skipped.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Return task IDs that are blocked because a dependency failed.
    pub fn blocked_by_failure(&self) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|(id, node)| {
                !self.completed.contains(id)
                    && !self.failed.contains(id)
                    && node
                        .dependencies
                        .iter()
                        .any(|dep| self.failed.contains(dep))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_chain() {
        let mut g = TaskGraph::new();
        g.add_task(TaskId::SdkInstall, vec![]);
        g.add_task(TaskId::ExtBuild("a".into()), vec![TaskId::SdkInstall]);
        g.add_task(
            TaskId::RuntimeBuild("r".into()),
            vec![TaskId::ExtBuild("a".into())],
        );

        // Only SDK is ready initially
        assert_eq!(g.ready_tasks(), vec![TaskId::SdkInstall]);

        g.mark_complete(&TaskId::SdkInstall);
        assert_eq!(g.ready_tasks(), vec![TaskId::ExtBuild("a".into())]);

        g.mark_complete(&TaskId::ExtBuild("a".into()));
        assert_eq!(g.ready_tasks(), vec![TaskId::RuntimeBuild("r".into())]);

        g.mark_complete(&TaskId::RuntimeBuild("r".into()));
        assert!(!g.has_remaining());
    }

    #[test]
    fn test_parallel_tasks() {
        let mut g = TaskGraph::new();
        g.add_task(TaskId::ExtBuild("a".into()), vec![]);
        g.add_task(TaskId::ExtBuild("b".into()), vec![]);
        g.add_task(TaskId::ExtBuild("c".into()), vec![]);

        let mut ready = g.ready_tasks();
        ready.sort_by_key(|a| a.to_string());
        assert_eq!(ready.len(), 3);
    }

    #[test]
    fn test_failure_blocks_dependents() {
        let mut g = TaskGraph::new();
        g.add_task(TaskId::ExtBuild("a".into()), vec![]);
        g.add_task(
            TaskId::ExtImage("a".into()),
            vec![TaskId::ExtBuild("a".into())],
        );

        g.mark_failed(&TaskId::ExtBuild("a".into()));
        assert!(g.ready_tasks().is_empty());
        assert!(g.has_failures());
        assert_eq!(g.blocked_by_failure(), vec![TaskId::ExtImage("a".into())]);
    }
}
