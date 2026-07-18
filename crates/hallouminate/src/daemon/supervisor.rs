//! Task supervisor (ADR daemon-rework G5): owns the daemon's long-lived
//! loops. `spawn(name, factory)` monitors the task's `JoinError` — panics
//! and cancellations are observed and logged distinctly, never unwrapped.
//! A panicked task is rebuilt via its factory and restarted with
//! exponential backoff; restarts are counted against an OTP-style
//! intensity cap (`restart_intensity_cap` per `restart_intensity_window`),
//! and exceeding the cap escalates through the seeded [`Ladder`] to the
//! escalation hook instead of killing the daemon. All five `server.rs`
//! loops (`Maintenance`, `CatchUp`, `WatcherPump`, `IdleExit`, `Signal`)
//! are wired through this supervisor's `spawn`.
//!
//! Restart visibility: the supervisor keeps per-task restart counters
//! (`restart_count`) for status reporting. It deliberately does not bump
//! the heartbeat registry on restart — a crash-looping task must still
//! look stalled to the watchdog, not alive.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::heartbeat::TaskName;
use super::ladder::{Ladder, LadderAction, LadderOutcome};

/// First restart happens this long after a panic.
const BACKOFF_FLOOR: Duration = Duration::from_secs(1);
/// Backoff (and the cap-exceeded cool-down) never exceeds this.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Called when a task's restart intensity has crossed the ladder's
/// `act_at` threshold. Only `LadderAction::WatchdogTrip` is seeded today
/// (`DaemonState::new`); the hook records the trip and logs — it does not
/// itself abort or restart beyond the supervisor's normal backoff (real
/// stall-triggered aborts are `watchdog.rs`'s separate stall detector).
/// Contract: the hook is called from the monitor task and must not panic
/// (a panicking hook kills supervision) and must not block (signal, don't
/// remediate inline).
pub(crate) type EscalationHook = Arc<dyn Fn(TaskName, LadderAction) + Send + Sync>;

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

/// Per-task lifetime restart counters, readable for status reporting.
#[derive(Debug)]
struct RestartCounters {
    counts: [AtomicU64; TASK_COUNT],
}

impl Default for RestartCounters {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// Supervises long-lived daemon tasks: restart-on-panic with backoff,
/// intensity-capped, ladder-escalated past the cap.
pub(crate) struct Supervisor {
    restart_intensity_cap: u32,
    restart_intensity_window: Duration,
    ladder: Ladder,
    escalate: EscalationHook,
    shutdown: CancellationToken,
    restarts: Arc<RestartCounters>,
}

impl Supervisor {
    /// `restart_intensity_cap` / `restart_intensity_window` come from
    /// `DaemonConfig`; `shutdown` is the daemon's shutdown token
    /// (`DaemonState::shutdown_token`). `escalate` receives the ladder's
    /// action when a task exceeds the cap persistently.
    pub(crate) fn new(
        restart_intensity_cap: u32,
        restart_intensity_window: Duration,
        ladder: Ladder,
        escalate: EscalationHook,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            restart_intensity_cap,
            restart_intensity_window,
            ladder,
            escalate,
            shutdown,
            restarts: Arc::new(RestartCounters::default()),
        }
    }

    /// Lifetime restart count for `task` (0 when never restarted).
    #[allow(dead_code)]
    pub(crate) fn restart_count(&self, task: TaskName) -> u64 {
        self.restarts.counts[slot(task)].load(Ordering::Relaxed)
    }

    /// Spawn `factory()` as a supervised task. The factory rebuilds the
    /// task future on every restart; the *future* may panic (that is what
    /// supervision is for) but the factory closure itself must not — a
    /// panicking factory kills the monitor. The returned handle is the monitor's:
    /// it resolves when the task exits normally, is cancelled, or the
    /// shutdown token fires — never because of a task panic.
    pub(crate) fn spawn<F, Fut>(&self, name: TaskName, mut factory: F) -> JoinHandle<()>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let cap = self.restart_intensity_cap;
        let window = self.restart_intensity_window;
        let ladder = self.ladder;
        let escalate = Arc::clone(&self.escalate);
        let shutdown = self.shutdown.clone();
        let restarts = Arc::clone(&self.restarts);

