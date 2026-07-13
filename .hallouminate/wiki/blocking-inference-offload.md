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
reindex are wrapped; the two hottest inference paths are not (#217), and the
per-key embedder lock is held across entire bulk indexes (#219).

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

## Lock granularity gap (#219)

`handle_index` acquires the per-ResourceKey embedder guard and holds it across
the whole `index_corpus(...)` await (`src/app/daemon/dispatch.rs:532-543`), so
every `ground` on the same key queues on that mutex
(`src/app/daemon/state.rs:156-172`) for the full bulk index — minutes on large
corpora. Fix direction in #219: re-acquire per embed batch so grounds
interleave.

See also: [ort-arena-retention](ort-arena-retention.md) for why resident
arena memory makes process topology matter, and
[daemon-and-cli](daemon-and-cli.md) for the request-concurrency model.

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-07-13 · Supersedes: —_
