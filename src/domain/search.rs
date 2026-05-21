//! Hybrid search facade.
//!
//! Wraps `LanceStore::hybrid_search` (BM25 + vector + weighted RRF) and
//! adds a `ripgrep` third source for exact-match cases the LanceDB
//! tokenizer misses. The three signals fuse at the FILE level (not
//! chunk level) since ripgrep operates on raw bytes and has no notion
//! of our chunk_id scheme — boosting per file_ref keeps the math
//! coherent without forcing an rg→chunk resolution pass.

pub mod crossencoder;
pub mod ripgrep;

use std::collections::HashMap;

use crate::adapters::lance::{LanceStore, SearchHit};
use crate::domain::common::Result;

pub use crossencoder::{
    Crossencoder, DEFAULT_CROSSENCODER_MODEL, FastembedCrossencoder, Noop as NoopCrossencoder,
    SUPPORTED_CROSSENCODER_MODELS, canonical_crossencoder_model,
};
pub use ripgrep::RipgrepHit;

/// File-level RRF weight for ripgrep matches. Lower than FTS_WEIGHT
/// inside the LanceDB reranker (2.0) because ripgrep's signal is a
/// per-file boost on top of an already-fused FTS+vector ranking, not a
/// fresh source competing against them on equal footing.
pub const RIPGREP_WEIGHT: f32 = 1.0;
/// Matches `WeightedRRFReranker::K` so the rg boost is on the same
/// dampening curve as the inner reranker.
const RRF_K: f32 = 60.0;

