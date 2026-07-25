---
status: reviewed
last_verified: 2026-07-22
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/188
  - https://github.com/paulnsorensen/hallouminate/issues/204
---
# Observability and tracing

The daemon emits structured `tracing` events under a namespacing convention that mirrors the crate/slice layout described in [architecture](architecture.md), so an operator filtering by target can isolate one concern — indexing, search, the socket transport — without wading through everything else the daemon logs.[^1]

## Target namespacing

Targets follow `hallouminate::<slice>::<concern>`, for example `hallouminate::indexer::apply`, `hallouminate::ground::search`, `hallouminate::daemon::socket`. This is deliberately parallel to the domain crate's slice organization (`corpus`, `ground`, `indexer`, `search` — see [architecture](architecture.md)) rather than one flat target per crate, because a real debugging session is almost always scoped to one slice's behavior: "why did this search rank oddly" stays inside `hallouminate::ground::*` and `hallouminate::search::*`, and never needs the indexer's or the socket transport's events turned on at all. `RUST_LOG=hallouminate::ground=debug` isolates exactly that slice without a custom filter expression.

## Warn versus debug

Events at `warn` are reserved for conditions an operator should look at without being told to — a degraded auxiliary pass, a config value that fell back to a default because the declared one was invalid, a scan that hit its supervisor deadline (see [concurrency-and-supervision](concurrency-and-supervision.md)). Everything else — the per-file chunk counts during a scan, the ranked candidate scores inside a search, per-request timing — is `debug`, because it's only useful when actively investigating a specific request and would otherwise drown the warn-level signal in volume. The dividing line is not severity of the underlying event but whether an operator who is *not already looking for a problem* needs to see it. A failed embed call is `warn` because nobody would think to enable debug logging in advance of it happening; a slow-but-successful chunk pass is `debug` because it's only interesting in the context of an investigation someone already started.

## Why a degraded auxiliary pass logs rather than fails

Some ranking and enrichment passes — a crossencoder rerank, a keyword-extraction pass over a chunk — are improvements over the base result, not requirements for producing one. When one of these passes errors (a model call times out, a malformed input reaches the reranker), the daemon logs a `warn` event naming which pass degraded and why, then falls back to the result it would have returned without that pass, rather than failing the whole request. Failing an entire `ground` call because reranking hiccuped would make a caller's search less available for a failure mode that only cost them ranking quality, not correctness — the base retrieval result is still real and still usable. The warn log is what keeps that silent-to-the-caller degradation visible to an operator instead of actually silent everywhere.

## Where an operator should look first

When results look wrong — missing hits, stale content, unexpected ranking — the first place to check is whether any `warn`-level event fired for the request's corpus in the relevant window: a degraded rerank pass, a supervisor-killed scan, a config fallback. Those explain most "results look wrong" reports without needing debug logging at all. If nothing at `warn` explains it, the next step is `corpus_stats` (see [mcp-surface](mcp-surface.md)) to check whether the corpus is actually indexed and current — a stale or partially-indexed corpus produces results that look wrong for reasons that have nothing to do with search ranking. Only after ruling both of those out is it worth enabling `debug` on the specific slice (`ground`, `search`, or `indexer`) implicated by the symptom.

See [architecture](architecture.md), [concurrency-and-supervision](concurrency-and-supervision.md), [mcp-surface](mcp-surface.md).

[^1]: `crates/hallouminate-daemon/src/logging.rs:1-40`; https://github.com/paulnsorensen/hallouminate/issues/188
[^2]: `crates/hallouminate-domain/src/ground/rerank.rs:60-95`; https://github.com/paulnsorensen/hallouminate/issues/204

_Source: issues #188 and #204 · Updated: 2026-07-22_