status: blocked: out of context
next: cook
artifact: .cheese/notes/issue-139-rerank-timeout.md
Implemented #139 per-request rerank timeout with fusion fallback; build verification was in flight when context ran out.

# Issue #139 — rerank timeout with fusion fallback

## Goal
Bound `Crossencoder::rerank` to `RERANK_TIMEOUT` (2s) via `spawn_blocking` + `tokio::time::timeout`, falling back to un-reranked fusion order on timeout, per the locked design in the task prompt (original GitHub issue #139).

## Done (implementation complete, NOT yet verified by a green build/test run)

1. `src/app/daemon/state.rs`
   - `CrossencoderGuard` converted from `MutexGuard<'a, ...>` to `OwnedMutexGuard<HashMap<String, FastembedCrossencoder>>` (dropped the lifetime param).
   - `crossencoder()` now does `Arc::clone(&self.inner.crossencoders).lock_owned().await` instead of `self.inner.crossencoders.lock().await`.
   - Added `impl Crossencoder for CrossencoderGuard` (forwards via `(**self).rerank(query, hits)`).
   - Added imports: `Crossencoder` from `crate::domain::search`, `SearchHit` from `crate::adapters::lance`, `Result` from `crate::domain::common`.
   - Updated the `crossencoder_guard_updates_last_use_on_drop` unit test (~line 1038) to build the guard via `Mutex::new(HashMap::new()).lock_owned().await`.

2. `src/domain/ground/orchestrate.rs`
   - Added `const RERANK_TIMEOUT: Duration = Duration::from_secs(2);` (comment cites #139 and crossencoder.rs:6,30-32's ~1.25s typical worst case).
   - Added `async fn rerank_with_timeout(crossencoder: Box<dyn Crossencoder>, query: String, hits: Vec<SearchHit>, timeout: Duration) -> Result<(Vec<SearchHit>, bool)>` — spawns the rerank on `spawn_blocking`, clones hits up front as the fallback, returns `(hits, applied)`.
   - `ground()` and `ground_union()` signatures changed: `crossencoder: Option<&mut dyn Crossencoder>` → `Option<Box<dyn Crossencoder>>`. Both call sites now call `rerank_with_timeout` and gate the `normalize_scores`/`z_score` stamping on `applied`.
   - Added two new unit tests in the `mod tests` block: `rerank_with_timeout_returns_fusion_order_when_crossencoder_stalls` (sleeping stub, 20ms timeout, asserts `applied == false` and order unchanged) and `rerank_with_timeout_applies_the_rerank_on_the_fast_path` (fast reversing stub, asserts `applied == true` and order reversed). Both use a local `hit_for_timeout_test` helper building a minimal `SearchHit`.

3. `src/app/daemon/dispatch.rs` (`handle_ground`, ~line 370-423)
   - `crossencoder` guard is no longer kept `mut`/borrowed; it's moved into `crossencoder_box: Option<Box<dyn Crossencoder>>` via `.map(|g| Box::new(g) as Box<dyn Crossencoder>)`, then passed by value into `ground`/`ground_union`.
   - Removed the now-invalid `drop(crossencoder);` line (crossencoder is consumed by the ground call); `drop(embedder);` remains.

4. `tests/cross_repo_union.rs` — the two call sites using `ReversingCrossencoder` (around former lines 260-266 and 434-440) changed from `let mut crossencoder = ReversingCrossencoder; ... Some(&mut crossencoder)` to `Some(Box::new(ReversingCrossencoder))` directly (dropped the intermediate `let mut crossencoder` binding).

## NOT done / left to verify

- **`cargo build --lib --tests` was launched in the background (task id `bt4s8puva`) and had not produced output yet when I hit the context ceiling.** Check `/private/tmp/claude-501/-Users-paul-conductor-workspaces-hallouminate-doha-v1/55dbb3e5-59fa-4bfb-9131-3a4f5e6e67e2/tasks/bt4s8puva.output` if still present, otherwise rerun `cargo build --lib --tests` fresh.
- `cargo fmt --check` not run — the orchestrate.rs edit likely left a missing blank line between the new `rerank_with_timeout` fn and the following `relative_path_for` doc comment. Run `cargo fmt` (apply) first, diff to confirm no semantic drift, then `cargo fmt --check` for the gate.
- `cargo clippy --all-targets` not run — confirm exact flags via justfile/CI config first, then run.
- `cargo test` not run at all — need to confirm: (a) the two new `rerank_with_timeout_*` unit tests pass, (b) `crossencoder_guard_updates_last_use_on_drop` still passes with the `lock_owned` change, (c) all of `tests/cross_repo_union.rs` passes with the `Box::new(ReversingCrossencoder)` change, (d) no other workspace test broke from the `ground`/`ground_union` signature change (grepped all callers — only `dispatch.rs` and `tests/cross_repo_union.rs` pass `Some(...)` for crossencoder; every other call site passes `None`, which is signature-compatible with `Option<Box<dyn Crossencoder>>`).
- Not yet committed. Per the task instructions, COMMIT as soon as gates pass (conventional commit referencing #139); if bailing again, `git add -f` and commit whatever partial state exists so it survives worktree cleanup.
- The actual `/cook` handoff slug at `.cheese/cook/<slug>.md` still needs to be written after gates pass, with `taste_test: deferred-to-orchestrator`.

## Resume steps
1. Check/rerun `cargo build --lib --tests` — fix any compile errors (most likely spot: the `rerank_with_timeout` orchestrate.rs edit, or the `(**self).rerank(...)` double-deref in state.rs's `impl Crossencoder for CrossencoderGuard`).
2. `cargo fmt` (apply) then `cargo fmt --check` (gate).
3. `cargo clippy --all-targets` (confirm flags from justfile/CI first).
4. `cargo test` — full suite, confirm green, no skips.
5. Commit via the `commit` skill: conventional commit, e.g. `fix(daemon): bound crossencoder rerank with a per-request timeout (#139)`, touching `src/app/daemon/state.rs`, `src/domain/ground/orchestrate.rs`, `src/app/daemon/dispatch.rs`, `tests/cross_repo_union.rs`.
6. Write `.cheese/cook/issue-139-rerank-timeout.md` handoff slug: `status: ok`, `next: press` (or per orchestrator's instruction), `taste_test: deferred-to-orchestrator`.
7. Do NOT push (per original task instructions).
