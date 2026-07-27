//! LanceDB maintenance scheduler: the background loop that runs periodic
//! compaction + version pruning (see `LanceStore::maintain`), deferred while
//! the daemon is active or under I/O pressure (ADR-003). Deferral is bounded
//! (ADR daemon-rework-001): a due pass deferred past `daemon.defer_bound_secs`
//! runs anyway -- paced when I/O pressure is elevated.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::backpressure;
use super::debt::{self, DebtLevel};
use super::pressure::IoPressureProbe;
use super::state::DaemonState;
use super::state::WorkClass;
use hallouminate_adapters::{MaintenanceOptions, MaintenanceStats};
use hallouminate_config::DaemonConfig;
use hallouminate_domain::common::HallouminateError;
use hallouminate_domain::indexer::ChunkStore;

/// Grace window for `maintain`'s prune cutoff: versions younger than this
/// are retained, letting in-flight queries drain before their snapshotted
/// version's files can be deleted. Queries don't hold the write lane, so
/// this is the only thing protecting them from a maintenance tick's version
/// prune.
const MAINTENANCE_PRUNE_GRACE_SECS: u64 = 300;

/// How long a deferred pass waits before rechecking the defer gates. The
/// final recheck is shortened so the forced pass lands exactly on the defer
/// bound instead of up to one recheck late.
const DEFER_RECHECK: Duration = Duration::from_secs(60);

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

/// Maintenance-pass pacing (ADR daemon-rework-001). A defer-bound-forced
/// pass under elevated I/O pressure runs `Paced` -- bounded compaction
/// slices with sleeps in between -- because the bound overrides PSI but
/// never licenses full-speed compaction onto a saturated system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    Full,
    Paced {
        /// Max source fragments compacted per slice
        /// (`MaintenanceOptions::max_fragments_per_slice`).
        slice_budget: usize,
        /// Sleep between slices, yielding I/O to the pressured host.
        sleep: Duration,
    },
}

/// Pace for a pass forced by the defer bound: paced under elevated I/O
/// pressure, full speed otherwise. A zero configured budget is clamped to 1
/// -- a zero-fragment slice could never catch up, so pacing would never
/// terminate.
fn forced_pace(pressure_elevated: bool, daemon: &DaemonConfig) -> Pace {
    if pressure_elevated {
        Pace::Paced {
            slice_budget: usize::try_from(daemon.paced_slice_budget)
                .unwrap_or(usize::MAX)
                .max(1),
            sleep: Duration::from_millis(daemon.paced_slice_sleep_ms),
        }
    } else {
        Pace::Full
    }
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

    fn success(mut self, gc_ran: bool, gc_stats: GcStats, stats: MaintenanceStats) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::info!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "success",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            gc_ran,
            roots_collected = gc_stats.roots_collected as u64,
            rows_removed = gc_stats.rows_removed,
            fragments_removed = stats.fragments_removed,
            fragments_added = stats.fragments_added,
            old_versions_pruned = stats.old_versions_pruned,
            "periodic LanceDB maintenance completed",
        );
        self.finished = true;
    }

    fn failure(mut self, gc_ran: bool, gc_stats: GcStats, error: &HallouminateError) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::warn!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "failure",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            gc_ran,
            roots_collected = gc_stats.roots_collected as u64,
            rows_removed = gc_stats.rows_removed,
            error = %error,
            "periodic LanceDB maintenance failed",
        );
        self.finished = true;
    }

    fn shutdown(mut self, gc_ran: bool, gc_stats: GcStats) {
        let (queue_wait_ms, maintenance_ms, total_ms) = self.durations();
        tracing::info!(
            target: "hallouminate::lance",
            maintenance_event = "finished",
            maintenance_id = self.maintenance_id,
            outcome = "shutdown",
            queue_wait_ms,
            maintenance_ms,
            total_ms,
            gc_ran,
            roots_collected = gc_stats.roots_collected as u64,
            rows_removed = gc_stats.rows_removed,
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

/// Sleeps `total`, bumping the Maintenance heartbeat every <= 60s so the
/// self-armed watchdog's stall window can't trip while a single long
/// interval sleep is in flight.
async fn sleep_with_heartbeat(state: &DaemonState, total: Duration) {
    const CHUNK: Duration = Duration::from_secs(60);
    let mut remaining = total;
    while remaining > CHUNK {
        tokio::time::sleep(CHUNK).await;
        state
            .heartbeat()
            .bump(super::heartbeat::TaskName::Maintenance);
        if debt::level() == DebtLevel::Hard {
            return;
        }
        remaining -= CHUNK;
    }
    tokio::time::sleep(remaining).await;
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
            _ = sleep_with_heartbeat(&state, Duration::from_secs(jittered_sleep_secs(interval.as_secs()))) => {}
        }
        let due_since = tokio::time::Instant::now();
        let defer_bound = Duration::from_secs(state.baseline().daemon.defer_bound_secs);
        let mut pace = Pace::Full;
        state.reset_defer_count();
        // ADR daemon-rework-001: Hard debt forces the pass past both the
        // Active and IoPressure defer gates below, re-sampled at tick start,
        // at every defer recheck, and during the interval sleep itself, so a
        // Hard onset is caught within one recheck/chunk rather than waiting
        // out the full defer bound or sleep interval.
        let mut hard_forced = false;
        if debt::level() == DebtLevel::Hard {
            hard_forced = true;
            pace = forced_pace(probe.elevated(), &state.baseline().daemon);
        } else {
            while let Some(reason) = state.maintenance_defer_reason(probe.as_ref()) {
                let deferred_for = due_since.elapsed();
                let hard_onset = debt::level() == DebtLevel::Hard;
                if deferred_for >= defer_bound || hard_onset {
                    // The bound is real, not merely counted (the 2026-07-17
                    // incident deferred 1000 consecutive times with only a
                    // WARN): the due pass now runs despite the standing
                    // defer reason, or immediately on a Hard debt onset.
                    hard_forced = hard_onset;
                    pace = forced_pace(probe.elevated(), &state.baseline().daemon);
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        ?reason,
                        deferred_secs = deferred_for.as_secs(),
                        defer_bound_secs = defer_bound.as_secs(),
                        hard_onset,
                        paced = match pace {
                            Pace::Paced { .. } => true,
                            Pace::Full => false,
                        },
                        "maintenance defer bound reached; running the deferred pass",
                    );
                    break;
                }
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
                let recheck = DEFER_RECHECK.min(defer_bound - deferred_for);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(recheck) => {}
                }
                state
                    .heartbeat()
                    .bump(super::heartbeat::TaskName::Maintenance);
            }
        }
        // A Hard-forced pass stays `Pace::Full` even under elevated PSI --
        // unlike the defer-bound-forced path above, which calls `forced_pace`.
        // Hard already blocks writes, so fast debt recovery outranks pacing.
        let tick = match pace {
            Pace::Full => state.run_maintenance_tick(true).await,
            Pace::Paced { .. } => state.run_maintenance_pass(pace, true).await,
        };
        state
            .heartbeat()
            .bump(super::heartbeat::TaskName::Maintenance);
        if hard_forced {
            // The forced pass just ran off a possibly-stale Hard reading;
            // re-read + classify real debt so a write-idle-but-read-active
            // daemon doesn't keep running full-speed passes on stale debt
            // until the next mutation happens to refresh `OBSERVED`.
            backpressure::refresh_observed(&state).await;
        }
        if tick == MaintenanceTick::Stop {
            break;
        }
    }
}

