use crate::adapters::lance::LanceStore;
use crate::domain::common::{CorpusConfig, Result};
use crate::domain::corpus::{CorpusChunker, scan};
use crate::domain::embeddings::EmbedBatch;

pub use super::apply::{ApplyStats, DEFAULT_BATCH_SIZE, apply};
pub use super::plan::{FileSnapshot, IndexPlan, MtimeCandidate, Upsert, plan};

pub type IndexStats = ApplyStats;

/// Crust facade: scan → snapshot → plan → apply.
pub async fn index_corpus(
    corpus: &CorpusConfig,
    store: &LanceStore,
    embedder: &mut dyn EmbedBatch,
    chunker: &dyn CorpusChunker,
) -> Result<IndexStats> {
    let disk = scan(corpus)?;
    let db = store.list_files(&corpus.name).await?;
    let p = plan(disk, db);
    apply(p, store, embedder, chunker, corpus, DEFAULT_BATCH_SIZE).await
}