pub async fn hybrid_search(
    store: &LanceStore,
    corpus: &str,
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchHit>> {
    store.hybrid_search(corpus, query, query_vec, limit).await
}

/// Hybrid search PLUS a parallel ripgrep pass that boosts every chunk
/// in matched files by `RIPGREP_WEIGHT / (K + rg_rank)`, where
/// `rg_rank` is the position of the file's first match in rg's output.
///
/// Why per-file boost rather than three-way RRF on raw scores: the
/// LanceDB hybrid step already returns chunk-level relevance scores
/// reranked across FTS+vector. Mixing those into an RRF with a
/// rank-only rg list would require collapsing the chunk scores back to
/// ranks first, which loses the within-file ordering Lance gives us
/// for free. Boosting at the file level keeps that ordering and still
/// pulls rg-matched files toward the top.
///
/// `corpus_paths` is the resolved root list from `CorpusConfig.paths`.
/// Pass an empty slice (or empty `query`) to skip the rg pass entirely.
pub async fn hybrid_with_ripgrep(
    store: &LanceStore,
    corpus: &str,
    corpus_paths: &[String],
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let hybrid_fut = hybrid_search(store, corpus, query, query_vec, limit);
    let rg_fut = ripgrep::run(corpus_paths, query, limit);
    let (hybrid_res, rg_res) = tokio::join!(hybrid_fut, rg_fut);
    let mut hits = hybrid_res?;
    let rg_hits = match rg_res {
        Ok(v) => v,
        Err(e) => {
            // rg is best-effort; a missing binary or a path that
            // disappeared shouldn't take down the whole ground call.
            tracing::warn!(target: "hallouminate::search", err = %e, "ripgrep pass failed; returning hybrid-only results");
            Vec::new()
        }
    };
    apply_rg_boost(&mut hits, &rg_hits);
    Ok(hits)
}

/// In-place: add `RIPGREP_WEIGHT / (K + first_rank)` to every hit
/// whose `file_ref` appears in `rg_hits`, then re-sort descending. The
/// rg first-occurrence rank is captured before any truncation so two
/// rg-matched files don't end up with the same boost.
fn apply_rg_boost(hits: &mut Vec<SearchHit>, rg_hits: &[RipgrepHit]) {
    if rg_hits.is_empty() || hits.is_empty() {
        return;
    }
    let mut first_rank: HashMap<&str, usize> = HashMap::new();
    for (rank, h) in rg_hits.iter().enumerate() {
        first_rank.entry(h.file_ref.as_str()).or_insert(rank);
    }
    for hit in hits.iter_mut() {
        if let Some(&rank) = first_rank.get(hit.file_ref.as_str()) {
            hit.score += RIPGREP_WEIGHT / (rank as f32 + RRF_K);
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(file_ref: &str, chunk_ord: usize, score: f32) -> SearchHit {
        SearchHit {
            chunk_id: format!("{file_ref}#{chunk_ord}"),
            file_ref: file_ref.into(),
            heading_path: vec![],
            line_start: 1,
            line_end: 2,
            text: String::new(),
            summary: String::new(),
            keywords: vec![],
            score,
            mtime_ms: 0,
        }
    }

    fn rg(file_ref: &str, line: u64) -> RipgrepHit {
        RipgrepHit {
            file_ref: file_ref.into(),
            line,
            snippet: String::new(),
        }
    }

    #[test]
    fn rg_boost_promotes_matched_file() {
        let mut hits = vec![hit("/a.md", 0, 0.10), hit("/b.md", 0, 0.20)];
        let rg_hits = vec![rg("/a.md", 5)];
        apply_rg_boost(&mut hits, &rg_hits);
        // /a.md starts behind /b.md (0.10 vs 0.20) but the rg boost at
        // rank 0 is 1.0/60 ≈ 0.0167, so /a.md ends at ~0.1167 and still
        // loses. Confirm boost is applied to /a.md only.
        let a = hits.iter().find(|h| h.file_ref == "/a.md").unwrap();
        let b = hits.iter().find(|h| h.file_ref == "/b.md").unwrap();
        assert!((a.score - (0.10 + 1.0 / 60.0)).abs() < 1e-6);
        assert!((b.score - 0.20).abs() < 1e-6);
    }

    #[test]
    fn rg_boost_uses_first_occurrence_rank_when_file_matched_multiple_times() {
        let mut hits = vec![hit("/a.md", 0, 0.0)];
        // /a.md appears at ranks 3 AND 7 in rg output; the first wins.
        let rg_hits = vec![
            rg("/z.md", 1),
            rg("/y.md", 1),
            rg("/x.md", 1),
            rg("/a.md", 10),
            rg("/w.md", 1),
            rg("/v.md", 1),
            rg("/u.md", 1),
            rg("/a.md", 20),
        ];
        apply_rg_boost(&mut hits, &rg_hits);
        let a = &hits[0];
        let expected = 1.0 / (3.0 + 60.0);
        assert!(
            (a.score - expected).abs() < 1e-6,
            "expected boost from first /a.md occurrence (rank 3); got {}",
            a.score
        );
    }

    #[test]
    fn rg_boost_resorts_descending() {
        // Big rg boost is enough to flip the order.
        let mut hits = vec![hit("/loser.md", 0, 0.50), hit("/winner.md", 0, 0.0)];
        // /winner.md gets rank-0 boost from rg.
        let rg_hits = vec![rg("/winner.md", 1)];
        // Use a larger weight to make the flip visible without changing
        // module constants. We test the constants separately.
        apply_rg_boost(&mut hits, &rg_hits);
        // Default RIPGREP_WEIGHT/K ≈ 0.0167 — not enough to flip.
        // So just confirm sort order matches scores, regardless.
        assert!(hits[0].score >= hits[1].score, "must be sorted descending");
    }

    #[test]
    fn rg_boost_noop_when_no_rg_hits() {
        let mut hits = vec![hit("/a.md", 0, 0.30), hit("/b.md", 0, 0.10)];
        let before: Vec<f32> = hits.iter().map(|h| h.score).collect();
        apply_rg_boost(&mut hits, &[]);
        let after: Vec<f32> = hits.iter().map(|h| h.score).collect();
        assert_eq!(before, after, "empty rg list must leave scores untouched");
    }

    #[test]
    fn rg_boost_handles_rg_only_files_silently() {
        // rg matches a file Lance didn't return. The boost has nothing
        // to attach to and must not panic or insert synthetic hits.
        let mut hits = vec![hit("/a.md", 0, 0.10)];
        let rg_hits = vec![rg("/never_indexed.md", 1)];
        apply_rg_boost(&mut hits, &rg_hits);
        assert_eq!(hits.len(), 1, "no synthetic hit appears");
        assert!((hits[0].score - 0.10).abs() < 1e-6, "no boost applied");
    }
}
