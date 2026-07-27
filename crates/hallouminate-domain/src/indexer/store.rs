use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::common::{CorpusKey, Result, RetiredRoot};
use crate::indexer::chunk::{PreparedFile, SearchHit};
use crate::indexer::plan::FileSnapshot;

/// Storage-agnostic port for chunk persistence: index-time writes
/// (`list_files`, `touch_mtime`, `delete_file`, `apply_batch`) plus
/// orphaned-root garbage collection (`distinct_roots`, `delete_root`), a
/// maintenance-time concern unrelated to indexing. `delete_root` takes a
/// `RetiredRoot`, constructible only by `retired_roots`, so a caller cannot
/// invoke it without first proving the root is absent from the filesystem.
///
/// Text in: embedding is an adapter-internal implementation detail of
/// `apply_batch`. Domain callers never see a vector.
#[async_trait]
pub trait ChunkStore: Send + Sync {
    async fn list_files(&self, corpus_key: &CorpusKey) -> Result<Vec<FileSnapshot>>;

    async fn touch_mtime(
        &self,
        corpus_key: &CorpusKey,
        file_ref: &str,
        mtime_ms: i64,
    ) -> Result<()>;

    async fn delete_file(&self, corpus_key: &CorpusKey, file_ref: &str) -> Result<()>;

    async fn distinct_roots(&self) -> Result<Vec<PathBuf>>;

    async fn delete_root(&self, root: &RetiredRoot) -> Result<u64>;

    async fn apply_batch(&self, files: Vec<PreparedFile>) -> Result<BatchWriteStats>;
}

/// Per-signal ranked lists from the retrieval backend, unfused.
///
/// LanceDB is a retrieval backend here, not a ranker: the `lancedb` 0.31
/// Rust `Reranker` trait takes exactly two lists, so N-signal fusion
/// cannot live in the adapter. Ranking belongs to `domain::search`.
#[derive(Debug, Default)]
pub struct SignalLists {
    /// chunk_ids in full-text rank order, best first.
    pub fts: Vec<String>,
    /// chunk_ids in vector rank order, best first. Empty when embeddings
    /// are disabled.
    pub vector: Vec<String>,
    /// Every retrieved chunk keyed by chunk_id: the union of `fts` and
    /// `vector`. Scores here are per-signal and NOT comparable across
    /// signals; the caller overwrites them with the fused score.
    pub hits: HashMap<String, SearchHit>,
}

/// Counts written by one `ChunkStore::apply_batch` call, so
/// `ApplyStats.embeddings_inserted` stays accurate now that embedding
/// happens inside the adapter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchWriteStats {
    pub chunks_written: usize,
    pub embeddings_written: usize,
}
