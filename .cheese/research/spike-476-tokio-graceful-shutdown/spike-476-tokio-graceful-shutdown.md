# Spike 476 — tokio-graceful-shutdown adoption evaluation

agent_resolution: gathered inline (no sub-agent fork; single-level nesting budget spent on gh/WebFetch calls directly)

## Recommendation

**Reject** adoption of `tokio-graceful-shutdown` as a wholesale replacement for
the daemon's lifecycle bookkeeping. Adopt narrowly, if at all, only as a
signal-catching + top-level timeout convenience; it cannot replace the
supervisor's panic-restart/backoff/ladder policy (out of scope for the
crate), and its shutdown-timeout path does not solve the daemon's hardest
problem — bounding `spawn_blocking` native inference work — any better than
the current hand-rolled code does.

## Part A — External: tokio-graceful-shutdown crate facts

- **Current version**: 0.20.0, published 2026-07-30. Repo `pushed_at`
  2026-07-30T21:52:43Z; 0 open issues (all historical issues closed).
  Release cadence: 0.19.0 (2025-09-28) → 0.19.1/0.19.2 (2025-11-20) →
  0.19.3 (2026-04-02) → 0.20.0 (2026-07-30). Actively maintained.
  Source: `gh api repos/Finomnis/tokio-graceful-shutdown` /
  `.../releases` (https://github.com/Finomnis/tokio-graceful-shutdown).
- **MSRV**: `rust-version = "1.85"`, `edition = "2024"` (crate's own
  `Cargo.toml`, fetched from `main`). hallouminate workspace pins
  `rust-version = "1.91"` (repo `Cargo.toml:8`) — no MSRV conflict.
- **Tokio compatibility**: crate depends on `tokio = { version = "1.39.0",
  features = ["signal","rt","macros","time"], default-features = false }`
  and `tokio-util = "0.7.10"`. hallouminate pins `tokio = { version = "1",
  features = [...] }` (repo `Cargo.toml:15`) — compatible, no floor conflict.
- **API model** (from `src/toplevel.rs`, `src/subsystem/*`,
  `src/error_action.rs`, `src/errors.rs` on `main`):
  - `Toplevel::new(root_fn)` builds the root subsystem `"/"` with
    `ErrorAction::Forward` hardcoded for both failure and panic — any error
    that reaches Toplevel always triggers a global shutdown; this cannot be
    changed.
  - `.catch_signals()` spawns a task that awaits SIGINT/SIGTERM (Unix) or
    CTRL_C/CTRL_BREAK/CTRL_CLOSE/CTRL_SHUTDOWN (Windows) and cancels the
    root `CancellationToken` — functionally equivalent to
    `spawn_signal_handlers` in `server.rs:181-260`.
  - `SubsystemHandle::start(SubsystemBuilder)` spawns a nested subsystem;
    `SubsystemBuilder::new(name, fn).on_failure(action).on_panic(action)`
    (default `ErrorAction::Forward` for both) `.detached()` to opt out of
    automatic parent→child cancellation propagation.
  - `ErrorAction` has exactly two variants: `Forward` (bubble to parent,
    no local reaction) and `CatchAndLocalShutdown` (store the error,
    shut down this subsystem + its children locally, do not forward).
    **No restart/retry variant exists.** Confirmed against the full
    `error_action.rs` source — this is the complete enum.
  - `NestedSubsystem` exposes `.join()`, `.initiate_shutdown()`,
    `.change_failure_action()/.change_panic_action()`, `.finished()`
    (lightweight completion future), `.is_finished()`, and `.abort()`
    (best-effort `AbortHandle::abort()`, explicitly *not* automatic —
    doc comment: "This action is performed on a best-effort base. It is
    not guaranteed that aborting a task is performed right away, or
    ever.").
  - `SubsystemHandle::on_shutdown_requested()` / `.is_shutdown_requested()`
    / `.request_shutdown()` / `.request_local_shutdown()` /
    `.create_cancellation_token()` are the subsystem-side shutdown-signal
    API. `cancel_on_shutdown()` (from `FutureExt`, not directly inspected
    but referenced throughout docs/examples) races an arbitrary future
    against the subsystem's cancellation token.
  - `Toplevel::handle_shutdown_requests(timeout)` (full source fetched):
    races "all subsystems finished" against "shutdown requested"; once
    shutdown starts, it does `tokio::time::timeout(timeout,
    toplevel_subsys.join())`. On success returns `Ok(())` or
    `Err(SubsystemsFailed(errors))`. **On timeout it returns
    `Err(GracefulShutdownError::ShutdownTimeout(errors))` and does
    nothing else** — it does not call `.abort()` on the tree.

- **On subsystem error** (returns `Err`): if `on_failure ==
  CatchAndLocalShutdown`, the error is captured locally, the subsystem and
  its children are cancelled (their `CancellationToken` fires — this only
  *asks* children to stop via their own `on_shutdown_requested()`/select
  loops, it does not forcibly kill them), and the error surfaces at
  `.join()`. If `Forward` (the default), the error propagates to the
  parent's error channel unchanged and the parent decides.
- **On subsystem panic**: `runner.rs` catches the `JoinError` from the
  inner `tokio::spawn`, classifies it via `e.is_panic()`, and produces
  `SubsystemError::Panicked(name)`; the crate never `.unwrap()`s a panic.
  Handled identically to a returned `Err` through the same `ErrorAction`
  (`on_panic`, independently configurable from `on_failure`).
- **On startup failure**: the user-supplied root closure passed to
  `Toplevel::new` runs as the `"/"` subsystem with `ErrorAction::Forward`
  hardcoded (not overridable) — any error/panic during startup (e.g.
  binding a socket) is forwarded straight to the Toplevel's error channel
  and a global shutdown is initiated immediately. There is no distinct
  "startup" phase or ordering guarantee — a subsystem spawned before
  another failing subsystem gets the same shutdown signal as everything
  else, concurrently.
- **On shutdown timeout**: confirmed by direct source read of
  `handle_shutdown_requests` — the timeout wraps the `.join()` future.
  Dropping that future when it times out does **not** abort the
  underlying subsystem tasks. The crate's own maintainer states the
  opposite is manual/optional: GitHub issue #103 ("Abort/cancel
  subsystems ungracefully", closed, merged as `NestedSubsystem::abort()`
  in 0.16.0) — maintainer quote: *"Turns out `.abort()` is much easier
  to implement than a shutdown with a timeout, so I went with that one.
  The user can wrap a timeout around `.shutdown()`, `.join()` and
  `.abort()`."* — i.e., **the caller, not the crate, is responsible for
  aborting on timeout**; `Toplevel::handle_shutdown_requests` does not do
  it automatically.
  - Critically for hallouminate: even where a caller does call `.abort()`,
    that is `tokio::task::AbortHandle::abort()`, which only takes effect
    at the task's next `.await` point (standard tokio semantics) and has
    **no effect on `spawn_blocking` work** — a blocking OS thread running
    native inference cannot be preempted by an async abort/cancel signal
    at all. This is the same limitation the current code already lives
    with; the crate does not close this gap.
- **Ordering guarantees between nested subsystems**: **none, by default.**
  Cancellation propagates from parent to all children *simultaneously* via
  a shared `CancellationToken` tree (confirmed: `runner.rs` comment "this
  is the main mechanism that forwards a cancellation to all the
  children" — fired on `joiner_token` drop, not sequenced). GitHub issue
  #80 ("Sequential shutting down of subsystems after SIGINT/SIGTERM",
  closed) confirms directly: maintainer states *"Everything in this crate
  is made in a way that a shutdown is always recursive... [CancellationToken]
  does not support blocking a shutdown request at some intermediate
  node."* The crate's answer is a manual pattern: give each subsystem
  a `NestedSubsystem::finished()` future and have downstream subsystems
  `select!` on it before reacting to their own shutdown signal (examples
  `19_sequential_shutdown.rs`, `20_orchestrated_shutdown_order.rs`, added
  in 0.14.3). **This means "admission stops before handler drain before
  storage release" is not a built-in guarantee — it would have to be
  hand-coded with the same `.finished()`-future wiring the daemon already
  achieves today via explicit `JoinSet` draining and ordered `.await`
  calls in `server.rs`.**
- **Restart policy**: confirmed absent. `ErrorAction` has only
  `Forward`/`CatchAndLocalShutdown`; there is no "respawn subsystem N
  times with backoff" primitive anywhere in `src/`. A restart-on-panic
  policy equivalent to hallouminate's `Supervisor` would have to be
  built entirely in application code around the crate (spawn a new
  `SubsystemBuilder` on error, re-implementing exactly what
  `supervisor.rs` does today, but without the crate's help).

## Part B — Local mapping

| Responsibility | Current location | Disposition vs. crate |
|---|---|---|
| Signal handling (SIGINT/SIGTERM → shutdown token) | `server.rs:181-260` (`spawn_signal_handlers`) | **REPLACED**: `Toplevel::catch_signals()` does the same job in ~1 line, though it owns its own `CancellationToken` rather than composing with the existing `DaemonState::shutdown_token()` (`state.rs:789`) unless constructed via `Toplevel::new_with_shutdown_token`. |
| Task tree / spawn-and-supervise 5 named loops (`Maintenance`, `CatchUp`, `WatcherPump`, `IdleExit`, `Signal`) | `supervisor.rs:1-265` (`Supervisor::spawn`) | **CONFLICTS**: the crate models a subsystem tree with two-outcome `ErrorAction`, not an OTP-style panic-restart-with-backoff loop. Mapping `supervisor.rs`'s `tokio::select!` restart loop (lines 133-263) onto `SubsystemBuilder` would mean *not* using the crate's restart semantics at all and reimplementing the same loop inside a subsystem closure — the crate adds a wrapper layer, not a replacement. |
| Cancellation token | `state.rs:789` (`DaemonState::shutdown_token`), threaded through `supervisor.rs:83,97,130`, `server.rs:175,192,495,547,557,576` | **REPLACED** (mechanically): `Toplevel`/`SubsystemHandle` are themselves built on `tokio_util::sync::CancellationToken` internally (confirmed in `toplevel.rs` imports and `subsystem::root_handle`), and `Toplevel::new_with_shutdown_token` accepts an externally-owned token, so the existing token could be reused. Net effect is renaming call sites, not removing the token. |
| Bounded handler drain (accept-loop `JoinSet<UnixStream handlers>`, 30s deadline, abort-on-timeout) | `server.rs:513-604` (`serve_on_listener`), `server.rs:606-635` (`drain_handlers`) | **RETAINED as app policy**: this is exactly the ordering problem issue #80 says the crate does not solve automatically — "stop admission, then drain handlers, then release resources" requires the same explicit `JoinSet` + `tokio::time::timeout` + `abort_all()` pattern hallouminate already has (`drain_handlers` lines 611-635), whether or not `Toplevel` sits on top. |
| Socket/lockfile cleanup order (release socket file, then flock) | `server.rs:362-394` (`finish_shutdown`), `server.rs:405-408` (`cleanup`), `server.rs:719-741` (`acquire_single_instance`) | **RETAINED as app policy**: pure Rust resource-drop ordering (comment at `server.rs:397-405` explains why socket removal must precede flock release); nothing in the crate's API concerns file/flock lifecycle. |
| Panic restart with exponential backoff | `supervisor.rs:133-263`, `backoff.rs:1-59` (shared `exponential_backoff_secs` curve, floor-doubling capped) | **RETAINED as app policy** — crate has no restart primitive (Part A). |
| Restart-intensity ladder / escalation | `ladder.rs` (`Ladder<A>`, `warn_at`/`act_at` two-threshold evaluator), `supervisor.rs:186-244`, documented in `.hallouminate/wiki/supervisor-restart-ladder.md` | **RETAINED as app policy** — no analog in the crate; `ErrorAction` is a one-shot per-error decision, not a windowed-intensity policy. |
| Watchdog heartbeat / stall abort | `watchdog.rs` (`Watchdog::spawn`/`stop`, `StallTracker`, `TripAction`, boot-backoff via `check_boot_backoff`) | **RETAINED as app policy** — orthogonal concern (liveness monitoring via `std::thread` + `mpsc`, not part of any async subsystem tree); crate has nothing comparable. |
| Idle-exit | `server.rs:489` (`spawn_idle_exit`), gated through `DaemonState::should_idle_exit`/`touch_activity` (`state.rs:1017-1234`) | **RETAINED as app policy** — an application-level idle timer feeding the same shutdown token; the crate's role would only be receiving that token's cancellation, exactly as today. |
| Startup failure handling (socket bind failure path) | `server.rs:490-510` (`(result, shutdown_deadline) = match serve_on_listener(...)`) then still runs `finish_shutdown` for cleanup even on bind failure | **CONFLICTS/PARTIAL**: crate's hardcoded `ErrorAction::Forward` on the root subsystem "/" means a startup error always triggers a *global* shutdown of everything already-started — matches current intent, but the crate offers no way to special-case "the bind failed, skip straight to cleanup without waiting on other subsystems," which the current code already does directly with a `match`. |

**Lines removed vs. added estimate**: signal handling (`server.rs:181-260`,
~50 lines) could shrink to ~5 lines using `.catch_signals()` — a genuine
small win. Everything else (`supervisor.rs` 265 lines, `backoff.rs` 59
lines, `ladder.rs` ~100 lines, `watchdog.rs` ~800 lines, the drain/cleanup
block in `server.rs` ~150 lines) would be **retained essentially
unchanged**, plus new glue code to adapt each into a `SubsystemBuilder`
closure and to reconcile `Supervisor`'s restart loop with `ErrorAction`'s
two-outcome model. Net estimate: **~45 lines removed, 30-60 lines added**
(new `Toplevel`/`SubsystemBuilder` wiring + adapter closures), for a
**net-neutral-to-negative** line count and added third-party dependency
surface, against ~1,200+ lines of policy code that must stay regardless.

## Part C — Four acceptance scenarios

| Scenario | Crate's documented behavior | Current code | Needs a compiled prototype? |
|---|---|---|---|
| **Startup failure** (e.g. socket bind fails) | Root subsystem error forwards to Toplevel (hardcoded `Forward`), triggers global shutdown of whatever else started; `handle_shutdown_requests` returns `Err(SubsystemsFailed)`. No distinction between "failed before serving" and "failed after serving." | `server.rs:490-494` matches on `serve_on_listener`'s `Result`, always runs `finish_shutdown` for cleanup regardless of outcome, then returns the original error via `result` at line 510. Same outcome, explicit control flow. | No — both behaviors are directly readable from source; the crate's forwarding rule is unconditional and documented. |
| **Task panic** | `SubsystemError::Panicked(name)` captured via `JoinError::is_panic()`, routed through `on_panic` `ErrorAction` (default `Forward` — kills the whole tree unless the app opts into `CatchAndLocalShutdown` per-subsystem). No restart. | `supervisor.rs:158-166,186-263` catches the panic, logs it, and restarts with backoff under the intensity cap, escalating via `Ladder` past the cap — task keeps running the daemon, never propagates to global shutdown. | No — the divergence (kill-the-tree vs. restart-with-backoff) is a documented, structural difference in `ErrorAction`'s two variants, not an implementation detail that needs runtime verification. |
| **Shutdown during blocked admission** (accept loop mid-`listener.accept()` or mid-semaphore-wait when shutdown fires) | `SubsystemHandle`-based loops must manually `select!` on `on_shutdown_requested()` against blocking operations, identical in shape to the current `tokio::select!` at `server.rs:556-567,575-584`. The crate provides no different primitive here — it's the same `CancellationToken`-driven `select!` pattern either way. | `server.rs:556-584` already does exactly this: select on `shutdown.cancelled()` vs. `listener.accept()`/semaphore acquire. | No for the admission-stop mechanism itself (same pattern). **Yes** if the question is whether adopting `Toplevel`'s automatic parent→child cancellation ordering changes *when* admission stops relative to in-flight handler drain — that ordering is not guaranteed by the crate (Part A, issue #80) and would need a prototype only if someone tries to rely on structural nesting instead of the explicit `JoinSet` drain hallouminate already has. |
| **Bounded cleanup** (handler drain timeout → abort stragglers, esp. `spawn_blocking` native inference work) | `handle_shutdown_requests(timeout)` times out and returns `Err(ShutdownTimeout)` but does **not** abort remaining tasks automatically (Part A, issue #103 quote). A caller wanting force-abort must call `NestedSubsystem::abort()` manually, which is a best-effort `AbortHandle::abort()` — has no effect on `spawn_blocking` OS threads per tokio's own cancellation model, which the daemon already relies on for inference (`.hallouminate/wiki/blocking-inference-offload.md`: model load, embedding, and single-file reindex all run under `spawn_blocking`/`block_in_place`). | `drain_handlers` (`server.rs:611-635`) already does the same "timeout then `abort_all()` on the `JoinSet`" pattern, with the identical limitation — `JoinSet::abort_all()` cannot preempt a `spawn_blocking` thread mid-inference either. | **Yes, but only to confirm the crate is no better, not to discover new behavior**: source reading already shows the crate has no different mechanism for interrupting `spawn_blocking` OS threads than `tokio::task::JoinHandle::abort()`/`JoinSet::abort_all()`, which is the same primitive the current code already uses. A prototype would only reconfirm tokio's own documented limitation (blocking tasks are not preemptible), not test anything specific to this crate. |

## Evidence table

| Claim | Source | Confidence |
|---|---|---|
| Current version 0.20.0, released 2026-07-30, actively maintained, 0 open issues | `gh api repos/Finomnis/tokio-graceful-shutdown`, `.../releases` — https://github.com/Finomnis/tokio-graceful-shutdown | certain |
| MSRV 1.85, edition 2024; workspace MSRV 1.91 — compatible | `Cargo.toml` fetched from crate `main` via `gh api .../contents/Cargo.toml`; local `Cargo.toml:8` | certain |
| Tokio dependency floor 1.39.0 with signal/rt/macros/time features; workspace pins `tokio = "1"` — compatible | crate `Cargo.toml` (dependencies section) vs. local `Cargo.toml:15` | certain |
| `ErrorAction` has exactly two variants, `Forward` and `CatchAndLocalShutdown`; no restart variant | `src/error_action.rs` full source, fetched from `main` — https://github.com/Finomnis/tokio-graceful-shutdown/blob/main/src/error_action.rs | certain |
| `handle_shutdown_requests` timeout does not abort remaining tasks; caller must call `.abort()` manually | `src/toplevel.rs` full source (direct read of the `Err(_) => ... ShutdownTimeout` branch); confirmed by maintainer in closed issue #103 | certain |
| No ordering guarantee between sibling subsystems; cancellation is recursive/simultaneous by default, sequencing requires manual `.finished()` wiring | `src/runner.rs` comment "the main mechanism that forwards a cancellation to all the children"; maintainer statement in closed issue #80 — https://github.com/Finomnis/tokio-graceful-shutdown/issues/80 | certain |
| `NestedSubsystem::abort()` is best-effort, not guaranteed timely, and cannot affect `spawn_blocking` OS-thread work | `src/subsystem/nested_subsystem.rs` doc comment; general tokio `spawn_blocking` cancellation semantics (well-established, not separately re-verified via docs.rs due to WebFetch extraction failures — see Open questions) | speculating (tokio spawn_blocking claim not independently re-confirmed this session; consistent with widely known tokio behavior) |
| hallouminate's daemon already offloads model load, single-file reindex, and both hot embedding paths to `spawn_blocking`/`block_in_place` | `.hallouminate/wiki/blocking-inference-offload.md` | certain |
| `Supervisor::spawn` restarts panicked tasks with exponential backoff (`backoff.rs`) under an intensity cap, escalating via `Ladder` past the cap; deliberately does not reset heartbeat on restart | `crates/hallouminate-daemon/src/supervisor.rs:1-265`, `backoff.rs`, `.hallouminate/wiki/supervisor-restart-ladder.md` | certain |
| `drain_handlers` already does bounded-timeout-then-abort_all on the connection-handler `JoinSet` | `crates/hallouminate-daemon/src/server.rs:606-635` | certain |
| Socket-then-flock cleanup order is a deliberate, documented invariant unrelated to any subsystem-tree mechanism | `crates/hallouminate-daemon/src/server.rs:362-408` (`finish_shutdown`/`cleanup` + surrounding comments) | certain |

## Open questions

- docs.rs's rendered API page (`https://docs.rs/tokio-graceful-shutdown/latest/`)
  could not be usefully extracted via `WebFetch` (it returned only a stub
  summary, no doc text) or via `crates.io` (page-title-only fetch). All
  API claims above are instead sourced directly from the crate's GitHub
  source on `main` at commit time of this research (2026-09-20), which is
  primary and arguably stronger evidence than the rendered docs, but a
  reviewer who wants the exact rustdoc wording for `cancel_on_shutdown`
  (referenced in Part A only by name, not by fetched source) should pull
  `src/future_ext.rs` directly.
- Alternative framing raised by the crate maintainer in issue #103: "cancel
  now" mechanism vs. "shutdown with attached timeout" — the maintainer
  chose to ship only `.abort()` (best-effort) rather than a real timeout
  primitive, calling a timeout-with-cancellation "much easier said than
  done." This is a list of alternatives the *crate* considered and
  rejected, not a recommendation for hallouminate.
- Whether hallouminate's own `tokio::task::JoinHandle::abort()`/
  `JoinSet::abort_all()` calls (already used in `drain_handlers` and
  elsewhere in `server.rs`) currently succeed at interrupting a
  `spawn_blocking`-wrapped inference call mid-execution, or merely detach
  from it, was not re-verified by a compiled test in this session — flagged
  in Part C as the one place a prototype could add value, though it would
  be testing tokio's own primitive, not anything specific to this crate.

## Confidence

**Certain** on the crate's documented API surface, version/maintenance
state, and the absence of restart/ordering/timeout-abort guarantees — all
drawn from primary source (crate `main` branch source files and the
maintainer's own closed-issue statements). **Certain** on the local
mapping — all citations are direct reads of the current file contents.
Overall recommendation confidence is **certain** for "reject wholesale
replacement," because the decisive gap (no restart policy, no automatic
timeout-abort, no ordering guarantee) is structural and documented, not a
runtime nuance that could flip on a compiled prototype.
