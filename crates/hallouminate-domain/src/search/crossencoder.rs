//! Crossencoder rerank step.
//!
//! Bi-encoder retrieval (the FTS+vector+rg fusion in this module's
//! parent) gives a cheap top-K cut. A crossencoder model scores each
//! (query, chunk_text) pair jointly, which is much more accurate but
//! costs ~1.25 s for the default N=50 candidate pool on a fast desktop
//! CPU, so we only run it on the post-fusion candidates.
//!
//! Concrete rerankers live in the adapters layer; they load ONNX models
//! and cache them under the same `cache_dir` as the bi-encoder so a single
//! `hallouminate config download` warms both.

use crate::common::{HallouminateError, Result};
use crate::indexer::chunk::SearchHit;

/// Trait so `ground()` can stay generic over "no rerank" (Noop) and
/// a concrete adapter-backed rerank. Sync, takes `&mut self` because
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

/// Recognised model identifiers. Matches the adapter reranker's
/// `RerankerModel` variants but uses lower-kebab-case for config-file
/// ergonomics. Names mirror the reranker's display strings (preserving the upstream
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
