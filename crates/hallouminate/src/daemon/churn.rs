//! Reindex-churn escalation (daemon-rework G7): consecutive zero-upsert
//! reindexes escalate through the seeded backpressure ladder -- the
//! incident's access-event feedback loop reindexed with zero effect at 200%
//! CPU for 17h and nothing noticed.
//!
//! Tracking is GLOBAL-consecutive, not per-path: the incident was one hot
//! path saturating the watcher, and any real upsert proves the loop is doing
//! useful work again -- one counter with reset-on-upsert catches that shape
//! without a per-path map nothing in the spec needs.

use std::path::Path;

use super::ladder::{Ladder, LadderAction, LadderOutcome};

/// Consecutive zero-upsert reindex tracker. Owned by the watcher pump (the
/// single caller), hence `&mut self` -- wiring wraps it in a lock only if it
/// ever shares the tracker.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct ChurnTracker {
    ladder: Ladder,
    consecutive_noops: u32,
    /// Re-armed by a real upsert; the act-tier action fires once per streak
    /// (re-firing `ForceMaintenance` on every subsequent noop would itself
    /// be churn).
    action_armed: bool,
}

impl ChurnTracker {
    /// Thresholds come from `DaemonConfig::{churn_warn_at, churn_act_at}`.
    /// The action is `ForceMaintenance`: churn is useless index work, and a
    /// forced maintenance pass reconciles index state without killing the
    /// watcher (`RestartTask` re-enters the same feedback loop;
    /// `WatchdogTrip` is for stalls, not busy loops).
    #[allow(dead_code)]
    pub(crate) fn new(warn_at: u32, act_at: u32) -> Self {
        Self {
            ladder: Ladder {
                warn_at,
                act_at,
                action: LadderAction::ForceMaintenance,
            },
            consecutive_noops: 0,
            action_armed: true,
        }
    }

