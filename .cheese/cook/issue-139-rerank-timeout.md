status: ok
next: press
artifact: .cheese/notes/issue-139-rerank-timeout.md
taste_test: deferred-to-orchestrator
Bounded Crossencoder::rerank to a 2s per-request timeout via spawn_blocking, falling back to un-reranked fusion order on timeout.

# Cook Report — issue-139-rerank-timeout

## Done
Per-request rerank timeout with fusion fallback (#139), finalized from a WIP cherry-pick (0c91bad): fixed compile fallout, ran all gates, amended into a clean commit.

## Changed
- `src/app/daemon/state.rs` — `CrossencoderGuard` now holds an `OwnedMutexGuard`, boxable as `Box<dyn Crossencoder>`; `impl Crossencoder for CrossencoderGuard` forwards via deref. Fixed compile fallout: dropped the unqualified `Result` import (it shadowed pre-existing 2-arg `std::result::Result` usages in `acquire_mutation_guard`/callers) and qualified the new `rerank()` return type as `crate::domain::common::Result<()>` instead; fixed the `crossencoder_guard_updates_last_use_on_drop` test's `lock_owned()` call by wrapping the `Mutex` in an `Arc` and removing a dead unused-guard binding.
- `src/domain/ground/orchestrate.rs` — added `RERANK_TIMEOUT` (2s) and `rerank_with_timeout()` (spawn_blocking + tokio::time::timeout); `ground()`/`ground_union()` take `Option<Box<dyn Crossencoder>>` and gate z-score normalization on `applied`. Two new unit tests: `rerank_with_timeout_returns_fusion_order_when_crossencoder_stalls`, `rerank_with_timeout_applies_the_rerank_on_the_fast_path`.
- `src/app/daemon/dispatch.rs` — `handle_ground` moves the crossencoder guard into a `Box<dyn Crossencoder>` instead of borrowing it.
- `tests/cross_repo_union.rs` — `ReversingCrossencoder` call sites pass `Some(Box::new(ReversingCrossencoder))` directly.

## Verified
- `cargo build --lib --tests`: pass (after fixing the `Result` shadowing + `lock_owned` fallout — none of this had ever compiled before this pass).
- `cargo fmt --all --check`: pass.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: no issues found.
- `cargo test --locked`: 816 passed, 9 ignored (15 suites) — includes both new `rerank_with_timeout_*` tests, the updated `crossencoder_guard_updates_last_use_on_drop`, and all 10 tests in `tests/cross_repo_union.rs`.

## Left / follow-ups
None — this was a mechanical gate-and-fallout pass on an already-complete design; taste-test deferred to the orchestrator per instructions. Not pushed (per task instructions).
