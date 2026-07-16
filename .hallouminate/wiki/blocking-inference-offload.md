---
status: reviewed
last_verified: 2026-07-13
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/217
  - https://github.com/paulnsorensen/hallouminate/issues/219
---
# Blocking-inference offload — coverage map

Which CPU-bound work hops off tokio worker threads, and which still runs
inline. Coverage is **partial** after #176: the model load and single-file
reindex are wrapped; the two hottest inference paths are not (#217). The
per-key embedder lock issue (#219) is resolved — see below.

## Wrapped (safe)

- Model load: `Embedder::try_new` under `block_in_place`
  (`src/app/daemon/state.rs:162`).
- Single-file reindex (add_markdown path): `index_single_file[_with_content]`
  use `block_in_place` (`src/app/daemon/dispatch.rs:1277,1328,1336`). Note the
  doc comment: `block_in_place` panics on a current-thread runtime — tests of
  these paths must use the `multi_thread` flavor.
- Crossencoder rerank: `rerank_with_timeout` wraps in `spawn_blocking` with an
  explanatory comment (`src/domain/ground/orchestrate.rs:12-26`) — the
  precedent the remaining gaps should copy.
- Filesystem ops in handlers (`read_no_follow`, `atomic_write_no_follow`, …)
  use `spawn_blocking` throughout `dispatch.rs`.

## Still inline on tokio workers (#217)

- Bulk index: `handle_index → index_corpus → run_in_batches` calls
  `embed_batch` directly inside an async fn (`src/domain/indexer/apply.rs:245`).
- Ground query embed: `embed_query` is a sync fn invoked inline on the ground
  path (`src/domain/ground/orchestrate.rs:169-183`).

A burst of embeds across distinct ResourceKeys can occupy multiple worker
threads simultaneously; the worst case starves the accept loop and the daemon
appears dead to every client — which then hangs indefinitely because tool RPCs
have no client-side timeout (#216).

## Lock granularity (#219 — resolved)

Embedding is adapter-owned since the US-002 refactor: `LanceStore` holds
`embedder: Arc<std::sync::Mutex<Option<Box<dyn EmbedBatch>>>>`
(`crates/hallouminate-adapters/src/lance.rs:548`), locked fresh per call
inside `run_embedding_blocking` (lines 626-651). `apply_batch`
(lines 939-1009) calls it once per batch via `run_in_batches`
(`crates/hallouminate-domain/src/indexer/apply.rs`), so the lock is released
between batches — a concurrent `ground` query embed (`hybrid_search`,
`lance.rs:1278-1336`, same `run_embedding_blocking` call) can acquire it
in the gap rather than queuing for the whole bulk index.

See also: [ort-arena-retention](ort-arena-retention.md) for why resident
arena memory makes process topology matter, and
[daemon-and-cli](daemon-and-cli.md) for the request-concurrency model.

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-07-13 · Supersedes: —_
