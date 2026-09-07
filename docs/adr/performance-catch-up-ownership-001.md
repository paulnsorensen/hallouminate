# One catch-up scheduler and durable task ownership

## Decision

WatchRegistry owns initial discovery and later reconciliation scheduling.
Ground registers resolved corpora with that registry instead of submitting a second Provisioner pass.
The production daemon no longer starts the separate Provisioner loop.[^1]

One active task token exists independently of a registration identity.
Root rekey keeps that active token until its task completes.
Completion also checks the registration generation before it changes registration state.
A stale completion cannot change a replacement registration.[^2]

The watcher pump remains active when native debouncer creation fails.
It uses registry notifications and periodic reconciliation without a native watcher.
This preserves a consumer for Ground discovery when the native backend is unavailable.[^3]

## Rationale

A corpus mutation lock prevents concurrent writes, but it does not remove duplicate scans.
One scheduler removes duplicate discovery submissions.
A task token tracks running work more accurately than a mutable registration status.
The reconcile-only path avoids an unconsumed queue after native backend failure.

## Wiki destination

This decision extends ADR-004 in `worktree-index-provisioning-adr.md` and supersedes its production Provisioner description.
It also updates the provisioning terms in `domain-model.md`.
The wiki mutation gate returns a Hard-debt timeout during this repair.
This tracked ADR preserves the decision until wiki writeback succeeds.

[^1]: crates/hallouminate-daemon/src/dispatch.rs; crates/hallouminate-daemon/src/state.rs::DaemonState
[^2]: crates/hallouminate-daemon/src/watch/registry.rs::CatchUpToken; crates/hallouminate-daemon/src/watch/registry.rs::WatchRegistry
[^3]: crates/hallouminate-daemon/src/watch/mod.rs::run_pump
