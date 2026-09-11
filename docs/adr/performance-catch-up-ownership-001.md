# One runtime catch-up scheduler with degraded reconciliation

## Decision

WatchRegistry schedules runtime discovery and later reconciliation.
Ground registers resolved corpora with that registry instead of submitting a second Provisioner pass.
The daemon removes the Provisioner loop and its task status slot from production and test builds.[^1]

The watcher pump remains active when native debouncer creation fails.
It uses registry notifications and periodic reconciliation without a native watcher.
This preserves a consumer for Ground discovery when the native backend is unavailable.

This failure is permanent for the pump's lifetime.
`spawn_corpus_watcher` builds the debouncer once, at startup.
The pump does not retry debouncer creation on any reconcile tick.
Recovery needs a daemon restart.
The supervisor re-enters the factory only after a panic, not after this failure.[^2]

## Rationale

The corpus guard serializes request mutations, but it does not remove duplicate scans.
One runtime scheduler removes the separate Provisioner submission.
Equivalent registrations already return without queueing new catch-up work.
The reconcile-only path avoids an unconsumed queue after native backend failure.

## Wiki destination

This decision extends ADR-004 in `worktree-index-provisioning-adr.md` and supersedes its production Provisioner description.
The same change marks ADR-001 superseded, replaces the Provisioner terms in `domain-model.md`, and updates the writer list in `daemon-and-cli.md`.

[^1]: crates/hallouminate-daemon/src/dispatch.rs::handle_ground
[^2]: crates/hallouminate-daemon/src/watch/mod.rs::spawn_corpus_watcher; crates/hallouminate-daemon/src/watch/mod.rs::PumpState::reconcile
