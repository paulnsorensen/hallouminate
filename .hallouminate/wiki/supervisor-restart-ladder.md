---
status: reviewed
last_verified: 2026-08-30
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/387
---
# ADR: Supervisor restart ladder

The daemon uses one generic `Ladder<A>` evaluator for reindex churn and
supervisor crash loops. The watchdog does not use this evaluator. The three
failure paths use one `LadderAction` type for status reports.[^1]

## Status

Accepted and shipped in #407, #408, and #409.

## Context

The daemon has five supervised tasks. The supervisor restarts a task after the
task panics. It uses exponential backoff from 1 second to 60 seconds.[^2]

Two settings define the restart-intensity limit:

- `restart_intensity_cap` is the permitted number of restarts.
- `restart_intensity_window_secs` is the measurement window.

The default limit is five restarts in 60 seconds.[^3] The supervisor must also
report a persistent crash loop. A normal restart is not sufficient evidence
for this report.

The daemon has two other failure paths. The churn tracker counts consecutive
reindexes that have no upserts. The watchdog detects a task that does not
change its heartbeat.[^4]

## Decision

Use `Ladder<A>` as a generic two-threshold evaluator. A count below
`warn_at` gives `Nothing`. A count from `warn_at` to `act_at - 1` gives
`Warn`. A count at or above `act_at` gives `Action(A)`.[^1]

Use separate ladder values for each count:

| Failure path | Input count | Evaluator | Action |
|---|---|---|---|
| Reindex churn | Consecutive reindexes with no upserts | `Ladder<LadderAction>` | `ForceMaintenance` |
| Supervisor crash loop | Quick panics after the intensity cap is exceeded | `Ladder<SupervisorAction>` | `RestartTask(name)` |
| Watchdog stall | No heartbeat progress for the stall interval | None | `WatchdogTrip`, then abort |

The supervisor ladder has fixed values of `warn_at: 3` and `act_at: 5`.
These values are not configuration settings.[^5] After the restart-intensity
cap is exceeded, that panic is strike 1. Each additional quick panic adds one
strike. With the default values, the action first occurs on quick panic 10:
five permitted panics, then five escalation strikes.[^2]

Escalation is sticky during a crash loop. A 60-second restart delay does not
clear the strike count. Only a task run that lasts for at least the configured
intensity window clears the backoff count and the strike count.[^2]

The escalation hook records `RestartTask(name)` and writes a log message. It
does not restart or stop the task. The normal supervisor loop continues to
restart the task.[^2]

Do not change the heartbeat when the supervisor restarts a task. A task that
repeatedly panics must still appear stalled to the watchdog. If a restart
changed the heartbeat, a crash loop could hide a stall.[^2]

Use `LadderAction` as the common status type for `ForceMaintenance`,
`RestartTask`, and `WatchdogTrip`. This type does not mean that all three
paths use `Ladder<A>`. The status report converts each variant to the wire
type with an explicit match.[^6]

## Alternatives rejected

- Use one configured ladder for all three paths. Rejected because each path
  measures a different condition. The watchdog has no increasing count.
- Reset the heartbeat after each restart. Rejected because this can make a
  crash loop appear healthy.
- Stop or restart the task in the escalation hook. Rejected because the
  supervisor already owns restart behavior. The hook only records the event.
- Put the supervisor thresholds in configuration. Rejected for this change
  because there was no operational requirement for more settings.

## Consequences

`daemon status` shows a lifetime `restarts=N` count for each task. It also
shows the most recent action from the common status type.[^6]

A permanent crash loop continues to produce `RestartTask(name)` actions
after strike 5. A healthy task run clears the escalation state. A watchdog
stall records `WatchdogTrip` and then aborts the process.[^2][^7]

[^1]: `crates/hallouminate-daemon/src/ladder.rs:1-51`
[^2]: `crates/hallouminate-daemon/src/supervisor.rs:29-45,109-113,133-263`
[^3]: `crates/hallouminate-config/src/lib.rs:228-235`
[^4]: `crates/hallouminate-daemon/src/churn.rs:1-9`; `crates/hallouminate-daemon/src/watchdog.rs:1-16`
[^5]: `crates/hallouminate-daemon/src/state.rs:549-558`
[^6]: `crates/hallouminate-daemon/src/status.rs:17-65`; `crates/hallouminate/src/cli.rs:338-394`
[^7]: `crates/hallouminate-daemon/src/server.rs:335-344`
