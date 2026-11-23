# Spike #478: ractor-supervisor as owner of the daemon's restart policy

agent_resolution: gathered inline (no sub-agent fork — single researcher session, primary sources fetched directly via `gh api` and `docs.rs`/crates.io).

## Recommendation

**Reject.** Do not adopt `ractor-supervisor`. Keep the custom
`supervisor.rs` + `backoff.rs` + `ladder.rs`. The crate's built-in meltdown
behavior (stop the supervisor with an error once `max_restarts` is
exceeded) is the *opposite* of hallouminate's explicitly chosen policy
(restart forever, escalate through the ladder, let the watchdog own
kill decisions) — see the ADR's "Alternatives rejected" section, which
already rejected "stop or restart the task in the escalation hook."
Reproducing the current policy on top of the crate requires hand-rolling
sticky-strike accounting, a lifetime restart counter, and a
meltdown-catching outer supervisor, which is roughly as much code as the
~150 lines it would remove, while adding a full actor-framework
dependency (`ractor`) and an actor-adaptation cost for five plain tokio
loops that are not actors today.

## Part A — External: ractor-supervisor and ractor

| Claim | Source | Confidence |
|---|---|---|
| `ractor-supervisor` latest is 0.2.0, published 2026-08-02; MSRV (rust-version) 1.88.0 | crates.io API `GET /api/v1/crates/ractor-supervisor` | certain |
| Release history: 0.1.1–0.1.9 all in Jan–Feb 2025 (rapid initial churn, two early versions yanked), then an ~18-month gap before 0.2.0 (2026-08-02) | crates.io API version list | certain |
| Lifetime downloads: 10,635 (all-time) vs. `ractor` itself at 1,154,035 | crates.io API | certain |
| GitHub repo `simke9445/ractor-supervisor`: 15 stars, 5 forks, 0 open issues, MIT license, last push 2026-08-02 | `gh api repos/simke9445/ractor-supervisor` | certain |
| Bus factor ~1: 22 of 24 commits by `simke9445`; `slawlor` (ractor's own maintainer, 1 commit) and `Matt3o12` (1 commit) contributed version-bump/feature PRs, not core logic | `gh api repos/simke9445/ractor-supervisor/contributors` | certain |
| All 6 closed issues are either dependency bumps (`ractor` 0.15.6→0.16.2) or the original 3-supervisor-kinds feature PR; no unresolved bug reports | `gh api repos/simke9445/ractor-supervisor/issues` | certain |
| `ractor` (the underlying actor framework) is actively maintained: 0.16.0–0.16.5 shipped within days of each other in Jul–Aug 2026, MSRV 1.85 | crates.io API `GET /api/v1/crates/ractor` | certain |
| Three supervisor kinds: `Supervisor` (static children, all 3 strategies), `DynamicSupervisor` (runtime add/remove, OneForOne only, optional `max_children`), `TaskSupervisor` (specialized `DynamicSupervisor` for wrapping bare futures) | github.com/simke9445/ractor-supervisor README.md (fetched verbatim) | certain |
| Strategies: OneForOne (only failing child restarts), OneForAll (all children stop+restart), RestForOne (failing child + all subsequently-started children restart); strategies apply to spawn errors, panics, and normal/abnormal exits alike | README.md "Supervision Strategies" | certain |
| Restart policy: Permanent (always restart), Transient (restart only on abnormal exit), Temporary (never restart) | README.md "Restart Policies"; `core.rs:225-232` `handle_child_exit` match arms | certain |
| Meltdown: `max_restarts` + `max_window` define a sliding window of restart timestamps; **"If more than `max_restarts` occur within `max_window`, the supervisor shuts down abnormally (meltdown)."** Implementation returns `Err(SupervisorError::Meltdown{reason:"max_restarts exceeded"})` from `track_global_restart` when `restart_log.len() > max_restarts`, which propagates out of `handle_supervisor_evt`/`schedule_restart` and ends the supervisor actor with that error | README.md "Meltdown Logic"; `core.rs:20-22,259-294` (fetched raw source, `raw/core.rs`) | certain |
| `reset_after` (supervisor-level): "If the supervisor sees no failures for the specified duration, it clears its meltdown log." (`core.rs:273-277`). `reset_after` (per-child, on `ChildSpec`/`TaskOptions`): "if a specific child remains up for the given duration, its own failure count is reset to zero on the next failure" (`core.rs:192-208`, `prepare_child_failure`) | README.md; `raw/core.rs:192-208,264-277` | certain |
| `backoff_fn` is a per-child hook, `ChildBackoffFn = Arc<dyn Fn(&str, restart_count: usize, last_fail: Instant, reset_after: Option<Duration>) -> Option<Duration>>`. Returning `None` restarts immediately; `Some(d)` delays the restart by `d`. Example in README implements manual exponential backoff (`1 << restart_count`) | README.md example code; `raw/core.rs:29-64` `ChildBackoffFn`, `raw/core.rs:316-326` `schedule_restart` calling it | certain |
| Restart counter (`ChildFailureState.restart_count`) is **not** a lifetime counter — it resets to 0 whenever `reset_after` elapses without a new failure, and is scoped per-child inside `HashMap<String, ChildFailureState>`; it is readable externally only via `InspectState(RpcReplyPort<SupervisorState>)` / `DynamicSupervisorState`, an async RPC call, not a plain getter | `raw/core.rs:151-213`; `raw/supervisor.rs:63-93` (`SupervisorMsg::InspectState`); `raw/dynamic.rs:36-45` (`DynamicSupervisorMsg::InspectState`) | certain |
| A plain async task (not an actor) is supervised via `TaskSupervisor::spawn_task(supervisor_ref, task_fn, TaskOptions)`, which wraps the closure in a `ChildSpec` under a `DynamicSupervisor`. Task lifecycle doc: normal completion → restart only if `Restart::Permanent`; panic → "the actor fails abnormally" → restart if `Permanent` or `Transient` | `raw/task.rs:1-92` (module doc, quoted verbatim), `raw/task.rs:228-253` (`spawn_task` impl) | certain |
| `TaskOptions` supports its own `backoff_fn` and `reset_after`, mirroring `ChildSpec`; `TaskSupervisorOptions` is a type alias for `DynamicSupervisorOptions` and carries `max_children`, `max_restarts`, `max_window`, `reset_after` | `raw/task.rs:160-183` | certain |
| Panics in children are observed via `ractor`'s own actor runtime, which catches an unwinding panic inside actor message-handling and converts it to a `SupervisionEvent::ActorFailed(cell, err)` delivered to the parent's `handle_supervisor_evt` — **except** if the crate/binary is built with `panic = "abort"`, in which case panics are not caught and the process aborts instead. hallouminate's workspace `Cargo.toml` sets no `panic` profile override, so the default `unwind` applies | web search summary of `docs.rs/ractor/latest/ractor/actor` content (not independently re-fetched verbatim — see Open questions); `raw/supervisor.rs:391-406` (`SupervisionEvent::ActorFailed` handling) | speculating (ractor's own panic-capture doc text was summarized by a fetch tool, not quoted verbatim from a page I retrieved directly) |
| `ractor-supervisor` logs via the `log` facade (`log::info!` in `supervisor.rs`), not `tracing`, which hallouminate uses exclusively (`tracing = "0.1"` workspace dep, `tracing::error!`/`warn!`/`info!` throughout `supervisor.rs`/`watchdog.rs`) | `raw/supervisor.rs:363` (`log::info!`); local `Cargo.toml:19`; local `supervisor.rs` throughout | certain |
| Meltdown terminates the *supervisor itself*, differing from hallouminate's current policy of restarting forever while only reporting via the ladder/escalation hook — this is a direct policy inversion, not a superset | derived from `core.rs:286-294` vs. local `supervisor.rs:196-244` (escalation is sticky/log-only, never stops the monitor) | certain |

