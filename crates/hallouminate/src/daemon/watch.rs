//! Filesystem watcher that incrementally re-indexes baseline corpus roots.
//!
//! Wires the otherwise-dead `[watch] debounce_ms` knob: `notify` +
//! `notify-debouncer-full` watch the boot baseline's corpus roots, and on a
//! debounced change the daemon reindexes just the affected markdown file
//! (`index_single_file`) or prunes its rows on delete. The debounce window is
//! `cfg.watch.debounce_ms`.
//!
//! Scope (spec Non-goal): only the **baseline** `[[corpus]]` and baseline
//! `[[repository]]` roots are watched. Repo-layer corpora are discovered
//! per-RPC from the client cwd, so the daemon never caches them and the
//! watcher cannot see them. This is a documented limitation, not a bug.
//!
//! Concurrency (spec Risk): every reindex takes the same per-corpus lock +
//! global write-lane (`acquire_mutation_guard`) that `handle_index` /
//! `handle_add_markdown` take, so a watch-triggered reindex never races the
//! daemon's own writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use hallouminate_adapters::LanceStore;
use hallouminate_domain::common::{CorpusConfig, canonicalize_or_passthrough, expand_tilde};
use hallouminate_domain::corpus::ensure_corpus_allows_file;

use super::dispatch::index_single_file_with_content;
use super::state::{DaemonState, WorkClass};

/// One watched location: the directory handed to `notify`, the corpus that
/// owns it, and — for a **file-path** corpus root — the exact declared file.
/// A file-path corpus root (e.g. `~/.claude/CLAUDE.md`) is watched at its
/// parent dir, and membership then requires an exact match against the declared
/// file so the watcher never reindexes sibling `.md` the corpus does not own,
/// matching `walker::scan`'s single-file semantics.
///
/// `notify` reports filesystem events with **canonical** paths (symlinked
/// ancestors resolved — e.g. macOS `/var` → `/private/var`), so membership and
/// prune-key construction match against the canonical forms resolved once at
/// setup *while the root exists*:
///
/// - `canonical_watched` — the resolved watched dir; `owning_corpus` prefixes
///   event paths against it, and the delete-prune path rebuilds the absent
///   file's `file_ref` as `canonical_watched.join(rel)`.
/// - `canonical_file_root` — the resolved declared file for a file-path root;
///   the exact-match membership test compares against it (`None` for dir roots).
///
/// `watched` (non-canonical) is retained only to hand to `debouncer.watch()`.
/// Canonicalizing the deleted path directly fails and would diverge from the
/// key the indexer wrote against the resolved ancestor, silently no-op'ing the
/// prune — the divergence the spec flagged as an open question.
struct WatchRoot {
    watched: PathBuf,
    canonical_watched: PathBuf,
    corpus: CorpusConfig,
    canonical_file_root: Option<PathBuf>,
    /// Recursion mode handed to `notify`. A directory root watches its whole
    /// subtree (`Recursive`); a file-path root watches only its parent dir's
    /// direct entries (`NonRecursive`) — enough to catch edits and the
    /// write-temp-then-rename atomic-save dance editors do on the file, but
    /// without flooding `owning_corpus` with events for an unrelated subtree
    /// it would only discard.
    mode: RecursiveMode,
}

const MAX_FAILURE_SIGNATURES: usize = 256;

// Keys on the full `anyhow::Error` display string (see the `e.to_string()`
// call site in `handle_changed_path`) because `index_single_file_with_content`
// returns an opaque `anyhow::Result` with no stable error variant to key on
// instead. Two known tradeoffs from this: (1) volatile error text (offsets,
// transient ids) yields a distinct signature per occurrence, defeating
// suppression for errors that vary slightly between reindex attempts; (2) the
// last window's suppressed count is never flushed once failures for a
// signature stop, so a trailing `suppressed: N` reminder can be lost.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FailureSignature {
    path: PathBuf,
    error: String,
}

struct FailureState {
    last_reported: Instant,
    last_seen: Instant,
    suppressed: u64,
}

struct FailureCoalescer {
    reminder: Duration,
    max_signatures: usize,
    states: HashMap<FailureSignature, FailureState>,
}

#[derive(Debug, PartialEq, Eq)]
enum FailureDecision {
    First,
    Suppress,
    Reminder { suppressed: u64 },
}

impl FailureCoalescer {
    fn new(reminder: Duration, max_signatures: usize) -> Self {
        Self {
            reminder,
            max_signatures,
            states: HashMap::new(),
        }
    }

    fn record(&mut self, path: &Path, error: &str, now: Instant) -> FailureDecision {
        if self.reminder.is_zero() {
            return FailureDecision::First;
        }

        let signature = FailureSignature {
            path: path.to_path_buf(),
            error: error.to_string(),
        };
        if let Some(state) = self.states.get_mut(&signature) {
            state.last_seen = now;
            if now.saturating_duration_since(state.last_reported) < self.reminder {
                state.suppressed = state.suppressed.saturating_add(1);
                return FailureDecision::Suppress;
            }
            let suppressed = state.suppressed;
            state.last_reported = now;
            state.suppressed = 0;
            return FailureDecision::Reminder { suppressed };
        }

        if self.states.len() >= self.max_signatures {
            self.evict_oldest();
        }
        self.states.insert(
            signature,
            FailureState {
                last_reported: now,
                last_seen: now,
                suppressed: 0,
            },
        );
        FailureDecision::First
    }

    fn evict_oldest(&mut self) {
        let mut oldest = None;
        for (signature, state) in &self.states {
            let replace = match &oldest {
                None => true,
                Some((_signature, last_seen)) => state.last_seen < *last_seen,
            };
            if replace {
                oldest = Some((signature.clone(), state.last_seen));
            }
        }
        if let Some((signature, _last_seen)) = oldest {
            self.states.remove(&signature);
        }
    }
}

/// Owns the background debouncer + event-pump task. Dropping it stops the
/// watcher (the debouncer's worker thread joins on drop; aborting the task
/// drops the event receiver so the thread's send fails and it exits).
pub struct WatcherHandle {
    _task: tokio::task::JoinHandle<()>,
    // The debouncer must outlive the watch session; held here so its worker
    // thread keeps running until this handle drops.
    _debouncer: Box<dyn std::any::Any + Send>,
}

