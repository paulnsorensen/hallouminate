# Ground retrieval eval (#150)

A fixed, small-scale eval harness for `hallouminate ground`'s retrieval
quality, used to gate #149's z-score threshold decision.

## Corpus: `fixtures/wiki/`

A frozen snapshot of this repo's own `.hallouminate/wiki/` at commit
`3d466ca` (16 pages, ~9.6k words). Frozen rather than pointed at the live
wiki so the eval is reproducible — the live wiki keeps changing as new pages
land, which would silently shift recall/MRR between runs for reasons
unrelated to the retrieval code under test. Copied verbatim (`cp -R`), never
hand-edited.

To refresh the snapshot after a deliberate wiki restructure:

```
rm -rf eval/fixtures/wiki && cp -R .hallouminate/wiki eval/fixtures/wiki
```

and update this file's pinned commit hash + `eval/queries.json` if page
names or content moved.

## Query set: `queries.json`

26 labelled queries, `{id, query, expected: [filename]}`. Two queries per
richer page (9 pages), aimed at distinct sections/paragraphs of that page so
a single query can't get lucky on page-level keyword density; one query per
shorter page or log-style page (7 pages: `index.md`, `log.md`, and similar).
Queries are short natural-language phrases lifted from the page's own
terminology (e.g. "claim mark HTML comment syntax confirmed superseded
contradicted" for `claim-provenance-marks.md`) — this makes them easy for
lexical/BM25 search by construction. See **Caveat** below.

`expected` is a list (usually length 1) of filenames under `fixtures/wiki/`;
a hit is scored the moment any ranked result's absolute path ends with one
of the expected filenames.

## Running it

```
cargo test --test eval_ground_recall -- --ignored --nocapture
```

`#[ignore]`d like `tests/cli_ground.rs`'s model-dependent test: needs network
for the crossencoder model download (~147MB, first run only) and takes
several minutes (four full daemon-index-query cycles).

## Metrics

For each of 4 configs (lexical-only, fusion-only, lexical+rerank,
fusion+rerank) and each query: rank of the first result whose path matches
`expected` (1-indexed, `None` if absent from the top-50 candidate pool).

- **Recall@5** — fraction of queries where that rank is `<= 5`.
- **MRR** — mean of `1/rank` (`0` when absent).

The fusion+rerank run additionally sweeps z-score thresholds
`[-2, -1, -0.5, 0, 0.5, 1, 2]` against the top-1 result's `z_score`, showing
how many queries a given cutoff would keep vs. drop, and how many of those
kept are actually correct at rank 1 — this is the #149 calibration input.

## Embedding model substitution

The config's default embedding model (`snowflake-arctic-embed-s`) has no
cached ONNX weight blob in `~/.cache/hallouminate/fastembed` on this
machine (only tokenizer/config files) — using it would trigger a second
model download. The eval pins `BAAI/bge-small-en-v1.5` (quantized) instead,
which is already fully cached. This means the eval does not measure the
config-default embedding model; treat the fusion-variant numbers as
representative of "a small bge-family embedding model", not the shipped
default specifically.

## Caveat: this query set does not discriminate between configs

All 4 configs scored Recall@5 = 1.000 on this query set (MRR 0.981-1.000,
see `.cheese/research/ground-retrieval-eval/findings.md`). The queries were
constructed by lifting distinctive terminology directly from each target
page, which makes them easy hits for lexical/BM25 search alone — the
fusion and rerank variants have no low-recall queries left to improve on.
A query set built this way over a 16-page corpus cannot show whether
embeddings or reranking earn their cost; a harder eval (paraphrased queries,
no shared vocabulary with the target page, or a much larger corpus with more
lexical distractors) would be needed to answer that question.
