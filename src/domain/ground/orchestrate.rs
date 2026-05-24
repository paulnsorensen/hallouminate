use std::collections::BTreeMap;
use std::time::Instant;

use crate::adapters::lance::LanceStore;
use crate::domain::common::{HallouminateError, Result};
use crate::domain::embeddings::{EmbedBatch, EmbedRole};
use crate::domain::search::{Crossencoder, fts_with_ripgrep, hybrid_with_ripgrep};

use super::bucket::build_docs;
use super::types::{GroundResponse, Stats};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundOpts {
    pub top_files: usize,
    pub chunks_per_file: usize,
    pub limit: usize,
}

impl Default for GroundOpts {
    fn default() -> Self {
        Self {
            top_files: 10,
            chunks_per_file: 3,
            limit: 50,
        }
    }
}

pub async fn ground(
    query: &str,
    corpus: &str,
    corpus_paths: &[String],
    store: &LanceStore,
    embedder: Option<&mut dyn EmbedBatch>,
    crossencoder: Option<&mut dyn Crossencoder>,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();
    // ON mode (embedder present): embed the query and fuse FTS + vector + rg.
    // OFF mode (None): lexical-only FTS + rg. The cross-encoder rerank below
    // applies in BOTH paths.
    let mut hits = match embedder {
        Some(embedder) => {
            let embeddings = embedder.embed_batch(&[query.to_string()], EmbedRole::Query)?;
            let query_vec = embeddings.into_iter().next().ok_or_else(|| {
                HallouminateError::Embed("embed_batch returned no vector for query".into())
            })?;
            hybrid_with_ripgrep(store, corpus, corpus_paths, query, &query_vec, opts.limit).await?
        }
        None => fts_with_ripgrep(store, corpus, corpus_paths, query, opts.limit).await?,
    };
    if let Some(rerank) = crossencoder {
        // The crossencoder is the most expensive step; skip it on empty
        // hit lists so a no-match query doesn't pay the model latency.
        if !hits.is_empty() {
            rerank.rerank(query, &mut hits)?;
        }
    }
    let stats = Stats { hits: hits.len() };
    let mut docs = build_docs(&hits, opts.top_files, opts.chunks_per_file)?;
    for doc in docs.values_mut() {
        doc.corpus = corpus.to_string();
    }
    Ok(GroundResponse {
        query: query.to_string(),
        took_ms: started.elapsed().as_millis() as u64,
        stats,
        docs,
        code: BTreeMap::new(),
        warnings: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::embeddings::EMBEDDING_DIM;

    /// Fake embedder whose `embed_batch` always returns an empty Vec, exercising
    /// the defensive `ok_or_else` branch in `ground` that protects against an
    /// embedder impl violating the "one input → one output" invariant.
    struct EmptyVecEmbedder;

    impl EmbedBatch for EmptyVecEmbedder {
        fn embed_batch(
            &mut self,
            _texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(Vec::new())
        }
    }

    /// Records the role each `embed_batch` call received so tests can assert
    /// the query side of the asymmetric-prefix wiring is `EmbedRole::Query`.
    #[derive(Default)]
    struct RoleRecordingEmbedder {
        roles: Vec<EmbedRole>,
    }

    impl EmbedBatch for RoleRecordingEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.roles.push(role);
            Ok(texts.iter().map(|_| [0.1_f32; EMBEDDING_DIM]).collect())
        }
    }

    async fn open_test_store(dir: &std::path::Path) -> LanceStore {
        crate::adapters::lance::LanceStore::open_or_create(
            dir,
            "BAAI/bge-small-en-v1.5",
            false,
            true,
        )
        .await
        .expect("open store")
    }

    #[tokio::test]
    async fn ground_errors_when_embedder_returns_no_vector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let mut embedder = EmptyVecEmbedder;
        let err = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            Some(&mut embedder),
            None,
            GroundOpts::default(),
        )
        .await
        .expect_err("empty embed vec must error");
        match err {
            HallouminateError::Embed(msg) => {
                assert!(
                    msg.contains("no vector"),
                    "embed error must mention missing vector: {msg}"
                );
            }
            other => panic!("expected Embed error, got: {other:?}"),
        }
    }

    /// OFF mode: with no embedder, `ground` must take the lexical-only path
    /// and return a well-formed (empty, for an empty store) response instead
    /// of erroring on a missing query vector.
    #[tokio::test]
    async fn ground_off_mode_returns_lexical_response_without_an_embedder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let resp = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            None,
            None,
            GroundOpts::default(),
        )
        .await
        .expect("OFF-mode ground must succeed on an empty store");
        assert_eq!(resp.query, "spice");
        assert_eq!(resp.stats.hits, 0, "empty store yields no hits");
        assert!(resp.docs.is_empty());
    }

    /// ON mode: `ground` must embed the query with `EmbedRole::Query` so the
    /// per-model instruction prefix matches the query side. The embed call
    /// runs before search, so an empty store still exercises it.
    #[tokio::test]
    async fn ground_embeds_query_with_query_role() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let mut embedder = RoleRecordingEmbedder::default();
        ground(
            "spice",
            "fixtures",
            &[],
            &store,
            Some(&mut embedder),
            None,
            GroundOpts::default(),
        )
        .await
        .expect("ON-mode ground");
        assert_eq!(
            embedder.roles,
            vec![EmbedRole::Query],
            "ground must embed the query exactly once, with the Query role"
        );
    }
}
