# Search reliability loop ADR 003

## ADR-003: Rerank indexed search text and preserve display text  [status: accepted]

- **Context:** Embedding, FTS, and FM consume deterministic `search_text`, but the optional crossencoder currently scores display `SearchHit.text`. Footnote definitions can therefore re-enter second-stage ranking even though first-stage retrieval excludes them.
- **Decision:** Add `SearchHit.search_text`, decode it from the existing schema-v4 Lance column, and feed it to `FastembedCrossencoder::rerank`. Continue using `SearchHit.text` for document aggregation and public Ground snippets. Missing schema-v4 `search_text` is an error rather than a fallback.
- **Alternatives:** Recomputing footnote-stripped text at rerank time was rejected because it loses the indexed breadcrumb and file-summary context. A separate rerank wrapper was rejected because it adds conversion and trait plumbing for the same row data. Returning `search_text` publicly was rejected because it breaks the display contract.
- **Consequences:** Every ranking stage consumes the same contextualized representation while user-visible evidence remains unchanged. Adding a public `SearchHit` field is source-breaking for exhaustive external struct literals, but no trait signature, database schema version, or Ground JSON field changes.

The affected seams are `crates/hallouminate-domain/src/indexer/chunk.rs:51-70`, `crates/hallouminate-adapters/src/lance.rs:469-516`, and `crates/hallouminate-adapters/src/crossencoder.rs:58-108`.

_Source: approved search-reliability-loop spec · Updated: 2026-07-24 · Supersedes: —_