        tokio::spawn(async move {
            let mut recent_panics: Vec<Instant> = Vec::new();
            let mut consecutive_quick_panics: u32 = 0;
            let mut escalation_strikes: u32 = 0;
            loop {
                let started = Instant::now();
                let mut handle = tokio::spawn(factory());
                let result = tokio::select! {
                    res = &mut handle => res,
                    () = shutdown.cancelled() => {
                        // Abort and observe the join — the cancellation arm
                        // below logs it; a JoinError is never unwrapped.
                        handle.abort();
                        handle.await
                    }
                };
                match result {
                    Ok(()) => {
                        tracing::info!(
                            target: "hallouminate::daemon",
                            task = ?name,
                            "supervised task exited normally; not restarting"
                        );
                        return;
                    }
                    Err(err) if err.is_panic() => {
                        let panic = panic_message(err.into_panic());
                        tracing::error!(
                            target: "hallouminate::daemon",
                            task = ?name,
                            panic = %panic,
                            "supervised task panicked"
                        );
                    }
                    Err(err) if err.is_cancelled() => {
                        tracing::info!(
                            target: "hallouminate::daemon",
                            task = ?name,
                            "supervised task cancelled; not restarting"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::error!(
                            target: "hallouminate::daemon",
                            task = ?name,
                            error = %err,
                            "supervised task failed (neither panic nor cancellation); not restarting"
                        );
                        return;
                    }
                }

                // Panic path: restart with backoff under the intensity cap.
                let now = Instant::now();
                if now.duration_since(started) >= window {
                    // A healthy stretch of uptime resets both the backoff
                    // curve and the escalation strikes.
                    consecutive_quick_panics = 0;
                    escalation_strikes = 0;
                }
                consecutive_quick_panics += 1;
                recent_panics.push(now);
                recent_panics.retain(|t| now.duration_since(*t) <= window);
                restarts.counts[slot(name)].fetch_add(1, Ordering::Relaxed);

                // Once escalated, stay escalated: escalation is sticky via
                // `escalation_strikes`, independent of how the cool-down
                // (capped at BACKOFF_CAP) compares to the intensity window,
                // so the ladder keeps firing for a permanent crash loop even
                // when the pruned count alone would stay under the cap. Any
                // quick panic while strikes are outstanding is a further
                // strike; only healthy uptime (the reset above) de-escalates.
                let delay = if recent_panics.len() as u32 > cap || escalation_strikes > 0 {
                    escalation_strikes += 1;
                    tracing::error!(
                        target: "hallouminate::daemon",
                        task = ?name,
                        restarts_in_window = recent_panics.len(),
                        cap,
                        strike = escalation_strikes,
                        "restart intensity cap exceeded"
                    );
                    match ladder.evaluate(escalation_strikes) {
                        LadderOutcome::Nothing => {}
                        LadderOutcome::Warn => {
                            tracing::warn!(
                                target: "hallouminate::daemon",
                                task = ?name,
                                strike = escalation_strikes,
                                "escalation ladder warning: task is crash-looping"
                            );
                        }
                        LadderOutcome::Action(action) => {
                            tracing::error!(
                                target: "hallouminate::daemon",
                                task = ?name,
                                action = ?action,
                                "escalation ladder action triggered"
                            );
                            escalate(name, action);
                        }
                    }
                    BACKOFF_CAP
                } else {
                    backoff_for(consecutive_quick_panics)
                };
                tracing::warn!(
                    target: "hallouminate::daemon",
                    task = ?name,
                    delay_secs = delay.as_secs_f64(),
                    "restarting supervised task after backoff"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = shutdown.cancelled() => {
                        tracing::info!(
                            target: "hallouminate::daemon",
                            task = ?name,
                            "supervised task not restarted: daemon shutting down"
                        );
                        return;
                    }
                }
            }
        })
    }
}

