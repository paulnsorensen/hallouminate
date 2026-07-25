---
status: draft
last_verified: 2026-07-24
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055379
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055531
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055759
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055988
---
# Ground search quality ADRs

These five decisions define the proposed issue #288 design; none records shipped behavior yet.[^spec]

## ADR-001 — Index `search_text`, display `text`

**Status:** proposed.

**Decision:** Preserve `text` as the rendering authority under its current preparation contract. Build deterministic `search_text` from heading breadcrumb + file summary + footnote-stripped body, and use it for embedding, FTS, and FM.[^spec]

**Evidence:** RRF is magnitude-blind, while Sourcegraph's BM25F work shows that structural separation prevents incidental path/symbol fields from dominating. Applying that principle to footnotes is a reasoned adaptation, not a directly documented footnote standard.[^fusion][^literal]

**Rejected:** weight retuning leaves all inputs polluted; footnote-only chunk splitting changes boundaries; BM25F-only weighting repairs neither embeddings nor FM; replacing display `text` changes the snippet and footnote contract.

**Consequences:** schema v4 and every search index rebuild. “Verbatim” means unchanged display behavior unless implementation explicitly revises the existing claim-comment stripping rule.

## ADR-002 — Key corpora by name plus canonical root

**Status:** proposed.

**Decision:** Introduce an identity equivalent to `CorpusKey { name, canonical_root }`, canonicalized via `canonicalize_or_passthrough(expand_tilde(..))`; persist `root` and apply name-plus-root predicates to every store operation.[^spec]

**Evidence:** #215 made deletes root-safe, yet name-only search still combines sibling-worktree rows. Diverged worktrees must not answer with one another's content.[^worktree]

**Rejected:** git-common-directory identity merges branches; name-only identity preserves duplicates; main-checkout resolution discards worktree-local semantics; post-search dedup may hide conflicts and choose the wrong version.

**Consequences:** worktrees own distinct rows; storage grows until #286. Deliberate multi-root corpora union root-scoped queries with per-hit provenance.

## ADR-003 — Rebuild older derived schemas automatically

**Status:** proposed; amends the current fail-loud stale-schema convention.

**Decision:** Version `< 4` logs, recreates the derived chunks table, and runs catch-up indexing; `= 4` opens normally; `> 4` remains fatal.[^spec]

**Evidence:** Filesystem Markdown is canonical and Lance rows are reconstructible, while both new columns change index construction.[^filesystem]

**Rejected:** manual reindex creates avoidable broken-start states; fail-on-older wastes safe reconstructibility; in-place migration is more complex than authoritative rebuild; opening newer schemas is unsafe.

**Consequences:** first open may spend minutes re-embedding large corpora; migration must be observable and preserve the existing newer-schema failure.

## ADR-004 — Let evaluation choose the reranker default

**Status:** proposed; thresholds are provisional and agent-introduced.

**Decision:** Choose the cheapest supported candidate with absolute MRR gain `>= 0.05` and added p50 `<= 500 ms`; if none qualifies, remain opt-in.[^spec]

**Evidence:** Vendors treat reranking as a latency/precision option, and adaptive-reranking research argues against unconditional work on confident queries. The present eval is saturated and cannot decide.[^rerank][^eval]

**Rejected:** always enabling the current model is unmeasured and slow; permanent opt-in avoids learning; choosing only by size ignores quality; confidence gating is deferred to #287 because it needs per-signal evidence.

**Consequences:** a failed flip is a valid result; scheduled evaluation gates regressions rather than requiring a candidate to qualify on every run.

## ADR-005 — Deterministic context now, LLM context later

**Status:** proposed.

**Decision:** Prepend heading breadcrumb and file summary to the stripped body, and require authored sections to open self-contained. Defer credentialed 50–100-token LLM context to #284.[^spec]

**Evidence:** Anthropic reports fewer retrieval failures when context precedes embedding and BM25 indexing, but its full technique adds API keys, cost, caching, and offline-policy decisions.[^context]

**Rejected:** raw-body indexing loses document context; LLM context changes product shape; contextualized embedding models do not repair FTS/FM; breadcrumb-only context lacks document purpose.

**Consequences:** all signals gain stable context without credentials; [ground-search-evaluation](ground-search-evaluation.md) must detect any new summary-density bias.

## Shared constraints

- Display `text` is the evidence/rendering authority; `search_text` and Lance rows are derived.
- Worktree isolation precedes orphan cleanup.
- The reranker flip waits for measurements.
- LLM context and confidence gating remain explicit follow-ups.[^spec]

[^spec]: [Issue #288 — embedded specification](https://github.com/paulnsorensen/hallouminate/issues/288).
[^fusion]: [RRF vs fusion research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055379).
[^rerank]: [Reranking-default research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055531).
[^literal]: [Literal-match density research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055759).
[^context]: [Agent-facing retrieval research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055988).
[^worktree]: [worktree-corpus-identity](worktree-corpus-identity.md); [issue #215](https://github.com/paulnsorensen/hallouminate/issues/215).
[^filesystem]: [design-rationale](design-rationale.md), “Filesystem is the source of truth; LanceDB is derived.”
[^eval]: `eval/README.md:75-85`; [ground-search-evaluation](ground-search-evaluation.md).

_Source: issue #288 plus research comments 1–4 · Updated: 2026-07-24 · Supersedes: —_