# Worktree index provisioning — ADRs

These are the decisions for the approved `worktree-index-provisioning` spec (mold session 2026-08-30, issue #427, parts 1, 3 and 4).

## ADR-001: Provision the new roots with a dedicated task that uses catch_up_corpus [status: accepted]

- **Context:** <certain> The boot-time `catch_up_index` indexes only the baseline corpora (`crates/hallouminate-daemon/src/server.rs:145-151`). The daemon finds the repo-layer roots later. Each git worktree is such a root. These roots stay empty, because no code fills them. The `maintenance_loop` sleeps first, and its interval is approximately 30 minutes (`maintenance.rs:261-266`). Thus the loop is too slow for this function.
- **Decision:** The supervisor starts a Provisioner. The Provisioner has a seen-set with `CorpusKey` keys and an mpsc queue. Both continue while the daemon operates. After `handle_ground` finds the corpus, it sends the corpus to the Provisioner. The Provisioner runs the existing `catch_up_corpus` (`dispatch.rs:1463-1505`) immediately. It holds the mutation lock for that corpus and then a write-lane permit. If the pass fails, the Provisioner removes the key. Then a subsequent ground request starts a new pass.
- **Rejected:** Two alternatives. First, do the pass at the maintenance tick. But the silent interval of 30 minutes stays. Second, start a `tokio::spawn` task for each root. But this task bypasses all of the pacing controls.

## ADR-002: Copy the vectors at file level with the existing content_hash, with no schema change [status: accepted]

- **Context:** <certain> Each chunk row already contains a file-level blake3 `content_hash` (`crates/hallouminate-domain/src/indexer/chunk.rs:35`, `plan.rs:44`). One store contains only one embedding model (`validate_existing_metadata`, `lance.rs:859-870`). If the models are different, the daemon stops with an error. The files in a worktree are identical to the files in the main clone.
- **Decision:** `LanceStore::apply_batch` finds the donor rows by `content_hash` in any root or corpus. If the donor has the same number of chunks as the new chunking, the code copies the donor vectors in `ord` sequence. If the counts are different, the code makes new embeddings. The `ChunkStore` trait does not change.
- **Rejected:** Two alternatives. First, add a chunk-level hash column for `search_text`. But this column needs a v5 schema change and a backfill. The team deferred it as the follow-up worktree-index-provisioning-F001 and published a GitHub issue. Second, use git blob SHA values and offsets as the keys. But the daemon does not know about git, and these keys do not show how the code makes `search_text`.

## ADR-003: Remove part 4 of issue #427 (root GC), because the code is shipped [status: accepted]

- **Context:** <certain> Issue #427 says that no code removes the old roots. This statement is not correct. PR #304 added `gc_scan` and `gc_delete` (`maintenance.rs:600-681`). These functions operate at each maintenance tick. They also do a fail-closed stat check (`common/paths.rs:46`, `retired_roots`).
- **Decision:** This spec does not include GC work. The spec records the corrected statement as a non-goal. A dead root stays for a maximum of one maintenance interval (the default is approximately 30 minutes). This condition uses storage only. It is not a correctness problem, because #288 keeps the corpora isolated.
