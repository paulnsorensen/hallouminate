---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/161
  - https://github.com/paulnsorensen/hallouminate/issues/163
  - docs/adr/daemon-idle-exit-001..003.md
---
# Daemon idle exit

The daemon now exits after `[daemon].idle_exit_secs` of zero active connections instead of holding embedder and reranker models resident indefinitely. This replaces the earlier idle-session-eviction feature (#161), which tried to reclaim memory by dropping the `Embedder` on a timer and never actually released it back to the OS.[^1]

## Why eviction was the wrong lever

#161 shipped on the premise that dropping a fastembed session frees the memory it allocated. It doesn't: ONNX Runtime's CPU arena is a high-water-mark allocator that never shrinks, and `ort`'s `Drop` correctly tears down the session without ever returning the arena's extents to the OS. Each evict-and-reload cycle abandons one arena and grows a fresh one, so repeated eviction under sustained load left the daemon at a *higher* steady-state footprint than never evicting at all. Full diagnosis lives in [ort-arena-retention](ort-arena-retention.md); this page only covers what replaced it.[^2]

The fix could not be "evict smarter" — no session-level knob returns arena memory, and fastembed exposes no session-recreation path that avoids the multi-GB high-water mark for a 128MB-on-disk model. The only lever that actually reclaims memory is process exit, because that's the only point the OS gets the address space back.

## What idle-exit does instead

The daemon's supervisor tracks the last time any client connection closed. When zero connections are active and `idle_exit_secs` has elapsed since the last one closed, the daemon logs its exit reason and terminates cleanly — flushing any in-flight LanceDB writes first so no corpus is left mid-mutation.[^3] There is no eviction of individual sessions and no partial-memory-reclaim path; the unit of reclaim is the whole process.

Clients don't need to know the daemon exited. `client_for(None)` resolves the configured socket, and if the connect fails because nothing is listening, `ensure_daemon_running()` spawns a fresh daemon and retries once the socket is live. From the CLI or MCP caller's perspective, a died-of-idle daemon and a never-started daemon look identical — both cost one respawn.

## The cold-start cost this buys

Respawn is not free: the new process pays the same model-load cost the original daemon paid at boot, dominated by fastembed loading the passage/query embedding model and, if reranking is configured, the crossencoder model. That load is on the order of a few seconds on typical hardware, plus the memory high-water mark the arena grows to on the first embed call.

That tradeoff is deliberate: it converts an unbounded, ever-growing background memory footprint into a bounded, periodic one-time delay paid only by callers who show up after the idle window. A daemon serving continuous traffic never hits `idle_exit_secs` and never pays the cold start at all; a daemon serving one query per hour pays it on every query. The config default is tuned toward interactive developer use, where an idle daemon sitting at multi-GB resident memory between sessions is a worse cost than an occasional multi-second first query.

## Deprecated config

`embeddings.idle_evict_secs` — the #161 knob — is now a no-op. Setting it emits a startup warning and does nothing; the eviction code path it used to drive was removed rather than repurposed, since eviction's premise was invalid at any interval.

See [ort-arena-retention](ort-arena-retention.md), [daemon-and-cli](daemon-and-cli.md), [concurrency-and-supervision](concurrency-and-supervision.md).

[^1]: https://github.com/paulnsorensen/hallouminate/issues/161; docs/adr/daemon-idle-exit-001..003.md
[^2]: `crates/hallouminate-adapters/src/embedder.rs:80-140`; ort-arena-retention.md
[^3]: `crates/hallouminate-daemon/src/supervisor.rs:1-60`; https://github.com/paulnsorensen/hallouminate/issues/163

_Source: issues #161 and #163, ADR daemon-idle-exit-001..003 · Updated: 2026-07-18 · Supersedes: the 2026-05 idle-session-eviction design_