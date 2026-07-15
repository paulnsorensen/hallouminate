# Ground retrieval eval findings (#150)

Spike for #150, gating #149's z-score threshold decision. Numbers below are
from one real run of `cargo test --test eval_ground_recall -- --ignored
--nocapture` against `eval/fixtures/wiki/` (16-page frozen snapshot,
`3d466ca`) and `eval/queries.json` (26 labelled queries). See `eval/README.md`
for corpus/query methodology and the embedding-model substitution
(bge-small-en-v1.5, not the config-default arctic-embed-s).

## Recall@5 / MRR by config

| Config | Recall@5 | MRR |
| --- | --- | --- |
| lexical-only (no vector, no rerank) | 1.000 | 0.981 |
| fusion-only (vector+lexical+rg, no rerank) | 1.000 | 0.981 |
| lexical+rerank (no vector, crossencoder) | 1.000 | 1.000 |
| fusion+rerank (vector+lexical+rg, crossencoder) | 1.000 | 1.000 |

All 4 configs hit Recall@5 = 1.000 (26/26 queries land their expected doc in
the top 5). The two rerank configs additionally hit MRR = 1.000 (every
expected doc lands at rank 1); the two no-rerank configs sit at 0.981,
i.e. one query out of 26 has its expected doc at rank 2 instead of rank 1
(lexical scoring ties or near-ties pushed one candidate ahead).

**What this does and doesn't show:** the eval as built cannot distinguish
embeddings-on from embeddings-off, or rerank-on from rerank-off, on recall —
there's no headroom left; lexical-only already gets every query into the top
5. It does show the crossencoder rerank nudging near-miss rank-2 hits up to
rank-1 (the MRR delta), which is the one place these 26 queries have any
signal. See `eval/README.md` § Caveat for why: queries were constructed from
each target page's own terminology, which is close to a best-case scenario
for BM25. This is not evidence that embeddings/rerank are unnecessary in
production — it's evidence that *this specific eval* is too easy to tell.

## z-score threshold sweep (fusion+rerank run)

| threshold | queries gate would keep | of those, correct at rank 1 |
| --- | --- | --- |
| -2.0 | 26 | 26 |
| -1.0 | 26 | 26 |
| -0.5 | 26 | 26 |
| 0.0 | 26 | 26 |
| 0.5 | 26 | 26 |
| 1.0 | 26 | 26 |
| 2.0 | 26 | 26 |

Every query's top-1 z-score is `>= 2.0` — the sweep is flat because every
query in this set is an easy, confident win for the crossencoder (consistent
with the MRR = 1.000 above). **This run gives #149 no calibration signal**:
a flat 26/26 at every threshold from -2 to 2 means the corpus/query pair
never produces a low-confidence top-1 result, so there's no data point
showing where a threshold would actually start trading recall for
precision. Before using a z-score cutoff as a production gate, #149 needs a
harder eval run (see caveat above) that actually produces some low-z,
incorrect top-1 hits to calibrate against — this run cannot supply that
number.

## Ripgrep boost: no independent on/off knob

Checked `src/domain/search.rs` and `src/domain/ground/orchestrate.rs`:
`hybrid_with_ripgrep` (embeddings-on path) and `fts_with_ripgrep`
(embeddings-off path) are the *only* two search entry points
`orchestrate.rs` calls, and both always run the ripgrep pass
(`ripgrep::run`) and apply `apply_rg_boost` unconditionally — there is no
`SearchConfig`/`EmbeddingsConfig` field or `GroundArgs` flag that disables
it. `grep`ing `config.rs` for "ripgrep" only turns up a doc-comment
reference, not a setting. So all 4 variants in this eval already include the
ripgrep boost; it cannot be isolated as a 5th variant without adding a new
code path (not attempted here, out of scope for a spike).

## Citation-format options (decision left to the user)

Three options for how `ground` results carry citation metadata back to a
caller, with trade-offs. Not implemented or decided here — #150 is a
measurement spike, and this decision belongs to whoever owns the citation
contract.

1. **Structured path+line string** (`"[path:L5-15]"`), embedded in the
   snippet or as a sibling field.
   - Pro: simplest, no new dependency, easy to grep/parse in logs.
   - Pro: matches the `heading_path` / `line_range` shape hallouminate's
     `ground` MCP tool already returns.
   - Con: caller has to parse the string format themselves; no shared
     schema with any SDK-level citation UI.

2. **Anthropic SDK `CitationsConfig`-shaped output** (structured
   `{source, location: {start, end}, cited_text}` matching the Claude API's
   citations feature).
   - Pro: if the eventual caller renders citations through Claude (as this
     harness likely does), the shape is pre-aligned with what the API
     already expects — zero translation layer.
   - Con: couples `hallouminate`'s domain output to a third-party API
     shape; a non-Claude caller needs a translation layer, not none.

3. **Hybrid: internal structured type, format-specific serializers**
   (`Citation { path, line_range, snippet }` in `domain::ground::types`,
   with a `to_anthropic_citation()` / `to_string()` per consumer).
   - Pro: keeps `domain` free of an external API shape while still
     supporting Option 2 as one output format; supports adding more callers
     later without touching the core type.
   - Con: more surface area than Options 1/2 for what's currently a single
     consumer — YAGNI risk if there's only ever going to be one caller.

## Files

- `eval/fixtures/wiki/` — frozen 16-page corpus snapshot (commit `3d466ca`)
- `eval/queries.json` — 26 labelled queries
- `tests/eval_ground_recall.rs` — `#[ignore]`d integration test, 4 configs +
  z-score sweep
- `eval/README.md` — methodology
