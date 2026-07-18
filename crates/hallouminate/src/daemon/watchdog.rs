//! Internal watchdog + crash-loop boot backoff (ADR daemon-rework-004/-005).
//!
//! A dedicated OS thread (not a tokio task — it must survive a frozen
//! runtime) polls per-task heartbeat epochs from `heartbeat::HeartbeatRegistry`.
//! When a monitored task makes no progress for the stall window, the watchdog
//! persists a trip timestamp to the trip-state file and fires the injected
//! trip action (production: `std::process::abort`; tests: a recorder).
//!
//! At boot, `check_boot_backoff` reads the trip-state file and computes an
//! escalating backoff floor — `boot_backoff_floor_secs` doubling per recent
//! trip up to `boot_backoff_cap_secs` (10s → 5min with defaults). Within the
//! floor the caller should exit with [`BOOT_BACKOFF_EXIT_CODE`]; the daemon
//! NEVER refuses to start permanently (every decision is `Proceed` or a
//! bounded wait ≤ the cap), and trips older than the decay window reset the
//! escalation. Wiring the thread into server startup and the boot check into
//! the entrypoint is W1's job; this module is the complete self-contained unit.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::heartbeat::{HeartbeatRegistry, TaskName, TaskStatus, compare_epochs};

/// Distinct exit code for "boot refused: within the crash-loop backoff
/// floor". `EX_TEMPFAIL` from sysexits.h — a temporary failure the caller
/// should retry later — which is exactly the contract: never permanent.
#[allow(dead_code)]
pub(crate) const BOOT_BACKOFF_EXIT_CODE: i32 = 75;

/// Trips older than this are forgotten (escalation reset). Must comfortably
/// exceed one full trip cycle — stall detection (default 300s) plus the
/// capped backoff (300s) — so a persistent wedge keeps escalating across
/// cycles instead of resetting; one quiet hour wipes the slate clean.
const TRIP_DECAY_SECS: u64 = 3600;

/// Upper bound on stored trip timestamps, so the file stays tiny even under
/// clock weirdness. Far above the 6 trips needed to reach the backoff cap.
const MAX_STORED_TRIPS: usize = 32;

/// Default trip-state file location: sibling of the daemon socket, so it
/// follows the same per-user runtime-dir conventions (`HALLOUMINATE_SOCKET`
/// override, `$XDG_RUNTIME_DIR/hallouminate/`, or `~/.cache/hallouminate/`).
#[allow(dead_code)]
pub(crate) fn default_trip_state_path() -> PathBuf {
    super::socket::daemon_socket_path().with_file_name("watchdog-trips")
}

/// Read the trip timestamps still inside the decay window, oldest first.
/// Corruption-tolerant by design (a missing, unreadable, or garbled file
/// must never block boot): unparseable lines are skipped, and timestamps in
/// the future are dropped — a future trip is evidence of a wall-clock
/// discontinuity, and clamping it instead would re-anchor to `now` on every
/// boot, imposing the cap for as long as the skew lasts (a de-facto
/// permanent refusal). Dropping errs toward starting: worst case the daemon
/// is still wedged, trips again, and re-records with the current clock.
///
/// File format: newline-delimited unix-epoch seconds, one trip per line —
/// a torn write loses at most one line, never the whole history.
fn read_recent_trips(path: &Path, now_unix: u64) -> Vec<u64> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let cutoff = now_unix.saturating_sub(TRIP_DECAY_SECS);
    let mut trips = Vec::new();
    for line in contents.lines() {
        let Ok(at) = line.trim().parse::<u64>() else {
            continue;
        };
        if at >= cutoff && at <= now_unix {
            trips.push(at);
        }
    }
    trips.sort_unstable();
    if trips.len() > MAX_STORED_TRIPS {
        trips.drain(..trips.len() - MAX_STORED_TRIPS);
    }
    trips
}

