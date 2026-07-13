use crate::common::{CorpusConfig, Result};
use crate::corpus::scan;
use crate::indexer::store::ChunkStore;

pub use super::apply::{ApplyStats, DEFAULT_BATCH_SIZE, apply};
pub use super::format::HandlerRegistry;
pub use super::plan::{FileSnapshot, IndexPlan, MtimeCandidate, Upsert, plan};

pub type IndexStats = ApplyStats;

/// Crust facade: scan → snapshot → plan → apply.
///
/// The store embeds passages internally when it owns an embedder
/// (embeddings-OFF mode indexes with null embeddings and builds no vector
/// index). `registry` dispatches each file to its format handler.
pub async fn index_corpus(
    corpus: &CorpusConfig,
    store: &dyn ChunkStore,
    registry: &HandlerRegistry,
) -> Result<IndexStats> {
    let disk = scan(corpus)?;
    let db: std::collections::HashMap<crate::common::FileRef, FileSnapshot> = store
        .list_files(&corpus.name)
        .await?
        .into_iter()
        .map(|s| {
            (
                crate::common::FileRef::new(std::path::PathBuf::from(&s.file_ref)),
                s,
            )
        })
        .collect();
    let p = plan(disk, db);
    apply(p, store, registry, corpus, DEFAULT_BATCH_SIZE, None).await
}
