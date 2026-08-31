# Worktree index provisioning — ADRs

Decisions behind the approved `worktree-index-provisioning` spec (mold session 2026-08-30, issue #427 parts 1/3/4).

## ADR-001: Provision lazily-discovered roots via a dedicated task reusing catch_up_corpus [status: accepted]

- **Context:** <certain> Boot-time `catch_up_index` covers only baseline corpora (`crates/hallouminate-daemon/src/server.rs:145-151`); lazily-discovered repo-layer roots (every git worktree) start empty and nothing fills them. `maintenance_loop` sleeps first with a ~30-min jittered interval (`maintenance.rs:261-266`), too slow for provisioning.
- **Decision:** A supervisor-spawned Provisioner (per-daemon-lifetime seen-set keyed by `CorpusKey` + mpsc queue), fed by `handle_ground` after corpus resolution, runs the existing `catch_up_corpus` (`dispatch.rs:1463-1505`) immediately under the per-corpus mutation lock + write-lane permit. Failure clears the seen-mark so a later ground retries.
- **Rejected:** riding the maintenance tick (30-min silent window survives); naked `tokio::spawn` per root (bypasses all pacing).

## ADR-002: File-level vector reuse via existing content_hash, no schema migration [status: accepted]

- **Context:** <certain> Every chunk row already carries a file-level blake3 `content_hash` (`crates/hallouminate-domain/src/indexer/chunk.rs:35`, `plan.rs:44`); one store holds exactly one embedding model (`validate_existing_metadata`, `lance.rs:859-870`, mismatch fatal). Worktree files are byte-identical to their main clone.
- **Decision:** Inside `LanceStore::apply_batch`, look up donor rows by `content_hash` (any root/corpus) and copy vectors ord-aligned when the donor chunk count equals the fresh chunking's count; else embed. `ChunkStore` trait unchanged.
- **Rejected:** chunk-level `search_text`-hash column (v5 bump + backfill — deferred as follow-up worktree-index-provisioning-F001, published as a GitHub issue); git blob SHA + offset keys (daemon is git-agnostic; doesn't capture search_text derivation).

## ADR-003: Drop issue #427 part 4 (root GC) as already shipped [status: accepted]

- **Context:** <certain> Issue #427 claimed roots are never cleaned up; in fact #304 shipped `gc_scan`/`gc_delete` (`maintenance.rs:600-681`) running on every maintenance tick with a fail-closed stat check (`common/paths.rs:46` `retired_roots`).
- **Decision:** No GC work in this spec; the corrected premise is recorded as a spec non-goal. Dead roots persist at most one maintenance interval (~30 min default), which is storage-only, never a correctness issue under #288 isolation.