/// Backoff for the nth consecutive quick panic: floor doubling to the cap.
fn backoff_for(consecutive: u32) -> Duration {
    let exponent = consecutive.saturating_sub(1).min(6);
    BACKOFF_FLOOR
        .saturating_mul(1u32 << exponent)
        .min(BACKOFF_CAP)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast_ref::<&str>() {
        Some(s) => (*s).to_owned(),
        None => match payload.downcast_ref::<String>() {
            Some(s) => s.clone(),
            None => "non-string panic payload".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    fn no_escalation() -> EscalationHook {
        Arc::new(|task, action| {
            panic!("unexpected escalation for {task:?}: {action:?}");
        })
    }

    fn ladder(warn_at: u32, act_at: u32) -> Ladder {
        Ladder {
            warn_at,
            act_at,
            action: LadderAction::WatchdogTrip,
        }
    }

    fn supervisor(cap: u32, escalate: EscalationHook, shutdown: CancellationToken) -> Supervisor {
        Supervisor::new(cap, WINDOW, ladder(1, 2), escalate, shutdown)
    }

    #[tokio::test(start_paused = true)]
    async fn panicked_task_is_restarted_until_it_settles() {
        let sup = supervisor(100, no_escalation(), CancellationToken::new());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        sup.spawn(TaskName::WatcherPump, move || {
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    panic!("boom {attempt}");
                }
                std::future::pending::<()>().await
            }
        });

        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "two panics must produce exactly two restarts, then the task settles",
        );
        assert_eq!(sup.restart_count(TaskName::WatcherPump), 2);
        assert_eq!(
            sup.restart_count(TaskName::Maintenance),
            0,
            "restarting WatcherPump must not count against other tasks",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restart_backoff_doubles_per_consecutive_panic() {
        let sup = supervisor(100, no_escalation(), CancellationToken::new());
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&starts);
        sup.spawn(TaskName::Maintenance, move || {
            record.lock().unwrap().push(Instant::now());
            async { panic!("always") }
        });

        // Starts at t=0 then after 1s, 2s, 4s of backoff.
        tokio::time::sleep(Duration::from_secs(8)).await;
        let starts = starts.lock().unwrap();
        let mut gaps = Vec::new();
        for w in starts.windows(2) {
            gaps.push(w[1] - w[0]);
        }
        assert_eq!(
            gaps[..3],
            [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            "backoff must start at the floor and double per consecutive panic",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_uptime_resets_the_backoff_curve() {
        let sup = supervisor(100, no_escalation(), CancellationToken::new());
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&starts);
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        sup.spawn(TaskName::CatchUp, move || {
            record.lock().unwrap().push(Instant::now());
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                match attempt {
                    // Quick panic, then a healthy run past the window, then
                    // another quick panic; the post-healthy backoff must be
                    // back at the floor.
                    0 => panic!("quick"),
                    1 => {
                        tokio::time::sleep(WINDOW + Duration::from_secs(1)).await;
                        panic!("after healthy uptime");
                    }
                    2 => panic!("quick again"),
                    _ => std::future::pending::<()>().await,
                }
            }
        });

        tokio::time::sleep(Duration::from_secs(120)).await;
        let starts = starts.lock().unwrap();
        assert_eq!(starts.len(), 4, "expected exactly three restarts");
        assert_eq!(
            starts[2] - starts[1],
            WINDOW + Duration::from_secs(1) + BACKOFF_FLOOR,
            "a panic after healthy uptime must back off at the floor again \
             (without the reset this gap would be a 2s second-step backoff)",
        );
        assert_eq!(
            starts[3] - starts[2],
            2 * BACKOFF_FLOOR,
            "the quick panic after the reset resumes the doubling curve",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn normal_exit_is_not_restarted() {
        let sup = supervisor(100, no_escalation(), CancellationToken::new());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let monitor = sup.spawn(TaskName::IdleExit, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async {}
        });

        monitor.await.expect("monitor must not panic");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(sup.restart_count(TaskName::IdleExit), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_the_task_without_restart() {
        let shutdown = CancellationToken::new();
        let sup = supervisor(100, no_escalation(), shutdown.clone());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let monitor = sup.spawn(TaskName::Signal, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>()
        });

        tokio::time::sleep(Duration::from_secs(1)).await;
        shutdown.cancel();
        monitor
            .await
            .expect("monitor must exit cleanly on shutdown");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(sup.restart_count(TaskName::Signal), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_backoff_cancels_the_restart() {
        let shutdown = CancellationToken::new();
        let sup = supervisor(100, no_escalation(), shutdown.clone());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let monitor = sup.spawn(TaskName::WatcherPump, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { panic!("boom") }
        });

        // Cancel while the monitor is inside the first 1s backoff sleep.
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.cancel();
        monitor
            .await
            .expect("monitor must exit cleanly on shutdown");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "no restart may happen once shutdown fired",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exceeding_the_intensity_cap_escalates_through_the_ladder() {
        let fired: Arc<Mutex<Vec<(TaskName, LadderAction)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&fired);
        let escalate: EscalationHook = Arc::new(move |task, action| {
            sink.lock().unwrap().push((task, action));
        });
        // Cap 2-in-60s; ladder warns on strike 1, acts on strike 2.
        let sup = Supervisor::new(2, WINDOW, ladder(1, 2), escalate, CancellationToken::new());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let monitor = sup.spawn(TaskName::Maintenance, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { panic!("crash loop") }
        });

        tokio::time::sleep(Duration::from_secs(300)).await;
        let fired = fired.lock().unwrap();
        assert!(
            fired.len() >= 2,
            "a persistent crash loop must keep re-firing the ladder's action \
             (one strike per cool-down), not fire it once and settle; got {}",
            fired.len(),
        );
        for entry in fired.iter() {
            assert_eq!(entry, &(TaskName::Maintenance, LadderAction::WatchdogTrip));
        }
        assert!(
            !monitor.is_finished(),
            "the supervisor must keep the monitor alive past the cap — the daemon survives",
        );
        assert!(
            attempts.load(Ordering::SeqCst) > 3,
            "restarts must continue (cool-down paced) after escalation",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restarts_exactly_at_the_cap_do_not_escalate() {
        let fired: Arc<Mutex<Vec<(TaskName, LadderAction)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&fired);
        let escalate: EscalationHook = Arc::new(move |task, action| {
            sink.lock().unwrap().push((task, action));
        });
        let sup = Supervisor::new(2, WINDOW, ladder(1, 2), escalate, CancellationToken::new());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        sup.spawn(TaskName::IdleExit, move || {
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    panic!("boom {attempt}");
                }
                std::future::pending::<()>().await
            }
        });

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(
            fired.lock().unwrap().is_empty(),
            "the cap is exclusive: exactly cap restarts in the window must not escalate",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn strike_one_warns_without_firing_the_action() {
        let fired: Arc<Mutex<Vec<(TaskName, LadderAction)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&fired);
        let escalate: EscalationHook = Arc::new(move |task, action| {
            sink.lock().unwrap().push((task, action));
        });
        // act_at 100 is unreachable in this test's horizon: only warns fire.
        let sup = Supervisor::new(
            2,
            WINDOW,
            ladder(1, 100),
            escalate,
            CancellationToken::new(),
        );
        sup.spawn(TaskName::WatcherPump, || async { panic!("crash loop") });

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            fired.lock().unwrap().is_empty(),
            "warn-level strikes must not invoke the escalation hook",
        );
    }

    #[test]
    fn backoff_curve_is_floor_doubling_capped() {
        assert_eq!(backoff_for(1), Duration::from_secs(1));
        assert_eq!(backoff_for(2), Duration::from_secs(2));
        assert_eq!(backoff_for(6), Duration::from_secs(32));
        assert_eq!(backoff_for(7), Duration::from_secs(60), "capped at 60s");
        assert_eq!(backoff_for(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn panic_message_extracts_str_and_string_payloads() {
        assert_eq!(panic_message(Box::new("static str")), "static str");
        assert_eq!(panic_message(Box::new(String::from("owned"))), "owned");
        assert_eq!(panic_message(Box::new(42_u32)), "non-string panic payload");
    }
}
