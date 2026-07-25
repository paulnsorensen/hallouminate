---
status: draft
last_verified: 2026-07-24
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067056188
---
# Ground search evaluation

Ground ranking changes must be decided by a discriminative, model-backed evaluation rather than the current saturated golden set. Issue #288 proposes file-level retrieval metrics, chunk-level ordering assertions, latency measurement, and a scheduled regression gate before any crossencoder default changes.[^1]

## Why the current harness cannot decide

The frozen fixture contains 16 wiki pages and 26 queries whose wording was lifted from their target pages. Four variants already score Recall@5 = 1.000, with MRR between 0.981 and 1.000, so the set cannot show whether embeddings or reranking earn their cost.[^2]

The existing harness still supplies useful machinery:

- a reproducible frozen corpus;
- lexical-only, fusion-only, lexical-plus-rerank, and fusion-plus-rerank variants;
- file-level Recall@5 and MRR;
- a 50-chunk candidate pool and top-10 file rollup;
- model-dependent execution isolated behind an ignored test.[^3]

## Proposed golden-query contract

Each query keeps its file-level expectation and gains an expected top chunk:

```json
{
  "id": "footnote-inversion",
  "query": "why are Hallouminate's daemon and config layers separate from the application crate",
  "expected": ["architecture.md"],
  "expected_chunk": {
    "file": "architecture.md",
    "heading_prefix": "Architecture"
  }
}
```

The discriminative set must include:

- the reproduced footnote inversion, where `architecture.md` prose must rank above its citation-definition chunk;
- sibling-worktree identity, where only rows belonging to the requesting canonical root may appear;
- paraphrased natural-language queries that do not copy distinctive target vocabulary;
- lexical distractors that repeat query terms without owning the requested topic.

A file hit alone is insufficient for the first case: the test must assert the top chunk or heading prefix inside that file.

## Variant and metric matrix

`just eval` should report, for every variant:

- file-level Recall@5;
- file-level MRR;
- pass/fail for every chunk-level top-result assertion;
- p50 query latency and added p50 versus the matching no-rerank variant.

The matrix keeps the existing four variants and adds at least one small fastembed reranker candidate selected from the models supported by the dependency at implementation time. Candidate names and measurements belong in eval output, not in this design page.

## Crossencoder default decision

The default changes only if the cheapest candidate achieves both proposed thresholds on the discriminative set:

1. absolute MRR gain of at least `+0.05` over fusion without reranking; and
2. added p50 latency no greater than `500 ms`.

The first complete run selected `none-qualified`, so `search.crossencoder` remains opt-in. The strongest fusion quality result, Jina v2 base multilingual, improved MRR from `0.8917` to `0.9583` but added `3,272 ms` p50—well above the `500 ms` ceiling. BGE base added `2,632 ms` while missing the MRR threshold at `+0.0458`; the other candidates also failed at least one threshold.[^measurement] Changing the locked thresholds requires an explicit ADR update, not an implementation-time adjustment.[^4]

## Automation

A dedicated `.github/workflows/eval.yml` should run the real-model evaluation on a schedule and by manual dispatch. It is separate from the release-oriented `nightly.yml`. The job fails when a committed quality threshold or chunk-level assertion regresses. The root `justfile` exposes `just eval`, while compiler-heavy validation continues to run through `just verify` and its cross-worktree lease.

## Research boundary

The production-readiness research compiled eight recurring practices; they are evidence, not eight new acceptance requirements for issue #288:[^5]

1. evaluate retrieval components separately from end-to-end answer quality;
2. make relevance regression a repeatable CI gate;
3. cover exact identifiers, structural queries, and natural-language queries explicitly;
4. define an index freshness mechanism and cadence;
5. measure staleness or embedding drift rather than waiting for complaints;
6. layer path normalization, exact content hashes, and targeted near-duplicate detection;
7. treat empty or low-confidence retrieval as a named observable case; and
8. make ranking changes deployable without rebuilding unrelated source state.

Issue #288 selects the relevance gate, discriminative query coverage, root-aware identity, and safe derived-state rebuild. Broader freshness monitoring, near-duplicate detection, empty-result policy, and ranking-deployment work remain outside its acceptance criteria.

See [ground-search-quality](ground-search-quality.md) for the complete draft, [ground-search-quality-adrs](ground-search-quality-adrs.md) for the decisions, and [mcp-surface](mcp-surface.md) for the current `ground` response contract.

[^1]: https://github.com/paulnsorensen/hallouminate/issues/288
[^2]: `eval/README.md:24-37,75-85`; `.cheese/research/ground-retrieval-eval/findings.md:10-56`.
[^3]: `tests/eval_ground_recall.rs:31-76,132-194,223-261`.
[^4]: https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055531
[^5]: https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067056188
[^measurement]: `eval/baseline.json` (`fusion-without-rerank` and the four fusion candidate rows).

_Source: GitHub issue #288 and its production-readiness research · Updated: 2026-07-24 · Supersedes: —_