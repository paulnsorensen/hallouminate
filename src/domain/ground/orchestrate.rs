use std::collections::BTreeMap;
use std::time::Instant;

use crate::adapters::lance::LanceStore;
use crate::domain::common::{HallouminateError, Result};
use crate::domain::embeddings::EmbedBatch;
use crate::domain::search::hybrid_search;

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
    store: &LanceStore,
    embedder: &mut dyn EmbedBatch,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();
    let embeddings = embedder.embed_batch(&[query.to_string()])?;
    let query_vec = embeddings.into_iter().next().ok_or_else(|| {
        HallouminateError::Embed("embed_batch returned no vector for query".into())
    })?;
    let hits = hybrid_search(store, corpus, query, &query_vec, opts.limit).await?;
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
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn ground_errors_when_embedder_returns_no_vector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            crate::adapters::lance::LanceStore::open_or_create(dir.path(), "bge-small-en-v1.5")
                .await
                .expect("open store");
        let mut embedder = EmptyVecEmbedder;
        let err = ground(
            "spice",
            "fixtures",
            &store,
            &mut embedder,
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
}
