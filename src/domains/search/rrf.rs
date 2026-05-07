use std::collections::BTreeMap;

use crate::domains::common::ChunkId;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusedHit {
    pub chunk_id: ChunkId,
    pub score: f64,
    pub fts_rank: Option<u32>,
    pub vec_rank: Option<u32>,
}

pub fn rrf_fuse<A, B>(fts: &[(ChunkId, A)], vec: &[(ChunkId, B)], k: u32) -> Vec<FusedHit> {
    let denom = f64::from(k);
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
        entry.score += 1.0 / (denom + f64::from(rank));
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
        entry.score += 1.0 / (denom + f64::from(rank));
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
    fn rrf_fuse_emits_chunk_only_in_fts_with_no_vec_rank() {
        let out = rrf_fuse(&fts(&[1]), &vec(&[]), 60);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, ChunkId(1));
        assert_eq!(out[0].fts_rank, Some(1));
        assert_eq!(out[0].vec_rank, None);
        let expected = 1.0 / 61.0;
        assert!((out[0].score - expected).abs() < 1e-12);
    }

    #[test]
    fn rrf_fuse_emits_chunk_only_in_vec_with_no_fts_rank() {
        let out = rrf_fuse(&fts(&[]), &vec(&[7]), 60);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, ChunkId(7));
        assert_eq!(out[0].fts_rank, None);
        assert_eq!(out[0].vec_rank, Some(1));
        let expected = 1.0 / 61.0;
        assert!((out[0].score - expected).abs() < 1e-12);
    }

    #[test]
    fn rrf_fuse_sums_contributions_when_chunk_appears_in_both() {
        let out = rrf_fuse(&fts(&[5]), &vec(&[5]), 60);
        assert_eq!(out.len(), 1);
        let expected = 1.0 / 61.0 + 1.0 / 61.0;
        assert!((out[0].score - expected).abs() < 1e-12);
        assert_eq!(out[0].fts_rank, Some(1));
        assert_eq!(out[0].vec_rank, Some(1));
    }

    #[test]
    fn rrf_fuse_orders_chunk_in_both_above_chunk_in_one() {
        let out = rrf_fuse(&fts(&[1, 2]), &vec(&[1, 3]), 60);
        assert_eq!(out[0].chunk_id, ChunkId(1));
        assert!(out[0].score > out[1].score);
        assert!(out[0].score > out[2].score);
    }

    #[test]
    fn rrf_fuse_smaller_k_amplifies_top_rank_advantage() {
        let large_k = rrf_fuse(&fts(&[1, 2]), &vec(&[]), 60);
        let small_k = rrf_fuse(&fts(&[1, 2]), &vec(&[]), 1);
        let large_gap = large_k[0].score - large_k[1].score;
        let small_gap = small_k[0].score - small_k[1].score;
        assert!(
            small_gap > large_gap,
            "smaller k must widen the rank-1 vs rank-2 gap: small={small_gap}, large={large_gap}"
        );
    }

    #[test]
    fn rrf_fuse_returns_empty_when_both_inputs_empty() {
        let out: Vec<FusedHit> = rrf_fuse(&fts(&[]), &vec(&[]), 60);
        assert!(out.is_empty());
    }

    #[test]
    fn rrf_fuse_orders_higher_rank_chunk_first() {
        let out = rrf_fuse(&fts(&[10, 20, 30]), &vec(&[10, 20, 30]), 60);
        assert_eq!(out[0].chunk_id, ChunkId(10));
        assert_eq!(out[1].chunk_id, ChunkId(20));
        assert_eq!(out[2].chunk_id, ChunkId(30));
        assert!(out[0].score > out[1].score);
        assert!(out[1].score > out[2].score);
    }
}
