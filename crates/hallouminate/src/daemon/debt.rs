//! Maintenance debt accounting (ADR daemon-rework-001): real backlog
//! signals (fragment count, stale version count) classified into the
//! `DebtLevel` ladder by the `[daemon]` config thresholds. The latest
//! classification is recorded process-wide so the no-arg `level()` read in
//! `maintenance_loop`'s Hard=>forced-run branch works without a
//! `DaemonState` (one daemon per process makes the global per-daemon).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::DaemonConfig;

/// Real debt signals behind a `DebtLevel` classification, read from
/// `LanceStore::debt()` by `backpressure`'s mutation gate.
pub(crate) struct MaintenanceDebt {
    pub(crate) fragments: u64,
    pub(crate) stale_versions: u64,
}

/// Graduated maintenance debt level (ADR daemon-rework-001): `Soft` taxes
/// each mutation with a small delay; `Hard` blocks mutations (bounded) and
/// forces a maintenance run past the Active/IoPressure defer gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DebtLevel {
    Ok,
    Soft,
    Hard,
}

/// Classifies debt against the config thresholds: either signal reaching a
/// bound trips that level, and the worse signal wins.
pub(super) fn classify(debt: &MaintenanceDebt, cfg: &DaemonConfig) -> DebtLevel {
    if debt.fragments >= cfg.debt_hard_fragments
        || debt.stale_versions >= cfg.debt_hard_stale_versions
    {
        DebtLevel::Hard
    } else if debt.fragments >= cfg.debt_soft_fragments
        || debt.stale_versions >= cfg.debt_soft_stale_versions
    {
        DebtLevel::Soft
    } else {
        DebtLevel::Ok
    }
}

/// A timestamped classification cache. A struct with an injectable clock
/// (mirroring `maintenance_defer_reason_at`) so tests exercise instances
/// without touching the process-wide [`OBSERVED`].
pub(super) struct DebtCache {
    last: Mutex<Option<(DebtLevel, Instant)>>,
}

impl DebtCache {
    pub(super) const fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    pub(super) fn record(&self, level: DebtLevel) {
        self.record_at(level, Instant::now());
    }

    pub(super) fn record_at(&self, level: DebtLevel, at: Instant) {
        *self.last.lock().expect("debt cache lock") = Some((level, at));
    }

    /// The recorded level while it is younger than `ttl`; `None` asks the
    /// caller to re-read real debt. A `ttl` of zero disables caching.
    pub(super) fn fresh_level(&self, ttl: Duration) -> Option<DebtLevel> {
        self.fresh_level_at(ttl, Instant::now())
    }

    pub(super) fn fresh_level_at(&self, ttl: Duration, now: Instant) -> Option<DebtLevel> {
        let last = *self.last.lock().expect("debt cache lock");
        last.filter(|&(_, at)| now.saturating_duration_since(at) < ttl)
            .map(|(level, _)| level)
    }

    /// Last recorded level regardless of age (`Ok` before any record).
    pub(super) fn level(&self) -> DebtLevel {
        let last = *self.last.lock().expect("debt cache lock");
        last.map(|(level, _)| level).unwrap_or(DebtLevel::Ok)
    }
}

/// Process-wide observations feeding both `backpressure`'s mutation gate
/// and the maintenance loop's forced-run branch.
pub(super) static OBSERVED: DebtCache = DebtCache::new();

/// Test-only coordination for [`OBSERVED`]: a test that records `Hard`
/// into the shared cache holds `write` for its whole body; loop-spawning
/// tests whose defer assertions break under an ambient `Hard` reading hold
/// `read`. Async-aware (tokio) so holding a guard across the test body's
/// await points is sound. Proper isolation is the deferred move of the
/// cache into `DaemonState`.
#[cfg(test)]
pub(super) static OBSERVED_HARD_COORD: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Latest observed debt level. Staleness is bounded in the direction that
/// matters: debt only grows through mutations, and every mutation refreshes
/// the observation via `backpressure`.
pub(super) fn level() -> DebtLevel {
    #[cfg(test)]
    if let Some(level) = TEST_LEVEL.with(std::cell::Cell::get) {
        return level;
    }
    OBSERVED.level()
}

// Test-only override for `level()`, isolated per OS thread since Rust's
// default test harness reuses threads across sequential tests. Callers
// MUST reset to `None` (an RAII guard, mirroring `maintenance.rs`'s test
// module) so the override doesn't leak into the next test scheduled on the
// same thread. `None` (the default) falls through to `OBSERVED.level()`
// unchanged, so existing tests that never call this are unaffected.
#[cfg(test)]
thread_local! {
    static TEST_LEVEL: std::cell::Cell<Option<DebtLevel>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn set_test_level(level: Option<DebtLevel>) {
    TEST_LEVEL.with(|cell| cell.set(level));
}