impl WatcherHandle {
    /// Await the pump task; used by the supervisor factory so a watcher
    /// restart rebuilds the whole debouncer + pump pair. Holds `self` (and
    /// so the debouncer) alive until the pump future completes.
    pub(crate) async fn join(self) {
        let _ = self._task.await;
    }
}

/// Watch the baseline corpora roots and spawn a task that reindexes changed
/// markdown files (debounced by `cfg.watch.debounce_ms`). Returns `None` when
/// there are no watchable roots or the watcher backend fails to initialize —
/// the daemon still serves; auto-reindex is simply off.
pub fn spawn_corpus_watcher(state: &DaemonState) -> Option<WatcherHandle> {
    let cfg = state.baseline();
    let debounce = Duration::from_millis(cfg.watch.debounce_ms);
    let failure_reminder = Duration::from_secs(cfg.watch.failure_reminder_secs);
    let corpora = match cfg.effective_corpora() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "watcher: could not enumerate baseline corpora; auto-reindex disabled",
            );
            return None;
        }
    };

    // Collect a WatchRoot for every existing baseline corpus root.
    let mut roots: Vec<WatchRoot> = Vec::new();
    for corpus in &corpora {
        for raw in &corpus.paths {
            if let Some(root) = build_watch_root(corpus, raw) {
                roots.push(root);
            }
        }
    }
    if roots.is_empty() {
        return None;
    }

    // Affected paths pending reindex, coalesced across debounced batches (not
    // just within one) rather than forwarded whole-batch through an
    // unbounded channel: a write burst that outpaces the serial async
    // consumer used to retain every debounced batch in daemon memory
    // indefinitely. `pending` accumulates distinct paths; `wake` only signals
    // "something is pending" and is bounded to capacity 1 — the consumer
    // always drains the *whole* `pending` set on wake, so a second wake
    // queued while one is outstanding would be redundant. `try_send`
    // returning `Full` is that explicit overflow behavior: a no-op, never a
    // block or a panic, because the paths it would have carried are already
    // sitting in `pending`.
    let pending: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);

    let state_for_debouncer = state.clone();
    let pending_for_debouncer = pending.clone();
    let mut debouncer = match new_debouncer(debounce, None, move |res: DebounceEventResult| {
        // The debouncer worker thread calls this on each debounced batch.
        match res {
            Ok(events) => {
                record_pending(&pending_for_debouncer, &events);
                state_for_debouncer.record_watcher_events(events.len() as u64);
                // Non-blocking: `Full` means a wake is already queued (this
                // batch's paths are already recorded in `pending` above, so
                // the outstanding wake will pick them up); `Disconnected`
                // means the daemon is shutting down. Either way there is
                // nothing more to do here.
                let _ = wake_tx.try_send(());
            }
            Err(errors) => {
                for err in errors {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        error = %err,
                        "watcher: notify backend error",
                    );
                }
            }
        }
    }) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "watcher: failed to create debouncer; auto-reindex disabled",
            );
            return None;
        }
    };

    for root in &roots {
        if let Err(e) = debouncer.watch(&root.watched, root.mode) {
            tracing::warn!(
                target: "hallouminate::daemon",
                root = %root.watched.display(),
                error = %e,
                "watcher: failed to watch root; that corpus will not auto-reindex",
            );
        }
    }

    let state = state.clone();
    let shutdown = state.shutdown_token().clone();
    let task = tokio::spawn(async move {
        // Bridge the std mpsc receiver into the async runtime via
        // spawn_blocking-style recv with cancellation. We poll the channel on
        // a blocking thread per wake; simplest correct shape that respects
        // the shutdown token.
        let wake_rx = std::sync::Arc::new(std::sync::Mutex::new(wake_rx));
        let mut failures = FailureCoalescer::new(failure_reminder, MAX_FAILURE_SIGNATURES);
        loop {
            let wake_rx_recv = wake_rx.clone();
            let next = tokio::select! {
                _ = shutdown.cancelled() => break,
                got = tokio::task::spawn_blocking(move || {
                    wake_rx_recv.lock().expect("watch wake-rx mutex").recv()
                }) => got,
            };
            match next {
                Ok(Ok(())) => {}
                // Channel disconnected: the debouncer (and its `wake_tx`) was
                // dropped, so nothing more will ever arrive. Distinct from the
                // `shutdown.cancelled()` branch above, which is an expected,
                // silent exit: an unexpected disconnect while the daemon is
                // still meant to be serving is worth structured error context
                // so it shows up in telemetry instead of auto-reindex just
                // going quiet.
                Ok(Err(_recv_err)) => {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        "watcher: event channel disconnected unexpectedly; auto-reindex pump stopping",
                    );
                    break;
                }
                Err(join_err) => {
                    tracing::error!(
                        target: "hallouminate::daemon",
                        error = %join_err,
                        "watcher: blocking recv task failed; auto-reindex pump stopping",
                    );
                    break;
                }
            }
            state.heartbeat().bump(super::heartbeat::TaskName::WatcherPump);
            let paths: Vec<PathBuf> = {
                let mut set = pending.lock().expect("watch pending-paths mutex");
                set.drain().collect()
            };
            if !paths.is_empty() {
                process_change_batch(&state, &roots, paths, &mut failures).await;
            }
        }
    });

    Some(WatcherHandle {
        _task: task,
        _debouncer: Box::new(debouncer),
    })
}

