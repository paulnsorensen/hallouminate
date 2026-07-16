---
status: reviewed
last_verified: 2026-07-16
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/217
  - https://github.com/paulnsorensen/hallouminate/issues/219
---
# Blocking-inference offload — coverage map

Which CPU-bound work hops off tokio worker threads, and which still runs
inline. Coverage is now **complete**: the model load, single-file reindex,
and the two hottest inference paths (#217) are wrapped; the per-key embedder
lock issue (#219) is also resolved — see below.

## Wrapped (safe)

- Model load: `Embedder::try_new` under `spawn_blocking` in `init_embedder`
  (`crates/hallouminate/src/daemon/state.rs:94`).
- Single-file reindex (add_markdown path): `index_single_file[_with_content]`
  (`crates/hallouminate/src/daemon/dispatch.rs:1239,1255`) wrap the
  content-hash/plan/apply work in `block_in_place`
  (`crates/hallouminate/src/daemon/dispatch.rs:1279`). Note the doc comment:
  `block_in_place` panics on a current-thread runtime — tests of these paths
  must use the `multi_thread` flavor.
- Crossencoder rerank: `rerank_with_timeout` wraps in `spawn_blocking` with an
  explanatory comment (`crates/hallouminate-domain/src/ground/orchestrate.rs:11-26`) —
  the precedent the remaining gaps should copy.
- Filesystem ops in handlers (`read_no_follow`, `atomic_write_no_follow`, …)
  use `spawn_blocking` throughout `dispatch.rs`.

## Inference offload (#217 — resolved)

Both hot embedding paths named in #217 — bulk-index `embed_batch` and
ground-query `embed_query` — are adapter-owned since the US-002 refactor
(`crates/hallouminate-domain/src/ground/orchestrate.rs:318`) and no longer
inline on tokio workers. Bulk index and ground query both route through
`LanceStore::run_embedding_blocking`, which wraps the actual `embed_batch`
call in `tokio::task::spawn_blocking`
(`crates/hallouminate-adapters/src/lance.rs:626`, closure body through 651).
The old inline sites (`embed_query` and the direct `apply.rs:245` call) no
longer exist in the domain crate.

Worst-case worker starvation from a burst of embeds is resolved by this
offload — the embed work runs off the tokio worker pool. The remaining
failure mode, a client stuck waiting on a wedged daemon, is bounded by the
client-side per-class RPC timeouts added for #216 (`timeout_for`,
`crates/hallouminate/src/daemon/client.rs`).

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

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-07-16 · Supersedes: —_
