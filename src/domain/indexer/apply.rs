use crate::adapters::lance::{EMBEDDING_DIM, LanceStore, PreparedFile};
use crate::domain::common::{CorpusConfig, HallouminateError, Result};
use crate::domain::corpus::blake3_file;
use crate::domain::embeddings::{EmbedBatch, EmbedRole};

use super::format::HandlerRegistry;
use super::plan::{IndexPlan, MtimeCandidate};
use super::writer::{WriteRequest, file_ref_string, prepare_file};

/// Tallies of the work an [`apply`] run performed, returned to the caller for
/// reporting and assertions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    /// Files written through the upsert/fallthrough path (content (re)indexed).
    pub files_upserted: usize,
    /// Files whose content was unchanged; only the stored mtime was bumped.
    pub files_touched: usize,
    /// Files removed from the index because they vanished from disk.
    pub files_deleted: usize,
    /// Files that produced zero chunks (typically truncate-to-empty markdown).
    /// They are not represented in the chunks table and so cannot be made
    /// idempotent; the caller may want to filter these from the corpus. When
    /// the file previously had rows (the mtime-fallthrough batch), those rows
    /// are evicted and counted under `files_deleted` too — see
    /// [`EmptyFilePolicy::Evict`].
    pub files_skipped_empty: usize,
    /// Files gracefully skipped because their type is unsupported or extraction
    /// failed (corrupt workbook, non-UTF-8 text, …). Distinct from
    /// `files_skipped_empty`: a present-but-unreadable file must NEVER evict its
    /// last-good rows, on either the bulk or single-file path — a transient
    /// parse failure (atomic-save race, partial write, momentary corruption)
    /// must not silently drop a file from search.
    pub files_skipped_unreadable: usize,
    /// Total chunks written across all upserted files (both embedding modes).
    pub chunks_inserted: usize,
    /// Total embedding vectors written; zero when the embedder is `None`.
    pub embeddings_inserted: usize,
}

/// Whether `run_in_batches` should evict a truncated-to-empty file's stale
/// rows. `plan.upserts` covers files with no snapshot in the store (no rows
/// can exist yet, so `Retain` is a no-op); the mtime-fallthrough batch covers
/// files that HAD a snapshot (rows may exist), so it passes `Evict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyFilePolicy {
    Retain,
    Evict,
}

/// Invariants shared by every batch of one `apply` run: the store and
/// registry to write through, the run-wide `indexed_at_ms` stamp, and the
/// batch width. Groups what would otherwise be four extra parameters on
/// `run_in_batches` (mirroring `PrepareCtx` in format.rs).
struct RunCtx<'a> {
    store: &'a LanceStore,
    registry: &'a HandlerRegistry,
    indexed_at_ms: i64,
    batch_size: usize,
}

/// Default number of files prepared and embedded per batch when the caller
/// does not specify one. Bounds peak memory and embedder call width.
pub const DEFAULT_BATCH_SIZE: usize = 16;

