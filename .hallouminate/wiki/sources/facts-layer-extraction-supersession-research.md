---
status: draft
confidence: speculating
---
# Research: facts-layer extraction, supersession, inline retrieval, and prior art

An agent-run deep-tier `/briesearch` report for the hallouminate repository, published 2026-11-01, supports one finding: no source tests the facts-layer fork options directly, but each fork has partial evidence on 2025-2026 models.
Canonical source: cheese research corpus file `research/facts-layer-extraction-supersession-prior-art/facts-layer-extraction-supersession-prior-art.md` (local, outside git).
The report passed `ground-check` with 0 errors and `budget-check` with no findings; it used 19 searches, 24 extracts, and 6 researcher agents.

## Supported claims of the follow-up facts-layer report

The follow-up facts-layer report covers four gaps that the first two reports left open.

- Extraction: under a closed 96-relation ontology, 2026 models reach 84.0% entity F1 but only 21.7% to 39.9% triple F1 (GPT-5.1, Gemini 3 Pro, Claude Opus 4.5). Open extraction without normalization gives nearly one relation type per edge.
- Supersession: Wikibase, nanopublications, and Graphiti keep a changed fact and mark it. On the May 2026 STALE benchmark, the best model (Gemini-3.1-pro) scores 55.2% at handling outdated memories, and GPT-5.4 scores 15.7%.
- Inline retrieval: a metadata prefix inside embedded chunk text raised Context@5 from 33.3 to 55.0 (text-embedding-3-small) and from 26.7 to 61.7 (bge-m3) in a 2026 study.
- Prior art: Mem0 v2 writes add-only with exact-hash deduplication, and a user reported that contradictory facts are both stored.

## Relevance of the follow-up facts-layer report to hallouminate

The follow-up facts-layer report is the evidence base for [[facts-layer-extraction-evidence]] and [[facts-layer-supersession-evidence]].
It adds an inline-retrieval section to [[facts-layer-store-evidence]] and a memory-product section to [[facts-layer-landscape]].
It bears on all four forks in [[facts-layer-open-forks]] and records no design decision.

## Limitations of the follow-up facts-layer report

The follow-up facts-layer report has `speculating` confidence.
No source compares schema-first and schema-free extraction on the same data with 2025-2026 models.
No source tests whether a marked superseded fact gives better answers than a deleted one.
No source tests inline fact lines against a sidecar index.
STALE has no Claude results, and most memory-product numbers are vendor-reported.

_Source: cheese research corpus `research/facts-layer-extraction-supersession-prior-art/facts-layer-extraction-supersession-prior-art.md` · Updated: 2026-11-01_
