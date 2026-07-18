//! LanceDB maintenance scheduler: the background loop that runs periodic
//! compaction + version pruning (see `LanceStore::maintain`), deferred while
//! the daemon is active or under I/O pressure (ADR-003).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::debt::{self, DebtLevel};
use super::pressure::IoPressureProbe;
use super::state::DaemonState;
use super::state::WorkClass;
use hallouminate_adapters::{MaintenanceOptions, MaintenanceStats};
use hallouminate_domain::common::HallouminateError;

/// Grace window for `maintain`'s prune cutoff: versions younger than this
/// are retained, letting in-flight queries drain before their snapshotted
/// version's files can be deleted. Queries don't hold the write lane, so
/// this is the only thing protecting them from a maintenance tick's version
/// prune.
const MAINTENANCE_PRUNE_GRACE_SECS: u64 = 300;

static NEXT_MAINTENANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Whether the maintenance loop should keep ticking after a pass. `Stop`
/// means daemon shutdown was requested or the write lane was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MaintenanceTick {
    Continue,
    Stop,
}

/// Why a due maintenance pass was deferred instead of run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeferReason {
    /// A connection is active, or activity was seen in the last 60s.
    Active,
    /// No recent activity, but host I/O pressure is elevated.
    IoPressure,
}

/// Maintenance-loop pacing (ADR daemon-rework-001). `Full` is the only mode
/// this seed exercises; `Paced` is the shape dispatch B wires once it reads
/// real slice-budget/sleep thresholds -- deliberately left unconstructed
/// here so no config threshold gets invented ahead of that scope.
#[allow(dead_code)]
enum Pace {
    Full,
    Paced { slice_budget: u64, sleep: Duration },
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn jittered_sleep_secs(interval_secs: u64) -> u64 {
    let jitter_max = interval_secs / 10;
    let jitter = if jitter_max == 0 {
        0
    } else {
        fastrand::u64(0..=jitter_max)
    };
    interval_secs.saturating_add(jitter)
}

struct MaintenanceLifecycle {
    maintenance_id: u64,
    started_at: Instant,
    lane_acquired_at: Option<Instant>,
    finished: bool,
}

impl MaintenanceLifecycle {
    fn start() -> Self {
        let maintenance_id = NEXT_MAINTENANCE_ID.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            target: "hallouminate::lance",
            maintenance_event = "started",
            maintenance_id,
            "periodic LanceDB maintenance started",
        );
        Self {
            maintenance_id,
            started_at: Instant::now(),
            lane_acquired_at: None,
            finished: false,
        }
    }

    fn write_lane_acquired(&mut self) {
        let acquired_at = Instant::now();
        self.lane_acquired_at = Some(acquired_at);
        tracing::debug!(
            target: "hallouminate::lance",
            maintenance_event = "write_lane_acquired",
            maintenance_id = self.maintenance_id,
            queue_wait_ms = duration_ms(acquired_at.duration_since(self.started_at)),
            "periodic LanceDB maintenance acquired the write lane",
        );
    }

    fn success(mut self, stats: MaintenanceStats) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::info!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "success",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            fragments_removed = stats.fragments_removed,
            fragments_added = stats.fragments_added,
            old_versions_pruned = stats.old_versions_pruned,
            "periodic LanceDB maintenance completed",
        );
        self.finished = true;
    }

    fn failure(mut self, error: &HallouminateError) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::warn!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "failure",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            error = %error,
            "periodic LanceDB maintenance failed",
        );
        self.finished = true;
    }

    fn shutdown(mut self) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::info!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "shutdown",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            "periodic LanceDB maintenance stopped during shutdown",
        );
        self.finished = true;
    }

    fn durations(&self) -> (u64, u64, u64) {
        let finished_at = Instant::now();
        let total = finished_at.duration_since(self.started_at);
        let queue = match self.lane_acquired_at {
            Some(acquired_at) => acquired_at.duration_since(self.started_at),
            None => total,
        };
        let maintenance = match self.lane_acquired_at {
            Some(acquired_at) => finished_at.duration_since(acquired_at),
            None => Duration::ZERO,
        };
        (
            duration_ms(queue),
            duration_ms(maintenance),
            duration_ms(total),
        )
    }
}

