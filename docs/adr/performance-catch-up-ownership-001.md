# One runtime catch-up scheduler with degraded reconciliation

## Decision

WatchRegistry schedules runtime discovery and later reconciliation.
Ground registers resolved corpora with that registry instead of submitting a second Provisioner pass.
The daemon removes the Provisioner loop and its task status slot from production and test builds.[^1]

The watcher pump remains active when native debouncer creation fails.
It uses registry notifications and periodic reconciliation without a native watcher.
This preserves a consumer for Ground discovery when the native backend is unavailable.[^2]

## Rationale

The corpus guard serializes request mutations, but it does not remove duplicate scans.
One runtime scheduler removes the separate Provisioner submission.
Equivalent registrations already return without queueing new catch-up work.
The reconcile-only path avoids an unconsumed queue after native backend failure.

## Wiki destination

This decision extends ADR-004 in `worktree-index-provisioning-adr.md` and supersedes its production Provisioner description.
It also updates the provisioning terms in `domain-model.md`.
The final wiki update remains deferred until the related code PRs land.

[^1]: crates/hallouminate-daemon/src/dispatch.rs; crates/hallouminate-daemon/src/state.rs::DaemonState
[^2]: crates/hallouminate-daemon/src/watch/mod.rs::run_pump
