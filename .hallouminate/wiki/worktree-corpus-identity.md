---
status: reviewed
last_verified: 2026-07-13
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/215
---
# Worktree corpus identity — index rows stomp across worktrees

Indexing the same-named corpus from two git worktrees is mutually destructive:
each `index`/`add_markdown` run deletes the sibling worktree's rows from the
shared LanceDB table. Only the most-recently-indexed worktree's content is
searchable at any time, and the resulting re-embed churn keeps the daemon
permanently busy. Tracked as #215.

## The mechanism

- Repo-layer discovery walks up from the client's `cwd` and stops at the
  **worktree** root, because the tracked `.hallouminate/config.toml` is found
  before the `.git` boundary check (`src/app/config.rs:273-280`). The
  `worktree_main_root` hop (`src/app/config.rs:340-369`) only fires when NO
  repo config is found first — dead code for any repo that tracks its config.
- So `repo:<name>:wiki` resolves to a **different root per worktree**, but all
  rows land in one physical LanceDB table keyed only by a corpus-name column
  (`src/adapters/lance.rs:28,534-540`); row identity is
  `(corpus name, file_ref = absolute path)` (`src/domain/indexer/writer.rs:90-100`).
- `index_corpus` diffs disk-vs-DB by corpus name alone
  (`src/domain/indexer/index.rs:24`). Rows written from sibling worktrees are
  invisible to this worktree's walker, classified as deleted files, and removed
  (`src/domain/indexer/apply.rs:142-146`).
- The per-corpus mutation lock is keyed by corpus *name*
  (`src/app/daemon/state.rs:216,825-827`), so the stomping is serialized and
  clean — deterministic deletion of the wrong worktree's data.

## Symptoms

- `corpus_stats` row counts flip-flop as different worktrees index.
- Wiki pages you wrote from checkout A stop matching `ground` after any
  index/write from checkout B (on-disk files are untouched — only index rows).
- Constant full re-embeds: every index run re-embeds content the sibling
  deleted, pegging CPU and holding the write lane + embedder guard.

## Agreed fix direction (#215)

Root-scoped deletes: the index plan may only consider for deletion DB rows
whose `file_ref` lies under the current request's resolved corpus roots.
No schema change. Tradeoffs accepted: rows from retired worktree roots linger
until a future prune command; union search can show near-duplicate hits across
sibling worktrees. Alternatives rejected: per-root corpus identity (schema
migration + search dedup) and resolving worktrees to the main checkout
(reverts the deliberate worktree-local semantics of #132/#157).

## Related caveat: no watcher for repo-layer wikis

`spawn_corpus_watcher` enumerates only the daemon's boot-time **baseline**
corpora (`src/app/daemon/watch.rs:81-84`). A worktree-local repo-layer wiki
gets no auto-reindex-on-save at all — edits made outside `add_markdown` are
invisible until an explicit `index`.

See also: [daemon-and-cli](daemon-and-cli.md),
[worktree-dev-gotchas](worktree-dev-gotchas.md).

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-07-13 · Supersedes: —_
