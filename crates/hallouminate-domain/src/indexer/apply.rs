use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use crate::common::{CorpusConfig, CorpusKey, FileRef, HallouminateError, Mtime, Result};
use crate::corpus::blake3_file;
use crate::indexer::chunk::PreparedFile;
use crate::indexer::store::ChunkStore;

use super::format::HandlerRegistry;
use super::plan::{IndexPlan, MtimeCandidate};
use super::writer::{Prepared, SkipReason, WriteRequest, file_ref_string, prepare_file};

/// Maximum number of files that [`ApplyStats::skipped_unreadable`] names.
/// The `files_skipped_unreadable` count stays exact past this cap.
pub const MAX_REPORTED_SKIPS: usize = 100;

/// One present file that an [`apply`] run skipped without indexing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// The skipped file.
    pub file: FileRef,
    /// Why the indexer skipped it.
    pub reason: SkipReason,
}

/// Tallies of the work an [`apply`] run performed, returned to the caller for
/// reporting and assertions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
    /// The first [`MAX_REPORTED_SKIPS`] files counted in
    /// `files_skipped_unreadable`, in processing order, with the reason.
    pub skipped_unreadable: Vec<SkippedFile>,
    /// Total chunks written across all upserted files (both embedding modes).
    pub chunks_inserted: usize,
    /// Total embedding vectors written; zero when the embedder is `None`.
    pub embeddings_inserted: usize,
}

impl ApplyStats {
    /// Counts one unreadable file, and names it while fewer than
    /// [`MAX_REPORTED_SKIPS`] files are named.
    pub fn record_unreadable(&mut self, skipped: SkippedFile) {
        self.files_skipped_unreadable += 1;
        if self.skipped_unreadable.len() < MAX_REPORTED_SKIPS {
            self.skipped_unreadable.push(skipped);
        }
    }

    /// Adds every tally in `other` to `self`. The named skips stay capped
    /// at [`MAX_REPORTED_SKIPS`].
    pub fn merge(&mut self, other: ApplyStats) {
        let ApplyStats {
            files_upserted,
            files_touched,
            files_deleted,
            files_skipped_empty,
            files_skipped_unreadable,
            skipped_unreadable,
            chunks_inserted,
            embeddings_inserted,
        } = other;
        self.files_upserted += files_upserted;
        self.files_touched += files_touched;
        self.files_deleted += files_deleted;
        self.files_skipped_empty += files_skipped_empty;
        self.files_skipped_unreadable += files_skipped_unreadable;
        for file in skipped_unreadable {
            if self.skipped_unreadable.len() >= MAX_REPORTED_SKIPS {
                break;
            }
            self.skipped_unreadable.push(file);
        }
        self.chunks_inserted += chunks_inserted;
        self.embeddings_inserted += embeddings_inserted;
    }
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
    store: &'a dyn ChunkStore,
    registry: &'a HandlerRegistry,
    indexed_at_ms: i64,
    batch_size: usize,
    lane: Option<&'a LaneFn>,
}

/// Default number of files prepared and embedded per batch when the caller
/// does not specify one. Bounds peak memory and embedder call width.
pub const DEFAULT_BATCH_SIZE: usize = 16;

/// Guards against git's racy-clean problem: if a file is indexed and
/// rewritten within the same mtime millisecond, the stored mtime equals the
/// rewrite's mtime and mtime-equality gates (watcher stage-1
/// `mtime_matches_last_index` in watch.rs, and bulk `plan()`) would treat the
/// rewrite as already-indexed and skip it. Recording `mtime - 1` for any
/// mtime observed at or after `now_ms` forces those gates to fall through to
/// a content-hash check for racily-recorded files; the next reindex lands
/// strictly after this ms and records the true mtime, so gating converges.
fn smudge_racy_mtime(mtime: Mtime, now_ms: i64) -> Mtime {
    if mtime.0 >= now_ms {
        Mtime(mtime.0 - 1)
    } else {
        mtime
    }
}

