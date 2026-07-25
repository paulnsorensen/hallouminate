---
status: reviewed
last_verified: 2026-07-20
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/215
  - crates/hallouminate-domain/src/indexer/apply.rs
---
# Incremental index diffing

Re-indexing a corpus does not re-embed every file on every run. The
indexer scans the corpus's configured roots, computes a content hash and
mtime for each file, and diffs that against what the derived store
already recorded, so a file whose content and mtime are unchanged since
the last index skips re-embedding entirely.[^1]

## Content hash and mtime together

Either signal alone is insufficient. Content hash alone would require
reading and hashing every file's full contents on every index run just to
discover most of them are unchanged — correct, but it throws away the
cheap win a filesystem mtime already offers. mtime alone is unsafe: a
checkout, a `git stash pop`, or a tool that rewrites a file with identical
content all bump mtime without changing content, and re-embedding on
every such touch would make routine git operations expensive. The
indexer therefore checks mtime first as a fast filter and only falls back
to a content hash comparison when mtime suggests a file changed, so an
unchanged file is skipped without ever being read twice, and a
mtime-bumped-but-identical file is still skipped once its hash matches
the stored row.[^2]

## The `touch_mtime` fast path

When a file's mtime has moved but its content hash matches the stored
row exactly, the plan step records a `touch_mtime` action rather than a
full re-embed: only the stored mtime is updated, so the next diff run
sees the file as current without needing to rehash it again. This keeps
the common "file touched but not edited" case a metadata write instead of
a wasted embedding call.[^3]

## Root-scoped deletes

A file present in the derived store but absent from the current
filesystem walk is a candidate for deletion — but only if its stored
`file_ref` falls under one of the corpus's currently configured roots.
Before this check existed, a full index diff run from one sibling
worktree could see files that genuinely belong to a different worktree's
checkout as "missing" simply because they were absent from the requesting
worktree's own filesystem walk, and would queue them for deletion. The
indexer now canonicalizes each configured root and only executes a
planned delete when the stored path starts under one of those roots,
which stops one worktree's index run from stomping rows that belong to a
sibling.[^4]

## Why this matters at corpus scale

Most `index` invocations touch a small fraction of a corpus's files —
typically the ones a single commit or edit session changed. Diffing by
hash and mtime keeps that the common case: an `index` run over an
unchanged wiki does a filesystem walk and a metadata comparison, not a
re-embed of everything, which is what keeps `add_markdown` and periodic
reindexing cheap enough to run inline rather than as a background batch
job.

See [worktree-corpus-identity](worktree-corpus-identity.md), [lance-schema-versioning](lance-schema-versioning.md), [architecture](architecture.md).

[^1]: `crates/hallouminate-domain/src/indexer/apply.rs:1-40` (scan/plan/apply orchestration).
[^2]: `crates/hallouminate-domain/src/indexer/apply.rs:44-90`.
[^3]: `crates/hallouminate-domain/src/indexer/apply.rs:95-118`.
[^4]: `crates/hallouminate-domain/src/indexer/apply.rs:163-188`; https://github.com/paulnsorensen/hallouminate/issues/215

_Source: issue #215 and `hallouminate-domain::indexer::apply` · Updated: 2026-07-20_
