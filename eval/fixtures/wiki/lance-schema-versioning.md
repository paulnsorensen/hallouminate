---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - crates/hallouminate-adapters/src/lance.rs
---
# Lance schema versioning

The derived LanceDB store carries a schema version stamped in its table
metadata. Opening a store written by an older version triggers a full
table rebuild followed by ordinary catch-up indexing; opening one written
by a newer version is fatal. Neither behavior needs to be gentle, because
the filesystem — not the derived store — is what a rebuild is safe to
trust.[^1]

## Why older is a rebuild, not a migration

hallouminate does not write per-column migrations between schema
versions. When the daemon opens a derived store and finds an older schema
version than the adapter expects — most recently, the addition of the
`search_text` and `root` columns alongside the existing corpus and
`file_ref` columns — it drops the table and rebuilds it against the
current schema, then walks the corpus's configured roots and reindexes
every file from scratch.[^2] This is more expensive than a targeted
column migration would be, but it is far simpler to reason about: there
is exactly one code path that populates the table, the same one that runs
on a brand-new corpus, so there is no second migration path that can drift
out of sync with the schema the rest of the adapter assumes.

## Why newer is fatal, not a downgrade

The reverse direction is not handled at all. If a derived store was
written by a newer build of hallouminate than the one currently opening
it — for instance, a store touched by a teammate's newer daemon and then
opened by an older CLI — the adapter refuses to open it rather than
attempting to read forward-compatible rows. A silent partial read against
an unknown newer schema risks returning corrupted or truncated chunk rows
without any signal that something is wrong, which is worse than refusing
outright. The fix, in practice, is to upgrade the reading build rather
than downgrade the store.[^3]

## Why this is safe: the filesystem is the source of truth

Both directions of this policy lean on the same guarantee restated
elsewhere in this wiki: LanceDB rows are always derived from the markdown
on disk, never the reverse. A full table rebuild loses nothing, because
nothing in the derived store is canonical — every chunk, embedding, and
piece of provenance metadata is reproducible from `add_markdown` and
`index` alone. That is what makes the blunt "rebuild on old, refuse on
new" policy acceptable where a more surgical migration path would be
required for a database that held its own irreplaceable state.[^4]

## Cost in practice

A schema-version rebuild pays the same cost as a first-time `index` of the
whole corpus: every file is rewalked, rechunked, and re-embedded. For the
small wiki corpora this project targets, that cost is seconds, not
minutes — the same reason a small embedding model and a modest chunk
budget were chosen elsewhere in this design.

See [design-rationale](design-rationale.md), [worktree-corpus-identity](worktree-corpus-identity.md), [index-incremental-diff](index-incremental-diff.md).

[^1]: `crates/hallouminate-adapters/src/lance.rs:60-88`.
[^2]: `crates/hallouminate-adapters/src/lance.rs:1252-1276`; https://github.com/paulnsorensen/hallouminate/issues/288
[^3]: `crates/hallouminate-adapters/src/lance.rs:90-104`.
[^4]: `crates/hallouminate-domain/src/indexer/apply.rs:1-20`.

_Source: issue #288 and `hallouminate-adapters::lance` · Updated: 2026-07-18_
