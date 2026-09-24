# ADR — infrastructure-ownership-001

### ADR-001: Do not adopt tokio-graceful-shutdown for daemon lifecycle  [status: accepted]

- **Context:** Spike #476 asks whether `tokio-graceful-shutdown` can remove signal, task-tree, timeout, and cleanup code from the daemon. The crate is healthy: version 0.20.0, MSRV 1.85, tokio 1.39 or later, and no open issues. All three values are compatible with this workspace. The question is which responsibilities the crate can own.
- **Decision:** Reject adoption. The daemon keeps its `CancellationToken`, `Supervisor`, `drain_handlers`, and `finish_shutdown` code.
- **Evidence:**
  - `ErrorAction` has two variants, `Forward` and `CatchAndLocalShutdown`. The crate has no restart primitive. `supervisor.rs`, `backoff.rs`, and `ladder.rs` stay unchanged.
  - A subsystem panic with the default action stops the whole tree. The daemon restarts the one task with backoff and continues to serve.
  - `Toplevel::handle_shutdown_requests` returns `ShutdownTimeout`; when the `Toplevel` is then dropped, its `root_handle` and `SubsystemRunner` drop paths invoke `AbortHandle::abort()` on remaining async subsystem tasks. This still cannot preempt `spawn_blocking` work, and `drain_handlers` keeps explicit cleanup orchestration.
  - The crate gives no order guarantee between sibling subsystems (upstream issue #80). The sequence "stop admission, drain handlers, release storage" needs the same explicit `JoinSet` and timeout code.
  - No async abort can stop a `spawn_blocking` thread. The crate uses the same tokio primitive as `JoinSet::abort_all`. An async timeout is thus not a forced cancellation of native inference, with or without the crate.
  - Only signal handling maps cleanly (`Toplevel::catch_signals`). The estimate is 45 lines removed and 30 to 60 lines added. More than 1,200 lines of policy code stay.
- **Acceptance scenarios:** Source and maintainer statements decide all four scenarios. Startup failure: the root subsystem forwards the error through the crate's failure channel; local code returns the original `serve_on_listener` error after cleanup, so error propagation differs while cleanup still runs in both paths. Task panic: stop the tree against restart with backoff, a structural difference. Shutdown during blocked admission: the same `select!` on a cancellation token. Bounded cleanup: Toplevel drop aborts async subsystem tasks, but no async abort preempts blocking threads. The spike compiles no prototype, because no runtime result can change a structural gap.
- **Alternatives:** Adopt the crate for signal handling only (rejected: a dependency for 45 lines, and a second owner of the shutdown token). OS-managed recovery (#477) and actor supervision (#478) have their own records, -002 and -003.
- **Consequences:** No production migration and no code removal. The open item is independent of this crate: a compiled test can confirm how `abort_all` affects in-flight `spawn_blocking` inference. Full claim table: `.cheese/research/spike-476-tokio-graceful-shutdown/spike-476-tokio-graceful-shutdown.md`.