pub async fn apply(
    plan: IndexPlan,
    store: &LanceStore,
    mut embedder: Option<&mut dyn EmbedBatch>,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
    batch_size: usize,
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let batch_size = batch_size.max(1);
    // Captured once so every file written by this run shares a single
    // `indexed_at_ms`, regardless of how many batches it spans.
    let indexed_at_ms = chrono::Utc::now().timestamp_millis();
    let run = RunCtx {
        store,
        registry,
        indexed_at_ms,
        batch_size,
    };

    // Upserts: build write requests and run them in batches.
    let mut upsert_reqs: Vec<WriteRequest<'_>> = Vec::with_capacity(plan.upserts.len());
    for u in &plan.upserts {
        upsert_reqs.push(WriteRequest {
            corpus,
            file: &u.file,
            mtime: u.mtime,
        });
    }
    run_in_batches(
        upsert_reqs,
        &run,
        embedder.as_deref_mut(),
        &mut stats,
        EmptyFilePolicy::Retain,
    )
    .await?;

    // Mtime touches: hash-check each. If hash unchanged, just bump mtime.
    // Otherwise re-index (deferred into the upsert path). Skip the re-hash
    // when the caller already computed it (`known_hash` — see the
    // single-file reroute in `dispatch.rs`).
    let mut fallthrough: Vec<MtimeCandidate> = Vec::new();
    for cand in plan.mtime_touches {
        let new_hash = match &cand.known_hash {
            Some(h) => h.clone(),
            None => blake3_file(cand.file.as_path())?,
        };
        if new_hash == cand.snap.content_hash {
            store
                .touch_mtime(&cand.snap.corpus, &cand.snap.file_ref, cand.new_mtime.0)
                .await?;
            stats.files_touched += 1;
        } else {
            fallthrough.push(cand);
        }
    }
    let mut fallthrough_reqs: Vec<WriteRequest<'_>> = Vec::with_capacity(fallthrough.len());
    for c in &fallthrough {
        fallthrough_reqs.push(WriteRequest {
            corpus,
            file: &c.file,
            mtime: c.new_mtime,
        });
    }
    run_in_batches(
        fallthrough_reqs,
        &run,
        embedder,
        &mut stats,
        EmptyFilePolicy::Evict,
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
    run: &RunCtx<'_>,
    // `+ '_` decouples the trait-object lifetime from the reference lifetime
    // so `apply` can hand out two successive short reborrows via
    // `as_deref_mut()` without the first borrow being pinned for the whole
    // function body.
    mut embedder: Option<&mut (dyn EmbedBatch + '_)>,
    stats: &mut ApplyStats,
    empty_file_policy: EmptyFilePolicy,
) -> Result<()> {
    if reqs.is_empty() {
        return Ok(());
    }
    for chunk_of_reqs in reqs.chunks(run.batch_size) {
        let mut prepared: Vec<PreparedFile> = Vec::with_capacity(chunk_of_reqs.len());
        for req in chunk_of_reqs {
            // A real IO failure (file read) is a hard error — fail fast rather
            // than silently dropping a file. An unsupported type or a handler
            // extraction failure returns `Ok(None)`: prepare_file already logged
            // the skip, so just account it and move on.
            let pf = prepare_file(
                WriteRequest {
                    corpus: req.corpus,
                    file: req.file,
                    mtime: req.mtime,
                },
                run.registry,
                run.indexed_at_ms,
            )?;
            let Some(pf) = pf else {
                // Unsupported type or extraction failure — already logged by
                // prepare_file. Counted distinctly from truncate-to-empty so a
                // present-but-unreadable file does not evict its last-good rows
                // on the single-file path.
                stats.files_skipped_unreadable += 1;
                continue;
            };
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
                if empty_file_policy == EmptyFilePolicy::Evict {
                    // This batch's files all had a store snapshot (see the
                    // `Evict` doc comment), so stale rows may exist — evict
                    // them so the filesystem stays the source of truth.
                    let file_ref_str = file_ref_string(req.file)?;
                    tracing::info!(
                        target: "hallouminate::indexer",
                        corpus = %req.corpus.name,
                        file = %file_ref_str,
                        "evicting indexed file from search: re-index produced an empty file",
                    );
                    run.store
                        .delete_file(&req.corpus.name, &file_ref_str)
                        .await?;
                    stats.files_deleted += 1;
                }
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
        match embedder.as_deref_mut() {
            // ON mode: embed the passages and de-flatten back per file.
            Some(embedder) => {
                let mut vectors = if all_texts.is_empty() {
                    Vec::new()
                } else {
                    embedder.embed_batch(&all_texts, EmbedRole::Passage)?
                };
                if vectors.len() != all_texts.len() {
                    return Err(HallouminateError::Indexer(format!(
                        "embedder returned {} vectors for {} chunks",
                        vectors.len(),
                        all_texts.len()
                    )));
                }
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
                    pf.embeddings = Some(buf);
                }
            }
            // OFF mode: write null embeddings, count chunks but no embeddings.
            None => {
                for (pf, count) in prepared.iter_mut().zip(splits.iter().copied()) {
                    stats.chunks_inserted += count;
                    pf.embeddings = None;
                }
            }
        }
        let n = prepared.len();
        run.store.apply_batch(prepared).await?;
        stats.files_upserted += n;
    }
    Ok(())
}
