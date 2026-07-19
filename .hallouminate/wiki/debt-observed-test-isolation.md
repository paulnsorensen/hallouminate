# debt::OBSERVED test isolation — the Hard-recording coordination lock

`debt::OBSERVED` (`crates/hallouminate-daemon/src/debt.rs`) is a process-wide static `DebtCache` feeding both the backpressure mutation gate and `maintenance_loop`'s Hard→forced-run branch. Because the whole `cargo test` binary shares it across parallel tests, **any test that records `DebtLevel::Hard` into it silently breaks every concurrently running maintenance-defer test**: the loop reads `debt::level() == Hard`, skips the defer path, and the defer/forced-warn assertions fail. The failures look flaky and land in *other* files (`maintenance.rs`, `state.rs`) than the offending test.

## The rule

- A test that gets Hard into `OBSERVED` — directly, or indirectly via `acquire`/`refresh_observed` classifying a store with hard thresholds 0 — must hold `debt::OBSERVED_HARD_COORD.write().await` for its whole body.
- A loop-spawning test whose assertions break under an ambient Hard reading must hold `.read().await`.
- The lock is `#[cfg(test)]` `tokio::sync::RwLock` (a `std` RwLock guard across await points is rejected by clippy `await_holding_lock`, which the workspace denies).

## History

Found 2026-07-18 curing PR #268: a new acquire-integration test classified Hard through an empty store (thresholds 0) and never re-classified away, deterministically failing `due_pass_forced…` / `forced_pass_under_elevated…`. The pre-existing `hard_forced_pass_refreshes_the_debt_observation_afterward` test had the same latent race and passed only because its loop's post-pass refresh re-records Ok quickly. A record-Ok-at-end cleanup was tried first and was insufficient — paused-clock tests overlap the whole real-time window.

## Proper fix (deferred)

Move the cache into `DaemonStateInner` (per-daemon injection). Deferred as moderate-cost in the PR #268 affinage report; the coordination lock is the documented stopgap.
