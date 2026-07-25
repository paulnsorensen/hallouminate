# Ground retrieval evaluation (#288)

This model-backed integration target measures the production daemon ground path
before a reranker default may change.

## Corpus and queries

`fixtures/wiki/` is a deliberate frozen corpus. Issue #288 refreshes only the
files needed by the new cases: live `architecture.md` (including the citation
definitions after `## Testing`) and `worktree-corpus-identity.md`. Do not copy
`ground-search-evaluation.md`: it contains the literal eval query and would be a
meta-distractor rather than user knowledge.

`queries.json` labels every query with one exact expected chunk:
`{file, heading_path, line_start}`. Before any model-backed variant starts, the
engine prepares the frozen Markdown through the production handler and BGE
chunker and rejects any label that is not an actual prepared chunk. The set
includes footnote inversion, worktree isolation, paraphrase, and
lexical-distractor cases. Every measured variant records the expected and actual
top identity plus pass/fail. Only the locked `footnote-inversion` case must pass
every variant; requiring every query to pass every variant would force MRR to
1.0 and erase the comparison signal.

## Variants and measurement

The reporting matrix contains ten variants:

- `lexical-without-rerank`
- one `lexical-with-{model}` variant for each of the four models below
- `fusion-without-rerank`
- one `fusion-with-{model}` variant for each of the four models below

The candidate inventory remains exactly `SUPPORTED_CROSSENCODER_MODELS` from the
pinned fastembed dependency:

- `bge-reranker-base` — 1,112,459,588 weight bytes
- `bge-reranker-v2-m3` — 2,271,197,135 weight bytes
- `jina-reranker-v1-turbo-en` — 151,296,975 weight bytes
- `jina-reranker-v2-base-multiligual` — 1,114,040,223 weight bytes

Each variant indexes once, runs and discards one complete warm-up sweep, then
measures the identical query sweep. Per-query latency comes from
`GroundResponse.took_ms`. Reranked queries must carry a non-null rerank signal;
a timeout fallback makes the measurement incomplete. Both retrieval modes use
the same persistent model cache, while their fixed embedding configurations
require separate daemon runs.

The artifact records Recall@5, MRR, every file rank and top-chunk assertion,
warmed per-query latency, and warmed p50. p50 is nearest-rank after sorting,
using zero-based index `(n - 1) / 2`; an even sweep therefore uses its lower
middle. Reporting deltas compare each reranked variant to the no-rerank baseline
for the same retrieval mode.

Only the four `fusion-with-{model}` variants participate in qualification and
deterministic selection against `fusion-without-rerank`. A fusion candidate
qualifies at MRR gain `>= 0.05` and added p50 `<= 500 ms`; comparison order is
added p50, weight bytes, then stable model identifier. Lexical variants remain
reporting-only even when their metrics cross those thresholds. Measurement
records the inputs and qualification results; it does not choose or change the
runtime default.

## Committed decision

The first complete sweep selected `none-qualified`, so `search.crossencoder`
remains opt-in. `fusion-without-rerank` measured MRR `0.8917` at p50 `46 ms`.
The fusion candidates measured:

| Model | MRR | Gain | p50 | Added p50 | Qualifies |
|---|---:|---:|---:|---:|---|
| `bge-reranker-base` | 0.9375 | +0.0458 | 2,678 ms | 2,632 ms | no |
| `bge-reranker-v2-m3` | 0.9167 | +0.0250 | 8,403 ms | 8,357 ms | no |
| `jina-reranker-v1-turbo-en` | 0.8611 | -0.0306 | 1,054 ms | 1,008 ms | no |
| `jina-reranker-v2-base-multiligual` | 0.9583 | +0.0667 | 3,318 ms | 3,272 ms | no |

The committed measurements and decision are in `baseline.json`.

## Commands

```sh
just eval-measure
just eval
```

`just eval-measure` writes only the requested artifact through
`HALLOUMINATE_EVAL_OUTPUT`. Relative output paths resolve from the repository
root, so the default lands at `.context/issue-288-eval-results.json` regardless
of Cargo's test working directory. The command rejects any normalized or
canonical alias of `eval/baseline.json` and verifies that the baseline bytes did
not change. It does not check runtime default agreement.

`just eval` reads the committed baseline, reruns the production matrix, and
fails on missing variants or results, query-set digest drift, Recall@5/MRR floor
regressions, committed top-chunk pass-to-fail changes, invalid qualification
calculations, or runtime defaults that disagree with the recorded decision.
Every reporting variant is subject to floors and regression enforcement. A
selected decision must agree with `SearchConfig::default()`, the domain default
model constant, and the active CLI template value. A none-qualified decision
requires reranking to remain disabled in `SearchConfig` and the CLI template,
with the domain constant retained as the single commented opt-in model. Latency
remains the locked qualification contract; there is no separate latency-jitter
failure policy.

Both entry points are ignored tests because first use downloads multiple large
ONNX artifacts and the full CPU sweep is expensive. Ordinary tests still run
the prepared-label, p50, candidate and matrix, qualification-ordering,
default-agreement, comparator, artifact-isolation, and authoring-guidance checks
without loading models.