use std::collections::HashMap;
use std::path::PathBuf;

use crate::common::{CorpusConfig, CorpusKey, FileRef, Result};
use crate::corpus::{ScannedFile, scan};
use crate::indexer::store::ChunkStore;

pub use super::apply::{ApplyStats, DEFAULT_BATCH_SIZE, apply};
pub use super::format::HandlerRegistry;
pub use super::plan::{FileSnapshot, IndexPlan, IntoPlanInput, MtimeCandidate, Upsert, plan};

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
    let mut disk_by_key: HashMap<CorpusKey, Vec<ScannedFile>> = HashMap::new();
    for scanned in scan(corpus)? {
        disk_by_key
            .entry(scanned.corpus_key.clone())
            .or_default()
            .push(scanned);
    }

    let mut combined = IndexPlan::default();
    for corpus_key in corpus.corpus_keys() {
        let disk = disk_by_key.remove(&corpus_key).unwrap_or_default();
        let mut db: HashMap<FileRef, FileSnapshot> = HashMap::new();
        for snapshot in store.list_files(&corpus_key).await? {
            let file = FileRef::new(PathBuf::from(&snapshot.file_ref));
            db.insert(file, snapshot);
        }
        let mut root_plan = plan(disk, db);
        combined.upserts.append(&mut root_plan.upserts);
        combined.mtime_touches.append(&mut root_plan.mtime_touches);
        combined.deletes.append(&mut root_plan.deletes);
    }
    apply(combined, store, registry, corpus, DEFAULT_BATCH_SIZE, None).await
}