/// Insert every markdown path from one debounced batch into the shared
/// pending set, coalescing duplicates within *and across* batches — a burst
/// that touches one file many times (or arrives in several batches before the
/// consumer next drains) still reindexes it once per drain.
///
/// Filters by event *kind* first: `notify` 8.x's inotify backend subscribes
/// `WatchMask::OPEN`, so read-opens (including the reads reindexing itself
/// performs) surface as `EventKind::Access(_)`. Admitting those would make the
/// watcher feed itself — boot catch-up reads every corpus file, each read
/// emits an Access event, each Access event schedules a reindex, forever.
/// Only actual mutations schedule reindexing: `Create`, `Modify` (data,
/// metadata, or a rename's `Name(RenameMode::_)`), and `Remove`. The
/// catch-all `EventKind::Any` (used in tests and by backends that can't
/// distinguish, e.g. `PollWatcher`) is admitted too — dropping it risks
/// discarding a real mutation the backend just couldn't classify, and a
/// spurious reindex is cheap. `EventKind::Other` is admitted-by-default here
/// as well: notify's inotify backend only emits it for an inotify queue
/// overflow forcing a directory rescan (`Flag::Rescan`), never for a
/// non-mutating access, so treating it like `Any` is the conservative call.
fn record_pending(
    pending: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    events: &[notify_debouncer_full::DebouncedEvent],
) {
    let mut set = pending.lock().expect("watch pending-paths mutex");
    for event in events {
        if matches!(event.kind, notify::EventKind::Access(_)) {
            continue;
        }
        for path in &event.paths {
            // Extension-only, matching `format_from_extension`'s classification
            // without reading bytes: a deleted path no longer exists to sniff, and
            // reading an existing one just to decide admission would duplicate the
            // indexer's own read. Extensionless files fall through to `None` here
            // (never admitted) rather than risking a second, diverging extension
            // rule from the one `domain::indexer::format` owns.
            if !matches!(
                hallouminate_domain::indexer::format_from_extension(path),
                Some(Some(_))
            ) {
                continue;
            }
            set.insert(path.clone());
        }
    }
}

/// Build a `WatchRoot` for one declared corpus path, probing the filesystem to
/// decide what `notify` watches and how deeply:
///
/// - **Directory root** (exists, is a dir): watched at itself, `Recursive` —
///   any descendant the corpus globs accept is a member.
/// - **File-path root** (exists, is a file, e.g. `~/.claude/CLAUDE.md`): watched
///   at its parent dir, `NonRecursive` — only the parent's direct entries fire
///   events, which still catches edits and the editor write-temp-then-rename
///   atomic save on the file, without flooding `owning_corpus` with events for
///   an unrelated subtree it would discard.
///
/// Returns `None` for a not-yet-created root (e.g. a repo wiki dir absent at
/// boot); a later boot picks it up.
fn build_watch_root(corpus: &CorpusConfig, raw: &str) -> Option<WatchRoot> {
    let root = expand_tilde(raw);
    if root.is_dir() {
        let canonical_watched = canonicalize_or_passthrough(&root).into_path_buf();
        Some(WatchRoot {
            watched: root,
            canonical_watched,
            corpus: corpus.clone(),
            canonical_file_root: None,
            mode: RecursiveMode::Recursive,
        })
    } else if root.is_file() {
        let parent = root.parent()?.to_path_buf();
        let canonical_watched = canonicalize_or_passthrough(&parent).into_path_buf();
        let canonical_file_root = Some(canonicalize_or_passthrough(&root).into_path_buf());
        Some(WatchRoot {
            watched: parent,
            canonical_watched,
            corpus: corpus.clone(),
            canonical_file_root,
            mode: RecursiveMode::NonRecursive,
        })
    } else {
        None
    }
}

/// Reindex/prune every distinct path in one debounced batch. Holds a
/// connection guard for the whole batch and stamps the activity clock
/// afterward, mirroring `catch_up_index` (dispatch.rs) and
/// `handle_connection` (server.rs): without it, a watcher-triggered write
/// (in particular the delete/prune branch of `handle_changed_path`, which
/// touches neither an embedder nor the clock) can run while idle-exit tears
/// the process down mid-write, releasing the single-instance flock under a
/// live LanceDB writer (ADR-003).
async fn process_change_batch(
    state: &DaemonState,
    roots: &[WatchRoot],
    paths: Vec<PathBuf>,
    failures: &mut FailureCoalescer,
) {
    let _conn = state.enter_connection(WorkClass::Internal);
    for path in &paths {
        handle_changed_path(state, roots, path, failures).await;
    }
    state.touch_activity(WorkClass::Internal);
}

/// Reindex (or prune) one changed markdown path against whichever baseline
/// corpus owns it. Skips paths that no baseline corpus accepts.
async fn handle_changed_path(
    state: &DaemonState,
    roots: &[WatchRoot],
    path: &Path,
    failures: &mut FailureCoalescer,
) {
    let Some(owner) = owning_corpus(roots, path) else {
        return;
    };
    let corpus = &owner.corpus;
    let store = state.store();
    // Stage 1 of the change gate (ADR daemon-rework-003, "git's algorithm,
    // not git's state"): compare the on-disk mtime against the last-indexed
    // snapshot before taking any lock or reading any bytes. Equal means the
    // event is a no-op (e.g. the access-event feedback loop that burned 200%
    // CPU) and is shed for the price of one stat + one snapshot row read.
    if mtime_matches_last_index(&store, &corpus.name, path).await {
        tracing::debug!(
            target: "hallouminate::daemon",
            corpus = %corpus.name,
            path = %path.display(),
            "watcher: skipped event, mtime matches last-indexed snapshot",
        );
        return;
    }
    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(target: "hallouminate::daemon", error = %e, "watcher: lock failed");
            return;
        }
    };
    let exists = path.is_file();
    if exists {
        // `path.is_file()` above follows symlinks, so a symlinked leaf whose
        // target is a regular file elsewhere still reaches here. A single
        // no-follow read below both rejects the symlink and supplies the
        // content, closing the TOCTOU gap a separate check-then-read would
        // leave open to a symlink swapped in between the two calls.
        let relative = path
            .strip_prefix(&owner.canonical_watched)
            .expect("owning_corpus guarantees path starts_with canonical_watched");
        let (bytes, mtime) = match hallouminate_domain::corpus::read_no_follow_with_mtime(
            &owner.canonical_watched,
            relative,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    path = %path.display(),
                    error = ?e,
                    "watcher: skipping reindex, no-follow read failed",
                );
                return;
            }
        };
        let registry = state.make_registry();
        match index_single_file_with_content(&store, &registry, corpus, path, &bytes, mtime).await {
            Ok(stats) => {
                state.record_watcher_reindex(stats.files_upserted == 0);
                tracing::debug!(
                    target: "hallouminate::daemon",
                    corpus = %corpus.name,
                    path = %path.display(),
                    upserted = stats.files_upserted,
                    "watcher: reindexed changed file",
                );
            }
            Err(e) => {
                let error = e.to_string();
                match failures.record(path, &error, Instant::now()) {
                    FailureDecision::First => tracing::warn!(
                        target: "hallouminate::daemon",
                        path = %path.display(),
                        error = %error,
                        "watcher: reindex failed",
                    ),
                    FailureDecision::Suppress => {}
                    FailureDecision::Reminder { suppressed } => tracing::warn!(
                        target: "hallouminate::daemon",
                        path = %path.display(),
                        error = %error,
                        suppressed,
                        "watcher: reindex failed",
                    ),
                }
            }
        }
    } else {
        // Deleted (or moved away): prune the LanceDB rows keyed on the same
        // canonical file_ref the indexer wrote. The path no longer exists, so
        // canonicalizing it directly fails and falls through to the raw path —
        // which diverges from the stored key when the corpus root is reached
        // through a symlinked ancestor (the indexer canonicalized a live path,
        // resolving the symlink). Rebuild the key from the root we canonicalized
        // at setup (`canonical_watched`) joined with the path's tail under the
        // watched dir, so the prune matches regardless of symlinked ancestors.
        let file_ref = delete_file_ref(owner, path);
        if let Some(file_ref_str) = file_ref.as_path().to_str()
            && let Err(e) = store.delete_file(&corpus.name, file_ref_str).await
        {
            tracing::warn!(
                target: "hallouminate::daemon",
                path = %path.display(),
                error = %e,
                "watcher: prune failed",
            );
        }
    }
    drop(guard);
}