## Part B — Local mapping

Current behavior lives in:
- `crates/hallouminate-daemon/src/supervisor.rs` (583 lines) — restart loop, backoff application, intensity cap, sticky escalation strikes, lifetime restart counters, shutdown races
- `crates/hallouminate-daemon/src/backoff.rs` (59 lines) — shared `floor`-doubling-to-`cap` curve, used by both the supervisor and the watchdog's boot backoff
- `crates/hallouminate-daemon/src/ladder.rs` (101 lines) — generic `warn_at`/`act_at` evaluator shared across 3 failure paths
- `crates/hallouminate-daemon/src/watchdog.rs` (804 lines) — separate OS-thread stall detector + persisted trip history + boot backoff (out of scope for ractor-supervisor: it monitors heartbeats, not restarts)
- `crates/hallouminate-daemon/src/status.rs` — wires `restart_count`/`last_ladder_trip` into `StatusReport`
- `crates/hallouminate-daemon/src/state.rs:677-738` — supervisor construction, ladder seed (`warn_at: 3, act_at: 5`), escalation hook wiring
- `crates/hallouminate-daemon/src/server.rs:139-220,227-260` — five `sup.spawn(TaskName::X, factory)` call sites (WatcherPump, CatchUp, Signal, IdleExit; Maintenance is wired in `state.rs:768-780`)

