//! Per-task heartbeat epoch registry (ADR daemon-rework seed 4): each
//! long-lived daemon loop bumps its own epoch once per pass; the watchdog
//! (`watchdog.rs`) compares epochs across polls to detect a stalled task.
//! Bumped by `maintenance.rs`, `server.rs`'s loops, and `supervisor.rs`;
//! read by `watchdog.rs`.

use std::sync::atomic::{AtomicU64, Ordering};

/// The five long-lived daemon loops a watchdog can monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskName {
    Maintenance,
    CatchUp,
    WatcherPump,
    IdleExit,
    Signal,
}

const TASK_COUNT: usize = 5;

fn slot(task: TaskName) -> usize {
    match task {
        TaskName::Maintenance => 0,
        TaskName::CatchUp => 1,
        TaskName::WatcherPump => 2,
        TaskName::IdleExit => 3,
        TaskName::Signal => 4,
    }
}

/// A task's health as inferred by comparing heartbeat epochs across two
/// watchdog polls: `Stalled` when the epoch hasn't moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Alive,
    Stalled,
}

/// Per-task heartbeat epoch counters, bumpable by the owning loop and
/// readable by a future watchdog.
#[derive(Debug)]
pub(crate) struct HeartbeatRegistry {
    epochs: [AtomicU64; TASK_COUNT],
}

impl Default for HeartbeatRegistry {
    fn default() -> Self {
        Self {
            epochs: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl HeartbeatRegistry {
    /// Bump `task`'s epoch by one and return the new value.
    pub(crate) fn bump(&self, task: TaskName) -> u64 {
        self.epochs[slot(task)].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Current epoch for `task`.
    pub(crate) fn epoch(&self, task: TaskName) -> u64 {
        self.epochs[slot(task)].load(Ordering::Relaxed)
    }
}

/// Compare a task's epoch across two watchdog polls: `Stalled` when no
/// progress was made, `Alive` otherwise.
pub(crate) fn compare_epochs(previous: u64, current: u64) -> TaskStatus {
    if current == previous {
        TaskStatus::Stalled
    } else {
        TaskStatus::Alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_increments_only_the_targeted_task() {
        let registry = HeartbeatRegistry::default();
        assert_eq!(registry.bump(TaskName::Maintenance), 1);
        assert_eq!(registry.bump(TaskName::Maintenance), 2);
        assert_eq!(registry.epoch(TaskName::Maintenance), 2);
        assert_eq!(
            registry.epoch(TaskName::CatchUp),
            0,
            "bumping Maintenance must not move CatchUp's epoch",
        );
    }

    #[test]
    fn compare_epochs_reports_stalled_when_unchanged() {
        assert_eq!(compare_epochs(3, 3), TaskStatus::Stalled);
        assert_eq!(compare_epochs(3, 4), TaskStatus::Alive);
    }
}