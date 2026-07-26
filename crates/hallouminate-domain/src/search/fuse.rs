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

    /// Criterion 2 (chunk led by one signal must not outrank one led by two
    /// or more) holds only when `max_weight <= sum of the two smallest
    /// weights` — a general property of weighted RRF, not specific to this
    /// fusion. This test builds the tightest realizable configuration from
    /// the live `search::{FTS_WEIGHT, VECTOR_WEIGHT, RIPGREP_WEIGHT,
    /// CONTAINS_WEIGHT}` constants (max-weight signal leads the
    /// single-signal chunk; the two smallest-weight signals jointly lead the
    /// dual-signal chunk) and asserts whichever outcome that condition
    /// predicts, so it tracks whatever weights ship rather than a hardcoded
    /// configuration.
    ///
    /// At the shipped weights (2.0 / 1.0 / 0.5 / 0.5) `max_weight` (2.0)
    /// exceeds the two smallest' sum (0.5 + 0.5 = 1.0), so the condition is
    /// false and the single-signal chunk measurably outranks the dual-signal
    /// one — the accepted tradeoff recorded in ADR-006 (wiki), which kept
    /// these weights over a 1.0/1.0 alternative that satisfies the condition
    /// with zero margin, because 1.0/1.0 cost a measured 0.0137 Recall@5
    /// regression.
    #[test]
    fn criterion_2_holds_only_when_max_weight_le_sum_of_two_smallest() {
        use crate::search::{CONTAINS_WEIGHT, FTS_WEIGHT, RIPGREP_WEIGHT, VECTOR_WEIGHT};

        const POOL: usize = 50;
        let filler: Vec<String> = (0..POOL).map(|i| format!("filler{i:03}")).collect();

        // Build a list placing `first` at rank 0 and `last` at rank 49.
        let build = |first: &str, last: &str| -> Vec<String> {
            let mut ids = filler.clone();
            ids[0] = first.to_string();
            ids[POOL - 1] = last.to_string();
            ids
        };

        let single = "single_signal_chunk";
        let dual = "dual_signal_chunk";

        let mut weights = [
            ("fts", FTS_WEIGHT),
            ("vector", VECTOR_WEIGHT),
            ("rg", RIPGREP_WEIGHT),
            ("fm", CONTAINS_WEIGHT),
        ];
        weights.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let (smallest_a, smallest_b) = (weights[0], weights[1]);
        let largest = weights[3];
        let max_weight = largest.1;
        let two_smallest_sum = smallest_a.1 + smallest_b.1;

        // Max-weight signal leads `single`, last elsewhere. The two
        // smallest-weight signals jointly lead `dual`, last elsewhere. The
        // remaining (middle-weight) signal favours neither: `single` takes
        // the last slot, `dual` the next-worst, since two chunks cannot both
        // occupy the same rank in one list.
        let list_for = |name: &str, weight: f32| -> RankedList {
            let chunk_ids = if name == largest.0 {
                build(single, dual)
            } else if name == smallest_a.0 || name == smallest_b.0 {
                build(dual, single)
            } else {
                let mut ids = filler.clone();
                ids[POOL - 1] = single.to_string();
                ids[POOL - 2] = dual.to_string();
                ids
            };
            RankedList { weight, chunk_ids }
        };

        let lists = vec![
            list_for("fts", FTS_WEIGHT),
            list_for("vector", VECTOR_WEIGHT),
            list_for("rg", RIPGREP_WEIGHT),
            list_for("fm", CONTAINS_WEIGHT),
        ];

        let fused = fuse(&lists, K);

        if max_weight <= two_smallest_sum {
            assert!(
                position_of(&fused, dual) <= position_of(&fused, single),
                "criterion 2 holds at these weights (max {max_weight} <= two smallest sum \
                 {two_smallest_sum}): the dual-signal chunk must not rank below the \
                 single-signal one"
            );
        } else {
            assert!(
                score_of(&fused, single) > score_of(&fused, dual),
                "max_weight {max_weight} exceeds the two smallest weights' sum \
                 {two_smallest_sum}, so criterion 2's original wording is known to be \
                 violated at these weights (see ADR-006) — the single-signal chunk must \
                 outrank the dual-signal one in the tightest realizable configuration"
            );
        }
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
