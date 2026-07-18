//! Writer backpressure on maintenance debt (ADR daemon-rework-001):
//! mutations pay for maintenance debt, reads never stall. `Soft` taxes
//! each mutation with `debt_soft_delay_ms`; `Hard` blocks mutations until
//! debt drops below Hard (the forced maintenance pass completing), bounded
//! by `hard_block_wait_secs`, then fails with [`RETRYABLE_HARD_DEBT`]. The
//! gate runs BEFORE the corpus lock and write-lane permit, so a blocked
//! mutation holds nothing the forced maintenance pass needs (the spec's
//! no-cycle rule).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::debt::{self, DebtLevel, MaintenanceDebt};
use super::state::{DaemonState, MutationGuard};

/// Error for a mutation that timed out waiting out `DebtLevel::Hard`. The
/// operation was never started, so retrying is safe; dispatch should map
/// exactly this message to `ErrorKind::Retryable` on the wire (`ipc.rs`).
pub(crate) const RETRYABLE_HARD_DEBT: &str =
    "maintenance debt is Hard; mutation timed out waiting for maintenance -- retry shortly";

/// Debt-graduated gate, then the corpus-lock + write-lane acquisition that
/// `DaemonState::acquire_mutation_guard` delegates here (corpus lock, then
/// write-lane permit, in that documented order).
pub(super) async fn acquire(
    state: &DaemonState,
    corpus: &str,
) -> Result<MutationGuard, &'static str> {
    let daemon_cfg = &state.baseline().daemon;
    let maintenance_disabled = daemon_cfg.maintenance_interval_secs == 0;
    gate(
        || probe_admitting_hard_debt_without_maintenance(state, maintenance_disabled),
        Duration::from_millis(daemon_cfg.debt_soft_delay_ms),
        Duration::from_secs(daemon_cfg.hard_block_wait_secs),
        Duration::from_secs(daemon_cfg.debt_cache_ttl_secs.max(1)),
    )
    .await?;
    let corpus_guard = state.lock_corpus(corpus).await;
    let permit = state
        .write_lane()
        .acquire_owned()
        .await
        .map_err(|_| "write lane closed")?;
    Ok(MutationGuard::new(permit, corpus_guard))
}

/// Wraps [`observed_level`] so `gate` never enters its bounded Hard block
/// when automatic maintenance is disabled (`maintenance_interval_secs == 0`
/// -- `DaemonState::open` never spawns `maintenance_loop` in that case, see
/// the comment there). With nothing to pay the debt down, an unrelieved
/// Hard block would soft-lock every mutation forever. Downgrades the
/// observation to `Soft` instead: the mutation still pays
/// `debt_soft_delay_ms`, but is admitted.
async fn probe_admitting_hard_debt_without_maintenance(
    state: &DaemonState,
    maintenance_disabled: bool,
) -> DebtLevel {
    let level = observed_level(state).await;
    if level == DebtLevel::Hard && maintenance_disabled {
        if HARD_DEBT_NO_MAINTENANCE_WARN.should_log() {
            tracing::warn!(
                target: "hallouminate::daemon",
                "maintenance debt is Hard but automatic maintenance is disabled \
                 (daemon.maintenance_interval_secs = 0); admitting the mutation \
                 instead of blocking",
            );
        }
        return DebtLevel::Soft;
    }
    level
}

/// Current debt level: the cached observation while younger than
/// `debt_cache_ttl_secs`, else a fresh re-read via [`refresh_observed`]. The
/// cache addresses the spec risk of a metadata read on every mutation.
async fn observed_level(state: &DaemonState) -> DebtLevel {
    let daemon_cfg = &state.baseline().daemon;
    let ttl = Duration::from_secs(daemon_cfg.debt_cache_ttl_secs);
    if let Some(level) = debt::OBSERVED.fresh_level(ttl) {
        return level;
    }
    refresh_observed(state).await
}

/// Minimum gap between consecutive throttled warnings on the hot write
/// path -- a persistent condition under a write storm must not flood logs
/// (both callers below run on every uncached mutation).
const WARN_WINDOW: Duration = Duration::from_secs(60);

/// Throttles a warning to at most one log line per [`WARN_WINDOW`],
/// regardless of call volume. Each hot-path warning site gets its own
/// static instance so the two conditions below throttle independently.
struct WarnThrottle(Mutex<Option<Instant>>);

