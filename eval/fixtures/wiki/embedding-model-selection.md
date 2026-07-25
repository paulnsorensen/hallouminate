---
status: reviewed
last_verified: 2026-07-10
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/142
  - crates/hallouminate-domain/src/embeddings.rs
---
# Embedding model selection

The default embedding model is Snowflake's `snowflake-arctic-embed-s`, a
384-dimension sentence embedding served locally through fastembed. It is
not a design centerpiece — it is the derived convenience that turns wiki
prose into something a vector index can rank, and the domain crate treats
it as a swappable policy rather than a load-bearing dependency.[^1]

## Why Arctic-S over a larger model

`hallouminate-domain::embeddings` enumerates the supported-model identity
policy: model name, dimension, and whether the runtime should load the
full-precision or quantized ONNX weights.[^2] Arctic-S was chosen over
larger Arctic and BGE variants because the wiki corpora it serves are
small — a repo's `.hallouminate/wiki/` rarely exceeds a few hundred
chunks — and retrieval quality at that scale is dominated by whether the
right page surfaces at all, not by marginal gains from a bigger encoder.
A 384-dim vector keeps the LanceDB index small and keeps embed latency low
enough that `add_markdown` can re-embed a single file inline, on every
write, without a visible pause.

## Full-precision vs quantized

fastembed ships both a full-precision (`f32`) and an INT8-quantized ONNX
export for Arctic-S. hallouminate defaults to full-precision: quantization
shrinks the on-disk model and speeds inference, but it also shifts the
embedding space slightly, and a corpus embedded under one precision cannot
be safely searched under the other without a full re-embed. Since the
model choice is already a policy value stored per corpus, switching
precision is treated the same as switching model name — it invalidates
existing vectors rather than silently drifting them.[^3]

## The `~/.cache/hallouminate/fastembed` cache

fastembed downloads model weights on first use and caches them under
`~/.cache/hallouminate/fastembed` (or the `XDG_CACHE_HOME` equivalent),
keyed by model name and precision. The first `add_markdown` or `ground`
call after a fresh install or a config change to a previously-unused model
pays a one-time download cost — tens of megabytes for Arctic-S — before
any embedding happens. Subsequent calls, including from a different repo
or worktree, reuse the same cache, because the cache is keyed by model
identity, not by corpus or repo.[^4]

## Why the model is not the product

The design-rationale page frames embeddings as a derived convenience: the
markdown wiki is canonical, and the LanceDB vectors exist to make that
wiki fast to search, not to be an artifact in their own right. That framing
extends to model choice. Swapping Arctic-S for a different model, or
bumping its precision, does not touch the wiki's content — it only
forces a re-embed of existing rows on next `index`, the same mechanism
that already runs after a schema bump. There is no migration script
because there is no state to migrate: the filesystem is still the source
of truth, and vectors are always reproducible from it.[^5]

See [design-rationale](design-rationale.md), [chunk-budget-and-tokenizer](chunk-budget-and-tokenizer.md), [lance-schema-versioning](lance-schema-versioning.md).

[^1]: `crates/hallouminate-adapters/src/embedder.rs:14-40`.
[^2]: `crates/hallouminate-domain/src/embeddings.rs:1-38`.
[^3]: https://github.com/paulnsorensen/hallouminate/issues/142
[^4]: `crates/hallouminate-adapters/src/embedder.rs:44-61`.
[^5]: `crates/hallouminate-domain/src/embeddings.rs:40-52`.

_Source: issue #142 and `hallouminate-domain::embeddings` · Updated: 2026-07-10_
