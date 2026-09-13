# Catch-up write-lane scope

Status: accepted in PR #493; this record preserves the decision from PR #470.

## Decision

Catch-up holds its corpus guard from the initial scan through plan application.
It acquires the global write lane only when the plan contains a mutation.
An unchanged corpus returns without acquiring the global write lane.

The lock order remains corpus guard, then global write lane.
Maintenance-debt admission precedes both locks.
Boot catch-up and watcher reconciliation use this contract.

## Reason

Filesystem scans and stored-file listings do not mutate the store.
Holding the global write lane during those operations delays unrelated corpus writes.
The corpus guard prevents another request mutation from invalidating the plan during that scan.

This guarantee does not include orphaned-root garbage collection.
`maintenance::gc_delete` holds the write lane, but it does not acquire the corpus guard.
This split does not establish plan isolation from garbage collection.

## Evidence

- `crates/hallouminate-daemon/src/dispatch.rs`: `catch_up_index`, `catch_up_corpus`.
- `crates/hallouminate-daemon/src/watch/mod.rs`: `spawn_registration_catch_up`.
- `crates/hallouminate-daemon/src/state.rs`: `lock_corpus`, `acquire_write_lane`.
- Regressions: `catch_up_slow_scan_does_not_hold_write_lane`, `catch_up_scan_in_flight_blocks_same_corpus_guard`, `catch_up_apply_holds_lane_until_released`.

## Wiki destination

Merge this decision into `.hallouminate/wiki/daemon-and-cli.md` and the catch-up provisioning ADR during the final wiki update.
