use std::any::Any;
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

    /// Removes shared content rows that no root file map row references.
    /// Returns the number of content rows removed. Stores without shared
    /// content rows keep this default.
    async fn delete_orphan_content(&self) -> Result<u64> {
        Ok(0)
    }

    async fn apply_batch(&self, files: Vec<PreparedFile>) -> Result<BatchWriteStats>;

    /// Runs the slow, lock-free part of `apply_batch` (embedding) so a caller
    /// can do it before taking its write lane. Stores with no such work keep
    /// this default.
    async fn prepare_batch(&self, _files: &[PreparedFile]) -> Result<Option<PreparedBatch>> {
        Ok(None)
    }

    /// Writes `files` using the output of `prepare_batch` for the same files.
    /// Stores that prepare nothing keep this default, which is `apply_batch`.
    async fn commit_batch(
        &self,
        files: Vec<PreparedFile>,
        _prepared: Option<PreparedBatch>,
    ) -> Result<BatchWriteStats> {
        self.apply_batch(files).await
    }
}

/// Opaque work a store did in `ChunkStore::prepare_batch`, handed back to
/// `commit_batch`. Only the store that made it can read it.
pub struct PreparedBatch(Box<dyn Any + Send>);

impl PreparedBatch {
    pub fn new<T: Any + Send>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Returns the wrapped value, or `None` when it holds a different type.
    pub fn into_inner<T: Any + Send>(self) -> Option<T> {
        let Ok(value) = self.0.downcast::<T>() else {
            return None;
        };
        Some(*value)
    }
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
