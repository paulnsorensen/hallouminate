# Ground retrieval evaluation (#288)

This model-backed integration target measures the production Ground path without
changing the runtime crossencoder default.

## Corpus and queries

`fixtures/wiki/` is a deliberate frozen corpus. Issue #288 refreshes only the
files needed by the new cases: live `architecture.md` (including the citation
definitions after `## Testing`) and `worktree-corpus-identity.md`. Do not copy
`ground-search-evaluation.md`: it contains the literal eval query and would be a
meta-distractor rather than user knowledge.

`queries.json` labels every query with one exact expected chunk:
`{file, heading_path, line_start}`. Before any model-backed arm starts, the
engine prepares the frozen Markdown through the production handler and production
tokenizer and rejects any label that is not an actual prepared chunk. The set
includes footnote inversion, worktree isolation, paraphrase, and
lexical-distractor cases. Every measured arm records the expected and actual top
identity plus pass/fail.

## Production evaluation arms

The production embedding configuration comes from `EmbeddingsConfig::default()`:
full-precision Snowflake Arctic-S embeddings and the production cache directory.
No evaluation arm changes the runtime default crossencoder.

`just eval` runs exactly one arm:

- `fusion-without-rerank`

It compares Recall@5, MRR, and committed top-chunk assertions with
`eval/baseline.json`. A regression fails the scheduled evaluation.

`just eval-measure` runs exactly two arms:

1. `fusion-without-rerank`;
2. `fusion-with-jina-reranker-v1-turbo-en`.

The Jina arm uses a bounded 5s evaluation-only rerank timeout so a slow valid
inference is measured rather than reported as the production 2s fallback. It
does not change the runtime crossencoder default.

## Diagnostic artifact

`just eval-measure` writes
`.context/issue-288-eval-results.json`. The artifact contains:

- production embedding configuration and requested arm descriptors;
- Recall@5 and MRR for each arm;
- cold model-load time plus warm p50 and p95 latency;
- every query's rank, top chunk, latency, and reranker signal;
- per-query rank and top-chunk changes between the baseline and Jina arms;
- improved, unchanged, and regressed counts;
- timeout, model-load-failure, and other error diagnostics.

Both arms must complete for a successful measurement. A timeout or model-load
failure is recorded in a structurally valid partial artifact before the command
returns nonzero. The measurement never falls back silently and never declares a
winner. It cannot overwrite `eval/baseline.json`.

## Commands

```sh
just eval-measure
just eval
```

Both entry points are ignored tests because first use downloads production
embedding and reranker artifacts and the full query sweep is expensive. Ordinary
tests validate the production arm definitions, query labels, percentile helpers,
diagnostic schema, failure persistence policy, baseline comparison, artifact
isolation, and authoring guidance without loading models.