/// Append a trip at `now_unix`, pruning decayed entries, and atomically
/// rewrite the file (temp + rename) so a crash mid-write cannot corrupt it.
fn record_trip(path: &Path, now_unix: u64) -> io::Result<()> {
    let mut trips = read_recent_trips(path, now_unix);
    trips.push(now_unix);
    if trips.len() > MAX_STORED_TRIPS {
        trips.drain(..trips.len() - MAX_STORED_TRIPS);
    }
    let mut body = String::new();
    for at in &trips {
        body.push_str(&at.to_string());
        body.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Append `.tmp` rather than `with_extension`, which would *replace* an
    // existing extension on a caller-supplied dotted filename and could
    // collide with an unrelated sibling file.
    let mut tmp_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("watchdog-trips"))
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Trip-state summary for StatusReport (curd 9) and the boot check:
/// how many trips are inside the decay window, when the last one was, and
/// the backoff floor they currently impose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct TripSnapshot {
    pub(crate) recent_trips: u32,
    pub(crate) last_trip_unix: Option<u64>,
    pub(crate) backoff_secs: u64,
}

/// Snapshot the trip-state file at `now_unix` under the configured backoff
/// floor/cap (`boot_backoff_floor_secs` / `boot_backoff_cap_secs`).
#[allow(dead_code)]
pub(crate) fn trip_snapshot(
    path: &Path,
    floor_secs: u64,
    cap_secs: u64,
    now_unix: u64,
) -> TripSnapshot {
    let trips = read_recent_trips(path, now_unix);
    let recent_trips = u32::try_from(trips.len()).unwrap_or(u32::MAX);
    TripSnapshot {
        recent_trips,
        last_trip_unix: trips.last().copied(),
        backoff_secs: super::backoff::exponential_backoff_secs(floor_secs, cap_secs, recent_trips),
    }
}

/// What boot should do given the persisted trip state. Never a permanent
/// refusal: `Backoff::retry_after_secs` is always ≤ the configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum BootDecision {
    Proceed,
    /// Within the backoff floor — the caller should exit with
    /// [`BOOT_BACKOFF_EXIT_CODE`] and let the next spawn retry.
    Backoff {
        retry_after_secs: u64,
        backoff_secs: u64,
        recent_trips: u32,
    },
}

/// Boot-time crash-loop check: given the trip-state file and the configured
/// floor/cap, decide whether this start is inside the escalating backoff
/// floor measured from the most recent trip.
#[allow(dead_code)]
pub(crate) fn check_boot_backoff(
    path: &Path,
    floor_secs: u64,
    cap_secs: u64,
    now_unix: u64,
) -> BootDecision {
    let snapshot = trip_snapshot(path, floor_secs, cap_secs, now_unix);
    let Some(last) = snapshot.last_trip_unix else {
        return BootDecision::Proceed;
    };
    let ready_at = last.saturating_add(snapshot.backoff_secs);
    if now_unix >= ready_at {
        return BootDecision::Proceed;
    }
    BootDecision::Backoff {
        retry_after_secs: ready_at - now_unix,
        backoff_secs: snapshot.backoff_secs,
        recent_trips: snapshot.recent_trips,
    }
}

struct TaskWatch {
    task: TaskName,
    last_epoch: u64,
    last_progress: Instant,
}

/// Pure stall-detection core: tracks, per monitored task, the last observed
/// epoch and when it last advanced. Separated from the thread so tests can
/// drive it with synthetic instants.
struct StallTracker {
    watches: Vec<TaskWatch>,
    stall: Duration,
}

impl StallTracker {
    fn new(
        registry: &HeartbeatRegistry,
        tasks: &[TaskName],
        stall: Duration,
        now: Instant,
    ) -> Self {
        let mut watches = Vec::with_capacity(tasks.len());
        for &task in tasks {
            watches.push(TaskWatch {
                task,
                last_epoch: registry.epoch(task),
                last_progress: now,
            });
        }
        Self { watches, stall }
    }

    /// One watchdog poll: refresh each watch from the registry and return
    /// the tasks whose epoch has not advanced for at least the stall window.
    fn stalled_tasks(&mut self, registry: &HeartbeatRegistry, now: Instant) -> Vec<TaskName> {
        let mut stalled = Vec::new();
        for watch in &mut self.watches {
            let current = registry.epoch(watch.task);
            match compare_epochs(watch.last_epoch, current) {
                TaskStatus::Alive => {
                    watch.last_epoch = current;
                    watch.last_progress = now;
                }
                TaskStatus::Stalled => {
                    if now.saturating_duration_since(watch.last_progress) >= self.stall {
                        stalled.push(watch.task);
                    }
                }
            }
        }
        stalled
    }
}

