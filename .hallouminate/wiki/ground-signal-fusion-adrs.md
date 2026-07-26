---
status: draft
last_verified: 2026-07-26
confidence: medium
sources:
  - .cheese/specs/ground-signal-fusion.md
  - .cheese/notes/ground-ranking-fusion-audit.md
  - https://github.com/paulnsorensen/hallouminate/pull/290
---
# Ground signal fusion ADRs

Six decisions from the ranking audit that followed PR #290, plus one criterion-2 resolution decided after review (ADR-006). The approved implementation contract is [.cheese/specs/ground-signal-fusion.md]; these records preserve the rationale behind it.

### ADR-001: Move N-signal fusion into the domain layer [status: accepted]

- **Context:** Ground has four retrieval signals. BM25 and dense vector fuse inside LanceDB via a weighted RRF reranker; ripgrep and FM-Index `contains()` are applied afterwards as additive score bonuses. RRF is rank-based precisely because raw retriever scores are incommensurable, so an additive bonus is not on the same scale as a fused RRF score. Measured consequence: the full fused score range across a 50-candidate pool is 0.0225, while the two bonuses total 0.0333, enough for one literal substring hit to lift the worst-ranked chunk above the best-ranked one. The obvious fix, adding the extra signals as further ranked lists inside LanceDB's reranker, is unavailable: the `lancedb` 0.31 Rust crate's `Reranker` trait exposes only `rerank_hybrid(vector, fts)`, exactly two lists. The multi-list RRF API found during research is Python-SDK only.
- **Decision:** LanceDB becomes a retrieval backend returning per-signal ranked lists unfused. `hallouminate-domain::search` owns a single weighted RRF stage over all four signals as peers. `WeightedRRFReranker` retires.
- **Alternatives:** A second-level RRF treating LanceDB's fused output as one list, rejected because FTS and vector would pass through RRF twice and their effective weighting becomes hard to reason about. Making the bonuses rank-proportional and scaled to the fused range, rejected because it fixes the arithmetic while keeping a layering split that does not match the documented vendor pattern of signals-as-ranked-lists.
- **Consequences:** Ranking becomes our code, which is the point, but any optimisation inside LanceDB's `rerank_hybrid` is lost and retrieving two lists separately may cost more than one fused query. It also opens the door to adding further signals cheaply.

### ADR-002: Rank the literal signals by distinct-term match count [status: accepted]

- **Context:** RRF consumes ranked lists. The FM-Index `contains()` pass returns an unordered set with no intrinsic rank, and ripgrep returns hits with no relevance ordering. Converting these into ranked lists needs a quantity to sort by, and neither signal produces a score.
- **Decision:** Order both literal signals by the number of distinct query terms a chunk matched, descending. This is only possible once matching is per-term, which couples this decision to ADR-004.
- **Alternatives:** Ranking by raw match count, rejected because a chunk repeating one term would outrank a chunk covering several. Leaving the signals unranked and giving every member the same RRF rank, rejected because it reintroduces the flat-contribution problem this work exists to remove.
- **Consequences:** The per-term pass becomes required rather than optional, since without it there is no rank key. This is an agent-inferred ranking semantic rather than a user-chosen one, recorded as such in the spec's `agent_introduced_scope`, and is the most revisable decision in the set.

### ADR-003: Resolve ripgrep hits to chunk granularity [status: accepted]

- **Context:** Ripgrep currently fuses at file level. The code comments that rg "has no notion of our chunk_id scheme", but `RipgrepHit` carries `line: u64` from rg's JSON output and chunks carry `line_start` and `line_end`, so the mapping is arithmetic. The file-level restriction is a design choice, not a limitation.
- **Decision:** Map each hit's line into the chunk whose line range contains it, producing a chunk-level ranked list. Hits falling outside every chunk range are dropped.
- **Alternatives:** Keeping file-level fusion, rejected because a file-level rank boosts every chunk in that file equally, including irrelevant ones, and because it leaves ripgrep the only signal ranking at a different grain than the other three.
- **Consequences:** All four signals rank at chunk grain, which is what makes them true peers in fusion. The drop rule needs verification: if most hits land in footnote regions or stripped content, the signal could silently lose most of its matches.

### ADR-004: Whitespace and punctuation term splitting with stopword removal [status: accepted]

