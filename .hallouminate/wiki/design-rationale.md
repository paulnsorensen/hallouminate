---
status: reviewed
last_verified: 2026-06-30
sources:
  - .moshi/uploads/deep-research-report.md (LLM Wikis After the Karpathy Surge)
---
# Design rationale & non-goals

What hallouminate *is*, what it deliberately *isn't*, and why. This is the
"why this design not that one" page — for the *how* see
[architecture](architecture.md) and [daemon-and-cli](daemon-and-cli.md).

## Positioning: a compiled-memory layer, not a RAG dump

hallouminate is a repo-local implementation of the three-layer LLM-wiki
pattern: **immutable raw sources → an LLM-maintained markdown wiki → a
schema/instruction layer** that teaches the agent how to ingest, answer, and
maintain the wiki (the `SERVER_INSTRUCTIONS` + `wiki-conventions.md` pair).
The point is *knowledge accumulation*, not query-time retrieval: instead of
re-deriving the same facts from raw chunks on every question, the wiki
preserves synthesis, contradictions, and cross-links as a durable artifact.

This is why the wiki is the primary surface and embeddings are a derived
convenience, not the product. A good LLM wiki sits *between* raw sources and
answers — query the wiki first, fall back to raw retrieval only when evidence
is weak or freshness matters.

## Filesystem is the source of truth; LanceDB is derived

The on-disk markdown is canonical. LanceDB rows are derived state, refreshed
after `add_markdown` / `delete_markdown`; `index` is the only way to pick up
edits made outside hallouminate (`src/app/mcp/tools.rs:67-69`). The
consequence: the wiki stays human-readable and git-versionable, and the vector
store can always be rebuilt from disk. A database-first design would be more
transactional but less inspectable and less agent-friendly — re-derivability
and provenance are the priority here.

## Non-goals

hallouminate deliberately does **not** do these, so don't add them as if they
were missing features:

- **No enforced markdown content schema.** `add_markdown` stores content
  verbatim and imposes no schema — it only returns advisory lint warnings
  (empty links, empty mermaid blocks, heading-level jumps). The
  `wiki-conventions.md` rules (one topic per file, H1 first line, kebab slug)
  are conventions the *author* honours, not constraints the writer enforces.
- **No structural code intelligence.** Markdown memory is not an AST, a symbol
  graph, or CI truth, and is not meant to replace them. Code-level
  "where is X / what calls Y" belongs to tilth/serena; hallouminate captures
  the durable *why*, not the symbol-level *what*.

These are boundaries, not gaps. A wiki page is a compression layer: it
preserves structure while losing exactness, which is also why high-stakes
answers should stay quote-first against raw sources rather than trusting a
summarised page.

_Source: deep-research report "LLM Wikis After the Karpathy Surge" · Updated: 2026-06-30_