/// Trip action fired after the trip is persisted (production:
/// `Box::new(|_| std::process::abort())`; tests inject a recorder).
#[allow(dead_code)]
pub(crate) type TripAction = Box<dyn FnOnce(&[TaskName]) + Send>;

/// Handle to the running watchdog thread. Dropping the handle detaches the
/// thread; `stop` shuts it down promptly and joins it.
#[allow(dead_code)]
pub(crate) struct Watchdog {
    stop_tx: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl Watchdog {
    /// Spawn the watchdog on a dedicated OS thread. It polls `registry`
    /// every `poll`, and when any of `tasks` makes no progress for `stall`
    /// (config `watchdog_stall_secs`), it persists a trip to `trip_file`
    /// and then fires `on_trip` (production: `|_| std::process::abort()`).
    /// Trip persistence is ordered strictly before `on_trip` so the abort
    /// can never race the record; a persistence failure is logged and the
    /// trip still fires — a wedged daemon must die even if its breadcrumb
    /// cannot be written. One trip per watchdog lifetime: after firing,
    /// the thread exits (production never returns from the abort anyway).
    #[allow(dead_code)]
    pub(crate) fn spawn(
        registry: Arc<HeartbeatRegistry>,
        tasks: Vec<TaskName>,
        stall: Duration,
        poll: Duration,
        trip_file: PathBuf,
        on_trip: TripAction,
    ) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("hallouminate-watchdog".into())
            .spawn(move || {
                run(
                    &registry, &tasks, stall, poll, &trip_file, on_trip, &stop_rx,
                )
            })
            .expect("failed to spawn watchdog thread");
        Self { stop_tx, handle }
    }

    /// Shut the watchdog down and join it. Prompt: the thread's poll sleep
    /// doubles as the shutdown listener, so stop never waits a full poll.
    #[allow(dead_code)]
    pub(crate) fn stop(self) {
        drop(self.stop_tx);
        let _ = self.handle.join();
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    registry: &HeartbeatRegistry,
    tasks: &[TaskName],
    stall: Duration,
    poll: Duration,
    trip_file: &Path,
    on_trip: TripAction,
    stop_rx: &mpsc::Receiver<()>,
) {
    let mut tracker = StallTracker::new(registry, tasks, stall, Instant::now());
    loop {
        match stop_rx.recv_timeout(poll) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let stalled = tracker.stalled_tasks(registry, Instant::now());
        if stalled.is_empty() {
            continue;
        }
        let now_unix = unix_now_secs();
        match record_trip(trip_file, now_unix) {
            Ok(()) => tracing::error!(
                target: "hallouminate::daemon",
                stalled = ?stalled,
                stall_secs = stall.as_secs(),
                trip_file = %trip_file.display(),
                "watchdog trip: task heartbeat stalled; recorded trip, aborting",
            ),
            Err(e) => tracing::error!(
                target: "hallouminate::daemon",
                stalled = ?stalled,
                error = %e,
                trip_file = %trip_file.display(),
                "watchdog trip: failed to persist trip state; aborting anyway",
            ),
        }
        on_trip(&stalled);
        return;
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    const FLOOR: u64 = 10;
    const CAP: u64 = 300;

    fn trip_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("watchdog-trips")
    }

    fn write_trips(path: &Path, trips: &[u64]) {
        let mut body = String::new();
        for t in trips {
            body.push_str(&t.to_string());
            body.push('\n');
        }
        std::fs::write(path, body).expect("write trip fixture");
    }

    // The backoff curve itself (floor doubling to cap, zero-case, overflow
    // saturation) is tested in `super::backoff`; these tests exercise how the
    // watchdog *applies* it via `trip_snapshot` / `check_boot_backoff`.

    #[test]
    fn record_trip_creates_file_and_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/runtime/watchdog-trips");
        record_trip(&path, 1_000).expect("record into missing dirs");
        assert_eq!(read_recent_trips(&path, 1_000), vec![1_000]);
        assert!(
            !path.with_extension("tmp").exists(),
            "atomic-write temp file must not linger after rename",
        );
    }

    #[test]
    fn record_trip_appends_and_prunes_decayed_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let now = 100_000;
        let decayed = now - TRIP_DECAY_SECS - 1;
        let recent = now - 60;
        write_trips(&path, &[decayed, recent]);

        record_trip(&path, now).unwrap();

