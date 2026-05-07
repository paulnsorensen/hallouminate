use std::collections::BTreeMap;

use crate::domain::common::ChunkId;

use super::rrf::FusedHit;

pub fn convex_fuse<A, B>(fts: &[(ChunkId, A)], vec: &[(ChunkId, B)], alpha: f32) -> Vec<FusedHit> {
    let alpha = f64::from(alpha);
    let mut hits: BTreeMap<ChunkId, FusedHit> = BTreeMap::new();
    for (i, (id, _)) in fts.iter().enumerate() {
        let rank = (i as u32) + 1;
        let entry = hits.entry(*id).or_insert(FusedHit {
            chunk_id: *id,
            score: 0.0,
            fts_rank: None,
            vec_rank: None,
        });
        entry.fts_rank = Some(rank);
        entry.score += alpha * (1.0 / f64::from(rank));
    }
    for (i, (id, _)) in vec.iter().enumerate() {
        let rank = (i as u32) + 1;
        let entry = hits.entry(*id).or_insert(FusedHit {
            chunk_id: *id,
            score: 0.0,
            fts_rank: None,
            vec_rank: None,
        });
        entry.vec_rank = Some(rank);
        entry.score += (1.0 - alpha) * (1.0 / f64::from(rank));
    }
    let mut out: Vec<FusedHit> = hits.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts(ids: &[i64]) -> Vec<(ChunkId, f64)> {
        ids.iter().map(|i| (ChunkId(*i), 0.0)).collect()
    }

    fn vec(ids: &[i64]) -> Vec<(ChunkId, f64)> {
        ids.iter().map(|i| (ChunkId(*i), 0.0)).collect()
    }

    #[test]
    fn convex_fuse_alpha_one_orders_by_fts_ranking() {
        let out = convex_fuse(&fts(&[10, 20, 30]), &vec(&[30, 20, 10]), 1.0);
        let ids: Vec<i64> = out.iter().map(|h| h.chunk_id.0).collect();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn convex_fuse_alpha_zero_orders_by_vec_ranking() {
        let out = convex_fuse(&fts(&[10, 20, 30]), &vec(&[30, 20, 10]), 0.0);
        let ids: Vec<i64> = out.iter().map(|h| h.chunk_id.0).collect();
        assert_eq!(ids, vec![30, 20, 10]);
    }

    #[test]
    fn convex_fuse_returns_empty_when_both_inputs_empty() {
        let out = convex_fuse(&fts(&[]), &vec(&[]), 0.5);
        assert!(out.is_empty());
    }

    #[test]
    fn convex_fuse_emits_fts_only_chunk_with_no_vec_rank() {
        let out = convex_fuse(&fts(&[7]), &vec(&[]), 0.5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, ChunkId(7));
        assert_eq!(out[0].fts_rank, Some(1));
        assert_eq!(out[0].vec_rank, None);
        assert!((out[0].score - 0.5).abs() < 1e-12);
    }

    #[test]
    fn convex_fuse_emits_vec_only_chunk_with_no_fts_rank() {
        let out = convex_fuse(&fts(&[]), &vec(&[7]), 0.5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, ChunkId(7));
        assert_eq!(out[0].fts_rank, None);
        assert_eq!(out[0].vec_rank, Some(1));
        assert!((out[0].score - 0.5).abs() < 1e-12);
    }

    #[test]
    fn convex_fuse_sums_weighted_contributions_when_chunk_is_in_both() {
        let out = convex_fuse(&fts(&[5]), &vec(&[5]), 0.25);
        assert_eq!(out.len(), 1);
        let expected = 0.25 * 1.0 + 0.75 * 1.0;
        assert!((out[0].score - expected).abs() < 1e-12);
        assert_eq!(out[0].fts_rank, Some(1));
        assert_eq!(out[0].vec_rank, Some(1));
    }

    #[test]
    fn convex_fuse_alpha_half_promotes_chunk_in_both_above_chunk_in_one() {
        let out = convex_fuse(&fts(&[1, 2]), &vec(&[1, 3]), 0.5);
        assert_eq!(out[0].chunk_id, ChunkId(1));
        assert!(out[0].score > out[1].score);
        assert!(out[0].score > out[2].score);
    }
}
