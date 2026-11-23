# Maintenance compaction and per-batch index deltas (issue #463)

`LanceStore::maintain` (`crates/hallouminate-adapters/src/lance.rs`) merges every index delta into one index per name **before** it runs `OptimizeAction::Compact`. Do not remove that step.

## Why

- `apply_batch` calls `optimize(OptimizeAction::Index(Default::default()))` after each `merge_insert`. Lance's default `OptimizeOptions` (`num_indices_to_merge: None`) appends one delta index per call, so each small fragment ends up under its own ANN delta.
- Lance's compaction planner (`lance/src/dataset/optimize.rs`, `DefaultCompactionPlanner::plan`) never puts fragments with different index-coverage sets in one bin. A one-fragment bin whose candidacy is `CompactWithNeighbors` is a no-op.
- Result before the fix: `fragments_removed=0` on every pass, fragment count never drops, and the Hard-debt gate (`backpressure.rs`) blocks every mutation forever. The real store showed 515 data files and 515 `auxiliary.idx`+`index.idx` delta dirs under `_indices`.
- FTS and FM indexes alone did not block compaction in tests (embeddings-OFF stores compact); the ANN deltas did.

## Merge cost guard

`OptimizeOptions::merge(usize::MAX)` always rewrites the index files; Lance's no-op early returns fire only for `Some(0)` or `None`. A paced pass (`Pace::Paced`, ADR daemon-rework-001) calls `maintain` once per slice, so an unconditional merge would rewrite every index on every slice under I/O pressure. `maintain` therefore calls `index_deltas_need_merge` first and merges only when some index has `num_indices > 1` or `num_unindexed_rows > 0`. In steady state (one delta per index, everything indexed) the pass emits no `index_merge_*` events.



The guard skips `IndexType::Fm`: Lance's FM index statistics report `num_indexed_rows: 0`, so lancedb derives `num_unindexed_rows == total_rows` forever. Without the exclusion the guard never returns false and every pass rewrites the indexes (found by the /press attack `maintain_second_call_on_merged_steady_state_leaves_indices_untouched`).

Known gap (pre-existing, not fixed here, issue #464): a paced slice (`max_fragments_per_slice: Some(n)`) removes zero fragments when the backlog is one compaction bin larger than `n`, because Lance's `max_source_fragments` drops whole tasks over the budget (`take_while(total <= max)`) and `run_maintenance_pass_with` stops slicing on `removed < slice_budget`. Hard-forced passes run `Pace::Full`, so #463 drains; the defer-bound-forced paced path does not.

## Regression seam

`lance::tests::maintain_compacts_fragments_covered_by_per_batch_ann_index_deltas`: real store, ANN index built at 300 rows, 30 one-file batches, asserts several ANN deltas exist, then `maintain` must report `fragments_removed > 0`, reduce `debt().fragments`, and leave one ANN delta.

The daemon lifecycle test `state::tests::maintenance_tick_emits_correlated_structured_lifecycle` runs on a fresh store with no indexes, so the merge is skipped and the event order stays `started, write_lane_acquired, compaction_*, prune_*, finished`. When a merge runs, `index_merge_started` / `index_merge_finished` (with `index_segments` and `merge_ms`) sit between `write_lane_acquired` and `compaction_started`.

## Open follow-up (issue #465)

`merge(usize::MAX)` rewrites the base index too, so a merge is O(corpus) however many deltas exist; the guard only spares an idle store. A write-time cap in `apply_batch` does not reduce that (each trip is still a full merge, so under ingest it is more total work), and per-batch `merge(k)` with `k >= 1` drags the base index into every batch. The real reduction is a tiered policy: merge only the deltas beyond the base (`merge(num_indices - 1)`, O(rows since base)) each pass and do a full merge only when delta rows pass a fraction of the base. Deferred from the #463 cure as a design decision. `index_segments` and `merge_ms` on the `index_merge_*` events exist to size those thresholds.
