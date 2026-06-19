//! Crossencoder rerank step.
//!
//! Bi-encoder retrieval (the FTS+vector+rg fusion in this module's
//! parent) gives a cheap top-K cut. A crossencoder model scores each
//! (query, chunk_text) pair jointly, which is much more accurate but
//! ~100ms per batch, so we only run it on the post-fusion candidates.
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

/// Default crossencoder model. JinaRerankerV1TurboEn is ~33 MB and
/// runs in tens of ms on commodity CPUs — the right zone for an
/// optional rerank that has to stay under the daemon's perceived
/// snappiness budget for `ground`.
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
            .with_show_download_progress(false);
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

impl Crossencoder for FastembedCrossencoder {
    fn rerank(&mut self, query: &str, hits: &mut [SearchHit]) -> Result<()> {
        if hits.is_empty() {
            return Ok(());
        }
        let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        let scored = self
            .inner
            .rerank(query, &docs, false, None)
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
}
