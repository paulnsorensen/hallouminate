---
status: draft
last_verified: 2026-07-24
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
---
# Domain model

Issue #288 proposes derived `search_text`, worktree-specific `CorpusKey`, and automatic older-schema rebuilds. They are not shipped in the source state described here.[^spec]

## Status matrix

| Concern | Shipped baseline | Proposed |
|---|---|---|
| Search/display text | Prepared display `text` feeds rendering and search; claim comments are stripped, footnotes retained | Preserve display behavior; add search-only `search_text` |
| Corpus identity | Bare name with paths carried separately | Pair name with canonical root |
| Delete scope | #215 restricts deletion to active roots | Keep safety; apply pair identity everywhere |
| Search scope | Sibling-worktree rows may coexist | Read only the requesting root |
| Schema mismatch | Older schema fails stale | `< 4` rebuilds; `> 4` remains fatal |
| Source of truth | Filesystem Markdown | Unchanged; Lance remains derived |[^spec][^filesystem]

## Display `text`

`text` is the prepared chunk representation returned in search hits and rendered snippets. It is not byte-for-byte raw Markdown today: indexing removes inline claim-provenance comments before storing the chunk while retaining user-facing prose and footnotes.[^claims]

Issue #288's “verbatim `text`” requirement is therefore interpreted as **unchanged display behavior relative to today**, especially unchanged footnote rendering. A broader change to retain internal claim comments would require an explicit claim-provenance contract decision and is not implied by this draft.

## `search_text`

`search_text` is a deterministic, index-only representation:

```text
heading breadcrumb + "\n" + file summary + "\n" + footnote-stripped display body
```

It is added to `PreparedChunk` and the Lance chunks table. Embedding, FTS BM25, and FM exact-substring indexes consume it; snippets, rendering, line evidence, and every footnote mode continue to consume display `text`.[^spec]

Source bindings are `PreparedChunk` (`crates/hallouminate-domain/src/indexer/chunk.rs:9-20`), `PreparedFile.summary` (`chunk.rs:27-40`), and footnote exclusion in `crates/hallouminate-domain/src/footnotes.rs:14-22,82-88`.

### Invariants

- `text` is source-facing evidence; `search_text` is never rendered as source.
- `search_text` is reproducible and disposable with the index.
- Footnote definitions may remain in `text` while absent from search input.
- Breadcrumb and summary ordering is deterministic.
- Index preparation never rewrites Markdown.

LLM-generated context is excluded; #284 may layer it on later while deterministic context remains the credential-free floor.[^f001]

## `CorpusKey`

The proposed semantic identity is:

```rust
struct CorpusKey {
    name: String,
    canonical_root: PathBuf,
}
```

The exact Rust name is discretionary. Root resolution uses `canonicalize_or_passthrough(expand_tilde(configured_root))` consistently across scanning, planning, writes, and queries.[^spec]

### Propagation

The pair replaces bare-name identity through `ChunkStore`, including list, hybrid/lexical search, stats, touch, delete, and prepared-file writes. Lance rows gain UTF-8 `root`; predicates match `corpus = ? AND root = ?`, with `file_ref` identifying a file inside that scope.[^spec]

A repository-derived request resolves the requesting worktree and excludes siblings. A deliberately configured multi-root corpus unions root-scoped searches, while each hit retains its internal corpus/root provenance. This proposal does not by itself add a new public root field to MCP responses.

The current walker returns only `(FileRef, Mtime)` and planning deduplicates by `FileRef`, so implementation must carry owning root out of scan/plan. Overlapping configured roots need one deterministic ownership rule before coding.[^root-carriage]

### Invariants

- Same name plus different root means different identity.
- One worktree cannot list, search, touch, delete, replace, or count another's rows merely because names match.
- Root canonicalization policy is identical at every seam.
- Isolation does not clean retired roots; #286 owns garbage collection.[^f003]

## Schema-version semantics

`Meta.schema_version` is the bound marker in `crates/hallouminate-adapters/src/lance.rs:107-124`. Version 4 represents chunks with `search_text`, `root`, and rebuilt search indexes.[^spec]

| Stored version | Required open behavior |
|---|---|
| `< 4` | Log, recreate the derived table, then run existing catch-up indexing |
| `= 4` | Open normally |
| `> 4` | Fail with the existing fatal newer-schema error |

### Migration invariants

- Supported older schemas require no manual command.
- Rebuild progress/failure is logged; source Markdown is untouched.
- Search cannot treat a partial rebuild as complete.
- Newer schemas are never downgraded or opened optimistically.
- Catch-up regenerates embedding, FTS, and FM from `search_text`.[^spec]

## Bound existing entities

- Footnote block: existing `extract_footnotes` / `FootnoteMode`; only search input changes.
- File summary: existing `PreparedFile.summary`; proposed as document context on each chunk.
- Eval harness: existing `ground_recall_and_mrr_across_variants`; labels gain heading-prefix expectations and latency.
- Crossencoder config: existing `SearchConfig.crossencoder`; the default remains `None`, while `just eval-measure` compares the production baseline with the documented Jina-v1 opt-in without selecting a winner.[^loop]

## Approved follow-on search reliability terms

The approved `search-reliability-loop` spec adds four proposed terms without claiming they are shipped:

- **Source page** — a corpus-local evidence page whose indexed lead contains exact source identity and whose flexible body records supported claims, project relevance, and limitations.
- **Retrieval probe** — a query frozen before drafting and rerun after a write to verify exact page identity or natural discoverability.
- **Production baseline** — the scheduled fusion/no-crossencoder measurement derived from production embedding defaults.
- **Reranker diagnostic** — an explicit two-arm comparison between the production baseline and Jina v1 that reports evidence without selecting a winner.

The same approved design extends `SearchHit` with stored `search_text`, feeds that representation to the crossencoder, and continues building public Ground snippets from display `text`. This aligns every ranking stage on retrieval text without changing the Ground wire response.[^loop]

## Excluded concepts

LLM context (#284), full stale-page correction (#285), orphan cleanup (#286), confidence-gated reranking (#287), rendered-footnote changes, public response-shape expansion, and any database-first source-of-truth model are outside this proposal.[^spec]

[^spec]: [Issue #288 — specification and entity bindings](https://github.com/paulnsorensen/hallouminate/issues/288).
[^filesystem]: [design-rationale](design-rationale.md), “Filesystem is the source of truth; LanceDB is derived.”
[^claims]: `crates/hallouminate-domain/src/indexer/format.rs:180-190`; [claim-provenance-marks](claim-provenance-marks.md).
[^root-carriage]: `crates/hallouminate-domain/src/corpus/walker.rs:13-34`; `crates/hallouminate-domain/src/indexer/plan.rs:64-74`.
[^f001]: [Issue #284 — opt-in LLM context](https://github.com/paulnsorensen/hallouminate/issues/284).
[^f003]: [Issue #286 — orphan-row garbage collection](https://github.com/paulnsorensen/hallouminate/issues/286).

[^loop]: [Search reliability loop ADRs](search-reliability-loop-001.md), [production evaluation](search-reliability-loop-002.md), and [reranker representation](search-reliability-loop-003.md).

_Source: issue #288 entity bindings and the approved search-reliability-loop spec · Updated: 2026-07-24 · Supersedes: —_