- **Context:** Both literal signals receive the entire raw query string. Ripgrep runs `--fixed-strings` against the whole query and the FM-Index runs `contains(search_text, '<whole query>')`, so a multi-word natural-language question can never match. Confirmed empirically: running rg with the code's own flags over all twelve eval queries returned zero matching files for every query. The embedding tokenizer at `corpus/chunker.rs:111-115` is a subword tokenizer and would produce word pieces, not terms, so it cannot be reused.
- **Decision:** A dependency-free splitter in the domain layer: lowercase, split on whitespace and punctuation, drop a small English stopword list.
- **Alternatives:** Mirroring LanceDB's default FTS analyzer exactly, rejected for now because that analyzer's behaviour is unknown and may not be introspectable from Rust, though it would be more coherent. A content-word heuristic keeping only discriminative tokens, rejected as premature tuning surface before the eval can measure it.
- **Consequences:** The lexical signals and the FTS signal will disagree about what a term is. Accepted knowingly; the expanded eval is the instrument for deciding whether it matters.

### ADR-005: Author the expanded eval set before implementing [status: accepted]

- **Context:** PR #290 replaced the benchmark in the same commit as the ranking code, leaving its accuracy claim unmeasured rather than disproven. The current twelve-query set cannot distinguish a real gain from noise, since one query is 8.3 points of Recall@5 and the set already sits at 11/12. The fixture corpus and query set were also authored together in #290 by the same process, which makes the current 9/12 rank-1 result a ceiling estimate rather than a neutral one.
- **Decision:** Expand the query set to roughly 40 labelled queries and the fixture corpus to roughly 35 files, including abstract paraphrases with no lexical overlap and topically-adjacent distractor documents. Author and freeze all of it against unmodified `main`, and measure a baseline, before any ranking code in the spec is written.
- **Alternatives:** Shipping the ranking changes unmeasured and documenting that honestly, rejected by the user in favour of measurement. No-regression testing against the existing twelve queries plus fusion unit tests, rejected as unable to detect whether the change helped, only whether it broke something.
- **Consequences:** The committed `eval/baseline.json` is invalidated by design and must be hand-regenerated after human review, since no code path writes it. The work gains a sequencing constraint: eval first, code second. This is the decision that prevents repeating #290's pattern.

### ADR-006: Keep the shipped 0.5/0.5 literal weights and amend criterion 2 rather than restore 1.0/1.0 [status: accepted]

- **Context:** Acceptance criterion 2 (`.cheese/specs/ground-signal-fusion.md:103`) requires that a chunk ranked first by a single signal never outrank a chunk ranked first by two or more signals. The invariant holds only when `max_weight <= sum of the two smallest weights`. At the shipped weights (FTS 2.0, vector 1.0, ripgrep 0.5, FM 0.5), `2.0 > 0.5 + 0.5`, so the criterion is violated. At `k=60` and a 50-candidate pool, the tightest realizable configuration (one chunk first by FTS alone, last elsewhere; another first by ripgrep and FM together, last elsewhere) scores the single-signal chunk **+7.4924e-03** above the two-signal chunk under the shipped weights — a real inversion, not a rounding artifact. The `fuse.rs` invariant test's own (non-tightest) configuration shows the same violation at −3.704e-03. Restoring `w_rg`/`w_fm` to the spec's originally planned 1.0/1.0 would satisfy the invariant — at that weighting the tightest configuration is an **exact tie** (0.0000e+00), not a pass with margin, and the test's own configuration holds at +8.50e-05 — but the expanded 73-query eval set measured the 1.0/1.0 arm as a 0.0137 Recall@5 regression against 0.5/0.5, which the user weighed and rejected.
- **Decision:** Keep the shipped weights (FTS 2.0 / vector 1.0 / ripgrep 0.5 / FM 0.5). Amend acceptance criterion 2 into its weight-conditional form — the invariant holds only when `max_weight <= sum of the two smallest weights`, and is incompatible with any dominant signal weight — rather than restoring 1.0/1.0 to satisfy the criterion as originally worded.
- **Alternatives:** Restoring `w_rg`/`w_fm` to 1.0 to make the shipped build conform to criterion 2 as originally worded, rejected because it costs the measured 0.0137 Recall@5 regression for a criterion that, even satisfied, offers zero margin at its own tightest configuration. Leaving the criterion unresolved and unstated, rejected because that ships a violated acceptance criterion with nothing in the tree recording it.
- **Consequences:** Criterion 2's original wording assumed roughly equal signal weights and cannot coexist with a dominant signal weight under any RRF weighting scheme — this is a general property of weighted RRF, not specific to this implementation. Future weight changes must re-check `max_weight <= sum of the two smallest weights` before assuming the invariant holds; the `fuse.rs` test now asserts against the live `search::*` weight constants rather than a hardcoded configuration, so a future weight change that breaks the invariant fails the test instead of passing silently.

_Source: ranking audit session 2026-07-25 · Spec: .cheese/specs/ground-signal-fusion.md · Supersedes: —_
