//! Daemon auto-spawn for the MCP `serve` command.
//!
//! `ground` / `index` / etc. deliberately fail loudly when the daemon is
//! unreachable (see `lib.rs`) — interactive users get a clear "start the
//! daemon" hint. The MCP transport (`hallouminate serve`) is non-interactive:
//! the caller is Claude Code or another MCP host, and the user does not see
//! daemon-bootstrap errors. So `serve` auto-spawns a daemon on first launch.
//!
//! The flock in `acquire_single_instance` makes this safe under concurrent
//! `serve` launches: only one spawned daemon wins the lock; the others exit
//! cleanly and every `serve` polls the same socket.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

use super::client::connect_at;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse};
use super::socket::daemon_socket_path;

const CONNECTION_BUDGET: Duration = Duration::from_secs(90);
const INITIAL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound on the version probe's `Ping` round-trip. `call_raw` has no
/// built-in timeout, so a daemon that accepts the connection but never replies
/// would otherwise hang `ensure_daemon_running` at MCP startup. Cap it: an
/// elapsed probe is treated as unverifiable (→ restart), the same fate as a
/// transport error or a legacy bare-`"pong"` reply.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Size cap for `daemon-bootstrap.log`. The file is append-only across every
/// auto-spawn and failed start, so with no cap repeated restarts (or a daemon
/// that fails to come up in a crash loop) grow it forever. It is a startup
/// diagnostic, not an audit trail, so a simple truncate-at-startup is enough.
const MAX_BOOTSTRAP_LOG_BYTES: u64 = 1024 * 1024;

/// Ensure a daemon is reachable, spawning a detached one if not.
///
/// No-ops when `HALLOUMINATE_SOCKET` is set: the explicit-socket convention
/// means the caller (tests, custom launchers) manages daemon lifecycle.
pub async fn ensure_daemon_running() -> anyhow::Result<()> {
    if has_explicit_socket_override(std::env::var_os("HALLOUMINATE_SOCKET").as_deref()) {
        return Ok(());
    }

    let socket = daemon_socket_path()?;
    let socket = socket.as_path();
    if let Some((verdict, theirs)) = probe_running_daemon(socket).await {
        let theirs = theirs.as_deref().unwrap_or("unreported");
        match verdict {
            SkewVerdict::Adopt => return Ok(()),
            SkewVerdict::Newer => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    socket = %socket.display(),
                    ours = super::ipc::BUILD_IDENTITY,
                    theirs,
                    "running daemon is newer than this build in the same channel; adopting it \
                     (run `hallouminate daemon restart` to replace it with this build)",
                );
                return Ok(());
            }
            SkewVerdict::Restart => {
                tracing::info!(
                    target: "hallouminate::daemon",
                    socket = %socket.display(),
                    ours = super::ipc::BUILD_IDENTITY,
                    theirs,
                    "running daemon is older, from another channel, or unverifiable; restarting it",
                );
                super::lifecycle::stop_at(socket).await?;
            }
        }
    }

    // Stderr capture for the auto-spawned daemon. Lives in the XDG state
    // dir alongside the rotating tracing log; the bootstrap log catches
    // anything emitted before the subscriber installs (panics, early
    // config errors) and is the fallback diagnostic when the daemon
    // refuses to come up.
    let log_path = hallouminate_config::xdg::xdg_path(
        "XDG_STATE_HOME",
        "~/.local/state",
        &["hallouminate", "daemon-bootstrap.log"],
    );
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    truncate_log_if_oversized(&log_path, MAX_BOOTSTRAP_LOG_BYTES)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe()?;
    Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()?;

    let started = std::time::Instant::now();
    let socket_for_connect = socket.to_path_buf();
    wait_for_daemon_socket(
        socket,
        &log_path,
        CONNECTION_BUDGET,
        move || {
            let socket = socket_for_connect.clone();
            async move { UnixStream::connect(socket).await.is_ok() }
        },
        tokio::time::sleep,
        || started.elapsed(),
    )
    .await
}

async fn probe_running_daemon(socket: &Path) -> Option<(SkewVerdict, Option<String>)> {
    connect_at(socket).await.ok()?;
    Some(running_daemon_verdict(socket).await)
}

