---
status: reviewed
last_verified: 2026-08-02
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/pull/290
---
# Ground search quality

This began as issue #288's design spec and **shipped in PR #290** (2026-07-25): display/indexed text separation, canonical-root corpus keying (schema v4), and the eval gate now exist in the source. The reranker default stayed opt-in because no candidate cleared the thresholds (Decision 4). The spec body, acceptance checklist, and quality gates below are retained as the historical design record, not current status.[^spec]

## Spec metadata

- Slug: `ground-search-quality`; created: 2026-07-24; overridden gates: none.
- Agent-introduced scope: flip thresholds, scheduled `eval.yml`, summary-in-prepend, and the marketing/production-readiness non-goals.[^spec]

## Problem

- For `what is hallouminate`, a citation-dense `architecture.md` chunk ranked above its introductory prose because embedding, BM25, and exact-substring matching all consumed the same text. Rank-only RRF cannot recover discarded score magnitude.[^spec][^fusion]
- Same-named corpora from sibling worktrees leave coexisting absolute-path rows, so search can return stale duplicates. #215 fixed cross-root deletion, not search identity.[^handoff]
- The optional crossencoder is unmeasured on a useful decision set: the current four-variant eval is saturated at Recall@5 `1.000` and its queries reuse target vocabulary.[^eval]

## Goals

- Add derived, footnote-stripped, context-prepended `search_text` for embedding, FTS, and FM while preserving the existing display `text` contract.
- Make corpus identity `(name, canonical root)` and migrate older derived indexes automatically through schema version 4.
- Add discriminative chunk assertions, a small-reranker candidate, scheduled eval, and a measurement-based default decision.
- Require LLM-authored sections to open with self-contained chunk context.[^spec]

## Non-goals

