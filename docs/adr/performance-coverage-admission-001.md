# Daemon-wide coverage admission

Status: accepted in this change.

## Decision

Ground and corpus statistics share a six-permit coverage semaphore.
A permit covers the complete indexed-path scan and filesystem walk.
The blocking walk owns the permit until it exits, even if the request is cancelled.

This limit is independent of the six-root search limit and the global write lane.
It bounds simultaneous coverage operations, not total memory or end-to-end latency.

## Reason

A per-request limit permits each connection to start another set of coverage operations.
A request-owned permit releases too early if cancellation detaches an active blocking walk.
The daemon-wide permit and blocking-task ownership prevent both failures.

## Evidence

- `crates/hallouminate-daemon/src/state.rs`: `MAX_CONCURRENT_COVERAGE_CHECKS`, `coverage_gate`.
- `crates/hallouminate-daemon/src/dispatch.rs`: `corpus_coverage`, `collect_coverage_warnings`.
- Regression: `ground_coverage_caps_real_scans_and_releases_cancelled_walks`.

## Wiki destination

Merge this decision into `.hallouminate/wiki/daemon-and-cli.md` under coverage admission.
Hallouminate rejects two write attempts on 2026-09-07 because maintenance debt is Hard.
This tracked ADR preserves the decision without a direct wiki file edit.