impl WarnThrottle {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn should_log(&self) -> bool {
        let mut last = self.0.lock().expect("warn throttle lock");
        let now = Instant::now();
        let should_log = last.is_none_or(|at| now.duration_since(at) >= WARN_WINDOW);
        if should_log {
            *last = Some(now);
        }
        should_log
    }
}

static DEBT_READ_FAILURE_WARN: WarnThrottle = WarnThrottle::new();
static HARD_DEBT_NO_MAINTENANCE_WARN: WarnThrottle = WarnThrottle::new();
static HARD_DEBT_BLOCK_WARN: WarnThrottle = WarnThrottle::new();

/// Re-reads real debt from the store, classifies it, and records the fresh
/// level into `debt::OBSERVED`. Used by the mutation gate's cache-miss path
/// and by `maintenance_loop` to refresh the observation after a Hard-forced
/// pass -- otherwise a write-idle-but-read-active daemon that once observed
/// Hard keeps running full-speed passes off the stale reading. A failed
/// read fails OPEN (`Ok`, logged): a broken stats call must not stall the
/// write path -- the write itself surfaces real store errors.
pub(super) async fn refresh_observed(state: &DaemonState) -> DebtLevel {
    let daemon_cfg = &state.baseline().daemon;
    match state.store().debt().await {
        Ok(lance_debt) => {
            let level = debt::classify(
                &MaintenanceDebt {
                    fragments: lance_debt.fragments,
                    stale_versions: lance_debt.stale_versions,
                },
                daemon_cfg,
            );
            debt::OBSERVED.record(level);
            level
        }
        Err(error) => {
            if DEBT_READ_FAILURE_WARN.should_log() {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    error = %error,
                    "debt read failed; falling open on this observation",
                );
            }
            DebtLevel::Ok
        }
    }
}

