# Racy-clean mtime smudge at snapshot write

Why stored file mtimes are sometimes deliberately off by one millisecond, and why the fix lives at the *write* seam rather than in the equality gates.

## The race

Two equality gates trust `stored mtime_ms == on-disk mtime_ms` to mean "content unchanged": the watcher's stage-1 gate (`crates/hallouminate-daemon/src/watch.rs` `mtime_matches_last_index`, which sheds events *without reading content*) and the bulk planner (`crates/hallouminate-domain/src/indexer/plan.rs` `plan()`, which hash-verifies on equality). If a file is rewritten within the same millisecond as its recorded mtime (mtime preserved), equality can't see the change — git calls this the racy-clean problem. The bulk planner self-heals (it re-hashes on equality), but the watcher path served stale content until the next bulk index.

## The fix — smudge at the single write seam

`smudge_racy_mtime` in `crates/hallouminate-domain/src/indexer/apply.rs`: when a snapshot's mtime_ms is **at or after** `indexed_at_ms` (indexing finished within the file's mtime millisecond, so a later same-ms rewrite would be invisible), store `mtime_ms - 1`. Every future equality gate then falls through to the content hash for exactly the racy files. Converges: the next reindex lands strictly after that millisecond and records the true mtime. Wired at all three mtime-store sites in `apply()` (upsert, `touch_mtime` bump, hash-mismatch fallthrough).

## Why not fix the gates instead

- Hashing on equality in the *watcher* would gut stage-1's purpose (shedding the access-event storm with one stat, zero content reads — pinned by `unchanged_mtime_event_skips_without_reading_content`).
- `FileSnapshot` stores no `indexed_at` column, so a git-faithful "compare against index-write time" check at read time would need a Lance schema migration; the write-side smudge needs none.

## Test-fixture consequence

Any test that passes an explicit mtime **at or after wall-clock now** to the index path gets smudged storage (`mtime - 1`) and fails exact-mtime assertions. Backdate fixtures (`File::set_modified(now - 10s)`) — see the two `index_single_file_*` tests in `daemon/dispatch.rs`.

Related: [[daemon-and-cli]]
