---
status: reviewed
last_verified: 2026-07-24
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/215
  - https://github.com/paulnsorensen/hallouminate/issues/288
---
# Worktree corpus identity

Same-named corpora in sibling git worktrees no longer delete each other's rows after #215, but their rows still coexist under one name-only search identity. A query from one worktree can therefore return stale content from another. Issue #288 proposes making canonical root part of persisted corpus identity.[^1]

## Why worktrees resolve to different roots

Repo-layer discovery finds the tracked `.hallouminate/config.toml` inside each worktree before it reaches the git boundary. The derived `repo:<name>:wiki` therefore points at that worktree's own `.hallouminate/wiki`, which is required for branch-local knowledge and must not be collapsed onto the main checkout.

All worktrees share one daemon store, however. The current Lance rows carry a corpus name and an absolute `file_ref`, while search, list, touch, and delete predicates accept the corpus name as their storage scope. Different worktree roots therefore share a logical namespace even though their files are distinct.[^2]

## What #215 fixed

Before #215, a full index diff treated rows from sibling roots as deleted because those paths were absent from the requesting worktree's filesystem walk. The indexer now canonicalizes configured roots and executes a planned delete only when the stored `file_ref` starts under one of those roots.[^3]

That root-scoped-delete check prevents destructive stomping and needless re-embedding. It deliberately leaves sibling rows in the table, so it does not solve read isolation.

## Remaining search defect

The issue #288 reproduction queried `repo:hallouminate:wiki` from one Conductor worktree and received a stale `design-rationale.md` hit from another worktree. Name-only filtering made both roots eligible, and the absolute path merely exposed which sibling supplied the hit.[^4]

Observable symptoms are:

- duplicate logical pages with different absolute paths;
- `stale: true` hits from a sibling branch;
- rankings influenced by content that is not present in the requesting worktree;
- table growth proportional to active worktrees until orphan cleanup exists.

## Proposed identity in #288

The draft introduces `CorpusKey { name, canonical_root }` and persists the canonical root on every chunk row. Storage predicates for search, list, touch, delete, and batch replacement become `name AND root`, so a request sees only rows belonging to its resolved worktree root.[^5]

A configured corpus with multiple roots is searched as a union of root-scoped queries, retaining per-hit provenance. Canonical roots use the existing `canonicalize_or_passthrough(expand_tilde(..))` convention so indexing and querying derive identical keys.

The root column and `search_text` column share schema v4. Opening an older derived store triggers a table rebuild and normal catch-up indexing; opening a store written by a newer build remains fatal. Garbage collection for rows belonging to retired worktrees is deferred to #286.

This decision supersedes this page's earlier rejection of per-root identity. Root-scoped deletes were the smallest safe fix for #215; the live search reproduction in #288 supplies the missing evidence that read isolation also needs root-aware identity.

## Related watcher caveat

Repo-layer worktree wikis are not part of the daemon's boot-time baseline watcher set. Edits made outside `add_markdown` remain invisible until an explicit `index`; MCP writes reindex their target file immediately.

See [ground-search-quality](ground-search-quality.md), [ground-search-quality-adrs](ground-search-quality-adrs.md), [domain-model](domain-model.md), [config-layering](config-layering.md), and [worktree-dev-gotchas](worktree-dev-gotchas.md).

[^1]: https://github.com/paulnsorensen/hallouminate/issues/288
[^2]: `crates/hallouminate-adapters/src/lance.rs:237-388,1252-1276,1345-1509`.
[^3]: `crates/hallouminate-domain/src/indexer/apply.rs:163-188`; https://github.com/paulnsorensen/hallouminate/issues/215
[^4]: https://github.com/paulnsorensen/hallouminate/issues/288#issuecomment-5067056364
[^5]: Proposed interface in https://github.com/paulnsorensen/hallouminate/issues/288

_Source: issues #215 and #288 · Updated: 2026-07-24 · Supersedes: the 2026-07-13 decision to accept union-search duplicates and reject per-root identity_
