# Catch-up write-lane scope

Status: accepted in this change.

## Decision

Catch-up holds its corpus guard from the initial scan through plan application.
It acquires the global write lane only when the plan contains a mutation.
An unchanged corpus returns without acquiring the global write lane.

The lock order remains corpus guard, then global write lane.
Maintenance-debt admission still precedes both locks.
Boot catch-up, runtime provisioning, and watcher reconciliation use this contract.

## Reason

Filesystem scans and stored-file listings do not mutate the store.
Holding the global write lane during those operations delays unrelated corpus writes.
Releasing the corpus guard during planning permits a same-corpus writer to invalidate the plan.
The split avoids both failures without a second plan or a new stale-state protocol.

## Evidence

- `crates/hallouminate-daemon/src/dispatch.rs`: `catch_up_corpus`, `catch_up_corpus_inner`.
- `crates/hallouminate-daemon/src/backpressure.rs`: `acquire_corpus`, `acquire`.
- `crates/hallouminate-daemon/src/state.rs`: `acquire_corpus_guard`, `acquire_write_lane`.
- Regression: `unchanged_catch_up_does_not_hold_global_write_lane`.

## Wiki destination

Merge this decision into `.hallouminate/wiki/daemon-and-cli.md` and the catch-up provisioning ADR.
Hallouminate rejects wiki writes on 2026-09-07 because maintenance debt is Hard.
This tracked ADR preserves the decision without a direct wiki file edit.
