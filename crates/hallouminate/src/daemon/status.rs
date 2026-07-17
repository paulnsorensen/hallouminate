//! Daemon status reporting. Stub -- a later curd owns this module.

use super::ipc::{DebtLevel, StatusReport, TripState, WatcherCounters};
use super::state::DaemonState;

/// Default/empty status report. Curd 9 wires per-task heartbeat status,
/// real debt classification, watcher counters, and ladder trip state; this
/// stub exists so the `Status` IPC arm compiles before that lands.
pub(super) fn report(_state: &DaemonState) -> StatusReport {
    StatusReport {
        per_task: Vec::new(),
        debt: DebtLevel::Ok,
        defer_count: 0,
        watcher: WatcherCounters::default(),
        trips: TripState::None,
    }
}