/// Stage-1 change gate (ADR daemon-rework-003): does `path`'s on-disk mtime
/// equal the stored `FileSnapshot.mtime_ms` from the last index?
///
/// Stat-only — never reads content. The stat is no-follow
/// (`symlink_metadata`) and gates only regular files, so a symlinked leaf
/// never matches here and falls through to the no-follow read, which rejects
/// it. Millisecond truncation mirrors `mtime_ms_from_duration` (dispatch.rs),
/// which produced the stored value. Every failure — stat error, pre-epoch or
/// overflowing mtime, non-UTF-8 path, missing snapshot, store error — answers
/// `false`: the gate only skips work it can prove redundant; anything
/// unprovable proceeds to the full read-and-index path, which owns the loud
/// error handling.
async fn mtime_matches_last_index(store: &LanceStore, corpus: &str, path: &Path) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let Ok(mtime_ms) = i64::try_from(since_epoch.as_millis()) else {
        return false;
    };
    let file_ref = canonicalize_or_passthrough(path);
    let Some(file_ref) = file_ref.as_path().to_str() else {
        return false;
    };
    match store.get_file_snapshot(corpus, file_ref).await {
        Ok(Some(snap)) => snap.mtime_ms == mtime_ms,
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                path = %path.display(),
                error = %e,
                "watcher: snapshot lookup failed; proceeding to reindex",
            );
            false
        }
    }
}

/// Find the baseline corpus that owns `path`: the deepest watched root that is
/// a prefix of `path` and whose membership rule accepts it. Deepest-first so a
/// nested corpus root wins over a parent root.
///
/// Membership mirrors `walker::scan`: a directory root accepts any descendant
/// the corpus' globs/exclude allow, while a **file-path** root (watched at its
/// parent) accepts only the exact declared file — never a sibling `.md` under
/// the same parent, which `scan` would never index.
fn owning_corpus<'r>(roots: &'r [WatchRoot], path: &Path) -> Option<&'r WatchRoot> {
    let mut best: Option<(usize, &WatchRoot)> = None;
    for root in roots {
        // notify emits canonical paths, so prefix-match against the resolved
        // watched root — comparing against the unresolved `watched` would miss
        // every event under a symlinked ancestor (e.g. macOS `/var`).
        if !path.starts_with(&root.canonical_watched) {
            continue;
        }
        match &root.canonical_file_root {
            // File-path root: only the exact declared file is a member. Compare
            // against the canonical declared file (notify's canonical event path
            // equals it for create/modify/delete alike — no per-event
            // canonicalize, which would fail on the delete case where the path
            // no longer exists).
            Some(file) if file != path => continue,
            // Directory root: honor the corpus' glob/exclude so a watched dir
            // that also holds non-corpus markdown doesn't reindex files the
            // corpus would never have scanned.
            None if ensure_corpus_allows_file(&root.corpus, path).is_err() => continue,
            _ => {}
        }
        let depth = root.canonical_watched.components().count();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, root));
        }
    }
    best.map(|(_, r)| r)
}

