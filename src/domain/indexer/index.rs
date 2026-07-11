use crate::adapters::lance::LanceStore;
use crate::domain::common::{CorpusConfig, Result};
use crate::domain::corpus::scan;
use crate::domain::embeddings::EmbedBatch;

pub use super::apply::{ApplyStats, DEFAULT_BATCH_SIZE, apply};
pub use super::format::HandlerRegistry;
pub use super::plan::{FileSnapshot, IndexPlan, MtimeCandidate, Upsert, plan};

pub type IndexStats = ApplyStats;

/// Crust facade: scan → snapshot → plan → apply.
///
/// `embedder` is `None` in embeddings-OFF mode — indexing then writes null
/// embeddings and builds no vector index. `registry` dispatches each file to
/// its format handler.
pub async fn index_corpus(
    corpus: &CorpusConfig,
    store: &LanceStore,
    embedder: Option<&mut dyn EmbedBatch>,
    registry: &HandlerRegistry,
) -> Result<IndexStats> {
    let disk = scan(corpus)?;
    let db = store.list_files(&corpus.name).await?;
    let p = plan(disk, db);
    apply(
        p,
        store,
        embedder,
        registry,
        corpus,
        DEFAULT_BATCH_SIZE,
        None,
    )
    .await
}