async fn wait_for_daemon_socket<Connect, ConnectFuture, Sleep, SleepFuture, Elapsed>(
    socket: &Path,
    log_path: &Path,
    connection_budget: Duration,
    mut connect: Connect,
    mut sleep: Sleep,
    mut elapsed: Elapsed,
) -> anyhow::Result<()>
where
    Connect: FnMut() -> ConnectFuture,
    ConnectFuture: std::future::Future<Output = bool>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: std::future::Future<Output = ()>,
    Elapsed: FnMut() -> Duration,
{
    let mut poll_interval = INITIAL_POLL_INTERVAL;
    loop {
        if connect().await {
            return Ok(());
        }

        let remaining = connection_budget.saturating_sub(elapsed());
        if remaining.is_zero() {
            break;
        }

        sleep(poll_interval.min(remaining)).await;
        poll_interval = poll_interval.saturating_mul(2).min(MAX_POLL_INTERVAL);
    }

    anyhow::bail!(
        "daemon socket {} did not become reachable within the {}s total connection budget; see bootstrap log {}",
        socket.display(),
        connection_budget.as_secs(),
        log_path.display(),
    )
}

/// Pure predicate split out so tests can exercise both branches without
/// mutating process env (unsafe on edition 2024).
fn has_explicit_socket_override(env_value: Option<&std::ffi::OsStr>) -> bool {
    env_value.is_some_and(|v| !v.is_empty())
}

/// Caps `daemon-bootstrap.log` at `max_bytes` before the next append. Called
/// once per `ensure_daemon_running` invocation, right before the log is
/// (re)opened in append mode, so a log that grew past the cap on a prior
/// spawn (or crash loop) is reset to empty instead of appended to forever.
/// A missing file (first run) is not an error.
fn truncate_log_if_oversized(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => std::fs::File::create(path).map(|_| ()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Probe the already-running daemon's build via `Ping` and judge it against
/// our [`BUILD_IDENTITY`](super::ipc::BUILD_IDENTITY).
///
/// Tolerant by design (see the [`PongResult`](super::ipc::PongResult) wire-compat
/// note): a daemon from a release before the version field answers the bare
/// `"pong"` string — which has no `version` — and any transport or daemon-side
/// error is likewise unverifiable. All of these resolve to [`SkewVerdict::Restart`]
/// rather than erroring, so an unverifiable daemon is replaced, never adopted.
async fn running_daemon_verdict(socket: &Path) -> (SkewVerdict, Option<String>) {
    let client = match connect_at(socket).await {
        Ok(c) => c,
        Err(e) => {
            // Don't swallow the probe failure silently on this non-interactive
            // bootstrap path: log it so an alive-but-unprobeable daemon (which
            // we then treat as skew and restart) is diagnosable.
            tracing::debug!(
                target: "hallouminate::daemon",
                error = %e,
                "version probe could not connect to the running daemon; treating as unverifiable",
            );
            return (SkewVerdict::Restart, None);
        }
    };
    let probe = client.call_raw(DaemonRequest::new(
        std::env::current_dir().unwrap_or_default(),
        DaemonRequestPayload::Ping,
    ));
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(resp)) => (
            skew_verdict(super::ipc::BUILD_IDENTITY, &resp),
            reported_identity(&resp).map(str::to_owned),
        ),
        Ok(Err(e)) => {
            tracing::debug!(
                target: "hallouminate::daemon",
                error = %e,
                "version probe Ping failed; treating the daemon as unverifiable",
            );
            (SkewVerdict::Restart, None)
        }
        Err(_elapsed) => {
            // The daemon accepted the connection but did not answer within
            // `PROBE_TIMEOUT`. A wedged daemon must not hang startup — treat
            // the silence as unverifiable so the caller restarts it.
            tracing::debug!(
                target: "hallouminate::daemon",
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "version probe Ping timed out; treating the daemon as unverifiable",
            );
            (SkewVerdict::Restart, None)
        }
    }
}

/// The build identity a `Ping` reply reports, when the reply carries one.
fn reported_identity(resp: &DaemonResponse) -> Option<&str> {
    let DaemonResponse::Ok { result } = resp else {
        return None;
    };
    result.get("version").and_then(|v| v.as_str())
}

/// What `serve` does with an already-running daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkewVerdict {
    /// Same build: keep the daemon.
    Adopt,
    /// Older, other-channel, or unverifiable daemon: replace it.
    Restart,
    /// Newer daemon in the same channel: keep it.
    Newer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Nightly,
    Release,
}