fn corpus_key_for_file(corpus: &CorpusConfig, file: &FileRef) -> Result<CorpusKey> {
    let file = crate::common::canonicalize_or_passthrough(file.as_path());
    let mut owner: Option<CorpusKey> = None;
    for key in corpus.corpus_keys() {
        if !file.as_path().starts_with(&key.canonical_root) {
            continue;
        }
        match &owner {
            None => owner = Some(key),
            Some(current) => {
                let specificity = key.canonical_root.components().count();
                let current_specificity = current.canonical_root.components().count();
                if specificity > current_specificity {
                    owner = Some(key);
                }
            }
        }
    }
    owner.ok_or_else(|| {
        HallouminateError::Indexer(format!(
            "file is outside configured corpus roots: {}",
            file.as_path().display()
        ))
    })
}

pub async fn apply(
    plan: IndexPlan,
    store: &dyn ChunkStore,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
    batch_size: usize,
    precomputed: Option<(&FileRef, &[u8])>,
) -> Result<ApplyStats> {
    apply_with_lane(plan, store, registry, corpus, batch_size, precomputed, None).await
}

/// Opaque guard that a [`LaneFn`] returns. `apply_with_lane` holds it for the
/// duration of one store write and then drops it.
pub type LaneGuard = Box<dyn Send>;

/// Why a caller could not hand out the write lane, such as a closed lane
/// or a daemon that is shutting down. The pass that asked for the lane
/// aborts with this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LaneError(pub &'static str);

impl From<LaneError> for HallouminateError {
    fn from(error: LaneError) -> Self {
        HallouminateError::Indexer(error.0.to_string())
    }
}

/// Future that resolves once the caller owns the write lane, or fails with
/// the reason the lane is unavailable.
pub type LaneFuture =
    Pin<Box<dyn Future<Output = std::result::Result<LaneGuard, LaneError>> + Send>>;

/// Caller-supplied closure that acquires the write lane.
pub type LaneFn = dyn Fn() -> LaneFuture + Send + Sync;

async fn acquire_lane(lane: Option<&LaneFn>) -> Result<Option<LaneGuard>> {
    match lane {
        Some(acquire) => Ok(Some(acquire().await?)),
        None => Ok(None),
    }
}

