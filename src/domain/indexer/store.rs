use async_trait::async_trait;

use crate::domain::common::Result;
use crate::domain::indexer::chunk::{PreparedFile, SearchHit};
use crate::domain::indexer::plan::FileSnapshot;

/// Storage-agnostic port for chunk retrieval and persistence.
///
/// Text in, hits out: embedding is an adapter-internal implementation detail
/// of `hybrid_search` and `apply_batch`. Domain callers never see a vector.
#[async_trait]
pub trait ChunkStore: Send + Sync {
    async fn list_files(&self, corpus: &str) -> Result<Vec<FileSnapshot>>;

    async fn hybrid_search(
        &self,
        corpus: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>>;

    async fn touch_mtime(&self, corpus: &str, file_ref: &str, mtime_ms: i64) -> Result<()>;

    async fn delete_file(&self, corpus: &str, file_ref: &str) -> Result<()>;

    async fn apply_batch(&self, files: Vec<PreparedFile>) -> Result<BatchWriteStats>;
}

/// Counts written by one `ChunkStore::apply_batch` call, so
/// `ApplyStats.embeddings_inserted` stays accurate now that embedding
/// happens inside the adapter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchWriteStats {
    pub chunks_written: usize,
    pub embeddings_written: usize,
}
