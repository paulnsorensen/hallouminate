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

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use crate::domain::common::{CorpusConfig, canonicalize_or_passthrough, expand_tilde};
use crate::domain::corpus::sandbox::ensure_corpus_allows_file;

use super::dispatch::index_single_file_with_content;
use super::state::DaemonState;

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

/// Owns the background debouncer + event-pump task. Dropping it stops the
/// watcher (the debouncer's worker thread joins on drop; aborting the task
/// drops the event receiver so the thread's send fails and it exits).
pub struct WatcherHandle {
    _task: tokio::task::JoinHandle<()>,
    // The debouncer must outlive the watch session; held here so its worker
    // thread keeps running until this handle drops.
    _debouncer: Box<dyn std::any::Any + Send>,
}

/// Watch the baseline corpora roots and spawn a task that reindexes changed
/// markdown files (debounced by `cfg.watch.debounce_ms`). Returns `None` when
/// there are no watchable roots or the watcher backend fails to initialize —
/// the daemon still serves; auto-reindex is simply off.
pub fn spawn_corpus_watcher(state: &DaemonState) -> Option<WatcherHandle> {
    let cfg = state.baseline();
    let debounce = Duration::from_millis(cfg.watch.debounce_ms);
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

    let pending_for_debouncer = pending.clone();
    let mut debouncer = match new_debouncer(debounce, None, move |res: DebounceEventResult| {
        // The debouncer worker thread calls this on each debounced batch.
        match res {
            Ok(events) => {
                record_pending(&pending_for_debouncer, &events);
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
            let paths: Vec<PathBuf> = {
                let mut set = pending.lock().expect("watch pending-paths mutex");
                set.drain().collect()
            };
            if !paths.is_empty() {
                process_change_batch(&state, &roots, paths).await;
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
fn record_pending(
    pending: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    events: &[notify_debouncer_full::DebouncedEvent],
) {
    let mut set = pending.lock().expect("watch pending-paths mutex");
    for event in events {
        for path in &event.paths {
            // Extension-only, matching `format_from_extension`'s classification
            // without reading bytes: a deleted path no longer exists to sniff, and
            // reading an existing one just to decide admission would duplicate the
            // indexer's own read. Extensionless files fall through to `None` here
            // (never admitted) rather than risking a second, diverging extension
            // rule from the one `domain::indexer::format` owns.
            if !matches!(
                crate::domain::indexer::format_from_extension(path),
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
async fn process_change_batch(state: &DaemonState, roots: &[WatchRoot], paths: Vec<PathBuf>) {
    let _conn = state.enter_connection();
    for path in &paths {
        handle_changed_path(state, roots, path).await;
    }
    state.touch_activity();
}

/// Reindex (or prune) one changed markdown path against whichever baseline
/// corpus owns it. Skips paths that no baseline corpus accepts.
async fn handle_changed_path(state: &DaemonState, roots: &[WatchRoot], path: &Path) {
    let Some(owner) = owning_corpus(roots, path) else {
        return;
    };
    let corpus = &owner.corpus;
    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(target: "hallouminate::daemon", error = %e, "watcher: lock failed");
            return;
        }
    };
    let exists = path.is_file();
    let store = state.store();
    if exists {
        // `path.is_file()` above follows symlinks, so a symlinked leaf whose
        // target is a regular file elsewhere still reaches here. A single
        // no-follow read below both rejects the symlink and supplies the
        // content, closing the TOCTOU gap a separate check-then-read would
        // leave open to a symlink swapped in between the two calls.
        let relative = path
            .strip_prefix(&owner.canonical_watched)
            .expect("owning_corpus guarantees path starts_with canonical_watched");
        let (bytes, mtime) = match crate::domain::corpus::sandbox::read_no_follow_with_mtime(
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
        let mut embedder = if state.embeddings_enabled() {
            match state.embedder().await {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::warn!(target: "hallouminate::daemon", error = %e, "watcher: embedder unavailable; skipping reindex");
                    return;
                }
            }
        } else {
            None
        };
        let embedder_dyn: Option<&mut dyn crate::domain::embeddings::EmbedBatch> = embedder
            .as_mut()
            .map(|g| &mut **g as &mut dyn crate::domain::embeddings::EmbedBatch);
        match index_single_file_with_content(
            &store,
            embedder_dyn,
            &registry,
            corpus,
            path,
            &bytes,
            mtime,
        )
        .await
        {
            Ok(stats) => tracing::debug!(
                target: "hallouminate::daemon",
                corpus = %corpus.name,
                path = %path.display(),
                upserted = stats.files_upserted,
                "watcher: reindexed changed file",
            ),
            Err(e) => tracing::warn!(
                target: "hallouminate::daemon",
                path = %path.display(),
                error = %e,
                "watcher: reindex failed",
            ),
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
fn delete_file_ref(owner: &WatchRoot, path: &Path) -> crate::domain::common::FileRef {
    match path.strip_prefix(&owner.canonical_watched) {
        Ok(rel) => crate::domain::common::FileRef::new(owner.canonical_watched.join(rel)),
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
        let mut cfg = crate::app::config::Config::default();
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

        process_change_batch(&state, &roots, vec![deleted]).await;
        assert_ne!(
            state.last_activity_secs(),
            u64::MAX,
            "batch processing must stamp the activity clock so idle-exit does \
             not fire immediately after a delete-branch write",
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
        let mut cfg = crate::app::config::Config::default();
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
        handle_changed_path(&state, &roots, &note).await;
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

        handle_changed_path(&state, &roots, &note).await;

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
}
