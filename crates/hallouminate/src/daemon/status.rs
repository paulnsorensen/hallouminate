//! Daemon status reporting: assembles the wire [`StatusReport`] served by
//! the `Status` IPC arm from the daemon's own storage (`DaemonState`
//! counters, debt classification, ladder-trip snapshot). `daemon status`
//! reports TRUTH from this storage, not liveness.

use super::ipc;
use super::state::DaemonState;
use super::{debt, heartbeat, ladder};

/// Assemble the full [`ipc::StatusReport`] from `state`'s seeded storage:
/// consecutive-defer streak, watcher counters (including `noop_reindexes`),
/// maintenance debt level, and the last ladder trip.
///
/// `per_task` is empty until wiring W1 plumbs a
/// [`heartbeat::HeartbeatRegistry`] handle into `DaemonState` — the
/// registry module exists but nothing stores an instance the report could
/// read yet.
pub(super) fn report(state: &DaemonState) -> ipc::StatusReport {
    let (events, reindexes, noop_reindexes) = state.watcher_counters_snapshot();
    ipc::StatusReport {
        per_task: Vec::new(),
        debt: wire_debt(debt::level()),
        defer_count: state.defer_count(),
        watcher: ipc::WatcherCounters {
            events,
            reindexes,
            noop_reindexes,
        },
        trips: match state.last_ladder_trip() {
            None => ipc::TripState::None,
            Some(trip) => ipc::TripState::Tripped {
                action: wire_action(trip.action),
                at_secs: trip.at_secs,
            },
        },
    }
}

fn wire_debt(level: debt::DebtLevel) -> ipc::DebtLevel {
    match level {
        debt::DebtLevel::Ok => ipc::DebtLevel::Ok,
        debt::DebtLevel::Soft => ipc::DebtLevel::Soft,
        debt::DebtLevel::Hard => ipc::DebtLevel::Hard,
    }
}

fn wire_action(action: ladder::LadderAction) -> ipc::LadderAction {
    match action {
        ladder::LadderAction::ForceMaintenance => ipc::LadderAction::ForceMaintenance,
        ladder::LadderAction::RestartTask(task) => ipc::LadderAction::RestartTask(wire_task(task)),
        ladder::LadderAction::WatchdogTrip => ipc::LadderAction::WatchdogTrip,
    }
}

fn wire_task(task: heartbeat::TaskName) -> ipc::TaskName {
    match task {
        heartbeat::TaskName::Maintenance => ipc::TaskName::Maintenance,
        heartbeat::TaskName::CatchUp => ipc::TaskName::CatchUp,
        heartbeat::TaskName::WatcherPump => ipc::TaskName::WatcherPump,
        heartbeat::TaskName::IdleExit => ipc::TaskName::IdleExit,
        heartbeat::TaskName::Signal => ipc::TaskName::Signal,
    }
}

#[cfg(test)]
mod tests {
    use super::super::ipc;
    use super::super::{heartbeat, ladder};
    use super::*;
    use crate::config::Config;

    async fn test_state() -> DaemonState {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        DaemonState::open(cfg, None).await.expect("open")
    }

    #[tokio::test]
    async fn report_on_fresh_state_is_default_shaped() {
        // WHY: `daemon status` must report TRUTH from DaemonState storage —
        // a fresh daemon has recorded nothing, so every field must be its
        // zero/none shape, not an invented value.
        let state = test_state().await;
        let report = report(&state);
        assert!(
            report.per_task.is_empty(),
            "no heartbeat registry is plumbed into DaemonState yet (W1), \
             so per_task must be empty, got {:?}",
            report.per_task,
        );
        assert_eq!(report.debt, ipc::DebtLevel::Ok);
        assert_eq!(report.defer_count, 0);
        assert_eq!(report.watcher, ipc::WatcherCounters::default());
        assert!(
            matches!(report.trips, ipc::TripState::None),
            "fresh state has no recorded ladder trip, got {:?}",
            report.trips,
        );
    }

    #[tokio::test]
    async fn report_snapshots_defer_watcher_and_trip_from_state_storage() {
        // WHY: the acceptance criterion is that `daemon status` reports the
        // real counters — defer streak, watcher events/reindexes including
        // noop_reindexes, and the last ladder trip — faithfully from the
        // seeded DaemonState storage, not stubbed defaults.
        let state = test_state().await;
        state.increment_defer_count();
        state.increment_defer_count();
        state.record_watcher_events(3);
        state.record_watcher_reindex(false);
        state.record_watcher_reindex(true);
        state.record_ladder_trip(ladder::LadderAction::ForceMaintenance);

        let report = report(&state);
        assert_eq!(report.defer_count, 2);
        assert_eq!(
            report.watcher,
            ipc::WatcherCounters {
                events: 3,
                reindexes: 2,
                noop_reindexes: 1,
            },
        );
        match report.trips {
            ipc::TripState::Tripped { action, .. } => {
                assert_eq!(action, ipc::LadderAction::ForceMaintenance);
            }
            ipc::TripState::None => panic!("recorded trip must be reported, got None"),
        }
    }

    #[tokio::test]
    async fn report_maps_every_ladder_action_variant_onto_the_wire() {
        // WHY: the wire enums in ipc.rs mirror the daemon-internal ones by
        // hand; a missed or crossed arm in the mapping would silently report
        // the wrong escalation action. Drive every variant through
        // record_ladder_trip → report.
        let state = test_state().await;
        let cases = [
            (
                ladder::LadderAction::ForceMaintenance,
                ipc::LadderAction::ForceMaintenance,
            ),
            (
                ladder::LadderAction::WatchdogTrip,
                ipc::LadderAction::WatchdogTrip,
            ),
            (
                ladder::LadderAction::RestartTask(heartbeat::TaskName::Maintenance),
                ipc::LadderAction::RestartTask(ipc::TaskName::Maintenance),
            ),
            (
                ladder::LadderAction::RestartTask(heartbeat::TaskName::CatchUp),
                ipc::LadderAction::RestartTask(ipc::TaskName::CatchUp),
            ),
            (
                ladder::LadderAction::RestartTask(heartbeat::TaskName::WatcherPump),
                ipc::LadderAction::RestartTask(ipc::TaskName::WatcherPump),
            ),
            (
                ladder::LadderAction::RestartTask(heartbeat::TaskName::IdleExit),
                ipc::LadderAction::RestartTask(ipc::TaskName::IdleExit),
            ),
            (
                ladder::LadderAction::RestartTask(heartbeat::TaskName::Signal),
                ipc::LadderAction::RestartTask(ipc::TaskName::Signal),
            ),
        ];
        for (internal, wire) in cases {
            state.record_ladder_trip(internal);
            match report(&state).trips {
                ipc::TripState::Tripped { action, .. } => assert_eq!(
                    action, wire,
                    "internal action {internal:?} must map to wire action {wire:?}",
                ),
                ipc::TripState::None => panic!("trip for {internal:?} must be reported"),
            }
        }
    }

    #[tokio::test]
    async fn report_trip_timestamp_comes_from_the_recorded_snapshot() {
        // WHY: `at_secs` is the daemon's monotonic clock at trip time; the
        // report must carry the recorded value through, not re-stamp it.
        let state = test_state().await;
        state.record_ladder_trip(ladder::LadderAction::WatchdogTrip);
        let recorded = state
            .last_ladder_trip()
            .expect("trip just recorded")
            .at_secs;
        match report(&state).trips {
            ipc::TripState::Tripped { at_secs, .. } => assert_eq!(at_secs, recorded),
            ipc::TripState::None => panic!("recorded trip must be reported"),
        }
    }
}