- LLM-generated index-time context ([#284](https://github.com/paulnsorensen/hallouminate/issues/284)).
- Marketing or “primetime” positioning. **Agent-introduced.**
- Production-readiness work beyond these goals. **Agent-introduced.**
- Orphan-row garbage collection ([#286](https://github.com/paulnsorensen/hallouminate/issues/286)).
- Confidence-gated reranking ([#287](https://github.com/paulnsorensen/hallouminate/issues/287)).
- Full stale-page correction ([#285](https://github.com/paulnsorensen/hallouminate/issues/285)); this ingest only records explicit supersession notes.[^spec]

## Four delivery curds

1. **Indexed-text quality:** `PreparedChunk` gains `search_text = heading breadcrumb + file summary + footnote-stripped body`; the Lance column feeds embedding, FTS, and FM. Rendering stays on `text`.
2. **Worktree identity and migration:** an identity equivalent to `CorpusKey { name, canonical_root }` flows through `ChunkStore`; rows gain `root`; every store predicate matches name plus root; schema v4 rebuilds older derived tables through existing catch-up indexing.
3. **Evaluation and decision:** labels gain expected heading prefixes; the matrix adds supported small rerankers; `just eval` and scheduled `eval.yml` run real-model gates before any default flip.
4. **Authoring:** wiki-ingest and `wiki-conventions.md` require self-contained section openings; deterministic prepend is the credential-free baseline.[^spec]

See [domain-model](domain-model.md) for entity/status details, [ground-search-quality-adrs](ground-search-quality-adrs.md) for proposed decisions, and [ground-search-evaluation](ground-search-evaluation.md) for the gate.

## Decisions

1. Dual `text` / `search_text` columns rather than footnote chunk separation or BM25F-only weighting.
2. Per-worktree identity rather than shared git-common-directory identity.
3. Automatic rebuild for older schemas, amending the current fail-loud stale-schema convention.
4. Flip reranking only to the cheapest candidate with absolute MRR gain `>= 0.05` and added p50 `<= 500 ms`; otherwise remain opt-in. The first complete run selected `none-qualified`, so the default remains disabled.[^measurement]
5. Deterministic breadcrumb + summary prepend; defer LLM context to #284.
6. Explicit multi-root corpora union roots with per-hit provenance; eval gets a separate scheduled workflow; candidates come from fastembed support at implementation time.[^spec]

## Acceptance criteria

- [ ] Indexing stores existing-contract display `text` and derived `search_text` per chunk; embedding, FTS, and FM use only `search_text`.
- [ ] Snippets and footnote modes continue to serve display `text` unchanged from current behavior.
- [ ] `why are Hallouminate's daemon and config layers separate from the application crate` ranks the `architecture.md` introduction above its citation chunk.
- [ ] Written rows carry canonical root; search, list, delete, touch, stats, and batch replacement match name plus root.
- [ ] A worktree query returns only rows for that worktree's canonical root.
- [ ] Opening `schema_version < 4` logs and automatically rebuilds without a manual step.
- [ ] Opening a schema newer than the build fails with the existing fatal error.
- [ ] `just eval` reports file Recall@5/MRR, latency, and chunk-level top-result assertions across every variant, including the small reranker.
- [ ] The scheduled eval workflow fails when a committed metric or chunk assertion regresses.
- [ ] Wiki-ingest and `wiki-conventions.md` state the chunk-context rule.[^spec]

## Implementation locks

Grounding the draft against current code exposed five semantics that must be fixed in the coder contract before edits begin:

1. “Verbatim `text`” must preserve today's claim-comment stripping unless the claim-provenance display contract is intentionally changed.
2. When configured roots overlap, scan/plan must define which canonical root owns the file.
3. Root is required inside store identity; exposing it as a new public MCP response field is not implied and should not be added without a separate decision.
4. “Added p50” needs one formula, and a no-candidate result must remain a successful eval outcome rather than making scheduled CI permanently red.
5. The exact small-reranker candidates remain selected from the pinned fastembed version at implementation time.

## Deferred follow-ups

| ID | Issue | Deferred work |
|---|---|---|
| `ground-search-quality-F001` | [#284](https://github.com/paulnsorensen/hallouminate/issues/284) | Opt-in 50–100-token LLM context and comparison with deterministic prepend |
| `ground-search-quality-F002` | [#285](https://github.com/paulnsorensen/hallouminate/issues/285) | Complete stale arena-cap and worktree-delete wiki corrections |
| `ground-search-quality-F003` | [#286](https://github.com/paulnsorensen/hallouminate/issues/286) | Remove only rows whose recorded worktree root no longer exists |
| `ground-search-quality-F004` | [#287](https://github.com/paulnsorensen/hallouminate/issues/287) | Preserve per-signal evidence and gate reranking on disagreement |

## Risks

- `ChunkStore` identity changes ripple through domain, adapter, and daemon call sites.
- Version-4 rebuild re-embeds every corpus on first touch; large corpora may take minutes.
- No measured reranker cleared both thresholds; opt-in remains the recorded decision and #287 is the next path.[^measurement]
- Per-worktree storage grew with active worktrees until retired-root GC shipped in #304 (`distinct_roots` / `delete_root`); #286 tracked that cleanup.[^spec]

## Quality gates

- [ ] Focused tests run through `just verify cargo test ...` after each coherent core pass.
- [ ] `just eval` passes the footnote-inversion and worktree-isolation golden cases and records the default decision.
- [ ] Final `just verify` passes formatting, clippy, build, and workspace tests under the shared verification lease.

## Reproduction contract

Before: the citation chunk may rank first and stale sibling-worktree rows may appear. After: the introduction ranks first and only the requesting worktree's rows participate; golden cases enforce both properties.[^handoff]

[^spec]: [Issue #288 — embedded specification](https://github.com/paulnsorensen/hallouminate/issues/288).
[^fusion]: [Issue #288 — RRF research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055379).
[^handoff]: [Issue #288 — design-session reproduction](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067056364).
[^eval]: `eval/README.md:75-85`; `.cheese/research/ground-retrieval-eval/findings.md:10-56`.
[^measurement]: `eval/baseline.json` (`decision` and fusion variant metrics).

_Source: GitHub issue #288, embedded spec, six-comment design record, and PR #290 (landed) · Updated: 2026-08-02 · Supersedes: the 2026-07-25 draft that declared this spec "proposed, not shipped"_