impl DaemonState {
    /// Awaits `fut`, bumping the Maintenance heartbeat every 60s so a
    /// long-running scan/delete/compaction can't trip the watchdog's stall
    /// window while it's in flight.
    async fn bump_while<Fut: std::future::Future>(&self, fut: Fut) -> Fut::Output {
        tokio::pin!(fut);
        let mut bump_interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                biased;
                result = &mut fut => return result,
                _ = bump_interval.tick() => {
                    self.heartbeat().bump(super::heartbeat::TaskName::Maintenance);
                }
            }
        }
    }

    /// One LanceDB maintenance pass (compaction + version prune). Holds a
    /// connection guard for the write's duration so idle-exit defers instead
    /// of tearing the process down (and releasing the single-instance flock)
    /// under a live LanceDB write, mirroring `catch_up_index` (dispatch.rs)
    /// and the watcher's `process_change_batch`. This pass does NOT stamp
    /// the idle-activity clock (ADR-002) -- reintroducing that stamp would
    /// bring back #222.
    pub(super) async fn run_maintenance_tick(&self, allow_gc: bool) -> MaintenanceTick {
        self.run_maintenance_pass(Pace::Full, allow_gc).await
    }

    /// One maintenance pass at `pace` against the real store -- the
    /// `Pace::Paced` entry for a defer-bound-forced pass under pressure.
    async fn run_maintenance_pass(&self, pace: Pace, allow_gc: bool) -> MaintenanceTick {
        let store = self.store();
        self.run_maintenance_pass_with(
            pace,
            allow_gc,
            move |maintenance_id, max_fragments_per_slice| {
                let store = Arc::clone(&store);
                async move {
                    store
                        .maintain(MaintenanceOptions {
                            maintenance_id,
                            prune_older_than: Duration::from_secs(MAINTENANCE_PRUNE_GRACE_SECS),
                            max_fragments_per_slice,
                        })
                        .await
                }
            },
        )
        .await
    }

    /// Drives `maintain` once (`Pace::Full`, unbounded) or as a sequence of
    /// bounded compaction slices (`Pace::Paced`). Each slice is a complete
    /// `run_maintenance_tick_with` pass, so the write lane is released (and
    /// shutdown observed) between slices. Slicing stops when a slice removes
    /// fewer fragments than its budget (backlog caught up), reports no
    /// removal count (progress unmeasurable), or fails (matching `Full`,
    /// where a failed pass waits for the next interval tick).
    async fn run_maintenance_pass_with<F, Fut>(
        &self,
        pace: Pace,
        allow_gc: bool,
        mut maintain: F,
    ) -> MaintenanceTick
    where
        F: FnMut(u64, Option<usize>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<MaintenanceStats, HallouminateError>>,
    {
        let Pace::Paced {
            slice_budget,
            sleep,
        } = pace
        else {
            return self
                .run_maintenance_tick_with(allow_gc, |id| maintain(id, None))
                .await;
        };
        // GC is a machine-wide scan; running it once per slice would waste
        // work and rerun the scan pointlessly under I/O pressure, since
        // nothing new for it to collect appears mid-pass. Only the first
        // slice runs GC, and only if the caller allows GC at all.
        let mut run_gc = allow_gc;
        loop {
            let slice_removed: Arc<std::sync::Mutex<Option<usize>>> = Arc::default();
            let capture = Arc::clone(&slice_removed);
            let tick = self
                .run_maintenance_tick_with(run_gc, |maintenance_id| {
                    let slice = maintain(maintenance_id, Some(slice_budget));
                    async move {
                        let stats = slice.await?;
                        *capture.lock().expect("slice stats lock") = stats.fragments_removed;
                        Ok(stats)
                    }
                })
                .await;
            run_gc = false;
            if tick == MaintenanceTick::Stop {
                return MaintenanceTick::Stop;
            }
            let removed = slice_removed.lock().expect("slice stats lock").take();
            let Some(removed) = removed else {
                return MaintenanceTick::Continue;
            };
            if removed < slice_budget {
                return MaintenanceTick::Continue;
            }
            tokio::select! {
                biased;
                _ = self.shutdown_token().cancelled() => return MaintenanceTick::Stop,
                _ = tokio::time::sleep(sleep) => {}
            }
        }
    }

    pub(super) async fn run_maintenance_tick_with<F, Fut>(
        &self,
        run_gc: bool,
        maintain: F,
    ) -> MaintenanceTick
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<MaintenanceStats, HallouminateError>>,
    {
        let _conn = self.enter_connection(WorkClass::Internal);
        let shutdown = self.shutdown_token().clone();
        let mut lifecycle = MaintenanceLifecycle::start();
        let store = self.store();

        // GC's scan phase is read-only -- run it before acquiring the write
        // lane so the machine-wide scan doesn't block every other daemon
        // mutation for its duration. `gc_ms` sums the scan and delete
        // phases' own durations, excluding the lane-wait gap between them,
        // so it reports GC's real cost rather than lane contention.
        let scan_started = Instant::now();
        let gc_candidates = if run_gc {
            match self
                .gc_scan(store.as_ref(), lifecycle.maintenance_id, &shutdown)
                .await
            {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::warn!(
                        target: "hallouminate::lance",
                        gc_event = "scan_failed",
                        maintenance_id = lifecycle.maintenance_id,
                        error = %error,
                        "orphaned-root GC scan failed; continuing without collection this tick",
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let scan_ms = scan_started.elapsed();

        let permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                lifecycle.shutdown(run_gc, GcStats::default());
                return MaintenanceTick::Stop;
            }
            permit = self.write_lane().acquire_owned() => permit,
        };
        let Ok(_permit) = permit else {
            lifecycle.shutdown(run_gc, GcStats::default());
            return MaintenanceTick::Stop;
        };
        lifecycle.write_lane_acquired();

        // GC is best-effort storage reclaim, layered on top of the
        // pre-existing compaction job below: a GC failure (e.g. the
        // `supervise_scan` panic recovery path, issue #223) must not skip
        // compaction, or fragment debt accumulates unbounded every tick.
        // Partial progress is always kept, never zeroed, on error.
        let delete_started = Instant::now();
        let (gc_stats, gc_result) = self
            .gc_delete(
                store.as_ref(),
                gc_candidates,
                lifecycle.maintenance_id,
                &shutdown,
            )
            .await;
        let gc_ms = duration_ms(scan_ms + delete_started.elapsed());
        if let Err(error) = &gc_result {
            tracing::warn!(
                target: "hallouminate::lance",
                gc_event = "delete_failed",
                maintenance_id = lifecycle.maintenance_id,
                gc_ms,
                roots_collected = gc_stats.roots_collected as u64,
                rows_removed = gc_stats.rows_removed,
                error = %error,
                "orphaned-root GC delete failed partway; continuing compaction with partial results",
            );
        }
        if run_gc {
            tracing::debug!(
                target: "hallouminate::lance",
                gc_event = "finished",
                maintenance_id = lifecycle.maintenance_id,
                gc_ms,
                roots_collected = gc_stats.roots_collected as u64,
                rows_removed = gc_stats.rows_removed,
                "orphaned-root GC finished",
            );
        }

        self.heartbeat()
            .bump(super::heartbeat::TaskName::Maintenance);
        let maintenance = maintain(lifecycle.maintenance_id);
        tokio::pin!(maintenance);
        let mut bump_interval = tokio::time::interval(Duration::from_secs(60));
        let (result, shutdown_requested) = loop {
            tokio::select! {
                biased;
                result = &mut maintenance => break (result, false),
                _ = shutdown.cancelled() => break (maintenance.await, true),
                _ = bump_interval.tick() => {
                    self.heartbeat().bump(super::heartbeat::TaskName::Maintenance);
                }
            }
        };
        if shutdown_requested {
            lifecycle.shutdown(run_gc, gc_stats);
            return MaintenanceTick::Stop;
        }
        match result {
            Ok(stats) => lifecycle.success(run_gc, gc_stats, stats),
            Err(error) => lifecycle.failure(run_gc, gc_stats, &error),
        }
        MaintenanceTick::Continue
    }

    /// Phase 1 of orphaned-root GC: scan for retired roots. Read-only --
    /// does NOT require the write lane. Production code
    /// (`run_maintenance_tick_with`) runs this BEFORE acquiring the lane so
    /// the machine-wide scan doesn't block every other daemon mutation for
    /// its duration. Takes `store` as the `ChunkStore` port rather than
    /// reading `self.store()` internally, so a test can substitute a fake
    /// store without `DaemonState` itself needing to hold a trait object.
    async fn gc_scan(
        &self,
        store: &dyn ChunkStore,
        maintenance_id: u64,
        shutdown: &CancellationToken,
    ) -> std::result::Result<Vec<hallouminate_domain::common::RetiredRoot>, HallouminateError> {
        tracing::debug!(
            target: "hallouminate::lance",
            gc_event = "scan_started",
            maintenance_id,
            "orphaned-root GC scanning distinct roots",
        );
        // The scan runs before the write lane is acquired, so a shutdown
        // here must not force the daemon to wait out a full machine-wide
        // table scan before it can observe cancellation.
        let scan = store.distinct_roots();
        tokio::pin!(scan);
        let mut bump_interval = tokio::time::interval(Duration::from_secs(60));
        let known = loop {
            tokio::select! {
                biased;
                result = &mut scan => break result?,
                _ = shutdown.cancelled() => return Ok(Vec::new()),
                _ = bump_interval.tick() => {
                    self.heartbeat().bump(super::heartbeat::TaskName::Maintenance);
                }
            }
        };
        // Blocking `stat` calls -- run off the async runtime worker so a
        // hung stat on a stale/degraded mount can't stall it (the spec
        // explicitly puts network-mount timeouts in scope).
        tokio::task::spawn_blocking(move || hallouminate_domain::common::retired_roots(&known))
            .await
            .map_err(|e| HallouminateError::Indexer(format!("gc scan task panicked: {e}")))
    }

    /// Phase 2 of orphaned-root GC: delete the roots `gc_scan` found. Must
    /// run under the write lane. Rechecks each candidate's retirement
    /// immediately before deleting it -- reusing the exact same fail-closed
    /// `retired_roots` logic -- narrowing the TOCTOU window from the whole
    /// scan-plus-loop duration down to a single root (a root recreated
    /// between the batch scan and its delete is skipped, not deleted).
    /// Always returns the stats accumulated before any error: a partial
    /// collection's counts must never be silently zeroed just because a
    /// later root's delete failed. Observes `shutdown` between roots.
    async fn gc_delete(
        &self,
        store: &dyn ChunkStore,
        candidates: Vec<hallouminate_domain::common::RetiredRoot>,
        maintenance_id: u64,
        shutdown: &CancellationToken,
    ) -> (GcStats, std::result::Result<(), HallouminateError>) {
        let mut stats = GcStats::default();
        for candidate in candidates {
            if shutdown.is_cancelled() {
                return (stats, Ok(()));
            }
            let path = candidate.into_path_buf();
            let reconfirmed = match tokio::task::spawn_blocking(move || {
                hallouminate_domain::common::retired_roots(std::slice::from_ref(&path))
                    .into_iter()
                    .next()
            })
            .await
            {
                Ok(Some(root)) => root,
                Ok(None) => continue, // recreated, or now undeterminable this tick -- skip, not delete
                Err(e) => {
                    return (
                        stats,
                        Err(HallouminateError::Indexer(format!(
                            "gc recheck task panicked: {e}"
                        ))),
                    );
                }
            };
            let removed = match self.bump_while(store.delete_root(&reconfirmed)).await {
                Ok(removed) => removed,
                Err(e) => return (stats, Err(e)),
            };
            stats.roots_collected += 1;
            stats.rows_removed += removed;
            // Unconditional per-root bump, redundant with `bump_while`'s own
            // bump (which fires on its interval's first tick regardless of
            // how fast the delete resolves): a doubly-safe guard against any
            // future change to `bump_while`'s polling order, at the cost of
            // one relaxed atomic increment.
            self.heartbeat()
                .bump(super::heartbeat::TaskName::Maintenance);
            tracing::info!(
                target: "hallouminate::lance",
                gc_event = "root_collected",
                maintenance_id,
                root = %reconfirmed.as_path().display(),
                root_rows_removed = removed,
                "orphaned-root GC collected a retired root",
            );
        }
        (stats, Ok(()))
    }

    /// Convenience wrapper combining `gc_scan` + `gc_delete` for direct
    /// callers (tests) that don't need write-lane-hoisting. Production code
    /// (`run_maintenance_tick_with`) calls `gc_scan`/`gc_delete` separately
    /// so the read-only scan can run before the write lane is acquired.
    #[cfg(test)]
    pub(super) async fn run_gc(
        &self,
        maintenance_id: u64,
    ) -> (GcStats, std::result::Result<(), HallouminateError>) {
        let store = self.store();
        match self
            .gc_scan(store.as_ref(), maintenance_id, self.shutdown_token())
            .await
        {
            Ok(candidates) => {
                self.gc_delete(
                    store.as_ref(),
                    candidates,
                    maintenance_id,
                    self.shutdown_token(),
                )
                .await
            }
            Err(error) => (GcStats::default(), Err(error)),
        }
    }
}

/// Per-tick garbage-collection report: roots whose directories no longer
/// exist, collected before compaction and reported alongside
/// `MaintenanceStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct GcStats {
    pub(super) roots_collected: usize,
    pub(super) rows_removed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hallouminate_config::Config;
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    // Capture scaffolding mirrors state.rs tests; the sibling module's
    // test helpers are private and belong to another change, so they can't
    // be shared from here.
    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        strings: HashMap<String, String>,
        numbers: HashMap<String, u64>,
    }

    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<CapturedEvent>>>);

    impl EventCapture {
        fn maintenance_started(&self) -> bool {
            self.maintenance_started_count() > 0
        }

        fn maintenance_started_count(&self) -> usize {
            let mut count = 0;
            for e in self.0.lock().expect("capture lock").iter() {
                if e.strings.get("maintenance_event").map(String::as_str) == Some("started") {
                    count += 1;
                }
            }
            count
        }

        /// The defer-bound warn event, recognized by its `defer_bound_secs`
        /// field -- present only on the forced-pass warn.
        fn forced_event(&self) -> Option<CapturedEvent> {
            for e in self.0.lock().expect("capture lock").iter() {
                if e.numbers.contains_key("defer_bound_secs") {
                    return Some(e.clone());
                }
            }
            None
        }
    }

    impl<S: Subscriber> Layer<S> for EventCapture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut captured = CapturedEvent::default();
            event.record(&mut captured);
            self.0.lock().expect("capture lock").push(captured);
        }
    }

    impl Visit for CapturedEvent {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.numbers.insert(field.name().to_owned(), value);
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            let value = u64::try_from(value).expect("maintenance numeric fields are non-negative");
            self.numbers.insert(field.name().to_owned(), value);
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.strings
                .insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.strings
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    struct TestProbe(AtomicBool);

    impl TestProbe {
        fn new(elevated: bool) -> Self {
            Self(AtomicBool::new(elevated))
        }
    }

    impl IoPressureProbe for TestProbe {
        fn elevated(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    async fn test_state(configure: impl FnOnce(&mut Config)) -> (DaemonState, tempfile::TempDir) {
        let ground = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = ground.path().to_string_lossy().into_owned();
        // Keep the state's own maintenance task out of these tests; each
        // test drives `maintenance_loop` (or a pass) directly.
        cfg.daemon.maintenance_interval_secs = 0;
        configure(&mut cfg);
        let state = DaemonState::open(cfg, None).await.expect("open");
        (state, ground)
    }

    fn stats(fragments_removed: Option<usize>) -> MaintenanceStats {
        MaintenanceStats {
            fragments_removed,
            fragments_added: Some(1),
            old_versions_pruned: Some(0),
        }
    }

    /// The acceptance criterion (G1): WHEN external activity is continuous
    /// for longer than the defer bound AND debt is below Hard, maintenance
    /// runs no later than the bound -- defers are bounded, not counted.
    #[tokio::test(start_paused = true)]
    async fn due_pass_forced_no_later_than_defer_bound_despite_continuous_activity() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent
        // test would skip the defer path this test asserts on.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 150).await;
        // Continuous external activity: a connection held for the whole test.
        let _active = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        // Past interval + max jitter: the pass becomes due and defers.
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(51)).await;
        tokio::task::yield_now().await;
        // Two full 60s rechecks (120s deferred), then one second short of
        // the 150s bound: still deferred.
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert!(
            !capture.maintenance_started(),
            "maintenance must stay deferred until the defer bound"
        );
        // At the bound the shortened final recheck fires and the pass runs
        // despite the still-active connection.
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            capture.maintenance_started(),
            "a due pass deferred for defer_bound_secs must be forced to run"
        );
        let forced = capture.forced_event().expect("forced-pass warn event");
        assert_eq!(forced.numbers.get("defer_bound_secs"), Some(&150));
        assert_eq!(
            forced.numbers.get("deferred_secs"),
            Some(&150),
            "the forced pass must run at the bound, not later"
        );
        assert_eq!(
            forced.strings.get("paced").map(String::as_str),
            Some("false")
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    /// ADR daemon-rework-001: the bound overrides PSI, but a forced pass on
    /// a pressured host runs paced rather than full speed (or skipped).
    #[tokio::test(start_paused = true)]
    async fn forced_pass_under_elevated_pressure_runs_paced() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent
        // test would skip the defer path this test asserts on.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 150).await;
        let _active = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(true)),
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(51)).await;
        tokio::task::yield_now().await;
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            capture.maintenance_started(),
            "elevated pressure must pace the forced pass, not skip it"
        );
        let forced = capture.forced_event().expect("forced-pass warn event");
        assert_eq!(
            forced.strings.get("paced").map(String::as_str),
            Some("true")
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    /// `defer_bound_secs = 0` means "never wait": the due pass runs
    /// immediately even under continuous activity.
    #[tokio::test(start_paused = true)]
    async fn zero_defer_bound_forces_the_due_pass_immediately() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent
        // test would skip the defer path this test asserts on.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 0).await;
        let _active = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(51)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            capture.maintenance_started(),
            "a zero defer bound must force the due pass on its first recheck"
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    /// ADR daemon-rework-001: a Hard-forced pass must refresh the debt
    /// observation afterward, not just run once and leave the daemon
    /// spinning at full-speed passes off a stale Hard reading. `debt::OBSERVED`
    /// is a process-wide static shared across parallel tests; this test uses
    /// the same acceptance already documented on
    /// `recorded_observation_reaches_the_maintenance_loops_level_read` --
    /// it asserts on this task's own effects (started count, post-refresh
    /// level) rather than on OBSERVED staying untouched by anything else.
    #[tokio::test(start_paused = true)]
    async fn hard_forced_pass_refreshes_the_debt_observation_afterward() {
        // Exclusive OBSERVED slot: this test records Hard into the shared
        // cache (see debt::OBSERVED_HARD_COORD).
        let _coord = debt::OBSERVED_HARD_COORD.write().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 999_999).await;
        debt::OBSERVED.record(DebtLevel::Hard);
        let _active = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        // Past interval + max jitter: the Hard reading forces the pass
        // straight past the still-active connection, skipping the defer
        // loop entirely.
        tokio::time::advance(Duration::from_secs(111)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            capture.maintenance_started_count(),
            1,
            "Hard debt must force the pass despite the active connection"
        );
        // The post-pass `refresh_observed` call awaits real store I/O on
        // the blocking pool; a bounded parking poll (not a yield loop) is
        // required to let that real wall-clock work complete under a
        // paused-time test runtime.
        for _ in 0..500 {
            if debt::level() != DebtLevel::Hard {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            debt::level(),
            DebtLevel::Ok,
            "the post-pass refresh must reclassify against the fresh (empty) \
             store, not leave the stale Hard reading in place"
        );

        // A second interval: debt is no longer Hard, so the still-active
        // connection defers this pass instead of forcing it (the huge
        // defer_bound_secs never fires).
        tokio::time::advance(Duration::from_secs(111)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            capture.maintenance_started_count(),
            1,
            "a non-Hard reading must defer the second pass on the active connection"
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    #[test]
    fn forced_pace_is_full_when_pressure_not_elevated() {
        assert_eq!(forced_pace(false, &DaemonConfig::default()), Pace::Full);
    }

    #[test]
    fn forced_pace_reads_paced_config_when_pressure_elevated() {
        let daemon = DaemonConfig {
            paced_slice_budget: 4,
            paced_slice_sleep_ms: 250,
            ..DaemonConfig::default()
        };
        assert_eq!(
            forced_pace(true, &daemon),
            Pace::Paced {
                slice_budget: 4,
                sleep: Duration::from_millis(250),
            }
        );
    }

    #[test]
    fn forced_pace_clamps_zero_slice_budget_to_one() {
        let daemon = DaemonConfig {
            paced_slice_budget: 0,
            ..DaemonConfig::default()
        };
        assert_eq!(
            forced_pace(true, &daemon),
            Pace::Paced {
                slice_budget: 1,
                sleep: Duration::from_millis(daemon.paced_slice_sleep_ms),
            }
        );
    }

    #[tokio::test]
    async fn full_pass_runs_one_unbounded_slice() {
        let (state, _ground) = test_state(|_| {}).await;
        let calls: Arc<Mutex<Vec<Option<usize>>>> = Arc::default();
        let record = Arc::clone(&calls);
        let tick = state
            .run_maintenance_pass_with(Pace::Full, true, move |_, max_fragments| {
                record.lock().expect("calls lock").push(max_fragments);
                async move { Ok(stats(Some(1000))) }
            })
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);
        // One call, unbounded -- a huge removal count must not trigger
        // slicing in Full mode.
        assert_eq!(*calls.lock().expect("calls lock"), vec![None]);
    }

    #[tokio::test(start_paused = true)]
    async fn paced_pass_slices_until_a_slice_underfills_its_budget() {
        let (state, _ground) = test_state(|_| {}).await;
        let calls: Arc<Mutex<Vec<Option<usize>>>> = Arc::default();
        let script = Arc::new(Mutex::new(vec![Some(8usize), Some(8), Some(3)]));
        let record = Arc::clone(&calls);
        let feed = Arc::clone(&script);
        let started = tokio::time::Instant::now();
        let tick = state
            .run_maintenance_pass_with(
                Pace::Paced {
                    slice_budget: 8,
                    sleep: Duration::from_millis(500),
                },
                true,
                move |_, max_fragments| {
                    record.lock().expect("calls lock").push(max_fragments);
                    let removed = feed.lock().expect("script lock").remove(0);
                    async move { Ok(stats(removed)) }
                },
            )
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);
        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec![Some(8); 3],
            "every paced slice must carry the fragment budget; slicing stops \
             once a slice removes fewer fragments than the budget"
        );
        // Exactly the two inter-slice sleeps elapse on the paused clock.
        assert_eq!(started.elapsed(), Duration::from_millis(1000));
    }

    /// GC's `distinct_roots` scan is machine-wide; rerunning it once per
    /// paced compaction slice wastes the scan for no benefit (nothing new
    /// appears for GC to collect mid-pass). Only the first slice must run
    /// GC.
    #[tokio::test(start_paused = true)]
    async fn paced_pass_runs_gc_only_on_the_first_slice() {
        let (state, _ground) = test_state(|_| {}).await;
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let script = Arc::new(Mutex::new(vec![Some(8usize), Some(8), Some(3)]));
        let feed = Arc::clone(&script);
        let tick = state
            .run_maintenance_pass_with(
                Pace::Paced {
                    slice_budget: 8,
                    sleep: Duration::from_millis(500),
                },
                true,
                move |_, _max_fragments| {
                    let removed = feed.lock().expect("script lock").remove(0);
                    async move { Ok(stats(removed)) }
                },
            )
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);

        let events = capture.0.lock().expect("capture lock");
        let gc_scans = events
            .iter()
            .filter(|e| e.strings.get("gc_event").map(String::as_str) == Some("scan_started"))
            .count();
        assert_eq!(
            gc_scans, 1,
            "GC must scan exactly once across all three paced slices, not once per slice"
        );
    }

    #[tokio::test]
    async fn paced_pass_stops_when_a_slice_fails() {
        let (state, _ground) = test_state(|_| {}).await;
        let calls: Arc<Mutex<Vec<Option<usize>>>> = Arc::default();
        let record = Arc::clone(&calls);
        let tick = state
            .run_maintenance_pass_with(
                Pace::Paced {
                    slice_budget: 8,
                    sleep: Duration::from_millis(500),
                },
                true,
                move |_, max_fragments| {
                    record.lock().expect("calls lock").push(max_fragments);
                    async move { Err(HallouminateError::Config("slice failed".to_owned())) }
                },
            )
            .await;
        // A failed slice ends the pass like a failed Full pass: Continue,
        // and the backlog waits for the next interval tick.
        assert_eq!(tick, MaintenanceTick::Continue);
        assert_eq!(calls.lock().expect("calls lock").len(), 1);
    }

    #[tokio::test]
    async fn paced_pass_stops_when_removal_count_is_unknown() {
        let (state, _ground) = test_state(|_| {}).await;
        let calls: Arc<Mutex<Vec<Option<usize>>>> = Arc::default();
        let record = Arc::clone(&calls);
        let tick = state
            .run_maintenance_pass_with(
                Pace::Paced {
                    slice_budget: 8,
                    sleep: Duration::from_millis(500),
                },
                true,
                move |_, max_fragments| {
                    record.lock().expect("calls lock").push(max_fragments);
                    async move { Ok(stats(None)) }
                },
            )
            .await;
        // Unmeasurable progress must not slice forever.
        assert_eq!(tick, MaintenanceTick::Continue);
        assert_eq!(calls.lock().expect("calls lock").len(), 1);
    }

    /// A self-armed watchdog polls at stall/4; a naive single sleep for the
    /// whole jittered interval would starve it of any bump for up to the
    /// full interval. The interval sleep must be chunked so the epoch
    /// advances at least every 60s even mid-sleep.
    #[tokio::test(start_paused = true)]
    async fn maintenance_epoch_advances_at_least_every_60s_during_a_long_interval_sleep() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent
        // test would interrupt the interval sleep this test asserts on.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 0).await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(3600),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        let mut previous = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::Maintenance);
        // Advance in 60s steps through most of the hour-long interval sleep;
        // the epoch must move on every step, never stalling for a full 60s.
        for _ in 0..50 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
            let current = state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::Maintenance);
            assert!(
                current > previous,
                "Maintenance epoch must advance at least every 60s during the interval sleep"
            );
            previous = current;
        }

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    /// HIGH-3 (mid-pass gap): `maintain` itself can run past
    /// `watchdog_stall_secs` (e.g. a slow compaction). The tick must keep
    /// bumping the epoch while `maintain` is in flight, not just around it.
    #[tokio::test(start_paused = true)]
    async fn maintenance_epoch_advances_at_least_every_60s_during_a_long_pass() {
        let (state, _ground) = test_state(|_| {}).await;
        let task = tokio::spawn({
            let state = state.clone();
            async move {
                // This test's subject is `maintain`'s own long-running
                // heartbeat behaviour, not GC -- GC is disabled here so the
                // test isn't racing GC's `spawn_blocking` round-trip (a real
                // OS thread, not driven by this test's paused virtual clock)
                // against a fixed synchronization point.
                state
                    .run_maintenance_tick_with(false, |_id| async {
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        Ok(stats(Some(0)))
                    })
                    .await
            }
        });
        tokio::task::yield_now().await;

        let mut previous = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::Maintenance);
        for _ in 0..9 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
            let current = state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::Maintenance);
            assert!(
                current > previous,
                "Maintenance epoch must advance at least every 60s during a long pass"
            );
            previous = current;
        }

        let tick = task.await.expect("maintenance tick task");
        assert_eq!(tick, MaintenanceTick::Continue);
    }

    /// RAII guard for `debt::set_test_level`: resets to `None` on drop so
    /// the override never leaks into the next test scheduled on the same
    /// OS thread (Rust's default harness reuses threads across sequential
    /// tests).
    struct DebtLevelGuard;

    impl DebtLevelGuard {
        fn set(level: DebtLevel) -> Self {
            debt::set_test_level(Some(level));
            Self
        }
    }

    impl Drop for DebtLevelGuard {
        fn drop(&mut self) {
            debt::set_test_level(None);
        }
    }

    /// HIGH-2 (mid-sleep gap): a Hard debt onset during the interval sleep
    /// must wake `sleep_with_heartbeat` within one 60s chunk instead of
    /// waiting out the full jittered interval.
    #[tokio::test(start_paused = true)]
    async fn hard_debt_onset_during_interval_sleep_forces_pass_within_one_chunk() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent test
        // would fire maintenance before this test sets its own Hard onset.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 0).await;
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(3600),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        // Two ordinary 60s chunks with debt still Ok: no maintenance yet.
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            !capture.maintenance_started(),
            "maintenance must not start before the interval elapses"
        );

        // Debt turns Hard mid-sleep; the very next chunk must wake it.
        let _debt = DebtLevelGuard::set(DebtLevel::Hard);
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            capture.maintenance_started(),
            "a Hard debt onset mid-sleep must force the pass within one 60s chunk, \
             not wait out the full 3600s interval"
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    /// HIGH-2 (mid-defer-loop gap): a Hard debt onset while a due pass is
    /// deferring must force the pass within one `DEFER_RECHECK`, not wait
    /// for `defer_bound_secs` to elapse.
    #[tokio::test(start_paused = true)]
    async fn hard_debt_onset_during_defer_loop_forces_pass_within_one_recheck() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent test
        // would fire maintenance before this test sets its own Hard onset.
        let _coord = debt::OBSERVED_HARD_COORD.read().await;
        let (state, _ground) = test_state(|cfg| cfg.daemon.defer_bound_secs = 600).await;
        // Continuous external activity keeps the pass deferred until forced.
        let _active = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));
        tokio::task::yield_now().await;

        // Past interval + max jitter: the pass becomes due and defers.
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(51)).await;
        tokio::task::yield_now().await;
        // One full recheck deferred with debt still Ok.
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(
            !capture.maintenance_started(),
            "must stay deferred while debt is Ok and well short of the bound"
        );

        // Debt turns Hard; the next recheck must force the pass immediately,
        // long before the 600s defer bound.
        let _debt = DebtLevelGuard::set(DebtLevel::Hard);
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            capture.maintenance_started(),
            "a Hard debt onset during the defer loop must force the pass on the next recheck"
        );
        let forced = capture.forced_event().expect("forced-pass warn event");
        assert_eq!(
            forced.strings.get("hard_onset").map(String::as_str),
            Some("true")
        );
        assert!(
            forced
                .numbers
                .get("deferred_secs")
                .is_some_and(|&secs| secs < 600),
            "must fire well before the 600s defer bound, via the Hard onset, not the bound"
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    fn prepared_file(
        corpus_key: &hallouminate_domain::common::CorpusKey,
        file_ref: &str,
    ) -> hallouminate_domain::indexer::PreparedFile {
        hallouminate_domain::indexer::PreparedFile {
            file_ref: file_ref.to_string(),
            corpus_key: corpus_key.clone(),
            mtime_ms: 1,
            content_hash: "deadbeef".into(),
            summary: "summary".into(),
            keywords: vec![],
            frontmatter: None,
            indexed_at_ms: 1,
            chunks: vec![hallouminate_domain::indexer::PreparedChunk {
                ord: 0,
                heading_path: vec!["H".into()],
                line_start: 1,
                line_end: 2,
                text: "body".into(),
                search_text: "body".into(),
                claim_marks: None,
            }],
        }
    }

    /// Acceptance: "GC executes before compaction within a single
    /// maintenance pass." Seeds a retired root, then asserts the
    /// `maintain` closure -- standing in for compaction -- observes zero
    /// rows at that root by the time it runs, proving GC already ran.
    #[tokio::test]
    async fn gc_runs_before_compaction_within_a_maintenance_tick() {
        let (state, _ground) = test_state(|_| {}).await;
        let gone = tempfile::tempdir().expect("gone root");
        let key_gone = hallouminate_domain::common::CorpusKey::from_configured_root(
            "repo:gone:corpus",
            gone.path().to_str().expect("utf8"),
        );
        state
            .store()
            .apply_batch(vec![prepared_file(&key_gone, "/tmp/gone.md")])
            .await
            .expect("seed retired root");
        let gone_path = gone.path().to_path_buf();
        drop(gone);
        assert!(!gone_path.exists());

        let store = state.store();
        let key_for_closure = key_gone.clone();
        let tick = state
            .run_maintenance_tick_with(true, move |_id| {
                let store = Arc::clone(&store);
                let key = key_for_closure.clone();
                async move {
                    let stats_at_compaction = store
                        .corpus_chunk_stats(&key)
                        .await
                        .expect("stats during compaction step");
                    assert_eq!(
                        stats_at_compaction.total_chunks, 0,
                        "GC must have already removed the retired root's rows \
                         before the compaction step runs"
                    );
                    Ok(stats(Some(0)))
                }
            })
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);
    }

    /// Regression guard for the #215 sibling-wipe shape: a mutation from
    /// `retired_roots(&known)` to `known.clone()` (i.e. "collect every
    /// root") must fail this test. Seeds one retired root and one live
    /// (surviving) root, asserts the survivor's rows are untouched and
    /// `roots_collected == 1`, not 2.
    #[tokio::test]
    async fn run_gc_does_not_collect_a_surviving_root() {
        let (state, _ground) = test_state(|_| {}).await;
        let gone = tempfile::tempdir().expect("gone root");
        let key_gone = hallouminate_domain::common::CorpusKey::from_configured_root(
            "repo:gone:corpus",
            gone.path().to_str().expect("utf8"),
        );
        state
            .store()
            .apply_batch(vec![prepared_file(&key_gone, "/tmp/gone.md")])
            .await
            .expect("seed retired root");
        let gone_path = gone.path().to_path_buf();
        drop(gone);
        assert!(!gone_path.exists());

        let survivor = tempfile::tempdir().expect("survivor root");
        let key_survivor = hallouminate_domain::common::CorpusKey::from_configured_root(
            "repo:survivor:corpus",
            survivor.path().to_str().expect("utf8"),
        );
        state
            .store()
            .apply_batch(vec![prepared_file(&key_survivor, "/tmp/survivor.md")])
            .await
            .expect("seed survivor root");

        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let tick = state
            .run_maintenance_tick_with(true, |_id| async { Ok(stats(Some(0))) })
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);

        let stats_survivor = state
            .store()
            .corpus_chunk_stats(&key_survivor)
            .await
            .expect("stats survivor");
        assert_eq!(
            stats_survivor.total_chunks, 1,
            "GC must not delete a live sibling root's rows"
        );

        let events = capture.0.lock().expect("capture lock");
        let success = events
            .iter()
            .find(|e| e.strings.get("outcome").map(String::as_str) == Some("success"))
            .expect("success event");
        assert_eq!(
            success.numbers.get("roots_collected"),
            Some(&1),
            "only the retired root, not the survivor, must be collected"
        );
    }

    /// Correctness: GC must not fire on the watcher's churn-triggered
    /// `ForceMaintenance` path (`allow_gc = false`) -- only the scheduled
    /// tick collects retired roots, since churn events cluster exactly
    /// when a root is most likely to be transiently absent.
    #[tokio::test]
    async fn run_maintenance_tick_with_gc_disabled_does_not_collect() {
        let (state, _ground) = test_state(|_| {}).await;
        let gone = tempfile::tempdir().expect("gone root");
        let key_gone = hallouminate_domain::common::CorpusKey::from_configured_root(
            "repo:gone:corpus",
            gone.path().to_str().expect("utf8"),
        );
        state
            .store()
            .apply_batch(vec![prepared_file(&key_gone, "/tmp/gone.md")])
            .await
            .expect("seed retired root");
        let gone_path = gone.path().to_path_buf();
        drop(gone);
        assert!(!gone_path.exists());

        let tick = state.run_maintenance_tick(false).await;
        assert_eq!(tick, MaintenanceTick::Continue);

        let stats_gone = state
            .store()
            .corpus_chunk_stats(&key_gone)
            .await
            .expect("stats gone");
        assert_eq!(
            stats_gone.total_chunks, 1,
            "GC must not run when allow_gc = false, even though the root is retired"
        );
    }

    /// Acceptance: "the maintenance pass collects retired roots and reports
    /// `roots_collected` and `rows_removed`."
    #[tokio::test]
    async fn maintenance_tick_reports_gc_roots_collected_and_rows_removed() {
        let (state, _ground) = test_state(|_| {}).await;
        let gone = tempfile::tempdir().expect("gone root");
        let key_gone = hallouminate_domain::common::CorpusKey::from_configured_root(
            "repo:gone:corpus",
            gone.path().to_str().expect("utf8"),
        );
        state
            .store()
            .apply_batch(vec![prepared_file(&key_gone, "/tmp/gone.md")])
            .await
            .expect("seed retired root");
        drop(gone);

        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let tick = state
            .run_maintenance_tick_with(true, |_id| async { Ok(stats(Some(0))) })
            .await;
        assert_eq!(tick, MaintenanceTick::Continue);

        let events = capture.0.lock().expect("capture lock");
        let success = events
            .iter()
            .find(|e| e.strings.get("outcome").map(String::as_str) == Some("success"))
            .expect("success event");
        assert_eq!(success.numbers.get("roots_collected"), Some(&1));
        assert_eq!(success.numbers.get("rows_removed"), Some(&1));
    }

    /// Gap: `run_gc` must bump the maintenance heartbeat while retiring
    /// multiple roots, not just once around the whole call.
    #[tokio::test]
    async fn run_gc_bumps_heartbeat_for_each_retired_root() {
        let (state, _ground) = test_state(|_| {}).await;
        let mut gone_paths = Vec::new();
        for i in 0..3 {
            let gone = tempfile::tempdir().expect("gone root");
            let key = hallouminate_domain::common::CorpusKey::from_configured_root(
                format!("repo:gone{i}:corpus"),
                gone.path().to_str().expect("utf8"),
            );
            state
                .store()
                .apply_batch(vec![prepared_file(&key, &format!("/tmp/gone{i}.md"))])
                .await
                .expect("seed retired root");
            gone_paths.push(gone.path().to_path_buf());
            drop(gone);
        }
        for path in &gone_paths {
            assert!(!path.exists());
        }

        let before = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::Maintenance);
        let (result, gc_result) = state.run_gc(1).await;
        gc_result.expect("run_gc");
        let after = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::Maintenance);

        assert_eq!(result.roots_collected, 3);
        assert!(
            after >= before + 3,
            "heartbeat must bump at least once per retired root: before={before}, after={after}"
        );
    }
}