/// Parse an identity as semver and classify its channel. Nightlies are
/// `0.0.0-experimental.*`; every other parseable identity is a release.
fn classify_identity(identity: &str) -> Option<(Channel, semver::Version)> {
    let version = semver::Version::parse(identity).ok()?;
    let is_nightly = version.major == 0
        && version.minor == 0
        && version.patch == 0
        && version.pre.as_str().split('.').next() == Some("experimental");
    let channel = if is_nightly {
        Channel::Nightly
    } else {
        Channel::Release
    };
    Some((channel, version))
}

/// Pure skew verdict for a `Ping` response against `ours`. Order only
/// within one channel; across channels or on a parse failure, use exact
/// match. A non-`Ok` envelope or a missing `version` (the old bare-`"pong"`
/// daemon) yields [`SkewVerdict::Restart`].
fn skew_verdict(ours: &str, resp: &DaemonResponse) -> SkewVerdict {
    let DaemonResponse::Ok { result } = resp else {
        return SkewVerdict::Restart;
    };
    let Some(theirs) = result.get("version").and_then(|v| v.as_str()) else {
        return SkewVerdict::Restart;
    };
    let exact = if theirs == ours {
        SkewVerdict::Adopt
    } else {
        SkewVerdict::Restart
    };
    let (Some((our_channel, our_version)), Some((their_channel, their_version))) =
        (classify_identity(ours), classify_identity(theirs))
    else {
        return exact;
    };
    if our_channel != their_channel {
        return exact;
    }
    match their_version.cmp(&our_version) {
        std::cmp::Ordering::Less => SkewVerdict::Restart,
        std::cmp::Ordering::Equal => SkewVerdict::Adopt,
        std::cmp::Ordering::Greater => SkewVerdict::Newer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_socket_env_does_not_bypass_spawn() {
        assert!(!has_explicit_socket_override(None));
    }

    #[test]
    fn empty_socket_env_does_not_bypass_spawn() {
        // POSIX/XDG convention: empty env value treated as unset.
        assert!(!has_explicit_socket_override(Some(std::ffi::OsStr::new(
            ""
        ))));
    }

    #[test]
    fn set_socket_env_bypasses_spawn() {
        // Test harnesses set HALLOUMINATE_SOCKET to per-test sockets and
        // manage their own daemon. ensure_daemon_running must no-op there
        // so a stray hallouminate daemon doesn't get spawned during tests.
        assert!(has_explicit_socket_override(Some(std::ffi::OsStr::new(
            "/tmp/test.sock"
        ))));
    }

    #[tokio::test]
    async fn probe_running_daemon_reports_version_of_live_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                if line.is_empty() {
                    continue;
                }
                let response = DaemonResponse::ok(&super::super::ipc::PongResult {
                    version: super::super::ipc::BUILD_IDENTITY.to_string(),
                });
                let mut response = serde_json::to_string(&response).expect("serialize response");
                response.push('\n');
                write_half
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
                break;
            }
        });

        let (verdict, theirs) = probe_running_daemon(&socket)
            .await
            .expect("live socket must be detected");

        assert_eq!(
            verdict,
            SkewVerdict::Adopt,
            "live socket reports the current version"
        );
        assert_eq!(theirs.as_deref(), Some(super::super::ipc::BUILD_IDENTITY));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn version_probe_writes_no_client_row_and_status_writes_one() {
        use super::super::ipc::{DebtLevel, PongResult, StatusReport, TripState, WatcherCounters};
        use super::super::lifecycle::status_with;
        use super::super::oplog::{
            Client, ClientCall, OpLog, OperationRecord, Outcome, RecordKind,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let log_dir = dir.path().join("state");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            let responses = [
                DaemonResponse::ok(&PongResult {
                    version: super::super::ipc::BUILD_IDENTITY.to_string(),
                }),
                DaemonResponse::ok(&StatusReport {
                    per_task: Vec::new(),
                    debt: DebtLevel::Ok,
                    defer_count: 0,
                    watcher: WatcherCounters::default(),
                    trips: TripState::None,
                    degraded_watch_roots: 0,
                }),
            ];
            for response in responses {
                loop {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = tokio::io::BufReader::new(read_half);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.expect("read request");
                    if line.is_empty() {
                        continue;
                    }
                    let mut response =
                        serde_json::to_string(&response).expect("serialize response");
                    response.push('\n');
                    write_half
                        .write_all(response.as_bytes())
                        .await
                        .expect("write response");
                    break;
                }
            }
        });
        let oplog = OpLog::in_dir(&log_dir, &hallouminate_config::LoggingConfig::default());
        let rows = |dir: &Path| -> Vec<OperationRecord> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            let mut rows = Vec::new();
            for entry in entries {
                let path = entry.expect("log dir entry").path();
                for line in std::fs::read_to_string(&path)
                    .expect("read segment")
                    .lines()
                {
                    rows.push(serde_json::from_str::<OperationRecord>(line).expect("row json"));
                }
            }
            rows
        };

        let (verdict, _) = running_daemon_verdict(&socket).await;

        assert_eq!(
            verdict,
            SkewVerdict::Adopt,
            "the probe reached the test daemon"
        );
        assert!(
            rows(&log_dir).is_empty(),
            "the bootstrap version probe must write no client_call row"
        );

        let mut call = ClientCall::start(&oplog, Client::Cli, "status", "status", None);
        status_with(&socket, Some(&mut call))
            .await
            .expect("status call");
        call.finish(Outcome::Ok);
        server.await.expect("server task");

        let [row] = <[OperationRecord; 1]>::try_from(rows(&log_dir))
            .expect("the lifecycle status command writes exactly one row");
        assert_eq!(row.op, "status");
        assert_eq!(row.outcome, Outcome::Ok);
        let RecordKind::ClientCall(fields) = row.kind else {
            panic!("status row must be a client_call row, got {:?}", row.kind);
        };
        assert_eq!(fields.client, Client::Cli);
    }

    // ── Curd C: version-skew detection ───────────────────────────────────

    #[test]
    fn pong_with_our_version_is_a_match() {
        let resp = DaemonResponse::ok(&super::super::ipc::PongResult {
            version: super::super::ipc::BUILD_IDENTITY.to_string(),
        });
        assert_eq!(
            skew_verdict(super::super::ipc::BUILD_IDENTITY, &resp),
            SkewVerdict::Adopt,
            "same version must match"
        );
    }

    #[test]
    fn pong_with_a_different_version_is_a_mismatch() {
        // A daemon from another release reports its own version; the new
        // client must NOT adopt it.
        let resp = DaemonResponse::ok(&super::super::ipc::PongResult {
            version: "0.0.0-stale".to_string(),
        });
        assert_eq!(
            skew_verdict("0.0.0-experimental.346.1", &resp),
            SkewVerdict::Restart,
            "a different version must not match"
        );
    }

    fn pong(version: &str) -> DaemonResponse {
        DaemonResponse::ok(&super::super::ipc::PongResult {
            version: version.to_string(),
        })
    }

    #[test]
    fn older_serve_does_not_restart_newer_daemon() {
        assert_eq!(
            skew_verdict(
                "0.0.0-experimental.345.1",
                &pong("0.0.0-experimental.346.1")
            ),
            SkewVerdict::Newer,
        );
    }

    #[test]
    fn newer_serve_restarts_older_daemon() {
        assert_eq!(
            skew_verdict(
                "0.0.0-experimental.346.1",
                &pong("0.0.0-experimental.345.1")
            ),
            SkewVerdict::Restart,
        );
    }

    #[test]
    fn nightly_numeric_prerelease_orders_numerically() {
        assert_eq!(
            skew_verdict(
                "0.0.0-experimental.345.2",
                &pong("0.0.0-experimental.346.1")
            ),
            SkewVerdict::Newer,
        );
        assert_eq!(
            skew_verdict("0.0.0-experimental.9.1", &pong("0.0.0-experimental.10.1")),
            SkewVerdict::Newer,
        );
    }

    #[test]
    fn nightly_serve_restarts_release_daemon() {
        assert_eq!(
            skew_verdict("0.0.0-experimental.346.1", &pong("0.11.1")),
            SkewVerdict::Restart,
        );
    }

    #[test]
    fn release_serve_restarts_nightly_daemon() {
        assert_eq!(
            skew_verdict("0.11.1", &pong("0.0.0-experimental.346.1")),
            SkewVerdict::Restart,
        );
    }

    #[test]
    fn unparseable_unequal_identities_restart_and_equal_adopt() {
        assert_eq!(
            skew_verdict("0.11.1", &pong("not-semver")),
            SkewVerdict::Restart,
        );
        assert_eq!(skew_verdict("dev", &pong("0.11.1")), SkewVerdict::Restart,);
        assert_eq!(skew_verdict("dev", &pong("dev")), SkewVerdict::Adopt,);
    }

    #[test]
    fn release_versions_order_within_the_channel() {
        assert_eq!(
            skew_verdict("0.11.1", &pong("0.11.0")),
            SkewVerdict::Restart,
        );
        assert_eq!(skew_verdict("0.11.0", &pong("0.11.1")), SkewVerdict::Newer,);
    }

    #[test]
    fn bare_pong_string_from_old_daemon_is_a_mismatch() {
        // Pre-Curd-C daemons answer Ping with the bare string `"pong"`, which
        // has no `version`. The client must treat that as a mismatch (→
        // restart), never as a match or a hard error.
        let resp = DaemonResponse::ok(&"pong");
        assert_eq!(
            skew_verdict(super::super::ipc::BUILD_IDENTITY, &resp),
            SkewVerdict::Restart,
            "legacy bare-pong daemon must be treated as skew"
        );
    }

    #[test]
    fn error_response_is_a_mismatch() {
        let resp = DaemonResponse::internal("boom");
        assert_eq!(
            skew_verdict(super::super::ipc::BUILD_IDENTITY, &resp),
            SkewVerdict::Restart,
            "an error response is unverifiable → mismatch"
        );
    }

    #[tokio::test]
    async fn socket_available_after_thirty_seconds_connects_within_total_budget() {
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let connect_attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let connect_elapsed = elapsed.clone();
        let observed_connect_attempts = connect_attempts.clone();
        let sleep_elapsed = elapsed.clone();
        let socket = Path::new("/tmp/hallouminate-delayed.sock");
        let log = Path::new("/tmp/hallouminate-bootstrap.log");

        wait_for_daemon_socket(
            socket,
            log,
            CONNECTION_BUDGET,
            || {
                observed_connect_attempts.set(observed_connect_attempts.get() + 1);
                std::future::ready(connect_elapsed.get() > Duration::from_secs(30))
            },
            |delay| {
                sleep_elapsed.set(sleep_elapsed.get() + delay);
                std::future::ready(())
            },
            || elapsed.get(),
        )
        .await
        .expect("daemon becoming available in the second window must connect");

        assert_eq!(elapsed.get(), Duration::from_millis(30_400));
        assert_eq!(
            connect_attempts.get(),
            33,
            "polling must continue past 30 seconds without restarting the wait"
        );
    }

    #[tokio::test]
    async fn unavailable_socket_exhausts_total_budget_with_connection_context() {
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let sleep_elapsed = elapsed.clone();
        let delays = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed_delays = delays.clone();
        let socket = Path::new("/tmp/hallouminate-unavailable.sock");
        let log = Path::new("/tmp/hallouminate-bootstrap.log");

        let error = wait_for_daemon_socket(
            socket,
            log,
            CONNECTION_BUDGET,
            || std::future::ready(false),
            |delay| {
                observed_delays.borrow_mut().push(delay);
                sleep_elapsed.set(sleep_elapsed.get() + delay);
                std::future::ready(())
            },
            || elapsed.get(),
        )
        .await
        .expect_err("an unavailable daemon must exhaust its bounded budget");
        let error = error.to_string();
        let delays = delays.borrow();

        assert_eq!(elapsed.get(), CONNECTION_BUDGET);
        assert_eq!(delays.len(), 92);
        assert_eq!(
            &delays[..3],
            &[
                INITIAL_POLL_INTERVAL,
                Duration::from_millis(400),
                Duration::from_millis(800),
            ],
        );
        for delay in &delays[3..delays.len() - 1] {
            assert_eq!(*delay, MAX_POLL_INTERVAL);
        }
        assert_eq!(
            delays.last().copied(),
            Some(Duration::from_millis(600)),
            "the final sleep must be clipped to the remaining budget"
        );
        assert_eq!(
            error,
            "daemon socket /tmp/hallouminate-unavailable.sock did not become reachable within the 90s total connection budget; see bootstrap log /tmp/hallouminate-bootstrap.log"
        );
    }

    // ── bootstrap log size cap ──

    #[test]
    fn undersized_log_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-bootstrap.log");
        std::fs::write(&path, b"small").expect("write");
        truncate_log_if_oversized(&path, 1024).expect("truncate check");
        assert_eq!(std::fs::read(&path).expect("read"), b"small");
    }

    #[test]
    fn oversized_log_is_truncated_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-bootstrap.log");
        std::fs::write(&path, vec![b'x'; 2048]).expect("write");
        truncate_log_if_oversized(&path, 1024).expect("truncate");
        assert_eq!(
            std::fs::read(&path).expect("read").len(),
            0,
            "log past the cap must be reset to empty, not left to grow further"
        );
    }

    #[test]
    fn missing_log_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.log");
        assert!(truncate_log_if_oversized(&path, 1024).is_ok());
    }
}