/// Canonical `file_ref` to prune for a now-deleted `path`, matching the key the
/// indexer wrote. `path` is notify's canonical event path; strip the canonical
/// watched prefix and re-root under `canonical_watched` (resolved at setup while
/// the root existed). Canonicalizing the now-absent path directly fails and
/// would fall through to the raw path, diverging from the stored key under a
/// symlinked-ancestor root and silently no-op'ing the prune. Falls back to
/// `canonicalize_or_passthrough(path)` if the path is somehow not under the
/// watched root (shouldn't happen: `owning_corpus` already required the prefix).
fn delete_file_ref(owner: &WatchRoot, path: &Path) -> hallouminate_domain::common::FileRef {
    match path.strip_prefix(&owner.canonical_watched) {
        Ok(rel) => hallouminate_domain::common::FileRef::new(owner.canonical_watched.join(rel)),
        Err(_) => canonicalize_or_passthrough(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(name: &str, root: &str, globs: &[&str]) -> CorpusConfig {
        CorpusConfig {
            name: name.into(),
            paths: vec![root.into()],
            globs: globs.iter().map(|g| g.to_string()).collect(),
            exclude: vec![],
            global: false,
        }
    }

    /// Test-only `WatchRoot` builder: canonical fields mirror their non-canonical
    /// inputs (no symlink), matching the common non-symlinked-root case. Pass the
    /// already-canonical paths here since `owning_corpus` matches notify's
    /// canonical event paths against the canonical fields.
    fn watch_root(watched: &str, corpus: CorpusConfig, file_root: Option<&str>) -> WatchRoot {
        let mode = if file_root.is_some() {
            RecursiveMode::NonRecursive
        } else {
            RecursiveMode::Recursive
        };
        WatchRoot {
            watched: PathBuf::from(watched),
            canonical_watched: PathBuf::from(watched),
            corpus,
            canonical_file_root: file_root.map(PathBuf::from),
            mode,
        }
    }

    fn name_of(owner: Option<&WatchRoot>) -> Option<String> {
        owner.map(|r| r.corpus.name.clone())
    }

    fn disabled_coalescer() -> FailureCoalescer {
        FailureCoalescer::new(Duration::ZERO, MAX_FAILURE_SIGNATURES)
    }

    /// Set `path`'s modified time exactly (nanosecond precision), for tests
    /// that pin the stage-1 mtime gate's compare against the stored snapshot.
    fn set_mtime(path: &Path, to: std::time::SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_times");
        file.set_times(std::fs::FileTimes::new().set_modified(to))
            .expect("set mtime");
    }

    /// A file-path corpus root is watched at its parent dir, but only the exact
    /// declared file is a member. A sibling `.md` under the same parent — which
    /// `walker::scan` would never index for a single-file root — must NOT be
    /// attributed to the corpus, even though the corpus glob (`**/*.md`) would
    /// otherwise match it. This is the watch.rs ownership-divergence fix.
    #[test]
    fn file_root_rejects_sibling_md_under_watched_parent() {
        let cfg = corpus("claude-config", "/home/u/.claude/CLAUDE.md", &["**/*.md"]);
        let roots = vec![watch_root(
            "/home/u/.claude",
            cfg.clone(),
            Some("/home/u/.claude/CLAUDE.md"),
        )];

        // The declared file is owned.
        assert_eq!(
            name_of(owning_corpus(
                &roots,
                Path::new("/home/u/.claude/CLAUDE.md")
            ))
            .as_deref(),
            Some("claude-config"),
            "the declared file must be a member"
        );
        // A sibling `.md` under the same parent is NOT owned, despite matching
        // the glob — scan would never index it for a single-file root.
        assert!(
            owning_corpus(&roots, Path::new("/home/u/.claude/RTK.md")).is_none(),
            "a sibling .md must not be attributed to a file-path corpus"
        );
    }

    /// The delete case (path no longer on disk) still resolves the owning
    /// corpus, since membership is a path compare, not a filesystem probe.
    #[test]
    fn file_root_owns_declared_file_even_when_absent() {
        let cfg = corpus("claude-config", "/home/u/.claude/CLAUDE.md", &["**/*.md"]);
        let roots = vec![watch_root(
            "/home/u/.claude",
            cfg,
            Some("/home/u/.claude/CLAUDE.md"),
        )];
        assert_eq!(
            name_of(owning_corpus(
                &roots,
                Path::new("/home/u/.claude/CLAUDE.md")
            ))
            .as_deref(),
            Some("claude-config"),
            "a deleted owned file must still resolve so its rows can be pruned"
        );
    }

    /// A directory root keeps glob-based membership: any descendant the corpus
    /// globs accept is owned. Guards against the fix over-restricting dir roots.
    #[test]
    fn dir_root_accepts_glob_matched_descendant() {
        let cfg = corpus("wiki", "/srv/wiki", &["**/*.md"]);
        let roots = vec![watch_root("/srv/wiki", cfg, None)];
        assert_eq!(
            name_of(owning_corpus(
                &roots,
                Path::new("/srv/wiki/topics/spice.md")
            ))
            .as_deref(),
            Some("wiki"),
            "a dir root must own any glob-matched descendant"
        );
        assert!(
            owning_corpus(&roots, Path::new("/srv/wiki/notes.txt")).is_none(),
            "a non-glob-matched file under a dir root is not owned"
        );
    }

    /// The delete-prune key must be rebuilt under the *canonical* watched root,
    /// not by canonicalizing the now-absent path (which fails and falls through
    /// to the raw path). notify emits the canonical event path; the indexer wrote
    /// its file_ref against the resolved ancestor too, so re-rooting the canonical
    /// tail under `canonical_watched` yields a key that matches. This pins the
    /// resolved-root behavior the spec flagged as the symlink open question.
    #[test]
    fn delete_file_ref_rebuilds_under_canonical_root() {
        // `watched` is the symlinked path the user configured; `canonical_watched`
        // is its resolved target. notify reports the deleted file under the
        // resolved root, matching what the indexer canonicalized while it existed.
        let owner = WatchRoot {
            watched: PathBuf::from("/link/wiki"),
            canonical_watched: PathBuf::from("/real/wiki"),
            corpus: corpus("wiki", "/link/wiki", &["**/*.md"]),
            canonical_file_root: None,
            mode: RecursiveMode::Recursive,
        };
        let deleted = Path::new("/real/wiki/topics/spice.md");
        assert_eq!(
            delete_file_ref(&owner, deleted).as_path(),
            Path::new("/real/wiki/topics/spice.md"),
            "prune key must re-root the canonical tail under the canonical (resolved) root, \
             matching the key the indexer wrote against the resolved ancestor"
        );
    }

    /// `owning_corpus` must resolve an event under a symlinked-ancestor root:
    /// notify reports `/real/wiki/...` while the configured `watched` is the
    /// symlinked `/link/wiki`. Matching against the non-canonical `watched`
    /// would miss the event entirely (the create/modify reindex never fires),
    /// which is the macOS `/var → /private/var` breakage this fix closes.
    #[test]
    fn owning_corpus_matches_canonical_event_path_under_symlinked_root() {
        let owner = WatchRoot {
            watched: PathBuf::from("/link/wiki"),
            canonical_watched: PathBuf::from("/real/wiki"),
            corpus: corpus("wiki", "/link/wiki", &["**/*.md"]),
            canonical_file_root: None,
            mode: RecursiveMode::Recursive,
        };
        let roots = vec![owner];
        assert_eq!(
            name_of(owning_corpus(
                &roots,
                Path::new("/real/wiki/topics/spice.md")
            ))
            .as_deref(),
            Some("wiki"),
            "a canonical event path under a symlinked root must resolve to its corpus"
        );
    }

    /// A directory corpus root is watched recursively (whole subtree), while a
    /// file-path corpus root is watched at its parent dir non-recursively — only
    /// the parent's direct entries, enough to catch edits + atomic-rename saves
    /// on the file without flooding the event pump with an unrelated subtree.
    #[test]
    fn recursion_mode_is_recursive_for_dir_root_nonrecursive_for_file_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir_root = tmp.path().join("wiki");
        std::fs::create_dir(&dir_root).expect("mkdir");
        let file_root = tmp.path().join("CLAUDE.md");
        std::fs::write(&file_root, "# config\n").expect("write file root");

        let dir_corpus = corpus("wiki", dir_root.to_str().unwrap(), &["**/*.md"]);
        let dir_wr =
            build_watch_root(&dir_corpus, dir_root.to_str().unwrap()).expect("dir root must build");
        assert_eq!(
            dir_wr.mode,
            RecursiveMode::Recursive,
            "a directory corpus root must be watched recursively"
        );
        assert_eq!(dir_wr.watched, dir_root, "a dir root is watched at itself");
        assert!(
            dir_wr.canonical_file_root.is_none(),
            "a dir root has no file-membership constraint"
        );

        let file_corpus = corpus("claude-config", file_root.to_str().unwrap(), &["**/*.md"]);
        let file_wr = build_watch_root(&file_corpus, file_root.to_str().unwrap())
            .expect("file root must build");
        assert_eq!(
            file_wr.mode,
            RecursiveMode::NonRecursive,
            "a file-path corpus root must be watched non-recursively at its parent"
        );
        assert_eq!(
            file_wr.watched,
            tmp.path(),
            "a file-path root is watched at its parent dir"
        );
        assert!(
            file_wr.canonical_file_root.is_some(),
            "a file-path root pins the exact declared file for membership"
        );
    }

    /// A not-yet-created root (neither dir nor file) yields no WatchRoot — a
    /// later boot picks it up once it exists.
    #[test]
    fn build_watch_root_skips_absent_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let absent = tmp.path().join("not-there");
        let cfg = corpus("ghost", absent.to_str().unwrap(), &["**/*.md"]);
        assert!(
            build_watch_root(&cfg, absent.to_str().unwrap()).is_none(),
            "an absent root must not produce a WatchRoot"
        );
    }

    /// Plain (non-symlinked) root: `canonical_watched == watched`, so the key
    /// is the path itself. Guards against the rebuild altering the common case.
    #[test]
    fn delete_file_ref_is_identity_for_plain_root() {
        let owner = watch_root("/srv/wiki", corpus("wiki", "/srv/wiki", &["**/*.md"]), None);
        assert_eq!(
            delete_file_ref(&owner, Path::new("/srv/wiki/topics/spice.md")).as_path(),
            Path::new("/srv/wiki/topics/spice.md"),
            "a non-symlinked root must prune the path unchanged"
        );
    }

    /// ADR-003 regression: the delete/prune branch of `handle_changed_path`
    /// acquired no connection guard and never touched the activity clock, so
    /// idle-exit could tear down the daemon (and release the single-instance
    /// flock) mid-write. Batch processing must hold a guard for the whole
    /// batch and stamp the clock afterward, exactly like `catch_up_index`
    /// (dispatch.rs) and `handle_connection` (server.rs).
    #[tokio::test]
    async fn process_change_batch_touches_activity_after_a_stale_clock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = crate::config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        // Sentinel no stamp can produce: the activity clock stores monotonic
        // seconds since process start, so a fresh stamp is always small.
        state.set_last_activity_secs_for_test(u64::MAX);

        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]),
            None,
        )];
        // Never created on disk: `handle_changed_path` takes the delete/prune
        // branch — the branch that acquired no guard and stamped no clock
        // before the fix.
        let deleted = corpus_dir.join("gone.md");

        let mut failures = disabled_coalescer();
        process_change_batch(&state, &roots, vec![deleted], &mut failures).await;
        assert_ne!(
            state.last_activity_secs(),
            u64::MAX,
            "batch processing must stamp the activity clock so idle-exit does \
             not fire immediately after a delete-branch write",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_changed_path_records_watcher_reindex_counters() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = crate::config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");

        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let corpus_dir = corpus_dir.canonicalize().expect("canonicalize corpus dir");
        let note = corpus_dir.join("note.md");
        std::fs::write(&note, "# Note\n\nbody\n").expect("write note");

        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]),
            None,
        )];
        let mut failures = disabled_coalescer();

        handle_changed_path(&state, &roots, &note, &mut failures).await;
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 1, 0),
            "first reindex of a new file must count as a real (non-noop) reindex",
        );

        // Same file, unchanged content and mtime: the stage-1 mtime gate
        // (ADR daemon-rework-003) sheds the event before any read — a skip,
        // not a reindex, so no counter moves.
        handle_changed_path(&state, &roots, &note, &mut failures).await;
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 1, 0),
            "an event whose mtime matches the stored snapshot must be skipped, \
             not counted as a reindex",
        );

        // mtime moved but content did not: the gate lets it through and the
        // indexer takes the hash fast path, upserting nothing — a noop
        // reindex.
        let bumped = std::fs::metadata(&note)
            .expect("stat note")
            .modified()
            .expect("note mtime")
            + Duration::from_millis(10);
        set_mtime(&note, bumped);
        handle_changed_path(&state, &roots, &note, &mut failures).await;
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 2, 1),
            "reindexing unchanged content must count as a noop reindex",
        );
    }

    /// Acceptance (ADR daemon-rework-003 stage 1): WHEN a watched file emits
    /// an event but its mtime equals the stored snapshot, the watcher SHALL
    /// NOT read the file's content. Encoded by rewriting the content *behind*
    /// a restored mtime: only a content read could notice the rewrite, so a
    /// gate regression reaches the indexer's same-mtime hash check, reindexes
    /// the new content, and fails both assertions below.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_mtime_event_skips_without_reading_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = crate::config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");

        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let corpus_dir = corpus_dir.canonicalize().expect("canonicalize corpus dir");
        let note = corpus_dir.join("note.md");
        std::fs::write(&note, "# Note\n\nbody\n").expect("write note");
        let indexed_mtime = std::fs::metadata(&note)
            .expect("stat note")
            .modified()
            .expect("note mtime");

        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]),
            None,
        )];
        let mut failures = disabled_coalescer();
        handle_changed_path(&state, &roots, &note, &mut failures).await;

        let file_ref = canonicalize_or_passthrough(&note)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();
        let indexed = state
            .store()
            .get_file_snapshot("wiki", &file_ref)
            .await
            .expect("snapshot query")
            .expect("initial index must store a snapshot");

        // Rewrite the content, then put the mtime back: from the outside the
        // file looks untouched, and only a content read could tell otherwise.
        std::fs::write(&note, "# Note\n\nrewritten body the gate must not see\n")
            .expect("rewrite note");
        set_mtime(&note, indexed_mtime);

        handle_changed_path(&state, &roots, &note, &mut failures).await;

        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 1, 0),
            "the skip must not count as a reindex",
        );
        let after = state
            .store()
            .get_file_snapshot("wiki", &file_ref)
            .await
            .expect("snapshot query")
            .expect("snapshot must survive the skip");
        assert_eq!(
            after.content_hash, indexed.content_hash,
            "unchanged mtime must skip without reading content — the stored \
             hash still describes the pre-rewrite content",
        );
    }

    /// Security regression: the watcher must read a changed file's content
    /// through a **no-follow** filesystem resolution, so a corpus contributor
    /// cannot make the daemon index — and later serve back through Ground —
    /// content from **outside** the corpus root by pointing an in-corpus path
    /// at an external file via a symlink.
    ///
    /// The fix collapses validation and content-read into one atomic no-follow
    /// read (`sandbox::read_no_follow_with_mtime`) instead of a symlink *check*
    /// followed by a separate ambient re-read of the same path — the gap a
    /// symlink swapped in between the two could race (TOCTOU). This test guards
    /// the resulting property: a symlinked leaf whose target lives outside the
    /// watched root is rejected, and the outside content never reaches the
    /// store. A regression to an ambient read (which follows symlinks) would
    /// index the secret and fail the final assertion.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_never_indexes_content_through_a_symlink_out_of_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = crate::config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");

        // A real in-root corpus with one genuine markdown file. Canonicalize
        // the root so the event path matches `canonical_watched` (tempdirs can
        // symlink an ancestor, e.g. macOS /var → /private/var).
        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let corpus_dir = corpus_dir.canonicalize().expect("canonicalize corpus dir");
        let note = corpus_dir.join("note.md");
        std::fs::write(&note, "# In-corpus\n\nbenign in-corpus content\n").expect("write note");

        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]),
            None,
        )];
        let file_ref = canonicalize_or_passthrough(&note)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        // Baseline: the real file indexes and lands a snapshot row.
        let mut failures = disabled_coalescer();
        handle_changed_path(&state, &roots, &note, &mut failures).await;
        let good = state
            .store()
            .get_file_snapshot("wiki", &file_ref)
            .await
            .expect("snapshot query")
            .expect("a real in-root file must be indexed");

        // Attack: replace the leaf with a symlink to a secret file OUTSIDE the
        // watched root, then trigger a reindex of the same in-corpus path.
        let secret_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&secret_dir).expect("mkdir outside");
        let secret = secret_dir.join("secret.md");
        std::fs::write(&secret, "# Secret\n\nSECRET_OUTSIDE_CONTENT\n").expect("write secret");
        std::fs::remove_file(&note).expect("rm note");
        std::os::unix::fs::symlink(&secret, &note).expect("symlink note -> secret");

        handle_changed_path(&state, &roots, &note, &mut failures).await;

        // The reindex through the symlink must have been rejected. Vulnerable
        // code would `canonicalize` the in-corpus path (following the symlink)
        // and index the outside content under the *resolved* key — so assert
        // the secret's own canonical path has no snapshot row anywhere in the
        // corpus. (Checking only the note's key would miss this: the follow
        // indexes under the target's key, not the link's.)
        let secret_ref = canonicalize_or_passthrough(&secret)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            state
                .store()
                .get_file_snapshot("wiki", &secret_ref)
                .await
                .expect("snapshot query")
                .is_none(),
            "the outside secret file's content must never be indexed — the \
             watcher must not follow an in-corpus symlink to a target outside \
             the watched root",
        );

        // And the in-corpus key must still hold the real file's content,
        // untouched by the rejected reindex.
        let after = state
            .store()
            .get_file_snapshot("wiki", &file_ref)
            .await
            .expect("snapshot query")
            .expect("the snapshot must survive a rejected symlink reindex");
        assert_eq!(
            after.content_hash, good.content_hash,
            "the store must still hold the real in-corpus file's content, never \
             the outside secret's",
        );
    }

    /// `record_pending` coalesces duplicate paths within one batch and across
    /// multiple batches recorded before a drain — the fix for the unbounded
    /// channel: paths accumulate in a bounded shared set instead of every
    /// debounced batch queuing separately.
    #[test]
    fn record_pending_coalesces_across_batches() {
        let pending: std::sync::Mutex<std::collections::HashSet<PathBuf>> =
            std::sync::Mutex::new(std::collections::HashSet::new());
        let a = PathBuf::from("/srv/wiki/a.md");
        let b = PathBuf::from("/srv/wiki/b.md");
        let ignored = PathBuf::from("/srv/wiki/notes.docx");

        let batch1 = vec![
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(a.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(a.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(ignored.clone()),
                std::time::Instant::now(),
            ),
        ];
        let batch2 = vec![notify_debouncer_full::DebouncedEvent::new(
            notify::Event::new(notify::EventKind::Any).add_path(b.clone()),
            std::time::Instant::now(),
        )];

        record_pending(&pending, &batch1);
        record_pending(&pending, &batch2);

        let drained: std::collections::HashSet<PathBuf> =
            pending.lock().expect("pending mutex").drain().collect();
        assert_eq!(
            drained,
            std::collections::HashSet::from([a, b]),
            "pending must coalesce the duplicate .md path within a batch and \
             across batches, while dropping the known-but-unsupported .docx path"
        );
    }

    /// `record_pending` must admit every extension `format_from_extension`
    /// (the indexer's own admission rule) accepts, case-insensitively — not a
    /// second, narrower `.md`-only rule the watcher used to maintain
    /// separately from `domain::indexer::format`. An uppercase `.MD` and a
    /// `.csv` (spreadsheet) must both be admitted; a known-unsupported `.docx`
    /// must still be dropped.
    #[test]
    fn record_pending_admits_every_indexer_supported_extension() {
        let pending: std::sync::Mutex<std::collections::HashSet<PathBuf>> =
            std::sync::Mutex::new(std::collections::HashSet::new());
        let uppercase_md = PathBuf::from("/srv/wiki/README.MD");
        let csv = PathBuf::from("/srv/wiki/data.csv");
        let unsupported = PathBuf::from("/srv/wiki/notes.docx");

        let batch = vec![
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(uppercase_md.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(csv.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Any).add_path(unsupported.clone()),
                std::time::Instant::now(),
            ),
        ];

        record_pending(&pending, &batch);

        let drained: std::collections::HashSet<PathBuf> =
            pending.lock().expect("pending mutex").drain().collect();
        assert_eq!(
            drained,
            std::collections::HashSet::from([uppercase_md, csv]),
            "an uppercase .MD and a .csv must be admitted (matching \
             format_from_extension), while a known-unsupported .docx is dropped"
        );
    }

    /// `record_pending` must never schedule a reindex for an `Access` event
    /// — the fix for the self-sustaining watcher loop: `notify` 8.x's inotify
    /// backend subscribes `WatchMask::OPEN`, so a plain read-open (including
    /// the read `handle_changed_path` performs while reindexing) surfaces as
    /// `EventKind::Access(_)`. Admitting Access events would make every
    /// reindex re-trigger itself.
    #[test]
    fn record_pending_drops_access_events() {
        let pending: std::sync::Mutex<std::collections::HashSet<PathBuf>> =
            std::sync::Mutex::new(std::collections::HashSet::new());
        let read = PathBuf::from("/srv/wiki/read.md");

        let batch = vec![notify_debouncer_full::DebouncedEvent::new(
            notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Open(
                notify::event::AccessMode::Any,
            )))
            .add_path(read.clone()),
            std::time::Instant::now(),
        )];

        record_pending(&pending, &batch);

        let drained: std::collections::HashSet<PathBuf> =
            pending.lock().expect("pending mutex").drain().collect();
        assert!(
            drained.is_empty(),
            "an Access(Open) event must never schedule a reindex — it is what \
             drives the watcher's self-feeding loop, not a real change"
        );
    }

    /// Companion to `record_pending_drops_access_events`: every mutation kind
    /// — create, data/metadata modify, a rename's `Name(RenameMode::Both)`,
    /// and remove — must still be admitted, so the Access filter above only
    /// narrows admission and does not regress real change detection.
    #[test]
    fn record_pending_admits_mutation_kinds() {
        let pending: std::sync::Mutex<std::collections::HashSet<PathBuf>> =
            std::sync::Mutex::new(std::collections::HashSet::new());
        let created = PathBuf::from("/srv/wiki/created.md");
        let modified = PathBuf::from("/srv/wiki/modified.md");
        let renamed = PathBuf::from("/srv/wiki/renamed.md");
        let removed = PathBuf::from("/srv/wiki/removed.md");

        let batch = vec![
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                    .add_path(created.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )))
                .add_path(modified.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::Both,
                )))
                .add_path(renamed.clone()),
                std::time::Instant::now(),
            ),
            notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                    .add_path(removed.clone()),
                std::time::Instant::now(),
            ),
        ];

        record_pending(&pending, &batch);

        let drained: std::collections::HashSet<PathBuf> =
            pending.lock().expect("pending mutex").drain().collect();
        assert_eq!(
            drained,
            std::collections::HashSet::from([created, modified, renamed, removed]),
            "create, modify (data + rename), and remove events must all still \
             schedule a reindex"
        );
    }

    #[test]
    fn failure_coalescer_reports_suppresses_reminds_and_distinguishes() {
        let start = Instant::now();
        let path = Path::new("/srv/wiki/note.md");
        let other_path = Path::new("/srv/wiki/other.md");
        let mut coalescer = FailureCoalescer::new(Duration::from_secs(60), 8);

        assert_eq!(
            coalescer.record(path, "missing fragment", start),
            FailureDecision::First
        );
        assert_eq!(
            coalescer.record(path, "missing fragment", start + Duration::from_secs(10)),
            FailureDecision::Suppress
        );
        assert_eq!(
            coalescer.record(path, "missing fragment", start + Duration::from_secs(20)),
            FailureDecision::Suppress
        );
        assert_eq!(
            coalescer.record(path, "missing fragment", start + Duration::from_secs(60)),
            FailureDecision::Reminder { suppressed: 2 }
        );
        assert_eq!(
            coalescer.record(path, "different error", start + Duration::from_secs(61)),
            FailureDecision::First
        );
        assert_eq!(
            coalescer.record(
                other_path,
                "missing fragment",
                start + Duration::from_secs(61)
            ),
            FailureDecision::First
        );
    }

    #[test]
    fn failure_coalescer_disabled_reports_every_occurrence() {
        let start = Instant::now();
        let path = Path::new("/srv/wiki/note.md");
        let mut coalescer = FailureCoalescer::new(Duration::ZERO, 1);

        assert_eq!(
            coalescer.record(path, "missing fragment", start),
            FailureDecision::First
        );
        assert_eq!(
            coalescer.record(path, "missing fragment", start),
            FailureDecision::First
        );
        assert!(coalescer.states.is_empty());
    }

    #[test]
    fn failure_coalescer_evicts_the_oldest_signature_at_capacity() {
        let start = Instant::now();
        let mut coalescer = FailureCoalescer::new(Duration::from_secs(60), 2);
        assert_eq!(
            coalescer.record(Path::new("/a"), "a", start),
            FailureDecision::First
        );
        assert_eq!(
            coalescer.record(Path::new("/b"), "b", start + Duration::from_secs(1)),
            FailureDecision::First
        );
        assert_eq!(
            coalescer.record(Path::new("/c"), "c", start + Duration::from_secs(2)),
            FailureDecision::First
        );
        assert_eq!(coalescer.states.len(), 2);
        assert_eq!(
            coalescer.record(Path::new("/a"), "a", start + Duration::from_secs(3)),
            FailureDecision::First
        );
    }
}
