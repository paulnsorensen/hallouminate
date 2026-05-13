use crate::adapters::lance::{LanceStore, PreparedFile, EMBEDDING_DIM};
use crate::domain::common::{CorpusConfig, HallouminateError, Result};
use crate::domain::corpus::{blake3_file, CorpusChunker};
use crate::domain::embeddings::EmbedBatch;

use super::plan::{IndexPlan, MtimeCandidate};
use super::writer::{prepare_file, WriteRequest};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub files_upserted: usize,
    pub files_touched: usize,
    pub files_deleted: usize,
    /// Files that produced zero chunks (typically empty markdown). They are
    /// not represented in the chunks table and so cannot be made
    /// idempotent; the caller may want to filter these from the corpus.
    pub files_skipped_empty: usize,
    pub chunks_inserted: usize,
    pub embeddings_inserted: usize,
}

pub const DEFAULT_BATCH_SIZE: usize = 16;

pub async fn apply(
    plan: IndexPlan,
    store: &LanceStore,
    embedder: &mut dyn EmbedBatch,
    chunker: &dyn CorpusChunker,
    corpus: &CorpusConfig,
    batch_size: usize,
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let batch_size = batch_size.max(1);
    let indexed_at_ms = chrono::Utc::now().timestamp_millis();

    // Upserts: build write requests and run them in batches.
    let upsert_reqs: Vec<WriteRequest<'_>> = plan
        .upserts
        .iter()
        .map(|u| WriteRequest {
            corpus,
            file: &u.file,
            mtime: u.mtime,
        })
        .collect();
    run_in_batches(upsert_reqs, batch_size, store, embedder, chunker, indexed_at_ms, &mut stats).await?;

    // Mtime touches: hash-check each. If hash unchanged, just bump mtime.
    // Otherwise re-index (deferred into the upsert path).
    let mut fallthrough: Vec<MtimeCandidate> = Vec::new();
    for cand in plan.mtime_touches {
        let new_hash = blake3_file(cand.file.as_path())?;
        if new_hash == cand.snap.content_hash {
            store
                .touch_mtime(&cand.snap.corpus, &cand.snap.file_ref, cand.new_mtime.0)
                .await?;
            stats.files_touched += 1;
        } else {
            fallthrough.push(cand);
        }
    }
    let fallthrough_reqs: Vec<WriteRequest<'_>> = fallthrough
        .iter()
        .map(|c| WriteRequest {
            corpus,
            file: &c.file,
            mtime: c.new_mtime,
        })
        .collect();
    run_in_batches(
        fallthrough_reqs,
        batch_size,
        store,
        embedder,
        chunker,
        indexed_at_ms,
        &mut stats,
    )
    .await?;

    // Deletes: one delete-by-(corpus, file_ref) per gone file.
    for snap in plan.deletes {
        store.delete_file(&snap.corpus, &snap.file_ref).await?;
        stats.files_deleted += 1;
    }

    tracing::debug!(
        target: "hallouminate::indexer",
        embeddings_inserted_total = stats.embeddings_inserted,
        "apply finished"
    );
    Ok(stats)
}

async fn run_in_batches(
    reqs: Vec<WriteRequest<'_>>,
    batch_size: usize,
    store: &LanceStore,
    embedder: &mut dyn EmbedBatch,
    chunker: &dyn CorpusChunker,
    indexed_at_ms: i64,
    stats: &mut ApplyStats,
) -> Result<()> {
    if reqs.is_empty() {
        return Ok(());
    }
    for chunk_of_reqs in reqs.chunks(batch_size) {
        let mut prepared: Vec<PreparedFile> = Vec::with_capacity(chunk_of_reqs.len());
        for req in chunk_of_reqs {
            // prepare_file failures (IO, non-UTF8) are real errors — fail
            // fast rather than silently dropping files from the index.
            let pf = prepare_file(
                WriteRequest {
                    corpus: req.corpus,
                    file: req.file,
                    mtime: req.mtime,
                },
                chunker,
                indexed_at_ms,
            )?;
            if pf.chunks.is_empty() {
                // Empty file → no rows would land in the chunks table, which
                // makes list_files unable to track this file and the next
                // run would re-attempt the same upsert. Skip and account.
                tracing::warn!(
                    target: "hallouminate::indexer",
                    file = %req.file.as_path().display(),
                    "skipping empty file (no chunks generated)"
                );
                stats.files_skipped_empty += 1;
                continue;
            }
            prepared.push(pf);
        }
        if prepared.is_empty() {
            continue;
        }
        // Embed all chunks across all prepared files in this batch in a single call.
        let mut all_texts: Vec<String> = Vec::new();
        let mut splits: Vec<usize> = Vec::with_capacity(prepared.len());
        for pf in &prepared {
            splits.push(pf.chunks.len());
            for c in &pf.chunks {
                all_texts.push(c.text.clone());
            }
        }
        let mut vectors = if all_texts.is_empty() {
            Vec::new()
        } else {
            embedder.embed_batch(&all_texts)?
        };
        if vectors.len() != all_texts.len() {
            return Err(HallouminateError::Indexer(format!(
                "embedder returned {} vectors for {} chunks",
                vectors.len(),
                all_texts.len()
            )));
        }
        // De-flatten vectors back into per-file embeddings.
        let mut iter = vectors.drain(..);
        for (pf, count) in prepared.iter_mut().zip(splits.iter().copied()) {
            let mut buf: Vec<[f32; EMBEDDING_DIM]> = Vec::with_capacity(count);
            for _ in 0..count {
                let v = iter.next().ok_or_else(|| {
                    HallouminateError::Indexer("embedding count drained early".into())
                })?;
                buf.push(v);
            }
            stats.chunks_inserted += count;
            stats.embeddings_inserted += count;
            pf.embeddings = buf;
        }
        let n = prepared.len();
        store.apply_batch(prepared).await?;
        stats.files_upserted += n;
    }
    Ok(())
}
