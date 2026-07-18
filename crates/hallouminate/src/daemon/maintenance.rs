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
use crate::config::DaemonConfig;
use hallouminate_adapters::{MaintenanceOptions, MaintenanceStats};
use hallouminate_domain::common::HallouminateError;

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
        let due_since = tokio::time::Instant::now();
        let defer_bound = Duration::from_secs(state.baseline().daemon.defer_bound_secs);
        let mut pace = Pace::Full;
        // ADR daemon-rework-001: Hard debt forces the pass past both the
        // Active and IoPressure defer gates below. Inert while `debt::level()`
        // is stubbed to `Ok` -- dispatch B wires the real fragment/version
        // thresholds that can report `Hard`.
        let hard_forced = debt::level() == DebtLevel::Hard;
        if !hard_forced {
            state.reset_defer_count();
            while let Some(reason) = state.maintenance_defer_reason(probe.as_ref()) {
                let deferred_for = due_since.elapsed();
                if deferred_for >= defer_bound {
                    // The bound is real, not merely counted (the 2026-07-17
                    // incident deferred 1000 consecutive times with only a
                    // WARN): the due pass now runs despite the standing
                    // defer reason.
                    pace = forced_pace(probe.elevated(), &state.baseline().daemon);
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        ?reason,
                        deferred_secs = deferred_for.as_secs(),
                        defer_bound_secs = defer_bound.as_secs(),
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
            }
        }
        // A Hard-forced pass stays `Pace::Full` even under elevated PSI --
        // unlike the defer-bound-forced path above, which calls `forced_pace`.
        // Hard already blocks writes, so fast debt recovery outranks pacing.
        let tick = match pace {
            Pace::Full => state.run_maintenance_tick().await,
            Pace::Paced { .. } => state.run_maintenance_pass(pace).await,
        };
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
    /// One LanceDB maintenance pass (compaction + version prune). Holds a
    /// connection guard for the write's duration so idle-exit defers instead
    /// of tearing the process down (and releasing the single-instance flock)
    /// under a live LanceDB write, mirroring `catch_up_index` (dispatch.rs)
    /// and the watcher's `process_change_batch`. This pass does NOT stamp
    /// the idle-activity clock (ADR-002) -- reintroducing that stamp would
    /// bring back #222.
    pub(super) async fn run_maintenance_tick(&self) -> MaintenanceTick {
        self.run_maintenance_pass(Pace::Full).await
    }

    /// One maintenance pass at `pace` against the real store -- the
    /// `Pace::Paced` entry for a defer-bound-forced pass under pressure.
    async fn run_maintenance_pass(&self, pace: Pace) -> MaintenanceTick {
        let store = self.store();
        self.run_maintenance_pass_with(pace, move |maintenance_id, max_fragments_per_slice| {
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
        })
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
                .run_maintenance_tick_with(|id| maintain(id, None))
                .await;
        };
        loop {
            let slice_removed: Arc<std::sync::Mutex<Option<usize>>> = Arc::default();
            let capture = Arc::clone(&slice_removed);
            let tick = self
                .run_maintenance_tick_with(|maintenance_id| {
                    let slice = maintain(maintenance_id, Some(slice_budget));
                    async move {
                        let stats = slice.await?;
                        *capture.lock().expect("slice stats lock") = stats.fragments_removed;
                        Ok(stats)
                    }
                })
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
        tokio::time::advance(Duration::from_secs(111)).await;
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

        tokio::time::advance(Duration::from_secs(111)).await;
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

        tokio::time::advance(Duration::from_secs(111)).await;
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
            .run_maintenance_pass_with(Pace::Full, move |_, max_fragments| {
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
}