| Current behavior | Native in ractor-supervisor? | Evidence |
|---|---|---|
| Exponential backoff 1s→60s (`backoff.rs`, `supervisor.rs:267-276`) | **Yes, via hook.** `backoff_fn` closure can reuse the exact same `exponential_backoff_secs` curve, called with `restart_count` instead of a locally tracked `consecutive_quick_panics` | `raw/core.rs:29-64,316-326` |
| `restart_intensity_cap`/`restart_intensity_window_secs` (config-driven, `state.rs:678-679`) | **Partially, via `max_restarts`/`max_window` — but semantics differ.** Native field names map 1:1, but crossing the threshold **stops** the supervisor instead of escalating-and-continuing | `raw/core.rs:259-294`; local `supervisor.rs:186-244` |
| Sticky escalation strikes, `warn_at: 3`/`act_at: 5` (`ladder.rs`, `state.rs:682-686`, `supervisor.rs:199-240`) | **No.** No warn/act ladder concept exists in the crate; would have to be hand-rolled entirely inside a custom `backoff_fn` closure (it only returns `Option<Duration>`, so warn/act signalling needs a captured `Arc<AtomicU32>` + manual `tracing::warn!`/hook call inside the closure) | `raw/core.rs:29-64` (`ChildBackoffFn` signature has no side-channel for reporting) |
| Reset only after a healthy run ≥ window (`supervisor.rs:187-193`) | **Yes, natively.** `reset_after` (supervisor-level clears the meltdown log; per-child `ChildSpec.reset_after`/`TaskOptions.reset_after` clears that child's `restart_count`) map directly | `raw/core.rs:192-208,264-277` |
| Heartbeat NOT bumped on restart, so watchdog still sees a crash loop (`supervisor.rs` header comment lines 12-15) | **Not applicable either way — always adapter code.** ractor-supervisor has no heartbeat concept; the hook point moves from the custom loop to inside a `spawn_fn`/`backoff_fn` closure, but the "don't touch the heartbeat" invariant must be re-encoded by hand regardless of supervisor library | n/a — hallouminate-specific |
| Lifetime `restarts=N` in `daemon status` (`supervisor.rs:109-113`, `status.rs:27-31`) | **No.** `ChildFailureState.restart_count` resets on `reset_after`/meltdown-log-clear, is per-child not lifetime, and is only reachable via an async `InspectState` RPC call — `status::report` is a synchronous function today, so this would need either an async refactor of `status::report` or a separate `AtomicU64` side-counter bumped from inside `backoff_fn` | `raw/core.rs:151-213`; `raw/supervisor.rs:63-93`; local `status.rs:17-32` (sync fn) |
| Persistent trip history (`watchdog.rs` trip-state file) | **Out of scope.** This is the watchdog's stall-detection concern, entirely separate from restart supervision; unaffected by this spike either way | local `watchdog.rs:1-16,60-173` |
| `LadderAction` status wire type (`ForceMaintenance`/`RestartTask`/`WatchdogTrip`, `ladder.rs:14-22`, `status.rs:61-67`) | **No direct mapping.** Would need a manual translation layer from `SupervisionEvent`/meltdown `Err` into the existing `LadderAction::RestartTask(TaskName)` wire variant — the crate has no equivalent typed action, only log lines and the meltdown `Err` | `raw/supervisor.rs:391-406`; local `ladder.rs:14-22` |

**Lines removed vs. adapter lines added (estimate, not a committed diff):**
- Removable: the hand-written restart loop body in `supervisor.rs:133-263` (~130 lines) plus most of `backoff.rs`'s public surface (the curve itself, ~17 lines, would be reused inside a new `backoff_fn`, so net removal there is small, ~20 lines of boilerplate/wrapper).
- Not removable, must be rebuilt as adapter code: (a) a custom `backoff_fn` reproducing sticky strikes + ladder warn/act + escalation-hook call, ~40-60 lines; (b) a lifetime restart counter side-channel (`status.rs` currently reads `Supervisor::restart_count` synchronously, `status.rs:30`), ~15-20 lines; (c) a `LadderAction`-mapping layer from meltdown/`ActorFailed` events, ~20-30 lines; (d) an outer wrapping supervisor (or manual re-spawn-on-meltdown loop) to preserve "never truly stop restarting, only escalate" — since meltdown terminates the supervisor by design — ~30-50 lines, effectively re-implementing the removed loop's "keep going" property; (e) five call-site conversions in `server.rs`/`state.rs` from `FnMut() -> Fut (Output=())` factories to `ChildSpec`/`TaskOptions` + `Result<(), ActorProcessingErr>`-returning futures, ~10-15 lines each ≈ 50-75 lines.
- Net estimate: **~150 lines removable vs. ~155-235 lines of new adapter code**, i.e., roughly neutral to a net increase in supervisor-layer code, before counting the new `ractor` + `ractor-supervisor` dependency surface and the `log`→`tracing` bridging cost.

**Actor-adaptation cost for the five supervised tasks:** none of `Maintenance`, `CatchUp`, `WatcherPump`, `IdleExit`, `Signal` are `ractor` actors today — they are plain `tokio::select!`-driven loops closing over `DaemonState`/`CancellationToken` (`server.rs:139-260`, `state.rs:768-780`). `TaskSupervisor` avoids writing a full `Actor` impl per task (it wraps a bare `Fn() -> Fut` closure, `raw/task.rs:228-253`), so the *mechanical* adaptation is moderate: each factory changes its `Future<Output = ()>` to `Future<Output = Result<(), ActorProcessingErr>>` (map ordinary internal errors to `Err`; panics are caught by ractor's runtime as abnormal actor failures, not manually mapped to `Err`), and each `sup.spawn(TaskName::X, factory)` becomes a `TaskSupervisor::spawn_task(sup_ref, closure, TaskOptions::new().name(..).restart_policy(..))` call. The larger cost is structural, not mechanical: introducing `ractor`'s actor runtime (message passing, `ActorRef`, process registry, its own `tokio::spawn` usage) as a second concurrency substrate alongside the daemon's existing direct-tokio model, purely to host five loops that do not otherwise need actor semantics (no inter-task messaging, no ractor process-group features are used anywhere else in the daemon).

## Part C — Alternatives, briefly

| Mechanism | What it would own | Fit |
|---|---|---|
| `ractor-supervisor` | Restart policy (partially — meltdown semantics conflict) | Rejected (Part A/B above) |
| `tokio-graceful-shutdown` (0.20.0, 647K downloads) | Structured shutdown propagation, with manual orchestration hooks for ordering, and panic/error *propagation* across a subsystem tree; **not** restart/backoff. Docs: "Automatic shutdown on SIGINT/SIGTERM/Ctrl+C, Subsystem failure, Subsystem panic" and "Clean shutdown procedure with timeout and error propagation" — no recovery or backoff strategy mentioned | Would own shutdown propagation (a genuinely separate concern from restart policy), not the thing this spike is evaluating; hallouminate's shutdown is already handled by `CancellationToken` + `SHUTDOWN_DRAIN_TIMEOUT` (`server.rs:51-54,170-179`) |
| OS-managed recovery (`launchd`/`systemd`) | Whole-process restart on crash (e.g., systemd `Restart=on-failure` + `RestartSec`/`StartLimitBurst`, or launchd `KeepAlive`) | Owns process-level respawn, which the daemon already layers under its *own* watchdog (`watchdog.rs`'s `check_boot_backoff`/`BOOT_BACKOFF_EXIT_CODE`, exit code 75) — the daemon's design explicitly expects an external supervisor (launchd/systemd/a wrapper script) for that outer layer, and this spike's restart policy is a *within-process, per-task* concern (individual tokio loops), which OS-level restart cannot express at all (it only sees the whole process) |

## Open questions

- The exact verbatim wording of `ractor`'s own panic-capture/`ActorFailed` documentation was not re-confirmed via a direct docs.rs fetch (the WebFetch tool returned only a page-shape summary, not source text, for `docs.rs/ractor-supervisor` struct pages; the panic-capture claim is sourced from a web-search-tool summary of `docs.rs/ractor/latest/ractor/actor`, not a directly quoted page). Confidence on that one claim is `speculating`; everything else in Part A is sourced from the crate's own README.md and `.rs` source files fetched verbatim via `gh api`.
- Whether a custom outer "meltdown-catching" supervisor (to preserve "restart forever, never truly stop") is idiomatic in ractor-supervisor's own usage patterns, or considered an anti-pattern by its author, was not checked against any design-rationale doc — the crate has no ADR/design-notes file, only the README and inline `///` comments.
- No maintainer response-time data (e.g., issue-to-close latency) was gathered beyond "0 open issues currently" — insufficient sample to characterize responsiveness under a real bug report.

## Confidence

**Certain** on the crate's public API, meltdown semantics, and the direct 1:1/no-mapping table in Part B (all sourced from the crate's own README and `.rs` source, fetched verbatim). **Speculating** only on the one flagged claim about `ractor`'s internal panic-capture wording. The reject recommendation itself is a synthesis, not a single citable fact, but rests on the meltdown-inversion claim (certain) plus the bus-factor/download data (certain) plus the line-count estimate (a reasoned estimate, explicitly labeled as such, not a committed diff).
