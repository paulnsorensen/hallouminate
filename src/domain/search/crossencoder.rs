//! Crossencoder rerank step.
//!
//! Bi-encoder retrieval (the FTS+vector+rg fusion in this module's
//! parent) gives a cheap top-K cut. A crossencoder model scores each
//! (query, chunk_text) pair jointly, which is much more accurate but
//! costs ~1.25 s for the default N=50 candidate pool on a fast desktop
//! CPU, so we only run it on the post-fusion candidates.
//!
//! Models are loaded via `fastembed::TextRerank` (ONNX Runtime) and
//! cached under the same `cache_dir` as the bi-encoder so a single
//! `hallouminate config download` warms both.

use std::path::{Path, PathBuf};

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};

use crate::adapters::lance::SearchHit;
use crate::domain::common::{HallouminateError, Result};

/// Trait so `ground()` can stay generic over "no rerank" (Noop) and
/// "fastembed-backed rerank". Sync, takes `&mut self` because
/// `TextRerank::rerank` mutates the underlying ONNX session.
pub trait Crossencoder: Send {
    /// Reorder `hits` in place by descending crossencoder score for the
    /// given `query`. Implementations MUST preserve the input slice's
    /// contents (no inserts, no deletions) — they only reshuffle.
    fn rerank(&mut self, query: &str, hits: &mut [SearchHit]) -> Result<()>;
}

/// Default crossencoder model. JinaRerankerV1TurboEn is ~147 MB on disk
/// (145 MB full-precision ONNX, no quantized variant) and reranks the
/// default N=50 pool in ~1.25 s on a fast desktop CPU (~463 ms at N=20).
/// That latency is precisely why it is opt-in / off by default
/// (`crossencoder: None`): callers that need re-ranking accuracy accept
/// the cost explicitly.
pub const DEFAULT_CROSSENCODER_MODEL: &str = "jina-reranker-v1-turbo-en";

/// Recognised model identifiers. Matches `fastembed::RerankerModel`
/// variants but uses lower-kebab-case for config-file ergonomics.
/// Names mirror fastembed's display strings (preserving the upstream
/// `multiligual` typo in `jina-reranker-v2-base-multiligual` so the
/// canonical-name table doesn't disagree with `RerankerModel::Display`).
pub const SUPPORTED_CROSSENCODER_MODELS: &[&str] = &[
    "jina-reranker-v1-turbo-en",
    "jina-reranker-v2-base-multiligual",
    "bge-reranker-base",
    "bge-reranker-v2-m3",
];

pub fn canonical_crossencoder_model(name: &str) -> Result<&'static str> {
    match name {
        "jina-reranker-v1-turbo-en" => Ok("jina-reranker-v1-turbo-en"),
        // Accept the corrected English spelling as an alias for the
        // typo'd upstream identifier so users don't have to memorise it.
        "jina-reranker-v2-base-multiligual" | "jina-reranker-v2-base-multilingual" => {
            Ok("jina-reranker-v2-base-multiligual")
        }
        "bge-reranker-base" => Ok("bge-reranker-base"),
        "bge-reranker-v2-m3" => Ok("bge-reranker-v2-m3"),
        other => Err(HallouminateError::Config(format!(
            "unsupported crossencoder model {other:?}; choose one of {SUPPORTED_CROSSENCODER_MODELS:?}"
        ))),
    }
}

fn resolve_model(canonical: &'static str) -> RerankerModel {
    match canonical {
        "jina-reranker-v1-turbo-en" => RerankerModel::JINARerankerV1TurboEn,
        "jina-reranker-v2-base-multiligual" => RerankerModel::JINARerankerV2BaseMultiligual,
        "bge-reranker-base" => RerankerModel::BGERerankerBase,
        "bge-reranker-v2-m3" => RerankerModel::BGERerankerV2M3,
        _ => unreachable!("resolve_model takes a canonical name from canonical_crossencoder_model"),
    }
}

pub struct FastembedCrossencoder {
    inner: TextRerank,
    model_name: String,
}

