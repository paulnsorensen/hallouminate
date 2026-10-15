---
status: reviewed
last_verified: 2026-09-10
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/pull/434
  - https://github.com/lancedb/lancedb/pull/4100
---
# LanceDB adapter notes

How `crates/hallouminate-adapters/src/lance.rs` uses the `lancedb` crate
today, what it does not use, and the version traps found while reviewing
Renovate PR #434 (lancedb 0.37 → 0.38). Research slug:
`.cheese/research/lancedb-038-migration/lancedb-038-migration.md`.
Ranking effects of lance major bumps live in
[eval-harness-gotchas](eval-harness-gotchas.md); compaction and index-delta
merging live in
[maintenance-compaction-index-deltas](maintenance-compaction-index-deltas.md).

## lancedb 0.38.0 does not compile with default features

PR #434 fails clippy and build on every OS with:

```
error[E0599]: no variant named `Http` found for enum `error::Error`
  --> lancedb-0.38.0/src/job.rs:56:40
```

`src/job.rs` is new in 0.38 (async job handles). It constructs
`Error::Http` ungated, but `src/error.rs` declares that variant under
`#[cfg(feature = "remote")]`. lancedb's `default = []`, and
`crates/hallouminate-adapters/Cargo.toml` uses bare `lancedb = "0.37"`, so
the crate itself fails to build. No hallouminate call site is involved, and
none of the 0.38 breaking changes (manifest-authoritative table existence,
pydantic v2, branch merge, listing pagination) touch APIs we use.

Decision: hold #434 until 0.38.1 ships upstream fix lancedb #4100. The
workaround `lancedb = { version = "0.38", features = ["remote"] }` compiles
but enables an HTTP remote-table surface an embedded local store never
uses; use it only if a 0.38 feature becomes urgent, and revert on 0.38.1.

PR #473 (donor vector reuse) depends on `query().select(Select::columns(..))`,
`execute()` as an incremental `TryStream`, and `RecordBatch` decoding. All
three are unchanged 0.37.1 → 0.38.0, so #473 and #434 can land in either
order.

## What the adapter uses today

- `merge_insert` / delete-by-predicate for row upserts keyed by
  `CorpusKey { name, canonical_root }`.
- FTS index plus vector index through `ensure_search_indexes`
  (`Index::Auto` for the vector column).
- `OptimizeAction` merge and compact in `LanceStore::maintain`.
- `list_versions()` for the version count only.
- `query().select(Select::columns(..))` on the donor-reuse and staleness
  paths; `try_next()` streaming of `RecordBatch`.

## Verified available and unused (ranked backlog)

Each item was checked against the 0.38 Rust API and our call sites. None
is benchmarked; treat the value column as a hypothesis. Items 1 and 2
shipped; the rows stay as a record.

| # | Opportunity | Why | Value | Effort |
|---|---|---|---|---|
| 1 | **Shipped (#546).** Scalar indexes on the hot filter columns | #546 added them. The schema 7 split store (#612) moved the filters: BTree on `content_key` in `chunks`, and BTree on `corpus`, `root`, and `content_key` in `root_files` (`SCALAR_INDEXES` and `FILE_SCALAR_INDEXES`, `crates/hallouminate-adapters/src/lance.rs:884-891`). | high | low |
| 2 | **Shipped (#542).** `.select()` projection on `fts_scan` and `vector_scan` | #542 projects the hit columns, so a hit no longer decodes the `embedding` column. | high | low |
| 3 | Explicit `distance_type` on `nearest_to` | None is set, so lancedb uses L2. fastembed vectors are normalized and cosine is the intended metric. L2 and cosine agree on rank for unit vectors, so this is hygiene unless normalization ever changes. | medium | low |
| 4 | BTree on `indexed_at_ms` and `ord` | Hot staleness paths use `only_if("ord = 0")` and filter on `indexed_at_ms`. | medium | low |
| 5 | Explicit `IvfHnswSq` / `IvfHnswFlat` instead of `Index::Auto` | Memory-for-recall trade; needs a recall benchmark first. | low–medium | medium |
| 6 | `LabelList` index on the `List<Utf8>` column | Enables `array_contains_any`; no call site filters on it today. | low | low |
| 7 | Version checkout / restore | `list_versions()` is used but no time travel; could back "wiki as of commit X". | low | medium |

Do not adopt: FTS V3 `FtsIndexBuilder::block_size` (upstream flags it
experimental and may break); computed columns, materialized views, the
UDF catalog, LSM/MemWAL, the data loader, and `cherry_pick` are remote,
server, or Python/Node only.

_Source: PR #434 review, `.cheese/research/lancedb-038-migration/` · Updated: 2026-10-15 · Supersedes: —_