/// The graduated gate, generic over the level probe so tests drive level
/// sequences without a store. `Hard` re-probes every `poll` (the debt-cache
/// TTL -- fresher polls could only hit the cache) with one final probe at
/// the deadline, then fails retryable.
async fn gate<P, Fut>(
    mut probe: P,
    soft_delay: Duration,
    hard_wait: Duration,
    poll: Duration,
) -> Result<(), &'static str>
where
    P: FnMut() -> Fut,
    Fut: std::future::Future<Output = DebtLevel>,
{
    match probe().await {
        DebtLevel::Ok => Ok(()),
        DebtLevel::Soft => {
            tokio::time::sleep(soft_delay).await;
            Ok(())
        }
        DebtLevel::Hard => {
            // Throttled: under a write storm every blocked mutation reaches
            // this branch, so emit at most one WARN per `WARN_WINDOW` -- the
            // condition, not each instance, is what an operator needs.
            if HARD_DEBT_BLOCK_WARN.should_log() {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    hard_block_wait_secs = hard_wait.as_secs(),
                    "maintenance debt is Hard; blocking mutation until maintenance catches up",
                );
            }
            let deadline = tokio::time::Instant::now() + hard_wait;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(RETRYABLE_HARD_DEBT);
                }
                tokio::time::sleep_until(std::cmp::min(now + poll, deadline)).await;
                if probe().await != DebtLevel::Hard {
                    tracing::debug!(
                        target: "hallouminate::daemon",
                        "maintenance debt dropped below Hard; mutation unblocked",
                    );
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant as StdInstant;

    use super::debt::DebtCache;
    use super::*;
    use crate::config::{Config, DaemonConfig};

    const SOFT_DELAY: Duration = Duration::from_millis(250);
    const HARD_WAIT: Duration = Duration::from_secs(30);
    const POLL: Duration = Duration::from_secs(5);

    fn signals(fragments: u64, stale_versions: u64) -> MaintenanceDebt {
        MaintenanceDebt {
            fragments,
            stale_versions,
        }
    }

    // -- classify: config-threshold boundaries --

    #[test]
    fn classify_below_both_soft_thresholds_is_ok() {
        let cfg = DaemonConfig::default();
        assert_eq!(debt::classify(&signals(99, 49), &cfg), DebtLevel::Ok);
    }

    #[test]
    fn classify_reaching_either_soft_threshold_is_soft() {
        let cfg = DaemonConfig::default();
        assert_eq!(debt::classify(&signals(100, 0), &cfg), DebtLevel::Soft);
        assert_eq!(debt::classify(&signals(0, 50), &cfg), DebtLevel::Soft);
        assert_eq!(
            debt::classify(&signals(499, 249), &cfg),
            DebtLevel::Soft,
            "just under both hard thresholds must stay Soft",
        );
    }

    #[test]
    fn classify_reaching_either_hard_threshold_is_hard() {
        let cfg = DaemonConfig::default();
        assert_eq!(debt::classify(&signals(500, 0), &cfg), DebtLevel::Hard);
        assert_eq!(
            debt::classify(&signals(0, 250), &cfg),
            DebtLevel::Hard,
            "the worse signal wins even when the other is below Soft",
        );
    }

    // -- DebtCache TTL semantics (injected clock) --

    #[test]
    fn cache_serves_recorded_level_within_ttl_and_expires_at_ttl() {
        let cache = DebtCache::new();
        let base = StdInstant::now();
        cache.record_at(DebtLevel::Hard, base);
        let ttl = Duration::from_secs(5);
        assert_eq!(
            cache.fresh_level_at(ttl, base + Duration::from_millis(4_999)),
            Some(DebtLevel::Hard),
        );
        assert_eq!(
            cache.fresh_level_at(ttl, base + ttl),
            None,
            "a reading exactly ttl old must trigger a re-read",
        );
    }

    #[test]
    fn cache_ttl_zero_disables_caching() {
        let cache = DebtCache::new();
        let base = StdInstant::now();
        cache.record_at(DebtLevel::Soft, base);
        assert_eq!(cache.fresh_level_at(Duration::ZERO, base), None);
    }

    #[test]
    fn cache_level_defaults_ok_and_outlives_ttl_expiry() {
        let cache = DebtCache::new();
        assert_eq!(
            cache.level(),
            DebtLevel::Ok,
            "no observation yet must read as Ok, not stall anything",
        );
        let base = StdInstant::now();
        cache.record_at(DebtLevel::Hard, base);
        assert_eq!(
            cache.fresh_level_at(Duration::from_secs(5), base + Duration::from_secs(60)),
            None,
        );
        assert_eq!(
            cache.level(),
            DebtLevel::Hard,
            "the maintenance loop keys forced runs on the last observation regardless of cache age",
        );
    }

    #[test]
    fn recorded_observation_reaches_the_maintenance_loops_level_read() {
        // The activation seam: backpressure records into debt::OBSERVED and
        // maintenance_loop's forced-run branch reads debt::level(). Uses
        // Soft (not Hard) so a concurrently running mutating test that
        // happens to read the shared observation is delayed, never blocked.
        debt::OBSERVED.record(DebtLevel::Soft);
        assert_eq!(debt::level(), DebtLevel::Soft);
    }

    // -- gate: graduated behavior under a paused clock --

    #[tokio::test(start_paused = true)]
    async fn ok_debt_admits_the_mutation_without_delay() {
        let started = tokio::time::Instant::now();
        gate(|| async { DebtLevel::Ok }, SOFT_DELAY, HARD_WAIT, POLL)
            .await
            .expect("Ok debt admits");
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "no debt must cost no time",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn soft_debt_charges_the_per_mutation_delay() {
        let started = tokio::time::Instant::now();
        gate(|| async { DebtLevel::Soft }, SOFT_DELAY, HARD_WAIT, POLL)
            .await
            .expect("Soft debt admits after the delay");
        assert_eq!(tokio::time::Instant::now() - started, SOFT_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn hard_debt_fails_retryable_after_exactly_the_bounded_wait() {
        let started = tokio::time::Instant::now();
        let err = gate(|| async { DebtLevel::Hard }, SOFT_DELAY, HARD_WAIT, POLL)
            .await
            .expect_err("unrelieved Hard debt must not admit the mutation");
        assert_eq!(err, RETRYABLE_HARD_DEBT);
        assert_eq!(
            tokio::time::Instant::now() - started,
            HARD_WAIT,
            "the block must be bounded at hard_block_wait, not indefinite",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_debt_unblocks_as_soon_as_a_poll_sees_the_level_drop() {
        let probes = AtomicUsize::new(0);
        let probe = || {
            let n = probes.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 3 {
                    DebtLevel::Hard
                } else {
                    DebtLevel::Ok
                }
            }
        };
        let started = tokio::time::Instant::now();
        gate(probe, SOFT_DELAY, HARD_WAIT, POLL)
            .await
            .expect("mutation admitted once maintenance pays the debt down");
        assert_eq!(
            tokio::time::Instant::now() - started,
            3 * POLL,
            "unblocks at the first poll observing the drop, well before the bound",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_debt_gets_a_final_probe_at_the_deadline() {
        let probes = AtomicUsize::new(0);
        let probe = || {
            let n = probes.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    DebtLevel::Hard
                } else {
                    DebtLevel::Ok
                }
            }
        };
        // Shorter than POLL: one sleep straight to the deadline, one last
        // probe there.
        let short_wait = Duration::from_secs(2);
        let started = tokio::time::Instant::now();
        gate(probe, SOFT_DELAY, short_wait, POLL)
            .await
            .expect("a level drop at the deadline still admits the mutation");
        assert_eq!(tokio::time::Instant::now() - started, short_wait);
    }

    // -- acquire: config + store wiring through DaemonState --

    async fn open_state(tmp: &tempfile::TempDir, daemon: DaemonConfig) -> DaemonState {
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        cfg.daemon = daemon;
        DaemonState::open(cfg, None)
            .await
            .expect("open daemon state")
    }

    #[tokio::test]
    async fn acquire_on_a_fresh_store_classifies_ok_and_grants_the_guard_promptly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // ttl 0 pins classification to THIS store's real (empty) debt; the
        // huge soft delay would make a misclassification visible below.
        let daemon = DaemonConfig {
            debt_cache_ttl_secs: 0,
            debt_soft_delay_ms: 10_000,
            ..DaemonConfig::default()
        };
        let state = open_state(&tmp, daemon).await;
        let started = StdInstant::now();
        let _guard = state
            .acquire_mutation_guard("wiki")
            .await
            .expect("fresh store must not be backpressured");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an Ok-debt mutation paid a delay it must not pay",
        );
    }

    #[tokio::test]
    async fn acquire_charges_the_configured_soft_delay_when_debt_is_soft() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Soft threshold 0 makes even an empty store classify Soft; ttl 0
        // keeps the classification pinned to this store + config.
        let daemon = DaemonConfig {
            debt_soft_fragments: 0,
            debt_cache_ttl_secs: 0,
            debt_soft_delay_ms: 250,
            ..DaemonConfig::default()
        };
        let state = open_state(&tmp, daemon).await;
        let started = StdInstant::now();
        let _guard = state
            .acquire_mutation_guard("wiki")
            .await
            .expect("Soft debt admits after the delay");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "Soft debt must charge debt_soft_delay_ms per mutation",
        );
    }

    #[tokio::test]
    async fn acquire_admits_promptly_when_hard_debt_and_maintenance_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Exclusive OBSERVED slot: acquire's refresh records Hard into the
        // process-wide cache; loop-spawning tests hold `read` so they never
        // observe it (see debt::OBSERVED_HARD_COORD).
        let _coord = debt::OBSERVED_HARD_COORD.write().await;
        // Hard threshold 0 makes even an empty store classify Hard; ttl 0
        // keeps the classification pinned to this store + config.
        // maintenance_interval_secs 0 means DaemonState::open never spawns
        // maintenance_loop -- nothing would ever pay the debt down, so an
        // unrelieved Hard gate would soft-lock every mutation forever.
        let daemon = DaemonConfig {
            maintenance_interval_secs: 0,
            debt_hard_fragments: 0,
            debt_hard_stale_versions: 0,
            debt_cache_ttl_secs: 0,
            debt_soft_delay_ms: 10,
            hard_block_wait_secs: 30,
            ..DaemonConfig::default()
        };
        let state = open_state(&tmp, daemon).await;
        let started = StdInstant::now();
        let _guard = state
            .acquire_mutation_guard("wiki")
            .await
            .expect("Hard debt with maintenance disabled must not soft-lock the mutation");
        // acquire's refresh recorded Hard into the process-wide
        // debt::OBSERVED (nothing here ever re-classifies it away, unlike
        // the maintenance loop's post-pass refresh) -- restore it so
        // parallel loop-spawning tests don't inherit a forced-run trigger.
        // Proper isolation is the deferred OBSERVED-into-DaemonState move.
        debt::OBSERVED.record(DebtLevel::Ok);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must admit well under hard_block_wait_secs, not block for it; got {:?}",
            started.elapsed(),
        );
    }
}
