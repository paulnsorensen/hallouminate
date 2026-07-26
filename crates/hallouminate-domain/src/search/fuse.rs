//! Weighted Reciprocal Rank Fusion over N ranked lists.
//!
//! RRF is rank-based on purpose: raw retriever scores are not comparable
//! across retrievers, so every signal contributes through its rank instead
//! of its score. That is why signals join here as ranked lists rather than
//! as additive bonuses applied to an already-fused score.

use std::collections::HashMap;

/// One retrieval signal's contribution: its weight and its chunks in rank
/// order, best first. Position in `chunk_ids` *is* the rank.
#[derive(Debug, Clone)]
pub struct RankedList {
    pub weight: f32,
    pub chunk_ids: Vec<String>,
}

/// Weighted RRF over N ranked lists: sum of `weight / (k + rank)`.
///
/// Rank is 0-based, matching the LanceDB reranker this replaces so the
/// FTS and vector signals keep their existing relative behaviour.
///
/// Returns `(chunk_id, score)` in ranked order.
pub fn fuse(lists: &[RankedList], k: f32) -> Vec<(String, f32)> {
    let mut scores: HashMap<&str, (f32, usize)> = HashMap::new();
    for list in lists {
        let mut seen_in_list: HashMap<&str, ()> = HashMap::new();
        for (rank, chunk_id) in list.chunk_ids.iter().enumerate() {
            // A chunk repeated within one list contributes only from its
            // best rank; a duplicate is not extra evidence.
            if seen_in_list.insert(chunk_id.as_str(), ()).is_some() {
                continue;
            }
            let contribution = list.weight / (rank as f32 + k);
            let entry = scores.entry(chunk_id.as_str()).or_insert((0.0, 0));
            entry.0 += contribution;
            entry.1 += 1;
        }
    }

    let mut fused: Vec<(&str, f32, usize)> = scores
        .into_iter()
        .map(|(id, (score, signals))| (id, score, signals))
        .collect();

    // Score first, then how many signals backed the chunk, then chunk_id.
    //
    // The signal-count key breaks score ties toward the chunk more signals
    // agreed on. Corroboration across signals is the evidence RRF exists to
    // reward, so when two chunks score equally the more corroborated one
    // ranks higher; chunk_id only settles what remains, keeping the order
    // deterministic.
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(b.0))
    });

    fused
        .into_iter()
        .map(|(id, score, _)| (id.to_string(), score))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: f32 = 60.0;

    fn list(weight: f32, ids: &[&str]) -> RankedList {
        RankedList {
            weight,
            chunk_ids: ids.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn score_of(fused: &[(String, f32)], id: &str) -> f32 {
        fused
            .iter()
            .find(|(c, _)| c == id)
            .unwrap_or_else(|| panic!("{id} missing from fused output"))
            .1
    }

    fn position_of(fused: &[(String, f32)], id: &str) -> usize {
        fused
            .iter()
            .position(|(c, _)| c == id)
            .unwrap_or_else(|| panic!("{id} missing from fused output"))
    }

    #[test]
    fn empty_input_fuses_to_nothing() {
        assert!(fuse(&[], K).is_empty());
        assert!(fuse(&[list(1.0, &[])], K).is_empty());
    }

    #[test]
    fn contribution_is_weight_over_k_plus_zero_based_rank() {
        let fused = fuse(&[list(2.0, &["a", "b"])], K);
        // Rank is 0-based: "a" gets 2/(60+0), "b" gets 2/(60+1).
        assert!((score_of(&fused, "a") - 2.0 / 60.0).abs() < 1e-7);
        assert!((score_of(&fused, "b") - 2.0 / 61.0).abs() < 1e-7);
    }

    #[test]
    fn scores_accumulate_across_lists() {
        let fused = fuse(&[list(2.0, &["a"]), list(1.0, &["a"])], K);
        assert!((score_of(&fused, "a") - (2.0 / 60.0 + 1.0 / 60.0)).abs() < 1e-7);
    }

    #[test]
    fn chunk_backed_by_several_signals_outranks_one_backed_by_a_single_signal() {
        // "broad" is mid-ranked by three signals; "narrow" is first by one.
        let fused = fuse(
            &[
                list(1.0, &["narrow", "broad"]),
                list(1.0, &["broad"]),
                list(1.0, &["broad"]),
            ],
            K,
        );
        assert_eq!(position_of(&fused, "broad"), 0);
        assert!(score_of(&fused, "broad") > score_of(&fused, "narrow"));
    }

    /// The fusion invariant: a chunk ranked first by a single signal and
    /// last by every other signal must not outrank a chunk ranked first by
    /// two or more signals.
    ///
    /// This is the case the additive-bonus scheme got wrong, where one
    /// literal substring hit could lift the worst-ranked chunk in the pool
    /// above the best-ranked one.
    ///
    /// Note the two chunks cannot both occupy the last slot of the same
    /// list, so the single-signal chunk is last (rank 49) everywhere it is
    /// not first, and the two-signal chunk takes the next-worst slot
    /// (rank 48) in the lists where neither leads. That is the tightest
    /// realizable configuration, and the margin is genuinely narrow:
    /// 8.5e-05 at these weights.
    #[test]
    fn single_signal_first_cannot_outrank_two_signal_first() {
        const POOL: usize = 50;
        let filler: Vec<String> = (0..POOL).map(|i| format!("filler{i:03}")).collect();

        // Build a list placing `first` at rank 0 and the named chunks at
        // the given low ranks, padding with filler.
        let build = |first: &str, low: &[(&str, usize)]| -> Vec<String> {
            let mut ids: Vec<String> = filler.clone();
            ids[0] = first.to_string();
            for (id, rank) in low {
                ids[*rank] = (*id).to_string();
            }
            ids
        };

        let single = "single_signal_chunk";
        let dual = "dual_signal_chunk";

        // fts (weight 2.0): `single` first, `dual` last.
        let fts = build(single, &[(dual, POOL - 1)]);
        // vector (weight 1.0): `dual` first, `single` last.
        let vector = build(dual, &[(single, POOL - 1)]);
        // rg (weight 1.0): `dual` first, `single` last.
        let rg = build(dual, &[(single, POOL - 1)]);
        // fm (weight 1.0): neither leads. `single` is last, so `dual`
        // takes rank 48.
        let mut fm: Vec<String> = filler.clone();
        fm[POOL - 1] = single.to_string();
        fm[POOL - 2] = dual.to_string();

        let fused = fuse(
            &[
                RankedList {
                    weight: 2.0,
                    chunk_ids: fts,
                },
                RankedList {
                    weight: 1.0,
                    chunk_ids: vector,
                },
                RankedList {
                    weight: 1.0,
                    chunk_ids: rg,
                },
                RankedList {
                    weight: 1.0,
                    chunk_ids: fm,
                },
            ],
            K,
        );

        assert!(
            position_of(&fused, dual) < position_of(&fused, single),
            "chunk led by two signals must rank above one led by a single signal"
        );
        assert!(
            score_of(&fused, dual) > score_of(&fused, single),
            "the two-signal chunk must win on fused score, not on a tie-break"
        );
    }

    #[test]
    fn duplicate_within_a_list_counts_only_its_best_rank() {
        let with_dup = fuse(&[list(1.0, &["a", "b", "a"])], K);
        let without = fuse(&[list(1.0, &["a", "b"])], K);
        assert!((score_of(&with_dup, "a") - score_of(&without, "a")).abs() < 1e-7);
        // And the duplicate must not inflate its signal count into a
        // tie-break advantage.
        assert_eq!(with_dup.len(), 2);
    }

    #[test]
    fn signal_count_breaks_a_score_tie_before_chunk_id() {
        // "zzz" is backed by two lists, "aaa" by one, at identical score:
        // 1/60 + 1/60 == 2/60.
        let fused = fuse(
            &[
                list(2.0, &["aaa"]),
                list(1.0, &["zzz"]),
                list(1.0, &["zzz"]),
            ],
            K,
        );
        assert!((score_of(&fused, "aaa") - score_of(&fused, "zzz")).abs() < 1e-9);
        assert_eq!(
            position_of(&fused, "zzz"),
            0,
            "score tie must fall to the more corroborated chunk, not to chunk_id"
        );
    }

    #[test]
    fn chunk_id_settles_a_full_tie_deterministically() {
        let fused = fuse(&[list(1.0, &["b"]), list(1.0, &["a"])], K);
        assert!((score_of(&fused, "a") - score_of(&fused, "b")).abs() < 1e-9);
        assert_eq!(position_of(&fused, "a"), 0, "ascending chunk_id settles it");
    }
}