    /// Record one completed reindex pass; `noop` marks a pass that upserted
    /// no rows (`ApplyStats::files_upserted == 0`), `path` is the reindexed
    /// file (log context only). Returns the fired outcome so the call site
    /// (wiring task W3) executes any `LadderOutcome::Action`.
    #[allow(dead_code)]
    pub(crate) fn record_reindex(&mut self, noop: bool, path: &Path) -> LadderOutcome {
        if !noop {
            self.consecutive_noops = 0;
            self.action_armed = true;
            return LadderOutcome::Nothing;
        }
        self.consecutive_noops = self.consecutive_noops.saturating_add(1);
        match self.ladder.evaluate(self.consecutive_noops) {
            LadderOutcome::Nothing => LadderOutcome::Nothing,
            LadderOutcome::Warn => {
                self.warn(path);
                LadderOutcome::Warn
            }
            LadderOutcome::Action(action) => {
                if self.action_armed {
                    self.action_armed = false;
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        consecutive_noop_reindexes = self.consecutive_noops,
                        path = %path.display(),
                        action = ?action,
                        "churn: consecutive zero-upsert reindexes reached act threshold; escalating",
                    );
                    LadderOutcome::Action(action)
                } else {
                    self.warn(path);
                    LadderOutcome::Warn
                }
            }
        }
    }

    fn warn(&self, path: &Path) {
        tracing::warn!(
            target: "hallouminate::daemon",
            consecutive_noop_reindexes = self.consecutive_noops,
            path = %path.display(),
            "churn: consecutive zero-upsert reindexes past warn threshold",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// warn_at 3 / act_at 5 keeps streak arithmetic readable; the production
    /// defaults (10/50) are config's contract, tested in `config.rs`.
    fn tracker() -> ChurnTracker {
        ChurnTracker::new(3, 5)
    }

    fn noop(t: &mut ChurnTracker) -> LadderOutcome {
        t.record_reindex(true, Path::new("wiki/hot-page.md"))
    }

    fn real_upsert(t: &mut ChurnTracker) -> LadderOutcome {
        t.record_reindex(false, Path::new("wiki/hot-page.md"))
    }

    #[test]
    fn real_upserts_report_nothing() {
        let mut t = tracker();
        for _ in 0..20 {
            assert_eq!(
                real_upsert(&mut t),
                LadderOutcome::Nothing,
                "reindexes that upsert rows are healthy and must never escalate",
            );
        }
    }

    #[test]
    fn noop_streak_below_warn_threshold_reports_nothing() {
        let mut t = tracker();
        for count in 1..3 {
            assert_eq!(
                noop(&mut t),
                LadderOutcome::Nothing,
                "streak of {count} is below warn_at=3 and must stay quiet",
            );
        }
    }

    #[test]
    fn noop_streak_at_warn_threshold_warns() {
        let mut t = tracker();
        noop(&mut t);
        noop(&mut t);
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Warn,
            "the warn tier must fire exactly when the streak reaches warn_at",
        );
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Warn,
            "the warn tier must keep firing while the streak grows toward act_at",
        );
    }

    #[test]
    fn noop_streak_at_act_threshold_fires_force_maintenance() {
        let mut t = tracker();
        for _ in 0..4 {
            noop(&mut t);
        }
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Action(LadderAction::ForceMaintenance),
            "the act tier must fire the configured action when the streak reaches act_at",
        );
    }

    #[test]
    fn action_fires_once_per_streak_then_degrades_to_warn() {
        let mut t = tracker();
        for _ in 0..5 {
            noop(&mut t);
        }
        for _ in 0..10 {
            assert_eq!(
                noop(&mut t),
                LadderOutcome::Warn,
                "a fired action must not re-fire while the same streak continues",
            );
        }
    }

    #[test]
    fn real_upsert_resets_streak_and_rearms_action() {
        let mut t = tracker();
        for _ in 0..5 {
            noop(&mut t);
        }
        real_upsert(&mut t);
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Nothing,
            "a real upsert must reset the consecutive-noop streak to zero",
        );
        for _ in 0..3 {
            noop(&mut t);
        }
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Action(LadderAction::ForceMaintenance),
            "a reset streak that climbs back to act_at must fire the action again",
        );
    }

    #[test]
    fn streak_is_global_across_paths_and_reset_by_any_real_upsert() {
        let mut t = tracker();
        t.record_reindex(true, Path::new("wiki/a.md"));
        t.record_reindex(true, Path::new("wiki/b.md"));
        assert_eq!(
            t.record_reindex(true, Path::new("wiki/c.md")),
            LadderOutcome::Warn,
            "noops on different paths must accumulate one global streak",
        );
        t.record_reindex(false, Path::new("wiki/other.md"));
        assert_eq!(
            t.record_reindex(true, Path::new("wiki/a.md")),
            LadderOutcome::Nothing,
            "a real upsert on any path must reset the global streak",
        );
    }

    #[test]
    fn equal_warn_and_act_thresholds_fire_the_action_not_a_warn() {
        let mut t = ChurnTracker::new(2, 2);
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Nothing,
            "below the shared threshold the tracker must stay quiet",
        );
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Action(LadderAction::ForceMaintenance),
            "when warn_at == act_at the act tier must win at the shared threshold",
        );
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Warn,
            "after the shared-threshold action fires, the streak degrades to warns",
        );
    }

    #[test]
    fn saturated_streak_keeps_warning_without_panicking() {
        let mut t = tracker();
        t.consecutive_noops = u32::MAX;
        t.action_armed = false;
        assert_eq!(
            noop(&mut t),
            LadderOutcome::Warn,
            "a saturated streak must keep warning, not overflow",
        );
        assert_eq!(
            t.consecutive_noops,
            u32::MAX,
            "the streak must saturate at u32::MAX",
        );
    }

    /// Captures fmt-subscriber output so the test can assert on the actual
    /// log line, not just the returned outcome.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture mutex poisoned").clone())
                .expect("fmt subscriber emits utf-8")
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    fn captured_warns(run: impl FnOnce()) -> String {
        // Pin a permissive process-global dispatcher once. Without it, the
        // global MAX_LEVEL and callsite-interest caches rebuild every time a
        // parallel test's scoped subscriber is installed or dropped, and an
        // event emitted mid-rebuild can be skipped — observed as the act-tier
        // WARN vanishing from this capture while counts 3/4/6 arrive. With a
        // permanent global default the caches never collapse; the thread-local
        // default below still receives every event emitted in `run`.
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        });
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        capture.contents()
    }

    #[test]
    fn warn_tier_emits_distinct_warn_with_count_and_path() {
        let logs = captured_warns(|| {
            let mut t = tracker();
            for _ in 0..3 {
                noop(&mut t);
            }
        });
        assert!(
            logs.contains("WARN"),
            "warn tier must log at WARN level, got: {logs}",
        );
        assert!(
            logs.contains("churn:"),
            "the churn WARN must be distinguishable from other watcher WARNs, got: {logs}",
        );
        assert!(
            logs.contains("consecutive_noop_reindexes=3"),
            "the churn WARN must carry the consecutive count, got: {logs}",
        );
        assert!(
            logs.contains("hot-page.md"),
            "the churn WARN must carry the path context, got: {logs}",
        );
    }

    #[test]
    fn quiet_tiers_emit_no_warns() {
        let logs = captured_warns(|| {
            let mut t = tracker();
            noop(&mut t);
            noop(&mut t);
            real_upsert(&mut t);
        });
        assert_eq!(
            logs, "",
            "below warn_at (and on real upserts) churn must stay silent",
        );
    }

    #[test]
    fn act_tier_warn_names_the_fired_action_and_counts_keep_rising_after() {
        let logs = captured_warns(|| {
            let mut t = tracker();
            for _ in 0..6 {
                noop(&mut t);
            }
        });
        assert!(
            logs.contains("ForceMaintenance"),
            "the act-tier WARN must name the action it fired, got: {logs}",
        );
        assert!(
            logs.contains("consecutive_noop_reindexes=5"),
            "the act-tier WARN must carry the consecutive count, got: {logs}",
        );
        assert!(
            logs.contains("consecutive_noop_reindexes=6"),
            "warns must keep flowing with rising counts after the action fires, got: {logs}",
        );
    }
}
