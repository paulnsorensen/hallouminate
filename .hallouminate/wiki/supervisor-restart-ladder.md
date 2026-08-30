---
status: reviewed
last_verified: 2026-08-30
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/387
---
# Supervisor restart ladder

The daemon's task supervisor (`crates/hallouminate-daemon/src/supervisor.rs`)
restarts a panicked long-lived loop with exponential backoff, and — since
#407/#408/#409 — exposes per-task restart counts and escalates a
persistently crash-looping task through the same generic backpressure
ladder the reindex-churn tracker uses, rather than a bespoke mechanism.

## The shared `Ladder<A>` abstraction

`crates/hallouminate-daemon/src/ladder.rs` defines one generic two-threshold
evaluator: below `warn_at` → `Nothing`, at/above `warn_at` → `Warn`, at/above
`act_at` → `Action(A)`. Three escalation lanes exist in the daemon; two of
them are `Ladder` instances over different action types, the third
deliberately isn't:

| Lane | Trigger | Action | Ladder? |
|---|---|---|---|
| Reindex churn (`churn.rs`) | consecutive zero-upsert reindexes | `ForceMaintenance` | yes — `Ladder<LadderAction>` |
| Supervisor crash loop (`supervisor.rs`) | restart-intensity cap repeatedly exceeded | `RestartTask(name)` | yes — `Ladder<SupervisorAction>` |
| Watchdog stall (`watchdog.rs`) | a task's heartbeat stops advancing | `WatchdogTrip` → `abort()` | **no** — fired directly by the stall detector |

The watchdog's stall detector bypasses `Ladder` entirely because a stalled
task has *no* count to escalate against — it either is or isn't making
progress. Reusing the same `LadderAction` enum across the two ladder-driven
lanes (rather than one enum per lane) is what let `status.rs` render all
three trip kinds through one match arm.

## Two-layer crash-loop escalation

Restart intensity and the ladder measure different things, and both are
configurable independently (`crates/hallouminate-config/src/lib.rs`):

1. **`restart_intensity_cap` / `restart_intensity_window_secs`** (defaults
   `5` / `60`) — how many panics-and-restarts within the rolling window
   count as "this task is crash-looping" at all. Each time a task exceeds
   this cap, the supervisor increments that task's escalation-strike count
   and evaluates the strike count against the ladder.
2. **The ladder itself** (`warn_at: 3, act_at: 5`, hardcoded in
   `state.rs` — "invented defaults, no existing analog in debt.rs", i.e. not
   yet promoted to config) — how many separate crash-loop *episodes* a task
   racks up before the supervisor stops just backing off and fires
   `RestartTask(name)` through the escalation hook.

So a single flaky panic never escalates; a task has to blow through the
intensity cap five separate times before `RestartTask` fires. The hook only
records the trip (`DaemonStateInner::last_ladder_trip`, surfaced by `daemon
status`) and logs — it does not itself restart or abort. The supervisor's
own backoff loop (`BACKOFF_FLOOR` 1s → `BACKOFF_CAP` 60s) keeps restarting
the task regardless; the ladder action is a signal, not a kill switch.

## Deliberate: restarts don't reset the heartbeat

`supervisor.rs`'s module doc is explicit about this: restart visibility
(`restart_count`, read by `daemon status`) is tracked separately from the
heartbeat registry, and a restart **deliberately does not** bump the
task's heartbeat epoch. A crash-looping task must still look stalled to the
watchdog, not alive — otherwise a tight panic/restart cycle could keep
resetting the stall clock and mask a wedge that should trip the watchdog's
`abort()` path. The two failure classes (crash loop vs. stall) stay
observable through independent signals even though both ultimately funnel
into `LadderAction`.

## Status surface

`Supervisor::restart_count(task)` (lifetime counter, `AtomicU64` per task)
is read by `status::report()` and rendered as `restarts=N` per task line by
`hallouminate daemon` status output (`cli.rs`,
`render_status_report`) — see [daemon-and-cli](daemon-and-cli.md) for the
rest of the status/CLI surface. Before #408 this counter existed but had no
reader (`#[allow(dead_code)]`); #407 removed the stale allow once #408 wired
it into `StatusReport`.

_Source: `crates/hallouminate-daemon/src/{ladder,supervisor,churn,watchdog,state,status}.rs`, commits `ccbba55`/`5440dfe`/`302f4a4` (#407/#408/#409, closing #387) · Updated: 2026-08-30 · Supersedes: —_
