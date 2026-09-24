# Watcher batched reconcile — ADRs

These are the decisions for the approved `watcher-batched-reconcile` spec (mold session 2026-09-23). The spec follows the 2026-09-22 fseventsd incident, where a missing `.cheese` root caused repeated FSEvents stream restarts.

## ADR-001: Batch each reconcile through Debouncer::update_paths on notify release candidates [status: accepted, implemented]

- **Context:** <certain> The watcher pump owns one `notify_debouncer_full::Debouncer` for every corpus root (`crates/hallouminate-daemon/src/watch/mod.rs`, `PumpState`). notify-debouncer-full 0.7.0 exposes only per-path `watch` and `unwatch`. In notify 8.2.0 each call stops and rebuilds the whole macOS FSEvents stream (`fsevent.rs` `watch_inner`/`unwatch_inner`). A reconcile that changes K roots restarts the stream K times, and the daemon accepts up to 256 runtime registrations. The debouncer cannot pass notify 8's `paths_mut()` through: it keeps a private root list that must match the watcher, and it deprecated its `watcher()` accessor to a no-op for that reason. Upstream fixed this in notify 9.0.0-rc.2 with `Watcher::update_paths(Vec<PathOp>)` (notify-rs/notify#705), which reports partial failure through `UpdatePathsError { source, origin, remaining }`. notify-debouncer-full 0.8.0-rc.1 added `Debouncer::update_paths` and records only the applied operations in its root list.
- **Decision:** Pin `notify = "=9.0.0-rc.5"` and `notify-debouncer-full = "=0.8.0-rc.2"`. `PumpState::reconcile` builds one `Vec<PathOp>` (unwatches, then watches) and calls `Debouncer::update_paths` once. On failure it records the applied prefix, marks the `origin` root degraded for tick-only retry, and resubmits `remaining` until it is empty. An empty delta makes no native call.
- **Alternatives:** (A) Do nothing beyond the missing-root fix: keeps one full-stream restart per root change. (B) Replace the debouncer with a bare `RecommendedWatcher`, `paths_mut()`, and a custom pump debounce: rejected once upstream batch support was found; it adds custom code the library already provides. (C) One watcher per root on macOS: up to about 257 streams and threads at the registration cap, and one inotify instance per root on Linux. (D) Wait for stable notify 9: rejected by the user in favour of the release candidates now.
- **Consequences:** A successful batch uses one FSEvents restart. Failed path operations can require more calls for the remaining operations. An empty delta makes no native call. The daemon depends on exact release-candidate pins until stable notify 9.0.0 and notify-debouncer-full 0.8.0 publish (follow-up F004). notify 9.0.0-rc.4 keeps the watched-path form in `Event.paths`, and debouncer 0.8.0-rc.2 emits `remove` after `create`; the daemon watcher integration test guards both.
- **Gotchas found in implementation:**
  - notify 9 reports FSEvents event paths in the watched form (`/var/...`), not the canonical form (`/private/var/...`). `owning_corpus` and stored file refs use the canonical form. `canonical_event_path` re-roots each event path from `WatchRoot.watched` onto `canonical_watched` before ownership lookup.
  - An `unwatch` for a path the native watcher does not hold fails and consumes one extra `update_paths` call. Tests install paths natively before seeding `PumpState.installed`. Failed removals retain native ownership and their desired-mode intent. An unchanged removal or mode transition retries only on a reconcile tick; changed intent can retry immediately.[^restart-recovery]
  - `origin: None` means every path operation applied, but the FSEvents stream did not restart. The pump records those operations and retains surviving native registrations in `installed`. It marks those registrations degraded and schedules tick-only retries. Removed registrations still produce `Unwatch` operations before retry. An unwatch-only failure degrades and retries every surviving registration. Repeated watches restore delivery without losing native path ownership.[^restart-recovery]
  - A mode change (non-recursive to recursive) on one path emits `Unwatch` then `Watch` for that path in one batch. The watch loop skips a path only when `installed` holds the same mode and no retry is pending.
  - `canonical_event_path` picks the deepest match across both the `watched` and `canonical_watched` forms, so a nested symlinked root wins over a shallower plain root.
  - Each batch sorts its operations by path, so the batch order and the `remaining` split are deterministic.

## Related

- The watch registry design: `worktree-index-provisioning-adr.md` ADR-004.
- The missing-root fix lands before this change: missing roots get no native call, and failed installs retry only on the reconcile tick.


[^restart-recovery]: `crates/hallouminate-daemon/src/watch/mod.rs`: `build_reconcile_ops`, `apply_reconcile_ops`, and `degrade_installed`.

_Source: PR #535 review and native registration recovery · Updated: 2026-09-23 · Supersedes: clearing installed registrations after stream restart failure._

