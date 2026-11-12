---
status: reviewed
last_verified: 2026-11-12
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/pull/434
  - https://github.com/lancedb/lancedb/pull/4233
  - https://github.com/lancedb/lancedb/pull/4103
  - https://github.com/lance-format/lance/pull/9481
  - https://github.com/lance-format/lance/pull/8657
  - https://github.com/paulnsorensen/hallouminate/issues/682
---
# LanceDB adapter notes

How `crates/hallouminate-adapters/src/lance.rs` uses the `lancedb` crate
today, what it does not use, and the version traps found on the 0.37 → 0.41
upgrade (PR #434). The adapters crate pins `lancedb = "0.41"`
(`crates/hallouminate-adapters/Cargo.toml:12`; lock: 0.41.0). The upgrade
research is local-only and is not in git.
Ranking effects of lance major bumps live in
[eval-harness-gotchas](eval-harness-gotchas.md); compaction and index-delta
merging live in
[maintenance-compaction-index-deltas](maintenance-compaction-index-deltas.md).

## lancedb 0.38–0.40 do not compile with default features

lancedb 0.38.0, 0.39.0, and 0.40.0 fail with:

```
error[E0599]: no variant named `Http` found for enum `error::Error`
  --> lancedb-0.40.0/src/job.rs:102:40
```

`src/job.rs` builds `Error::Http` ungated, but `src/error.rs` declares that
variant under `#[cfg(feature = "remote")]`. lancedb's `default = []`, and
`crates/hallouminate-adapters/Cargo.toml` uses a bare `lancedb` dependency,
so the crate itself fails. Upstream issues #4096 and #4142 report it. The fix
(lancedb #4233 and #4103, a `remote_job_error` helper) ships first in
**0.41.0**; there is no 0.38.1. Dependabot PR #495 (0.40.0) was closed for
this reason. Hallouminate issues #635 (0.39.0) and #648 (0.40.0) record the
same defect.

Rule: never take a lancedb release in that range. Do not enable `remote` as
a workaround; it adds an HTTP remote-table surface an embedded store never
uses. The feature turns on `arrow-flight`, `tonic`, and `prost`. PR #434
merged as lancedb 0.41 with the bare dependency and no `remote` feature.

## lancedb 0.40 broke the exhaustive `IndexType` match

`refreshed_per_batch` in `lance.rs` matches `lancedb::index::IndexType` with
no wildcard arm. This is deliberate. A new index kind then fails to compile
and gets a review. `IndexType` is not `#[non_exhaustive]`. lancedb 0.40
added `ZoneMap`, `NGram`, `BloomFilter`, and `RTree`, and 0.41 added
`Custom(String)`. The `=> true` arm in `refreshed_per_batch`
(`lance.rs:899`) lists these variants. The store builds only BTree, FTS, Fm,
and auto vector indexes, so runtime behavior did not change (issues #629 and
#648). Do not add a `_ =>` arm.

## Open risk: first-create crash recovery

`open_or_create_table` (`lance.rs:2787`) lists names with `table_names()`,
opens a listed table, and creates an unlisted one. The adapter connects with
the default listing database (`lance.rs:1337`). In lancedb 0.41 that database
lists every `<name>.lance` child directory and does not read its manifest
(`lancedb-0.41.0/src/database/listing.rs:942-971`). A table directory without
a manifest, for example after a crash during the first `create_table`, is
therefore still listed, and the code calls `open_table` on it. Nobody has
verified the outcome. Issue #682 asks for a recovery test and, if needed, an
open-first flow. This matters only for first-open crash recovery.

## 0.41 upgrade facts

The lancedb 0.37 → 0.41 upgrade raises the workspace MSRV to 1.94 and needs
no reindex. The code changes are new `IndexType` arms and an unknown-key
check that does not depend on `serde_json` key order.

| lancedb | lance | arrow / datafusion | lancedb MSRV |
|---|---|---|---|
| 0.37.1 | 10.0.0 | 58 / 54 | 1.91 |
| 0.41.0 | 14.0.2 | 58 / 54 | 1.94 |

- The workspace `rust-version` follows the highest dependency floor, so it
  is 1.94 from 0.41 on.
- `IndexType` adds `ZoneMap`, `NGram`, `BloomFilter`, `RTree`, and
  `Custom(String)`. `refreshed_per_batch` matches it exhaustively and must
  gain an arm on each new variant.
- `table_names().execute()` still returns `Vec<String>`. The lazy-iterator
  breaking changes hit `list_databases` and namespace listings only.
- On-disk compatibility: existing tables keep their data file format,
  because appends validate against the manifest's `data_storage_format`.
  New tables default to format 2.2 instead of 2.1 (lance #8657); lance 10
  reads 2.2. The lance 14 BTree row-address domain (lance #9481, index
  version 1) applies only to stable-row-id datasets, and we do not enable
  stable row ids. No reindex is needed.
- Our vector index is `Index::Auto`, which builds IVF_PQ with L2. The 0.41
  cosine-distance scale fix (lancedb #3852) does not apply to us.
- lancedb 0.41 enables serde_json `preserve_order` without a feature gate
  (workspace `Cargo.toml`, lancedb #4292). Cargo unifies features across the
  build graph, so `default-features = false` cannot remove it. A
  `serde_json::Value` object now keeps the client's key order, and
  `deny_unknown_fields` reports the first bad key in that order. MCP batch
  envelopes therefore check unknown keys before item parsing
  (`reject_unknown_batch_fields` in `crates/hallouminate/src/mcp/tools.rs`).
  Do not write code or tests that depend on sorted `Value` keys.

## What the adapter uses today

- `merge_insert` / delete-by-predicate for row upserts keyed by
  `CorpusKey { name, canonical_root }`.
- FTS index plus vector index through `ensure_search_indexes`
  (`Index::Auto` for the vector column).
- An FM index on `search_text`. Nothing reads it; phrase mode uses a
  `strpos(lower(..))` scan without an index (#696).
- `OptimizeAction` merge and compact in `LanceStore::maintain`, paced by
  `max_source_fragments`.
- `list_versions()` for the version count only.
- `query().select(Select::columns(..))` on the donor-reuse and staleness
  paths; `try_next()` streaming of `RecordBatch`.

## Verified available and unused (ranked backlog)

Each item was checked against the 0.41 Rust API and our call sites. None
is benchmarked; treat the value column as a hypothesis.

| # | Opportunity | Why | Value | Effort |
|---|---|---|---|---|
| 1 | **Shipped (#546).** Scalar indexes on the hot filter columns | BTree on `content_key` in `chunks`, and BTree on `corpus`, `root`, and `content_key` in `root_files` (`SCALAR_INDEXES` and `FILE_SCALAR_INDEXES` in `lance.rs`). | high | low |
| 2 | **Shipped (#542).** `.select()` projection on `fts_scan` and `vector_scan` | A hit no longer decodes the `embedding` column. | high | low |
| 3 | Row or byte caps on paced compaction (#693) | lance 11 `max_source_rows` / `max_source_bytes`; fragment counts do not bound slice cost. | medium | low |
| 4 | `search_effort` on vector queries (#694) | One recall-versus-latency setting for IVF_PQ. Needs a corpus large enough to have an ANN index. | medium | low |
| 5 | BM25F `CombinedFieldsQuery` over separate fields (#695) | Native heading weighting instead of one concatenated `search_text`. Needs a schema bump and an ADR. | medium | high |
| 6 | NGram or FM index for the phrase prefilter, or drop the FM index (#696) | Phrase mode is case-insensitive; index case folding is not verified. | medium | medium |
| 7 | Explicit `distance_type` on `nearest_to` | None is set, so lancedb uses L2. fastembed vectors are normalized, so L2 and cosine agree on rank. Hygiene only. | low | low |
| 8 | Explicit `IvfHnswSq` / `IvfHnswFlat` instead of `Index::Auto` | Memory-for-recall trade; needs a recall benchmark first. | low–medium | medium |
| 9 | Version checkout / restore | Could back "wiki as of commit X". | low | medium |

Not candidates: a ZoneMap, BloomFilter, or BTree index on `indexed_at_ms`.
No query filters on `indexed_at_ms` (it is only read for stats). A
`LabelList` index has no call site that filters on the list column.

Undecided: a scalar index on `ord`. Two call sites filter on `ord = 0`.
`stored_content_keys` (`lance.rs:1830`) adds a `content_key IN (..)` filter
that the `content_key` BTree serves. `delete_orphan_content`
(`lance.rs:2312`) scans `chunks` with `ord = 0` alone, so no index serves
that scan. Nobody has measured the orphan sweep.

Do not adopt: FTS V3 `FtsIndexBuilder::block_size(256)` (upstream flags it
experimental and may break); RTree (needs `geo`); computed columns,
materialized views, Functions, MemWAL/RTE, the data loader, and
`cherry_pick` are remote, server, or Python/Node oriented.

_Source: PR #434, issues #629, #635, #648, #682, and local-only research (not in git) · Updated: 2026-11-12 · Supersedes: the 0.38 hold decision_
