---
status: reviewed
last_verified: 2026-07-12
confidence: medium
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/168
  - crates/hallouminate-adapters/src/crossencoder.rs
---
# Crossencoder reranking

`ground` supports an optional crossencoder rerank stage that runs after
the initial vector-and-exact-match retrieval has already produced a
ranked candidate list. It defaults to `None` — off — and most corpora
never enable it. The stage exists for the cases where fusion order alone
does not separate a genuinely relevant chunk from a lexically similar but
off-topic one, at the cost of a second model call per query.[^1]

## Why it runs after retrieval, not instead of it

The crossencoder scores a query against a small number of already-fused
candidates rather than the whole corpus. Crossencoders are pairwise —
query and passage together, not independently embedded — which makes
them more discriminating than the embedding-distance comparison but far
too slow to run over every chunk in a corpus. Retrieval narrows the field
first; reranking only ever reorders what retrieval already surfaced, so a
relevant chunk that retrieval missed entirely cannot be recovered by the
rerank stage.[^2]

## Why it defaults to off

Two costs kept crossencoder rerank opt-in rather than default-on. First,
it is a second model load and a second inference pass per query, which
roughly doubles `ground`'s latency floor on a cold cache. Second, the
quality win is corpus-dependent: for a small, well-curated wiki where
fusion order already ranks the right page first, reranking mostly
reshuffles ties and adds latency for no visible benefit. `hallouminate`
would rather ship a fast default and let a repo opt in once it has
evidence that its own corpus benefits, than pay the cost everywhere on
the assumption that it will.[^3]

## The 2000ms budget and fallback

When a corpus does enable reranking, the crossencoder port is called
under a `rerank_timeout_ms` budget of 2000ms. If the model call has not
returned within that window, `ground` abandons the rerank attempt and
falls back to the fusion order that retrieval already computed — it does
not block the caller waiting for a slow model, and it does not fail the
request. The timeout exists because ONNX inference on a cold or
memory-pressured host can spike well past what a query-time caller is
willing to wait, and a stale-but-fast ranking beats a fresh-but-late
one for an interactive search path.[^4]

## Supported models

Two crossencoder models are supported through the same fastembed-backed
port: `jina-reranker-v1-turbo-en` and `bge-reranker-base`. Both are loaded
lazily on first use and cached the same way the embedding model is, under
the fastembed cache directory. Neither is required — a corpus with no
`[search].crossencoder` configured never touches this code path at all.[^5]

See [embedding-model-selection](embedding-model-selection.md), [architecture](architecture.md), [design-rationale](design-rationale.md).

[^1]: `crates/hallouminate-domain/src/search.rs:1-24` (crossencoder port).
[^2]: `crates/hallouminate-adapters/src/crossencoder.rs:1-30`.
[^3]: https://github.com/paulnsorensen/hallouminate/issues/168
[^4]: `crates/hallouminate-domain/src/ground.rs:88-121`.
[^5]: `crates/hallouminate-adapters/src/crossencoder.rs:34-58`.

_Source: issue #168 and `hallouminate-adapters::crossencoder` · Updated: 2026-07-12_
