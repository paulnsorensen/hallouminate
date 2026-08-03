---
status: reviewed
last_verified: 2026-08-02
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/215
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/pull/290
  - https://github.com/paulnsorensen/hallouminate/pull/304
---
# Worktree corpus identity

Same-named corpora in sibling git worktrees are now fully isolated: they neither delete each other's rows (#215) nor surface each other's content in search (#290). Every chunk row is keyed by `CorpusKey { name, canonical_root }`, and every store predicate matches `corpus = ? AND root = ?`, so a query sees only rows under its own resolved worktree root.[^1]

## Why worktrees resolve to different roots

Repo-layer discovery finds the tracked `.hallouminate/config.toml` inside each worktree before it reaches the git boundary. The derived `repo:<name>:wiki` therefore points at that worktree's own `.hallouminate/wiki`, which is required for branch-local knowledge and must not be collapsed onto the main checkout.

All worktrees share one daemon store. Rows carry a corpus name, a canonical `root`, and an absolute `file_ref`; search, list, touch, delete, and batch-replace predicates scope on name **and** root (`corpus_key_filter`, `crates/hallouminate-adapters/src/lance.rs:555-567`). Distinct worktree roots therefore occupy distinct logical namespaces even though they share one store.[^2]

## What #215 fixed — delete stomping

Before #215, a full index diff treated rows from sibling roots as deleted because those paths were absent from the requesting worktree's filesystem walk. The indexer now canonicalizes configured roots and executes a planned delete only when the stored row's `CorpusKey` is among this request's configured keys (`crates/hallouminate-domain/src/indexer/apply.rs:184-200`).[^3]

That root-scoped-delete check prevented destructive stomping and needless re-embedding, but it deliberately left sibling rows in the table, so on its own it did not solve read isolation.

## What #290 fixed — the search leak (shipped)

The issue #288 reproduction queried `repo:hallouminate:wiki` from one Conductor worktree and received a stale `design-rationale.md` hit from another worktree: name-only filtering made both roots eligible.[^4] PR #290 closed this by making canonical root part of persisted identity:

- `CorpusKey { name, canonical_root }` is a real type (`crates/hallouminate-domain/src/common.rs:45`); root resolution uses `canonicalize_or_passthrough(expand_tilde(..))` at every seam.
- Lance rows gained a UTF-8 `root` column under schema **v4** (which also added derived `search_text`); `default_schema_version()` returns `4` (`crates/hallouminate-adapters/src/lance.rs:143-153`).
- Every `ChunkStore` predicate — search, list, touch, delete, stats, batch replacement — matches name plus root. A repo-derived request sees only its own root; a deliberately multi-root corpus unions root-scoped queries with per-hit provenance.
- Opening a `schema_version < 4` store logs, rebuilds the derived table, and runs catch-up indexing; opening a newer store stays fatal.

## Retired-root garbage collection (shipped)

Per-worktree storage no longer grows without bound. PR #304 (`feat(daemon): garbage-collect chunk rows at retired worktree roots`, commit `b25ade5`) added `ChunkStore::distinct_roots` and `delete_root`, evicting rows whose recorded root no longer exists on disk. Issue #286 tracked this orphan-cleanup work.

## Related watcher caveat

Repo-layer worktree wikis are not part of the daemon's boot-time baseline watcher set. Edits made outside `add_markdown` remain invisible until an explicit `index`; MCP writes reindex their target file immediately.

See [ground-search-quality](ground-search-quality.md), [ground-search-quality-adrs](ground-search-quality-adrs.md), [domain-model](domain-model.md), [config-layering](config-layering.md), and [worktree-dev-gotchas](worktree-dev-gotchas.md).

[^1]: `crates/hallouminate-domain/src/common.rs:45`; PR #290.
[^2]: `crates/hallouminate-adapters/src/lance.rs:555-567`; `merge_insert` scopes on `["corpus", "root", "chunk_id"]` (`lance.rs:1250`).
[^3]: `crates/hallouminate-domain/src/indexer/apply.rs:184-200`; https://github.com/paulnsorensen/hallouminate/issues/215
[^4]: https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067056364

_Source: issues #215/#288, PRs #290 and #304 · Updated: 2026-08-02 · Supersedes: the 2026-07-25 page that described #288 root-aware identity as merely proposed and GC as deferred to #286_
