# Daemon version skew

When `serve` finds a running daemon, it compares build identities. Inside one channel it restarts only an older daemon. Across channels it restarts on any mismatch. Shipped in PR #744.[^code]

## Build identity

- `BUILD_IDENTITY` is `HALLOUMINATE_BUILD_ID` when that variable is set and non-empty at compile time, else `CARGO_PKG_VERSION` (`crates/hallouminate-daemon/src/ipc.rs:392`).
- Every nightly keeps the same `CARGO_PKG_VERSION`, so `nightly.yml` sets `HALLOUMINATE_BUILD_ID` to the npm version `0.0.0-experimental.<run_number>.<run_attempt>`. Keep the build-job value equal to the `VERSION` in publish-and-smoke.
- The daemon `Ping` reply reports `BUILD_IDENTITY` as `version`.

## Skew rule

`skew_verdict(ours, resp)` (`bootstrap.rs:292`) returns `Adopt`, `Restart`, or `Newer`:

| Case | Verdict |
| --- | --- |
| Both nightlies (`0.0.0-experimental.*`), or both releases | semver order: daemon older → `Restart`; equal → `Adopt`; daemon newer → `Newer` |
| One nightly, one release | exact match: equal → `Adopt`, else `Restart` |
| Either identity fails to parse as semver | exact match |
| Bare legacy `"pong"`, error envelope, transport error, timeout | `Restart` |

`Newer` adopts the running daemon and logs a warning that names `hallouminate daemon restart`. It never calls `lifecycle::stop`.

## Why this design

- Exact string match alone let an older nightly `serve` restart a newer daemon, and two installs could restart each other in a loop.
- Plain semver across channels ranks every nightly below every release. A user who switches to nightly would then keep a stale release daemon. Exact match across channels makes a channel switch take effect.
- Numeric prerelease fields order numerically in semver, so `0.0.0-experimental.346.1` > `0.0.0-experimental.345.2` without a workflow change.

## Related

- [daemon-canonical-identity-002](daemon-canonical-identity-002.md): the discovery seam that finds the running daemon before this check runs.

[^code]: `crates/hallouminate-daemon/src/bootstrap.rs`: `classify_identity` (274), `skew_verdict` (292), `reported_identity` (248); unit tests `older_serve_does_not_restart_newer_daemon`, `newer_serve_restarts_older_daemon`, `nightly_serve_restarts_release_daemon`, `release_serve_restarts_nightly_daemon`.

_Source: PR #744 · Updated: 2026-11-15_