impl FastembedCrossencoder {
    pub fn try_new(model_name: &str, cache_dir: &Path) -> Result<Self> {
        let canonical = canonical_crossencoder_model(model_name)?;
        let model = resolve_model(canonical);
        let opts = RerankInitOptions::new(model)
            .with_cache_dir(PathBuf::from(cache_dir))
            .with_show_download_progress(true);
        let inner = TextRerank::try_new(opts).map_err(|e| {
            HallouminateError::Embed(format!(
                "init crossencoder {canonical}: {e}\n  \
                 hint: first run needs network to fetch the model into {}; \
                 run `hallouminate config download` to pre-warm the cache",
                cache_dir.display()
            ))
        })?;
        Ok(Self {
            inner,
            model_name: canonical.to_string(),
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

/// Bounded fastembed batch size for the ONNX rerank pass. Passing `None`
/// selects fastembed's internal default (256 sequences), which sets the ORT
/// CPU arena's high-water mark in the multi-GB range and is never reclaimed
/// afterwards — the same retention mode documented for the embedder in
/// `.hallouminate/wiki/ort-arena-retention.md`. `Some(32)` mirrors the
/// embedder's `EMBED_BATCH_SIZE` mitigation.
const RERANK_BATCH_SIZE: usize = 32;

impl Crossencoder for FastembedCrossencoder {
    fn rerank(&mut self, query: &str, hits: &mut [SearchHit]) -> Result<()> {
        if hits.is_empty() {
            return Ok(());
        }
        let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        let scored = self
            .inner
            .rerank(query, &docs, false, Some(RERANK_BATCH_SIZE))
            .map_err(|e| HallouminateError::Embed(format!("crossencoder rerank: {e}")))?;
        // fastembed returns RerankResult sorted by score descending,
        // each carrying the original `index` into the docs slice. Apply
        // that permutation in place via a destructive take/swap dance.
        let order: Vec<usize> = scored.iter().map(|r| r.index).collect();
        // `order` must be a true permutation of `0..hits.len()`.
        // `apply_permutation` indexes `hits[i]` directly, so an
        // out-of-range index would panic and a duplicate would drop one
        // hit while cloning another — both violating the trait's
        // "preserve contents" contract. The length check alone permits
        // both, so validate length, range, and uniqueness explicitly.
        if order.len() != hits.len() {
            return Err(HallouminateError::Embed(format!(
                "crossencoder returned {} scores for {} docs",
                order.len(),
                hits.len()
            )));
        }
        let mut seen = vec![false; hits.len()];
        for &idx in &order {
            let slot = seen.get_mut(idx).ok_or_else(|| {
                HallouminateError::Embed(format!(
                    "crossencoder returned out-of-range index {idx} for {} docs",
                    hits.len()
                ))
            })?;
            if std::mem::replace(slot, true) {
                return Err(HallouminateError::Embed(format!(
                    "crossencoder returned duplicate index {idx}"
                )));
            }
        }
        // Overwrite per-hit score with the crossencoder score so
        // downstream `build_docs` aggregation uses the new ranking.
        for r in &scored {
            if let Some(h) = hits.get_mut(r.index) {
                h.score = r.score;
            }
        }
        apply_permutation(hits, &order);
        Ok(())
    }
}

/// Reorder `hits` so that `hits[i] = hits[order[i]]`. Allocates a fresh
/// `Vec` because in-place permutation with the borrow-checker is
/// painful and the slices here are small (≤ `opts.limit`, typically 50).
fn apply_permutation(hits: &mut [SearchHit], order: &[usize]) {
    let reordered: Vec<SearchHit> = order.iter().map(|&i| hits[i].clone()).collect();
    for (slot, hit) in hits.iter_mut().zip(reordered) {
        *slot = hit;
    }
}

/// Passthrough impl for callers that have no crossencoder configured.
/// Lives here so `ground()` can take a single `&mut dyn Crossencoder`
/// instead of branching on `Option<...>` at every call site.
pub struct Noop;

impl Crossencoder for Noop {
    fn rerank(&mut self, _query: &str, _hits: &mut [SearchHit]) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(file_ref: &str, ord: usize, score: f32, text: &str) -> SearchHit {
        SearchHit {
            chunk_id: format!("{file_ref}#{ord}"),
            file_ref: file_ref.into(),
            heading_path: vec![],
            line_start: 1,
            line_end: 2,
            text: text.into(),
            summary: String::new(),
            keywords: vec![],
            score,
            mtime_ms: 0,
            claim_marks: vec![],
            z_score: None,
        }
    }

    #[test]
    fn noop_leaves_hits_untouched() {
        let mut hits = vec![hit("/a.md", 0, 0.1, "alpha"), hit("/b.md", 0, 0.9, "beta")];
        let before: Vec<String> = hits.iter().map(|h| h.chunk_id.clone()).collect();
        Noop.rerank("q", &mut hits).expect("noop never errors");
        let after: Vec<String> = hits.iter().map(|h| h.chunk_id.clone()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn apply_permutation_reorders_in_place() {
        let mut hits = vec![
            hit("/a.md", 0, 0.1, "a"),
            hit("/b.md", 0, 0.2, "b"),
            hit("/c.md", 0, 0.3, "c"),
        ];
        // Permute to reverse order.
        apply_permutation(&mut hits, &[2, 1, 0]);
        let ids: Vec<&str> = hits.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ids, vec!["/c.md#0", "/b.md#0", "/a.md#0"]);
    }

    #[test]
    fn canonical_model_rejects_unknown_names_with_hint() {
        let err = canonical_crossencoder_model("not-a-real-model").expect_err("must reject");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("unsupported crossencoder model"), "got: {msg}");
                assert!(msg.contains("jina-reranker-v1-turbo-en"), "got: {msg}");
            }
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[test]
    fn supported_models_round_trip_through_canonical() {
        for name in SUPPORTED_CROSSENCODER_MODELS {
            assert_eq!(
                canonical_crossencoder_model(name).expect("canonical"),
                *name
            );
        }
    }
    #[test]
    #[ignore = "downloads the ~147MB jina-reranker-v1-turbo-en model on first run; opt-in via --ignored"]
    fn fastembed_crossencoder_reranks_and_overwrites_scores() {
        let cache = tempfile::tempdir().expect("tempdir");
        let mut ce = FastembedCrossencoder::try_new(DEFAULT_CROSSENCODER_MODEL, cache.path())
            .expect("load reranker model");

        // Input order favours the IRRELEVANT doc (high fusion score) over the
        // truly relevant one; the cross-encoder must disagree and flip them.
        let query = "how do you grill halloumi cheese";
        let mut hits = vec![
            hit(
                "/paris.md",
                0,
                0.99,
                "The capital of France is Paris, a city on the river Seine.",
            ),
            hit(
                "/halloumi.md",
                0,
                0.10,
                "Halloumi is a brined cheese with a high melting point, so it grills and fries without falling apart.",
            ),
        ];

        let before: std::collections::HashMap<String, f32> =
            hits.iter().map(|h| (h.chunk_id.clone(), h.score)).collect();
        ce.rerank(query, &mut hits).expect("rerank");

        // (a) order flipped: the relevant doc is now first
        assert_eq!(
            hits[0].file_ref, "/halloumi.md",
            "cross-encoder must rank the relevant doc first"
        );
        assert!(
            hits[0].score >= hits[1].score,
            "hits must be sorted by descending rerank score"
        );
        // (b) every hit's score is replaced by the cross-encoder score —
        // assert a per-chunk_id delta from the captured pre-rerank value
        // (tests the overwrite contract, not the seed constants).
        for h in &hits {
            let prev = before[&h.chunk_id];
            assert!(
                (h.score - prev).abs() > 1e-4,
                "hit {} score must be overwritten by the cross-encoder (was {prev}, now {})",
                h.chunk_id,
                h.score
            );
        }
    }
}
