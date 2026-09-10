# Safe vector donors with bounded candidate storage

## Decision

A reusable donor must match the requested file's content hash and ordered embedding input.
The decoder compares stored `search_text` with each requested chunk's `search_text`.
Chunk count and ordinal validity remain necessary checks.
A rejected donor falls back to fresh embedding instead of supplying unrelated vectors.[^1]

The lookup consumes Arrow batches as a stream instead of collecting every matching batch.
It retains at most two pending candidate groups per requested content hash.
Completed vectors serve equivalent expectations in the same input batch.
Candidate storage therefore does not grow with the number of identical sibling roots for a fixed requested batch.[^2]

## Limits

This bound concerns retained decoder candidates, not whole-process memory.
Output vectors and input text still scale with requested chunks.
The database scan still visits matching rows as sibling copies increase.
This change does not claim constant scan CPU, a scalar hash index, or a measured native memory ceiling.

## Rationale

Equal file bytes do not guarantee equal embedding input.
For headingless Markdown, different filenames can produce different summaries and search text.
Hash-only reuse therefore transfers a vector for the wrong input.
Ordered text validation preserves reuse for genuinely equivalent worktree copies without a schema migration.
A small candidate budget can forgo an optimization; fresh embedding preserves correctness.

## Wiki destination

This decision replaces the hash-and-count-only rule in ADR-002 of `worktree-index-provisioning-adr.md`.
The candidate bound concerns lookup memory, not ADR-003's stored-row retention policy.
The wiki mutation gate returns a Hard-debt timeout during this repair.
This tracked ADR preserves the decision until wiki writeback succeeds.

[^1]: crates/hallouminate-adapters/src/lance.rs::donor_expectation; crates/hallouminate-adapters/src/lance.rs::decode_donor_batch
[^2]: crates/hallouminate-adapters/src/lance.rs::MAX_PENDING_DONOR_GROUPS_PER_HASH; crates/hallouminate-adapters/src/lance.rs::donor_vectors_batch