impl Drop for MaintenanceLifecycle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::warn!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "cancelled",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            "periodic LanceDB maintenance cancelled",
        );
    }
}

/// Background task: sleeps `interval` (plus jitter), then runs a maintenance
/// pass once the daemon is idle and I/O pressure is not elevated --
/// deferring and rechecking every 60s otherwise (ADR-003). Exits promptly on
/// `cancel` at every await point. `state` is a clone dedicated to this task.
pub(super) async fn maintenance_loop(
    state: DaemonState,
    cancel: CancellationToken,
    interval: Duration,
    probe: Arc<dyn IoPressureProbe>,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(jittered_sleep_secs(interval.as_secs()))) => {}
        }
        // ADR daemon-rework-001: Hard debt forces the pass past both the
        // Active and IoPressure defer gates below. Inert while `debt::level()`
        // is stubbed to `Ok` -- dispatch B wires the real fragment/version
        // thresholds that can report `Hard`.
        if debt::level() != DebtLevel::Hard {
            state.reset_defer_count();
            while let Some(reason) = state.maintenance_defer_reason(probe.as_ref()) {
                let consecutive_defers = state.increment_defer_count();
                if consecutive_defers > 10
                    && (consecutive_defers == 11 || consecutive_defers.is_multiple_of(10))
                {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        ?reason,
                        consecutive_defers,
                        "maintenance pass repeatedly deferred",
                    );
                } else {
                    tracing::debug!(
                        target: "hallouminate::daemon",
                        ?reason,
                        consecutive_defers,
                        "maintenance pass deferred",
                    );
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
        }
        if state.run_maintenance_tick().await == MaintenanceTick::Stop {
            break;
        }
    }
}

impl DaemonState {
    /// One LanceDB maintenance pass (compaction + version prune). Holds a
    /// connection guard for the write's duration so idle-exit defers instead
    /// of tearing the process down (and releasing the single-instance flock)
    /// under a live LanceDB write, mirroring `catch_up_index` (dispatch.rs)
    /// and the watcher's `process_change_batch`. This pass does NOT stamp
    /// the idle-activity clock (ADR-002) -- reintroducing that stamp would
    /// bring back #222.
    pub(super) async fn run_maintenance_tick(&self) -> MaintenanceTick {
        let store = self.store();
        self.run_maintenance_tick_with(move |maintenance_id| async move {
            store
                .maintain(MaintenanceOptions {
                    maintenance_id,
                    prune_older_than: Duration::from_secs(MAINTENANCE_PRUNE_GRACE_SECS),
                    max_fragments_per_slice: None,
                })
                .await
        })
        .await
    }

    pub(super) async fn run_maintenance_tick_with<F, Fut>(&self, maintain: F) -> MaintenanceTick
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<MaintenanceStats, HallouminateError>>,
    {
        let _conn = self.enter_connection(WorkClass::Internal);
        let shutdown = self.shutdown_token().clone();
        let mut lifecycle = MaintenanceLifecycle::start();
        let permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                lifecycle.shutdown();
                return MaintenanceTick::Stop;
            }
            permit = self.write_lane().acquire_owned() => permit,
        };
        let Ok(_permit) = permit else {
            lifecycle.shutdown();
            return MaintenanceTick::Stop;
        };
        lifecycle.write_lane_acquired();
        let maintenance = maintain(lifecycle.maintenance_id);
        tokio::pin!(maintenance);
        let (result, shutdown_requested) = tokio::select! {
            biased;
            result = &mut maintenance => (result, false),
            _ = shutdown.cancelled() => {
                (maintenance.await, true)
            }
        };
        if shutdown_requested {
            lifecycle.shutdown();
            return MaintenanceTick::Stop;
        }
        match result {
            Ok(stats) => lifecycle.success(stats),
            Err(error) => lifecycle.failure(&error),
        }
        MaintenanceTick::Continue
    }
}