        assert_eq!(
            read_recent_trips(&path, now),
            vec![recent, now],
            "decayed trip must be dropped, recent kept, new appended",
        );
    }

    #[test]
    fn record_trip_caps_stored_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let now = 100_000;
        let mut many: Vec<u64> = Vec::new();
        for i in 0..100 {
            many.push(now - 100 + i);
        }
        write_trips(&path, &many);

        record_trip(&path, now).unwrap();

        let stored = read_recent_trips(&path, now);
        assert_eq!(stored.len(), MAX_STORED_TRIPS);
        assert_eq!(
            *stored.last().unwrap(),
            now,
            "newest trip must survive the cap"
        );
    }

    #[test]
    fn read_recent_trips_returns_empty_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_recent_trips(&trip_file(&dir), 1_000).is_empty());
    }

    #[test]
    fn read_recent_trips_skips_corrupt_lines_and_keeps_valid_ones() {
        // A garbled file must never block boot: bad lines are skipped, the
        // parseable remainder still counts.
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        std::fs::write(&path, "garbage\n950\n\nnot-a-number\n960\n").unwrap();
        assert_eq!(read_recent_trips(&path, 1_000), vec![950, 960]);
    }

    #[test]
    fn read_recent_trips_drops_future_timestamps() {
        // A wall-clock jump backwards leaves future trips on disk. They must
        // be dropped, not clamped: clamping re-anchors to `now` on every
        // boot and would refuse for as long as the skew lasts.
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        write_trips(&path, &[5_000_000, 990]);
        assert_eq!(read_recent_trips(&path, 1_000), vec![990]);
    }

    #[test]
    fn boot_proceeds_with_no_trip_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_boot_backoff(&trip_file(&dir), FLOOR, CAP, 1_000),
            BootDecision::Proceed,
        );
    }

    #[test]
    fn boot_backs_off_within_floor_after_first_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        write_trips(&path, &[1_000]);
        assert_eq!(
            check_boot_backoff(&path, FLOOR, CAP, 1_005),
            BootDecision::Backoff {
                retry_after_secs: 5,
                backoff_secs: 10,
                recent_trips: 1,
            },
        );
    }

    #[test]
    fn boot_proceeds_once_backoff_elapsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        write_trips(&path, &[1_000]);
        assert_eq!(
            check_boot_backoff(&path, FLOOR, CAP, 1_010),
            BootDecision::Proceed,
            "at exactly floor seconds after the trip, boot must proceed",
        );
    }

    #[test]
    fn boot_backoff_escalates_with_repeated_trips() {
        // Acceptance criterion: N trips within the window ⇒ escalating
        // floor (third trip ⇒ 40s, measured from the latest trip).
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        write_trips(&path, &[900, 950, 1_000]);
        assert_eq!(
            check_boot_backoff(&path, FLOOR, CAP, 1_010),
            BootDecision::Backoff {
                retry_after_secs: 30,
                backoff_secs: 40,
                recent_trips: 3,
            },
        );
    }

    #[test]
    fn boot_backoff_caps_at_five_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let mut trips: Vec<u64> = Vec::new();
        for i in 0..10 {
            trips.push(900 + i * 10);
        }
        write_trips(&path, &trips);
        let BootDecision::Backoff {
            retry_after_secs,
            backoff_secs,
            ..
        } = check_boot_backoff(&path, FLOOR, CAP, 1_000)
        else {
            panic!("ten fresh trips must impose a backoff");
        };
        assert_eq!(backoff_secs, CAP);
        assert!(retry_after_secs <= CAP);
    }

    #[test]
    fn boot_never_refuses_permanently() {
        // The acceptance criterion's hard clause: whatever is on disk —
        // dense trip history, future timestamps, garbage — the wait is
        // bounded by the cap, so waiting out the cap always yields Proceed.
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let now = 1_000_000;
        let mut on_disk: Vec<u64> = Vec::new();
        for i in 0..200 {
            on_disk.push(now - i);
        }
        on_disk.push(now + 1_000_000); // future timestamp
        write_trips(&path, &on_disk);

        match check_boot_backoff(&path, FLOOR, CAP, now) {
            BootDecision::Proceed => {}
            BootDecision::Backoff {
                retry_after_secs, ..
            } => assert!(
                retry_after_secs <= CAP,
                "wait must never exceed the cap, got {retry_after_secs}",
            ),
        }
        assert_eq!(
            check_boot_backoff(&path, FLOOR, CAP, now + CAP),
            BootDecision::Proceed,
            "one full cap after the newest trip, boot must always proceed",
        );
    }

    #[test]
    fn boot_escalation_resets_after_quiet_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let now = 100_000;
        write_trips(
            &path,
            &[now - TRIP_DECAY_SECS - 10, now - TRIP_DECAY_SECS - 5],
        );
        assert_eq!(
            check_boot_backoff(&path, FLOOR, CAP, now),
            BootDecision::Proceed,
            "trips outside the decay window must not impose a floor",
        );
        let snapshot = trip_snapshot(&path, FLOOR, CAP, now);
        assert_eq!(snapshot.recent_trips, 0);
        assert_eq!(snapshot.backoff_secs, 0);
        assert_eq!(snapshot.last_trip_unix, None);
    }

    #[test]
    fn escalation_lifecycle_doubles_per_crash_cycle_to_cap_then_recovers() {
        // The acceptance criterion end to end: a persistently wedged daemon
        // that trips, waits out the floor, restarts, and trips again must
        // see the floor escalate 10→20→40→80→160→300 and hold at 300 —
        // and a quiet decay window later, boot proceeds with a clean slate.
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let mut now = 1_000_000;
        let mut seen = Vec::new();
        for _ in 0..7 {
            assert_eq!(
                check_boot_backoff(&path, FLOOR, CAP, now),
                BootDecision::Proceed,
                "each cycle starts after the previous floor elapsed",
            );
            record_trip(&path, now).unwrap();
            let BootDecision::Backoff { backoff_secs, .. } =
                check_boot_backoff(&path, FLOOR, CAP, now)
            else {
                panic!("a fresh trip must impose a floor");
            };
            seen.push(backoff_secs);
            now += backoff_secs; // respawn exactly when the floor expires
        }
        assert_eq!(seen, vec![10, 20, 40, 80, 160, 300, 300]);

        now += TRIP_DECAY_SECS;
        assert_eq!(
            trip_snapshot(&path, FLOOR, CAP, now).recent_trips,
            0,
            "a full quiet decay window resets the escalation",
        );
    }

    #[test]
    fn trip_snapshot_reports_count_latest_and_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        write_trips(&path, &[900, 1_000]);
        assert_eq!(
            trip_snapshot(&path, FLOOR, CAP, 1_050),
            TripSnapshot {
                recent_trips: 2,
                last_trip_unix: Some(1_000),
                backoff_secs: 20,
            },
        );
    }

    #[test]
    fn tracker_reports_no_stall_while_epochs_advance() {
        let registry = HeartbeatRegistry::default();
        let stall = Duration::from_secs(300);
        let start = Instant::now();
        let mut tracker = StallTracker::new(&registry, &[TaskName::Maintenance], stall, start);

        registry.bump(TaskName::Maintenance);
        assert!(
            tracker.stalled_tasks(&registry, start + stall).is_empty(),
            "a task that bumped since the last poll is alive even past the window",
        );
    }

    #[test]
    fn tracker_reports_stall_only_after_full_window() {
        let registry = HeartbeatRegistry::default();
        let stall = Duration::from_secs(300);
        let start = Instant::now();
        let mut tracker = StallTracker::new(&registry, &[TaskName::Maintenance], stall, start);

        assert!(
            tracker
                .stalled_tasks(&registry, start + stall - Duration::from_secs(1))
                .is_empty(),
            "no trip before the stall window elapses",
        );
        assert_eq!(
            tracker.stalled_tasks(&registry, start + stall),
            vec![TaskName::Maintenance],
        );
    }

    #[test]
    fn tracker_progress_resets_the_stall_clock() {
        let registry = HeartbeatRegistry::default();
        let stall = Duration::from_secs(300);
        let start = Instant::now();
        let mut tracker = StallTracker::new(&registry, &[TaskName::Maintenance], stall, start);

        registry.bump(TaskName::Maintenance);
        let mid = start + Duration::from_secs(200);
        assert!(tracker.stalled_tasks(&registry, mid).is_empty());
        assert!(
            tracker
                .stalled_tasks(&registry, mid + stall - Duration::from_secs(1))
                .is_empty(),
            "stall clock restarts from the last observed progress",
        );
        assert_eq!(
            tracker.stalled_tasks(&registry, mid + stall),
            vec![TaskName::Maintenance],
        );
    }

    #[test]
    fn tracker_reports_all_simultaneously_stalled_tasks() {
        let registry = HeartbeatRegistry::default();
        let stall = Duration::from_secs(300);
        let start = Instant::now();
        let mut tracker = StallTracker::new(
            &registry,
            &[TaskName::Maintenance, TaskName::WatcherPump],
            stall,
            start,
        );
        assert_eq!(
            tracker.stalled_tasks(&registry, start + stall),
            vec![TaskName::Maintenance, TaskName::WatcherPump],
        );
    }

    #[test]
    fn tracker_watches_only_requested_tasks() {
        let registry = HeartbeatRegistry::default();
        let stall = Duration::from_secs(300);
        let start = Instant::now();
        let mut tracker = StallTracker::new(&registry, &[TaskName::WatcherPump], stall, start);

        registry.bump(TaskName::WatcherPump);
        // Maintenance never bumps, but it is not monitored.
        assert!(
            tracker.stalled_tasks(&registry, start + stall).is_empty(),
            "an unmonitored frozen task must not trip the watchdog",
        );
    }

    #[test]
    fn watchdog_thread_persists_trip_before_firing_abort_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let registry = Arc::new(HeartbeatRegistry::default());
        let (tx, rx) = sync_channel::<(Vec<TaskName>, Vec<u64>)>(1);

        let on_trip_path = path.clone();
        let watchdog = Watchdog::spawn(
            Arc::clone(&registry),
            vec![TaskName::Maintenance],
            Duration::from_millis(50),
            Duration::from_millis(10),
            path.clone(),
            Box::new(move |stalled| {
                // Read the file inside the hook: proves persistence is
                // ordered before the (would-be) abort.
                let persisted = read_recent_trips(&on_trip_path, unix_now_secs());
                tx.send((stalled.to_vec(), persisted)).unwrap();
            }),
        );

        let (stalled, persisted) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watchdog must trip within the timeout");
        assert_eq!(stalled, vec![TaskName::Maintenance]);
        assert_eq!(
            persisted.len(),
            1,
            "exactly one trip must be on disk before the abort hook fires",
        );
        watchdog.stop();
    }

    #[test]
    fn watchdog_thread_fires_trip_even_when_persistence_fails() {
        // A wedged daemon must die even if its breadcrumb cannot be
        // written: point the trip file below a regular file so
        // create_dir_all fails, and require the abort hook anyway.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let registry = Arc::new(HeartbeatRegistry::default());
        let (tx, rx) = sync_channel::<Vec<TaskName>>(1);

        let watchdog = Watchdog::spawn(
            Arc::clone(&registry),
            vec![TaskName::Maintenance],
            Duration::from_millis(50),
            Duration::from_millis(10),
            blocker.join("unwritable/watchdog-trips"),
            Box::new(move |stalled| {
                tx.send(stalled.to_vec()).unwrap();
            }),
        );

        let stalled = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("trip must fire despite the persistence failure");
        assert_eq!(stalled, vec![TaskName::Maintenance]);
        watchdog.stop();
    }

    #[test]
    fn default_trip_state_path_is_sibling_of_the_daemon_socket() {
        let path = default_trip_state_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("watchdog-trips"),
        );
        assert_eq!(
            path.parent(),
            super::super::socket::daemon_socket_path().parent(),
            "trip state must live in the same runtime dir as the socket",
        );
    }

    #[test]
    fn watchdog_thread_stays_quiet_and_stops_promptly_while_tasks_are_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = trip_file(&dir);
        let registry = Arc::new(HeartbeatRegistry::default());
        let (tx, rx) = sync_channel::<Vec<TaskName>>(1);

        let watchdog = Watchdog::spawn(
            Arc::clone(&registry),
            vec![TaskName::Maintenance],
            Duration::from_secs(3600),
            Duration::from_millis(10),
            path.clone(),
            Box::new(move |stalled| {
                tx.send(stalled.to_vec()).unwrap();
            }),
        );

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            rx.try_recv().is_err(),
            "no trip may fire inside the stall window",
        );

        let stop_started = Instant::now();
        watchdog.stop();
        assert!(
            stop_started.elapsed() < Duration::from_secs(2),
            "stop must not wait out the stall window",
        );
        assert!(!path.exists(), "no trip file may be written without a trip");
    }
}