/// Like [`apply`], but acquires the write lane through `lane` around each
/// store write batch, not around the whole plan. File reads, hashing, and
/// chunking run without the lane. With `lane` set to `None`, this function
/// behaves as [`apply`].
pub async fn apply_with_lane(
    plan: IndexPlan,
    store: &dyn ChunkStore,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
    batch_size: usize,
    precomputed: Option<(&FileRef, &[u8])>,
    lane: Option<&LaneFn>,
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let batch_size = batch_size.max(1);
    let indexed_at_ms = chrono::Utc::now().timestamp_millis();
    let run = RunCtx {
        store,
        registry,
        indexed_at_ms,
        batch_size,
        lane,
    };

    let mut upsert_reqs: Vec<WriteRequest<'_>> = Vec::with_capacity(plan.upserts.len());
    for upsert in &plan.upserts {
        let corpus_key = match &upsert.corpus_key {
            Some(key) => key.clone(),
            None => corpus_key_for_file(corpus, &upsert.file)?,
        };
        upsert_reqs.push(WriteRequest {
            corpus_key,
            file: &upsert.file,
            mtime: smudge_racy_mtime(upsert.mtime, indexed_at_ms),
        });
    }
    run_in_batches(
        upsert_reqs,
        &run,
        &mut stats,
        EmptyFilePolicy::Retain,
        precomputed,
    )
    .await?;

    let mut fallthrough: Vec<MtimeCandidate> = Vec::new();
    for candidate in plan.mtime_touches {
        let new_hash = match &candidate.known_hash {
            Some(hash) => hash.clone(),
            None => blake3_file(candidate.file.as_path())?,
        };
        if new_hash == candidate.snap.content_hash {
            let _lane = acquire_lane(lane).await?;
            store
                .touch_mtime(
                    &candidate.snap.corpus_key,
                    &candidate.snap.file_ref,
                    smudge_racy_mtime(candidate.new_mtime, indexed_at_ms).0,
                )
                .await?;
            stats.files_touched += 1;
        } else {
            fallthrough.push(candidate);
        }
    }
    let mut fallthrough_reqs: Vec<WriteRequest<'_>> = Vec::with_capacity(fallthrough.len());
    for candidate in &fallthrough {
        fallthrough_reqs.push(WriteRequest {
            corpus_key: candidate.snap.corpus_key.clone(),
            file: &candidate.file,
            mtime: smudge_racy_mtime(candidate.new_mtime, indexed_at_ms),
        });
    }
    run_in_batches(
        fallthrough_reqs,
        &run,
        &mut stats,
        EmptyFilePolicy::Evict,
        precomputed,
    )
    .await?;

    let configured_keys = corpus.corpus_keys();
    for snapshot in plan.deletes {
        if configured_keys.contains(&snapshot.corpus_key) {
            let _lane = acquire_lane(lane).await?;
            store
                .delete_file(&snapshot.corpus_key, &snapshot.file_ref)
                .await?;
            stats.files_deleted += 1;
        } else {
            tracing::debug!(
                target: "hallouminate::indexer",
                file_ref = %snapshot.file_ref,
                corpus = %snapshot.corpus_key.name,
                root = %snapshot.corpus_key.canonical_root.display(),
                "skipping delete: corpus key outside this request's configured roots"
            );
        }
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
    stats: &mut ApplyStats,
    empty_file_policy: EmptyFilePolicy,
    precomputed: Option<(&FileRef, &[u8])>,
) -> Result<()> {
    let mut by_key: BTreeMap<CorpusKey, Vec<WriteRequest<'_>>> = BTreeMap::new();
    for req in reqs {
        by_key.entry(req.corpus_key.clone()).or_default().push(req);
    }
    for key_reqs in by_key.into_values() {
        for chunk_of_reqs in key_reqs.chunks(run.batch_size) {
            let mut prepared: Vec<PreparedFile> = Vec::with_capacity(chunk_of_reqs.len());
            for req in chunk_of_reqs {
                let bytes_override =
                    precomputed.and_then(
                        |(file, bytes)| {
                            if req.file == file { Some(bytes) } else { None }
                        },
                    );
                let prepared_file = prepare_file(
                    WriteRequest {
                        corpus_key: req.corpus_key.clone(),
                        file: req.file,
                        mtime: req.mtime,
                    },
                    run.registry,
                    run.indexed_at_ms,
                    bytes_override,
                )?;
                let prepared_file = match prepared_file {
                    Prepared::File(prepared_file) => prepared_file,
                    Prepared::Skipped(reason) => {
                        stats.record_unreadable(SkippedFile {
                            file: req.file.clone(),
                            reason,
                        });
                        continue;
                    }
                };
                if prepared_file.chunks.is_empty() {
                    tracing::warn!(
                        target: "hallouminate::indexer",
                        file = %req.file.as_path().display(),
                        "skipping empty file (no chunks generated)"
                    );
                    stats.files_skipped_empty += 1;
                    if empty_file_policy == EmptyFilePolicy::Evict {
                        let file_ref = file_ref_string(req.file)?;
                        tracing::info!(
                            target: "hallouminate::indexer",
                            corpus = %req.corpus_key.name,
                            root = %req.corpus_key.canonical_root.display(),
                            file = %file_ref,
                            "evicting indexed file from search: re-index produced an empty file",
                        );
                        let _lane = acquire_lane(run.lane).await?;
                        run.store.delete_file(&req.corpus_key, &file_ref).await?;
                        stats.files_deleted += 1;
                    }
                    continue;
                }
                prepared.push(prepared_file);
            }
            if prepared.is_empty() {
                continue;
            }
            let file_count = prepared.len();
            let _lane = acquire_lane(run.lane).await?;
            let write_stats = run.store.apply_batch(prepared).await?;
            stats.chunks_inserted += write_stats.chunks_written;
            stats.embeddings_inserted += write_stats.embeddings_written;
            stats.files_upserted += file_count;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use text_splitter::Characters;

    use super::*;
    use crate::common::{
        HallouminateError, RetiredRoot, canonicalize_or_passthrough, expand_tilde,
    };
    use crate::indexer::{BatchWriteStats, FileSnapshot, Upsert};

    #[derive(Default)]
    struct RecordingStore {
        deleted: Mutex<Vec<(CorpusKey, String)>>,
        batches: Mutex<Vec<CorpusKey>>,
        guards_live: Arc<AtomicUsize>,
        guards_live_at_write: Mutex<Vec<usize>>,
    }

    struct CountedGuard(Arc<AtomicUsize>);

    impl Drop for CountedGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ChunkStore for RecordingStore {
        async fn list_files(&self, _corpus_key: &CorpusKey) -> Result<Vec<FileSnapshot>> {
            Ok(Vec::new())
        }

        async fn touch_mtime(
            &self,
            _corpus_key: &CorpusKey,
            _file_ref: &str,
            _mtime_ms: i64,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_file(&self, corpus_key: &CorpusKey, file_ref: &str) -> Result<()> {
            self.deleted
                .lock()
                .map_err(|_| HallouminateError::Indexer("deleted mutex poisoned".into()))?
                .push((corpus_key.clone(), file_ref.to_string()));
            Ok(())
        }

        async fn distinct_roots(&self) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        async fn delete_root(&self, _root: &RetiredRoot) -> Result<u64> {
            Ok(0)
        }

        async fn apply_batch(&self, files: Vec<PreparedFile>) -> Result<BatchWriteStats> {
            let Some(first) = files.first() else {
                return Ok(BatchWriteStats::default());
            };
            if files.iter().any(|file| file.corpus_key != first.corpus_key) {
                return Err(HallouminateError::Indexer(
                    "recording store received a mixed corpus-key batch".into(),
                ));
            }
            self.batches
                .lock()
                .map_err(|_| HallouminateError::Indexer("batches mutex poisoned".into()))?
                .push(first.corpus_key.clone());
            self.guards_live_at_write
                .lock()
                .map_err(|_| HallouminateError::Indexer("guards mutex poisoned".into()))?
                .push(self.guards_live.load(Ordering::SeqCst));
            Ok(BatchWriteStats {
                chunks_written: files.iter().map(|file| file.chunks.len()).sum(),
                embeddings_written: 0,
            })
        }
    }

    fn snapshot(corpus_key: &CorpusKey, path: &Path) -> FileSnapshot {
        FileSnapshot {
            file_ref: path.to_string_lossy().into_owned(),
            corpus_key: corpus_key.clone(),
            mtime_ms: 0,
            content_hash: String::new(),
        }
    }

    #[tokio::test]
    async fn apply_only_deletes_snapshots_under_the_requested_roots() {
        let parent = tempfile::tempdir().expect("tempdir");
        let selected = parent.path().join("selected");
        let sibling = parent.path().join("sibling");
        std::fs::create_dir_all(&selected).expect("create selected root");
        std::fs::create_dir_all(&sibling).expect("create sibling root");
        let selected_file = canonicalize_or_passthrough(&selected)
            .into_path_buf()
            .join("gone.md");
        let sibling_file = canonicalize_or_passthrough(&sibling)
            .into_path_buf()
            .join("keep.md");
        let selected_key = CorpusKey::from_configured_root("docs", &selected.to_string_lossy());
        let sibling_key = CorpusKey::from_configured_root("docs", &sibling.to_string_lossy());
        let plan = IndexPlan {
            upserts: Vec::new(),
            mtime_touches: Vec::new(),
            deletes: vec![
                snapshot(&selected_key, &selected_file),
                snapshot(&sibling_key, &sibling_file),
            ],
        };
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![selected.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: Vec::new(),
            global: false,
        };
        let store = RecordingStore::default();
        let registry = HandlerRegistry::new(Characters, 384);

        let stats = apply(plan, &store, &registry, &corpus, 16, None)
            .await
            .expect("apply");

        assert_eq!(stats.files_deleted, 1);
        assert_eq!(
            *store.deleted.lock().expect("deleted mutex"),
            vec![(selected_key, selected_file.to_string_lossy().into_owned())]
        );
    }

    /// #215 regression: `corpus.paths` carry a literal `~` until consumption
    /// (the "expand at consumption time" convention), while stored `file_ref`s
    /// are canonical absolute paths. The delete-scope roots must tilde-expand
    /// before matching — otherwise an in-scope delete under a `~/…` root fails
    /// `starts_with` and is silently skipped, so the deleted file is never
    /// evicted from the index.
    #[tokio::test]
    async fn apply_expands_tilde_in_roots_so_home_rooted_deletes_fire() {
        // A `~`-rooted corpus path, kept non-existent on disk so both the root
        // and the file_ref resolve via passthrough to the same expanded prefix
        // (no symlink-canonicalization skew). The delete decision is pure path
        // logic, so no files on disk are needed.
        let raw_root = "~/.hallouminate-affinage-236-tilde-scope-test";
        let gone = expand_tilde(raw_root).join("gone.md");
        let corpus_key = CorpusKey::from_configured_root("docs", raw_root);
        let plan = IndexPlan {
            upserts: Vec::new(),
            mtime_touches: Vec::new(),
            deletes: vec![snapshot(&corpus_key, &gone)],
        };
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![raw_root.to_string()],
            globs: vec!["**/*.md".into()],
            exclude: Vec::new(),
            global: false,
        };
        let store = RecordingStore::default();
        let registry = HandlerRegistry::new(Characters, 384);

        let stats = apply(plan, &store, &registry, &corpus, 16, None)
            .await
            .expect("apply");

        assert_eq!(
            stats.files_deleted, 1,
            "a delete under a ~-rooted corpus must fire; an unexpanded root would skip it"
        );
        assert_eq!(
            *store.deleted.lock().expect("deleted mutex"),
            vec![(corpus_key, gone.to_string_lossy().into_owned())]
        );
    }

    #[tokio::test]
    async fn apply_splits_mixed_root_upserts_into_deterministic_batches() {
        let parent = tempfile::tempdir().expect("tempdir");
        let root_a = parent.path().join("a");
        let root_b = parent.path().join("b");
        std::fs::create_dir_all(&root_a).expect("create root a");
        std::fs::create_dir_all(&root_b).expect("create root b");
        let file_a = root_a.join("a.md");
        let file_b = root_b.join("b.md");
        std::fs::write(&file_a, "# A\n\nalpha\n").expect("write a");
        std::fs::write(&file_b, "# B\n\nbeta\n").expect("write b");
        let key_a = CorpusKey::from_configured_root("docs", &root_a.to_string_lossy());
        let key_b = CorpusKey::from_configured_root("docs", &root_b.to_string_lossy());
        let plan = IndexPlan {
            upserts: vec![
                Upsert {
                    file: FileRef::new(file_b),
                    mtime: Mtime(1),
                    corpus_key: Some(key_b.clone()),
                },
                Upsert {
                    file: FileRef::new(file_a),
                    mtime: Mtime(1),
                    corpus_key: Some(key_a.clone()),
                },
            ],
            mtime_touches: Vec::new(),
            deletes: Vec::new(),
        };
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![
                root_b.to_string_lossy().into_owned(),
                root_a.to_string_lossy().into_owned(),
            ],
            globs: vec!["**/*.md".into()],
            exclude: Vec::new(),
            global: false,
        };
        let store = RecordingStore::default();
        let registry = HandlerRegistry::new(Characters, 384);

        apply(plan, &store, &registry, &corpus, 16, None)
            .await
            .expect("apply");

        let mut expected = vec![key_a, key_b];
        expected.sort();
        assert_eq!(
            *store.batches.lock().expect("batches mutex"),
            expected,
            "one deterministic write batch per corpus root"
        );
    }

    #[tokio::test]
    async fn apply_with_lane_acquires_the_lane_once_per_write_batch() {
        let root = tempfile::tempdir().expect("tempdir");
        let key = CorpusKey::from_configured_root("docs", &root.path().to_string_lossy());
        let mut upserts = Vec::new();
        for name in ["a.md", "b.md", "c.md"] {
            let file = root.path().join(name);
            std::fs::write(&file, format!("# {name}\n\nbody of {name}\n")).expect("write file");
            upserts.push(Upsert {
                file: FileRef::new(file),
                mtime: Mtime(1),
                corpus_key: Some(key.clone()),
            });
        }
        let plan = IndexPlan {
            upserts,
            mtime_touches: Vec::new(),
            deletes: Vec::new(),
        };
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![root.path().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: Vec::new(),
            global: false,
        };
        let acquisitions = Arc::new(AtomicUsize::new(0));
        let guards_live = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&acquisitions);
        let live = Arc::clone(&guards_live);
        let lane: Box<LaneFn> = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            live.fetch_add(1, Ordering::SeqCst);
            let guard = CountedGuard(Arc::clone(&live));
            Box::pin(async move { Ok(Box::new(guard) as LaneGuard) })
        });
        let store = RecordingStore {
            guards_live: Arc::clone(&guards_live),
            ..RecordingStore::default()
        };
        let registry = HandlerRegistry::new(Characters, 384);

        let stats = apply_with_lane(plan, &store, &registry, &corpus, 1, None, Some(&*lane))
            .await
            .expect("apply_with_lane");

        let batch_count = store.batches.lock().expect("batches mutex").len();
        assert_eq!(stats.files_upserted, 3);
        assert_eq!(
            batch_count, 3,
            "batch size 1 must split three files into three batches"
        );
        assert_eq!(
            acquisitions.load(Ordering::SeqCst),
            batch_count,
            "the lane is acquired once per write batch"
        );
        assert_eq!(
            *store.guards_live_at_write.lock().expect("guards mutex"),
            vec![1, 1, 1],
            "exactly one lane guard is held while each batch writes"
        );
        assert_eq!(
            guards_live.load(Ordering::SeqCst),
            0,
            "every lane guard is released after its batch"
        );
    }

    // -- smudge_racy_mtime: git-style racy-clean handling --

    #[test]
    fn smudge_racy_mtime_backdates_an_mtime_recorded_within_the_same_ms_as_indexing() {
        // The file's mtime lands in the same millisecond apply() observed as
        // "now" -- a later same-ms rewrite would carry an identical mtime and
        // be invisible to mtime-equality gates. Backdating by 1ms forces those
        // gates to fall through to a content-hash check instead.
        assert_eq!(smudge_racy_mtime(Mtime(1_000), 1_000), Mtime(999));
    }

    #[test]
    fn smudge_racy_mtime_leaves_a_strictly_past_mtime_unchanged() {
        // No race is possible once the file's mtime is strictly before the
        // indexing timestamp -- a later rewrite necessarily bumps the mtime,
        // so equality gates already see it correctly.
        assert_eq!(smudge_racy_mtime(Mtime(999), 1_000), Mtime(999));
    }

    fn skipped(name: &str) -> SkippedFile {
        SkippedFile {
            file: FileRef::new(PathBuf::from(format!("/docs/{name}"))),
            reason: SkipReason::UnsupportedFormat,
        }
    }

    #[test]
    fn record_unreadable_counts_every_skip_and_names_up_to_the_cap() {
        let mut stats = ApplyStats::default();
        for i in 0..MAX_REPORTED_SKIPS + 5 {
            stats.record_unreadable(skipped(&format!("f{i}.bin")));
        }
        assert_eq!(stats.files_skipped_unreadable, MAX_REPORTED_SKIPS + 5);
        assert_eq!(stats.skipped_unreadable.len(), MAX_REPORTED_SKIPS);
        assert_eq!(stats.skipped_unreadable[0], skipped("f0.bin"));
    }

    #[test]
    fn merge_sums_every_tally_and_keeps_the_name_cap() {
        let mut into = ApplyStats {
            files_upserted: 1,
            files_touched: 2,
            files_deleted: 3,
            files_skipped_empty: 4,
            chunks_inserted: 5,
            embeddings_inserted: 6,
            ..Default::default()
        };
        for i in 0..MAX_REPORTED_SKIPS - 1 {
            into.record_unreadable(skipped(&format!("a{i}.bin")));
        }
        let mut extra = ApplyStats {
            files_upserted: 10,
            files_touched: 20,
            files_deleted: 30,
            files_skipped_empty: 40,
            chunks_inserted: 50,
            embeddings_inserted: 60,
            ..Default::default()
        };
        extra.record_unreadable(skipped("b0.bin"));
        extra.record_unreadable(skipped("b1.bin"));

        into.merge(extra);

        assert_eq!(into.files_upserted, 11);
        assert_eq!(into.files_touched, 22);
        assert_eq!(into.files_deleted, 33);
        assert_eq!(into.files_skipped_empty, 44);
        assert_eq!(into.chunks_inserted, 55);
        assert_eq!(into.embeddings_inserted, 66);
        assert_eq!(into.files_skipped_unreadable, MAX_REPORTED_SKIPS + 1);
        assert_eq!(into.skipped_unreadable.len(), MAX_REPORTED_SKIPS);
        assert_eq!(into.skipped_unreadable.last(), Some(&skipped("b0.bin")));
    }
}
