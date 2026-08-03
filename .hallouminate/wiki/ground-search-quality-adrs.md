---
status: reviewed
last_verified: 2026-08-02
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/pull/290
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055379
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055531
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055759
  - https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055988
  - https://www.anthropic.com/engineering/contextual-retrieval
---
# Ground search quality ADRs

These five decisions define the issue #288 design. PR #290 shipped the retrieval/display, worktree-identity, schema-migration, and evaluation portions (ADR-001/002/003, and the ADR-004 eval gate); the ADR-004 reranker default stayed opt-in and the ADR-005 LLM-context layer remains deferred to #284.[^spec]

## ADR-001 — Index search_text, display text

Status: shipped in #290 (schema v4).

Decision: Preserve text as the rendering authority under its current preparation contract. Build deterministic search_text from heading breadcrumb + file summary + footnote-stripped body, and use it for embedding, FTS, and FM.[^spec]

Evidence: RRF is magnitude-blind, while Sourcegraph's BM25F work shows that structural separation prevents incidental path/symbol fields from dominating. Applying that principle to footnotes is a reasoned adaptation, not a directly documented footnote standard.[^fusion][^literal]

Rejected: weight retuning leaves all inputs polluted; footnote-only chunk splitting changes boundaries; BM25F-only weighting repairs neither embeddings nor FM; replacing display text changes the snippet and footnote contract.

Consequences: schema v4 and every search index rebuild. “Verbatim” means unchanged display behavior unless implementation explicitly revises the existing claim-comment stripping rule.

## ADR-002 — Key corpora by name plus canonical root

Status: shipped in #290 (`CorpusKey`, `crates/hallouminate-domain/src/common.rs:45`).

Decision: Introduce an identity equivalent to CorpusKey { name, canonical_root }, canonicalized via canonicalize_or_passthrough(expand_tilde(..)); persist root and apply name-plus-root predicates to every store operation.[^spec]

Evidence: #215 made deletes root-safe, yet name-only search still combines sibling-worktree rows. Diverged worktrees must not answer with one another's content.[^worktree]

Rejected: git-common-directory identity merges branches; name-only identity preserves duplicates; main-checkout resolution discards worktree-local semantics; post-search dedup may hide conflicts and choose the wrong version.

Consequences: worktrees own distinct rows; storage grew until retired-root GC shipped in #304 (tracked by #286). Deliberate multi-root corpora union root-scoped queries with per-hit provenance.

## ADR-003 — Rebuild older derived schemas automatically

Status: shipped in #290; amends the prior fail-loud stale-schema convention.

Decision: Version < 4 logs, recreates the derived chunks table, and runs catch-up indexing; = 4 opens normally; > 4 remains fatal.[^spec]

Evidence: Filesystem Markdown is canonical and Lance rows are reconstructible, while both new columns change index construction.[^filesystem]

Rejected: manual reindex creates avoidable broken-start states; fail-on-older wastes safe reconstructibility; in-place migration is more complex than authoritative rebuild; opening newer schemas is unsafe.

Consequences: first open may spend minutes re-embedding large corpora; migration must be observable and preserve the existing newer-schema failure.

## ADR-004 — Let evaluation choose the reranker default

Status: eval gate shipped in #290; default remains opt-in — the first complete run selected `none-qualified`. Thresholds are agent-introduced.

Decision: Choose the cheapest supported candidate with absolute MRR gain >= 0.05 and added p50 <= 500 ms; if none qualifies, remain opt-in.[^spec]

Evidence: Vendors treat reranking as a latency/precision option, and adaptive-reranking research argues against unconditional work on confident queries. The present eval is saturated and cannot decide.[^rerank][^eval]

Rejected: always enabling the current model is unmeasured and slow; permanent opt-in avoids learning; choosing only by size ignores quality; confidence gating is deferred to #287 because it needs per-signal evidence.

Consequences: a failed flip is a valid result; scheduled evaluation gates regressions rather than requiring a candidate to qualify on every run.

## ADR-005 — Deterministic context now, LLM context later

Status: deterministic prepend shipped in #290; credentialed LLM context deferred to #284.

Decision: Prepend heading breadcrumb and file summary to the stripped body, and require authored sections to open self-contained. Defer credentialed 50–100-token LLM context to #284.[^spec]

Evidence: Anthropic's [Introducing Contextual Retrieval](sources/anthropic-contextual-retrieval.md) reports that chunk-specific context before embedding and BM25 indexing reduces retrieval failures; its 49% combined contextual-embedding/BM25 improvement and 67% reranked improvement are evidence for the retrieval principle, not a Hallouminate-specific guarantee.[^context][^anthropic-source]

Rejected: raw-body indexing loses document context; LLM context changes product shape; contextualized embedding models do not repair FTS/FM; breadcrumb-only context lacks document purpose.

Consequences: all signals gain stable context without credentials; [ground-search-evaluation](ground-search-evaluation.md) must detect any new summary-density bias.

Implementation note: the diagnostic keeps rerank completion separate from z-score presence because small or zero-spread pools validly omit z-scores. Ground exposes existing `rerank-timeout` and `crossencoder-unavailable` warnings so the evaluator rejects production fallbacks, and artifact writes use temporary-file-plus-rename so a hard-linked output cannot alter the committed baseline.[^implementation]

## Shared constraints

- Display text is the evidence/rendering authority; search_text and Lance rows are derived.
- Worktree isolation precedes orphan cleanup.
- The reranker flip waits for measurements.
- LLM context and confidence gating remain explicit follow-ups.[^spec]

[^spec]: [Issue #288 — embedded specification](https://github.com/paulnsorensen/hallouminate/issues/288).
[^fusion]: [RRF vs fusion research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055379).
[^rerank]: [Reranking-default research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055531).
[^literal]: [Literal-match density research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055759).
[^context]: [Agent-facing retrieval research](https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067055988).
[^anthropic-source]: [Introducing Contextual Retrieval](sources/anthropic-contextual-retrieval.md).
[^worktree]: [worktree-corpus-identity](worktree-corpus-identity.md); [issue #215](https://github.com/paulnsorensen/hallouminate/issues/215).
[^filesystem]: [design-rationale](design-rationale.md), “Filesystem is the source of truth; LanceDB is derived.”
[^eval]: eval/README.md:75-85; [ground-search-evaluation](ground-search-evaluation.md).
[^implementation]: crates/hallouminate/tests/eval_ground_recall.rs:582-646,1124-1147; crates/hallouminate-domain/src/ground/orchestrate.rs:155-174; crates/hallouminate-daemon/src/dispatch.rs:385-417.

_Source: issue #288 plus research comments 1–4, PR #290 (landed), and Anthropic's Introducing Contextual Retrieval · Updated: 2026-08-02 · Supersedes: the 2026-07-25 draft that marked ADR-001/002/003/005 "proposed"_