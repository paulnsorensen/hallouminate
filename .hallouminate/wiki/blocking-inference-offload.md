---
status: reviewed
last_verified: 2026-08-30
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

Paths below are post-#273: the daemon lived in `crates/hallouminate/src/daemon/`
at the time #217/#219 landed, and now lives in its own `hallouminate-daemon`
crate (see [architecture](architecture.md)). Citations here point at the
current crate layout, not the historical one.

## Wrapped (safe)

- Model load: `Embedder::try_new` under `spawn_blocking` in `init_embedder`
  (`crates/hallouminate-daemon/src/state.rs:67`).
- Single-file reindex (add_markdown path): `index_single_file[_with_content]`
  (`crates/hallouminate-daemon/src/dispatch.rs:1309,1325`) wrap the
  content-hash/plan/apply work in `block_in_place`
  (`crates/hallouminate-daemon/src/dispatch.rs:1352`). Note the doc comment:
  `block_in_place` panics on a current-thread runtime — tests of these paths
  must use the `multi_thread` flavor.
- Crossencoder rerank: `rerank_with_timeout` wraps in `spawn_blocking` with an
  explanatory comment (`crates/hallouminate-domain/src/ground/orchestrate.rs:13-27`) —
  the precedent the remaining gaps should copy.
- Filesystem ops in handlers (`read_no_follow`, `atomic_write_no_follow`, …)
  use `spawn_blocking` throughout `crates/hallouminate-daemon/src/dispatch.rs`
  (e.g. lines 607, 805).

## Inference offload (#217 — resolved)

Both hot embedding paths named in #217 — bulk-index (passage) and
ground-query embedding — are adapter-owned and no longer inline on tokio
workers. Bulk index (`LanceStore::apply_batch`,
`crates/hallouminate-adapters/src/lance.rs:1168`, embed call at line 1196)
and ground query (`LanceStore::vector_scan`, `lance.rs:1685`, embed call at
line 1695) both route through the private `run_embedding_blocking` helper
(`lance.rs:816-841`), which wraps the actual `EmbedBatch::embed_batch` call
in `tokio::task::spawn_blocking`. Neither call site embeds inline on a tokio
worker.

Worst-case worker starvation from a burst of embeds is resolved by this
offload — the embed work runs off the tokio worker pool. The remaining
failure mode, a client stuck waiting on a wedged daemon, is bounded by the
client-side per-class RPC timeouts added for #216 (`timeout_for`,
`crates/hallouminate-daemon/src/client.rs:232`).

## Lock granularity (#219 — resolved)

Embedding is adapter-owned: `LanceStore` holds
`embedder: Arc<std::sync::Mutex<Option<Box<dyn EmbedBatch>>>>`, locked fresh
per call inside `run_embedding_blocking`
(`crates/hallouminate-adapters/src/lance.rs:816-841`). `apply_batch`
(`lance.rs:1168`) calls it once per batch via `run_in_batches`
(`crates/hallouminate-domain/src/indexer/apply.rs:210`), so the lock is
released between batches — a concurrent `ground` query embed (`vector_scan`,
`lance.rs:1685`, same `run_embedding_blocking` call) can acquire it in the
gap rather than queuing for the whole bulk index.

See also: [ort-arena-retention](ort-arena-retention.md) for why resident
arena memory makes process topology matter,
[daemon-and-cli](daemon-and-cli.md) for the request-concurrency model, and
[supervisor-restart-ladder](supervisor-restart-ladder.md) for how a stuck
`spawn_blocking` task interacts with the watchdog rather than this offload
layer.

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-08-30 · Supersedes: —_
