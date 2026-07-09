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

use super::dispatch::index_single_file;
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

    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let mut debouncer = match new_debouncer(debounce, None, move |res| {
        // The debouncer worker thread calls this on each debounced batch.
        // Forward to the async side; a closed receiver (daemon shutting down)
        // is benign.
        let _ = tx.send(res);
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
        // a blocking thread per batch; simplest correct shape that respects
        // the shutdown token.
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        loop {
            let rx_recv = rx.clone();
            let next = tokio::select! {
                _ = shutdown.cancelled() => break,
                got = tokio::task::spawn_blocking(move || {
                    rx_recv.lock().expect("watch rx mutex").recv()
                }) => got,
            };
            let batch = match next {
                Ok(Ok(res)) => res,
                // Channel disconnected (debouncer dropped) or join error:
                // nothing more will arrive, so end the pump.
                _ => break,
            };
            let events = match batch {
                Ok(events) => events,
                Err(errors) => {
                    for err in errors {
                        tracing::warn!(
                            target: "hallouminate::daemon",
                            error = %err,
                            "watcher: notify backend error",
                        );
                    }
                    continue;
                }
            };
            // Collect distinct affected paths so a debounced batch that
            // touches one file many times reindexes it once.
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            let mut paths: Vec<PathBuf> = Vec::new();
            for event in events {
                for path in &event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    if seen.insert(path.clone()) {
                        paths.push(path.clone());
                    }
                }
            }
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
        match index_single_file(&store, embedder_dyn, &registry, corpus, path).await {
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
        state.set_last_activity_secs_for_test(1);

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

        assert!(
            state.should_idle_exit(300),
            "sanity: stale clock with no connections must be idle-eligible",
        );
        process_change_batch(&state, &roots, vec![deleted]).await;
        assert!(
            !state.should_idle_exit(300),
            "batch processing must stamp the activity clock so idle-exit does \
             not fire immediately after a delete-branch write",
        );
    }
}
