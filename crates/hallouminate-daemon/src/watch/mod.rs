//! Filesystem watcher that incrementally re-indexes registered corpus roots.
//!
//! Wires the otherwise-dead `[watch] debounce_ms` knob: `notify` +
//! `notify-debouncer-full` watch every root the `WatchRegistry` (`registry`
//! submodule) knows about, and on a debounced change the daemon reindexes
//! just the affected markdown file (`index_single_file`) or prunes its rows
//! on delete. The debounce window is `cfg.watch.debounce_ms`.
//!
//! Live registration: `spawn_corpus_watcher` seeds the boot baseline's
//! `[[corpus]]`/`[[repository]]` roots into `state.watch_registry()`, then
//! the pump task reconciles against that registry every loop iteration —
//! new registrations get `debouncer.watch()`'d when available, or are marked
//! degraded, and get catch-up'd without a watcher restart.
//! `register_runtime_corpora` (called from `dispatch::handle_ground`) registers
//! request-resolved repo-layer corpora, and `reload_repo_layer` (driven by the
//! reconcile tick) re-resolves and replaces a repo-layer source's registrations
//! as its config changes.
//!
//! Concurrency (spec Risk): every reindex takes the same per-corpus lock +
//! global write-lane (`acquire_mutation_guard`) that `handle_index` /
//! `handle_add_markdown` take, so a watch-triggered reindex never races the
//! daemon's own writes.

mod registry;

pub(crate) use registry::{ConfigSource, WatchRegistry};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, NoCache, new_debouncer_opt};

use hallouminate_adapters::LanceStore;
use hallouminate_domain::common::{
    CorpusConfig, CorpusKey, canonicalize_or_passthrough, expand_tilde,
};
use hallouminate_domain::corpus::ensure_corpus_allows_relative;

use super::churn::{ChurnTracker, ReindexEffect};
use super::dispatch::index_single_file_with_content;
use super::ladder::LadderOutcome;
use super::state::{DaemonState, WorkClass};
use registry::RegistrationId;
use tokio_util::task::TaskTracker;

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
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WatchRoot {
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

/// Coalesces `reload_repo_layer` failures per repo-layer config path: an
/// identical error logs once at `warn` and stays at `debug` on repeat, a
/// changed error logs `warn` again, and a success after a recorded failure
/// clears the entry so the caller can log a recovery transition.
struct ReloadFailureMemo(HashMap<PathBuf, String>);

impl ReloadFailureMemo {
    fn new() -> Self {
        Self(HashMap::new())
    }

    /// Records `path` failing with `error`. Returns `true` when this is a
    /// new failure (first occurrence or changed message) that should log at
    /// `warn`, `false` for a repeat that should stay at `debug`.
    fn record_failure(&mut self, path: &Path, error: &str) -> bool {
        if self.0.get(path).map(String::as_str) == Some(error) {
            return false;
        }
        self.0.insert(path.to_path_buf(), error.to_string());
        true
    }

    /// Clears `path`'s failure record. Returns `true` when it had one,
    /// meaning this success is a recovery worth logging.
    fn record_success(&mut self, path: &Path) -> bool {
        self.0.remove(path).is_some()
    }
}

/// Owns the background debouncer and event-pump task.
///
/// Call [`WatcherHandle::abort`] or [`WatcherHandle::join`] to stop the watcher
/// and release its physical `notify` watches. Dropping the handle alone detaches
/// the task and does not stop it.
pub(crate) struct WatcherHandle {
    task: tokio::task::JoinHandle<()>,
    tracker: TaskTracker,
}

impl WatcherHandle {
    /// Await the pump task; used by the supervisor factory so a watcher
    /// restart rebuilds the whole debouncer + pump pair.
    pub(crate) async fn join(self) {
        let result = self.task.await;
        self.tracker.close();
        self.tracker.wait().await;
        if let Err(join_err) = result {
            tracing::error!(target: "hallouminate::daemon", error = %join_err, "watcher: pump task ended abnormally");
            if join_err.is_panic() {
                std::panic::resume_unwind(join_err.into_panic());
            }
        }
    }

    /// Abort and await the watcher task so its debouncer releases every watch.
    pub(crate) async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
        self.tracker.close();
        self.tracker.wait().await;
    }

    /// Whether the pump task has already finished (aborted or panicked).
    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

fn reload_repo_layer(
    state: &DaemonState,
    path: &Path,
    tracker: &TaskTracker,
    failures: &mut ReloadFailureMemo,
) {
    macro_rules! report_failure {
        ($error:expr, $message:literal) => {{
            let error = $error.to_string();
            if failures.record_failure(path, &error) {
                tracing::warn!(target: "hallouminate::daemon", path = %path.display(), error = %error, $message);
            } else {
                tracing::debug!(target: "hallouminate::daemon", path = %path.display(), error = %error, $message);
            }
        }};
    }
    let repo = match hallouminate_config::load_repo_layer(path) {
        Ok(config) => config,
        Err(error) => {
            report_failure!(
                error,
                "watcher: repo-layer reload failed; retaining registrations for retry"
            );
            return;
        }
    };
    let effective = match hallouminate_config::merge_layers(state.baseline(), &repo) {
        Ok(config) => config,
        Err(error) => {
            report_failure!(
                error,
                "watcher: repo-layer validation failed; retaining registrations for retry"
            );
            return;
        }
    };
    let corpora = match effective.effective_corpora() {
        Ok(corpora) => corpora,
        Err(error) => {
            report_failure!(
                error,
                "watcher: repo-layer corpus validation failed; retaining registrations for retry"
            );
            return;
        }
    };
    let source = registry::ConfigSource::RepoLayer(path.to_path_buf());
    let result = state.watch_registry().replace_source(
        source,
        corpora,
        std::sync::Arc::new(effective),
        watch_roots_for,
    );
    match result {
        Ok(retired) => {
            if failures.record_success(path) {
                tracing::info!(target: "hallouminate::daemon", path = %path.display(), "watcher: repo-layer reload recovered");
            }
            for registration in retired {
                let state = state.clone();
                tracker.spawn(async move {
                    let _conn = state.enter_connection(WorkClass::Internal);
                    cleanup_retired_registration(&state, registration).await;
                    state.touch_activity(WorkClass::Internal);
                });
            }
        }
        Err(error) => {
            report_failure!(
                error,
                "watcher: repo-layer reload could not replace registrations; retaining registrations for retry"
            );
        }
    }
}

async fn cleanup_retired_registration(
    state: &DaemonState,
    registration: registry::RetiredRegistration,
) {
    let root_path = registration.root.clone();
    let retired = match tokio::task::spawn_blocking(move || {
        hallouminate_domain::common::retired_roots(std::slice::from_ref(&registration.root))
            .into_iter()
            .next()
            .map(|root| (root, registration.cfg))
    })
    .await
    {
        Ok(Some(retired)) => retired,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(target: "hallouminate::daemon", root = %root_path.display(), error = %error, "watcher: retired-root check failed; retaining storage rows");
            return;
        }
    };
    let (root, cfg) = retired;
    let resources = match state.resources_for(&cfg).await {
        Ok(resources) => resources,
        Err(error) => {
            tracing::warn!(target: "hallouminate::daemon", root = %root.as_path().display(), error = %error, "watcher: retired-root storage access failed; retaining storage rows");
            return;
        }
    };
    match resources.store.delete_root(&root).await {
        Ok(_) => {
            tracing::info!(target: "hallouminate::daemon", root = %root.as_path().display(), ground_dir = %cfg.storage.ground_dir, "watcher: retired root rows deleted");
        }
        Err(error) => {
            tracing::warn!(target: "hallouminate::daemon", root = %root.as_path().display(), error = %error, "watcher: retired-root cleanup failed; retaining retry work");
        }
    }
}

/// Bundles the watcher pump's live-reconcile state: the optional
/// debouncer, the set of paths currently `debouncer.watch()`'d, and the
/// flattened current root list used by `process_change_batch`.
struct PumpState {
    debouncer: Option<notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>>,
    installed: HashMap<PathBuf, RecursiveMode>,
    roots: Vec<WatchRoot>,
    /// Registrations already marked `Degraded` because the native backend
    /// is unavailable, so a later reconcile pass can skip the registry
    /// lock for them entirely instead of rewriting an identical
    /// observation every tick. Pruned to the current snapshot each pass.
    degraded_backend: HashSet<registry::RegistrationId>,
}

impl PumpState {
    /// Reconcile the live debouncer against the registry's current
    /// registrations: newly-registered roots get `debouncer.watch()`'d when
    /// available (or marked degraded when unavailable), any registration
    /// whose catch-up hasn't started gets one spawned, and `self.roots` is
    /// refreshed to the flattened current root list for the caller's
    /// subsequent `process_change_batch` call.
    fn reconcile(&mut self, state: &DaemonState, tracker: &TaskTracker) {
        let registry = state.watch_registry();
        registry.refresh_roots(watch_roots_for);
        let snapshot = registry.snapshot_roots();
        let desired = Self::desired_watches(&snapshot);
        self.drop_obsolete_watches(&desired);
        self.roots = snapshot.iter().map(|(_, r)| r.clone()).collect();
        self.install_or_degrade_watches(registry, &snapshot, desired);
        Self::admit_next_catch_up(registry, state, tracker);
    }

    /// Union of every distinct watched path across `snapshot`, recursive if
    /// any registration sharing that path wants recursion.
    fn desired_watches(
        snapshot: &[(registry::RegistrationId, WatchRoot)],
    ) -> HashMap<PathBuf, RecursiveMode> {
        let mut desired = HashMap::new();
        for (_, root) in snapshot {
            desired
                .entry(root.watched.clone())
                .and_modify(|mode| {
                    if root.mode == RecursiveMode::Recursive {
                        *mode = RecursiveMode::Recursive;
                    }
                })
                .or_insert(root.mode);
        }
        desired
    }

    /// Removes any currently-installed watch whose path or mode no longer
    /// matches `desired`.
    fn drop_obsolete_watches(&mut self, desired: &HashMap<PathBuf, RecursiveMode>) {
        let obsolete: Vec<PathBuf> = self
            .installed
            .iter()
            .filter(|(path, mode)| desired.get(*path) != Some(mode))
            .map(|(path, _)| path.clone())
            .collect();
        for path in obsolete {
            if let Some(debouncer) = self.debouncer.as_mut()
                && let Err(error) = debouncer.unwatch(&path)
            {
                tracing::debug!(target: "hallouminate::daemon", path = %path.display(), error = %error, "watcher: obsolete watch removal failed");
            }
            self.installed.remove(&path);
        }
    }

    /// Installs every not-yet-installed desired watch, or marks its
    /// registrations degraded when the native backend is unavailable or the
    /// install itself fails.
    fn install_or_degrade_watches(
        &mut self,
        registry: &registry::WatchRegistry,
        snapshot: &[(registry::RegistrationId, WatchRoot)],
        desired: HashMap<PathBuf, RecursiveMode>,
    ) {
        let live: HashSet<&registry::RegistrationId> = snapshot.iter().map(|(id, _)| id).collect();
        self.degraded_backend.retain(|id| live.contains(id));
        for (path, mode) in desired {
            if self.installed.contains_key(&path) {
                continue;
            }
            let ids: Vec<_> = snapshot
                .iter()
                .filter(|(_, root)| root.watched == path)
                .map(|(id, _)| id)
                .collect();
            let Some(debouncer) = self.debouncer.as_mut() else {
                for id in ids {
                    if self.degraded_backend.insert(id.clone()) {
                        registry.mark_backend_unavailable(id);
                        tracing::warn!(
                            target: "hallouminate::daemon",
                            root = %path.display(),
                            "watcher: native backend unavailable; root observed by periodic reconciliation only",
                        );
                    }
                }
                continue;
            };
            let was_degraded = ids.iter().any(|id| {
                matches!(
                    registry.observation(id),
                    Some(registry::Observation::Degraded { .. })
                )
            });
            match debouncer.watch(&path, mode) {
                Ok(()) => {
                    self.installed.insert(path.clone(), mode);
                    for id in ids {
                        registry.mark_watched(id);
                    }
                    if was_degraded {
                        tracing::info!(target: "hallouminate::daemon", root = %path.display(), "watcher: watch install recovered");
                    }
                }
                Err(error) => {
                    if !was_degraded {
                        tracing::warn!(target: "hallouminate::daemon", root = %path.display(), error = %error, "watcher: watch install failed; reconciliation continues");
                    }
                    for id in ids {
                        registry.mark_degraded(id, error.to_string());
                    }
                }
            }
        }
    }

    /// Admits the next queued catch-up pass, if the registry has one ready.
    fn admit_next_catch_up(
        registry: &registry::WatchRegistry,
        state: &DaemonState,
        tracker: &TaskTracker,
    ) {
        if let Some(id) = registry.begin_next_catch_up() {
            spawn_registration_catch_up(state.clone(), id, tracker);
        }
    }
}

/// Runs one registration's catch-up pass and records the outcome;
/// `finish_catch_up` re-queues the registration when events arrived
/// mid-flight.
fn spawn_registration_catch_up(state: DaemonState, id: RegistrationId, tracker: &TaskTracker) {
    tracker.spawn(async move {
        let _conn = state.enter_connection(WorkClass::Internal);
        let Some((corpus, cfg)) = state.watch_registry().config_for(&id) else {
            state.watch_registry().finish_catch_up(
                &id,
                Err("registration vanished before catch-up could start".into()),
            );
            state.touch_activity(WorkClass::Internal);
            return;
        };
        // Hold the per-corpus lock for the whole scan-through-apply span so
        // no other writer to this corpus interleaves; the global write-lane
        // permit is acquired by `catch_up_corpus` itself, only once a
        // non-empty plan confirms there is real work to apply.
        let outcome = {
            // Wait out maintenance debt before taking the corpus lock, so a
            // Hard-debt block never stalls same-corpus `index`/`add_markdown`.
            match super::backpressure::await_debt_gate(&state).await {
                Ok(()) => {}
                Err(e) => {
                    state.watch_registry().finish_catch_up(&id, Err(e.to_string()));
                    state.touch_activity(WorkClass::Internal);
                    return;
                }
            }
            let _guard = state.lock_corpus(&corpus.name).await;
            match state.resources_for(&cfg).await {
                Ok(res) => {
                    let reg = state.make_registry();
                    let lane_state = state.clone();
                    let shutdown = state.shutdown_token().clone();
                    // Race the lane wait against shutdown so a catch-up
                    // parked behind another writer's permit bails out as
                    // soon as shutdown is requested, instead of holding the
                    // corpus lock (and thus this task) open indefinitely.
                    match super::dispatch::catch_up_corpus(&res, &reg, &corpus, || {
                        let lane_state = lane_state.clone();
                        let shutdown = shutdown.clone();
                        async move {
                            tokio::select! {
                                biased;
                                () = shutdown.cancelled() => {
                                    Err("daemon shutting down; catch-up cancelled before taking the write lane")
                                }
                                permit = lane_state.acquire_write_lane() => permit,
                            }
                        }
                    })
                    .await
                    {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            tracing::warn!(
                                target: "hallouminate::daemon",
                                corpus = %corpus.name,
                                source = ?id.source,
                                error = %e,
                                "watcher: reconcile pass failed; will retry on the next reconcile tick",
                            );
                            Err(e.to_string())
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        corpus = %corpus.name,
                        source = ?id.source,
                        error = %e,
                        "watcher: reconcile pass failed; will retry on the next reconcile tick",
                    );
                    Err(e.to_string())
                }
            }
        };
        let recovered = outcome.is_ok() && state.watch_registry().has_error(&id);
        state.watch_registry().finish_catch_up(&id, outcome);
        if recovered {
            tracing::info!(
                target: "hallouminate::daemon",
                corpus = %corpus.name,
                source = ?id.source,
                "watcher: reconcile pass recovered",
            );
        }
    });
}

/// Derives this call's `ConfigSource` from `repo_path` (`None` means the
/// boot baseline) and registers request-resolved corpora with the live
/// watcher under that source.
pub(crate) fn register_runtime_corpora(
    state: &DaemonState,
    repo_path: Option<&std::path::Path>,
    corpora: &[CorpusConfig],
    cfg: &hallouminate_config::Config,
) -> Result<(registry::ConfigSource, bool), String> {
    let source = repo_path
        .map(|path| registry::ConfigSource::RepoLayer(path.to_path_buf()))
        .unwrap_or(registry::ConfigSource::Baseline);
    let cfg = std::sync::Arc::new(cfg.clone());
    let mut limit_reached = false;
    for corpus in corpora {
        let roots = watch_roots_for(corpus);
        if state
            .watch_registry()
            .is_registered_unchanged(&source, corpus, &cfg, &roots)
        {
            continue;
        }
        match state
            .watch_registry()
            .register(source.clone(), corpus.clone(), cfg.clone(), roots)
        {
            registry::RegisterOutcome::Conflict(message) => return Err(message),
            registry::RegisterOutcome::LimitReached => limit_reached = true,
            registry::RegisterOutcome::New | registry::RegisterOutcome::AlreadyRegistered => {}
        }
    }
    if limit_reached {
        tracing::warn!(
            target: "hallouminate::daemon",
            source = ?source,
            "watcher: registration limit reached; one or more corpora are not watched",
        );
    }
    Ok((source, limit_reached))
}

/// Builds the notify debouncer that reindexes changed markdown files: each
/// debounced batch is folded into `pending` and `wake` is notified so the
/// pump loop picks it up on its next iteration. Returns `None` when the
/// watcher backend fails to initialize; reconciliation continues without it.
fn build_debouncer(
    cfg: &hallouminate_config::Config,
    state: &DaemonState,
    wake: std::sync::Arc<tokio::sync::Notify>,
    pending: std::sync::Arc<std::sync::Mutex<PendingPaths>>,
    quiet: bool,
) -> Option<notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>> {
    let debounce = Duration::from_millis(cfg.watch.debounce_ms);
    // unbounded channel: a write burst that outpaces the serial async
    // consumer used to retain every debounced batch in daemon memory
    // indefinitely. `pending` accumulates distinct paths; `wake` only signals
    // "something is pending" and is bounded to capacity 1 — the consumer
    // always drains the *whole* `pending` set on wake, so a second wake
    // queued while one is outstanding would be redundant. `try_send`
    // returning `Full` is that explicit overflow behavior: a no-op, never a
    // block or a panic, because the paths it would have carried are already
    // sitting in `pending`.
    // Disable notify-debouncer-full's recursive file-ID map; large dependency trees can exhaust
    // memory.
    let state_for_debouncer = state.clone();
    let pending_for_debouncer = pending;
    let wake_for_debouncer = wake;
    match new_debouncer_opt(
        debounce,
        None,
        move |res: DebounceEventResult| {
            // The debouncer worker thread calls this on each debounced batch.
            match res {
                Ok(events) => {
                    record_pending(&pending_for_debouncer, &events);
                    state_for_debouncer.record_watcher_events(events.len() as u64);
                    // `Notify::notify_one()` carries a wake permit even when
                    // nothing is currently `.await`ing it, so a batch that lands
                    // between pump iterations is never lost the way a
                    // `try_send` on a full bounded channel would be.
                    wake_for_debouncer.notify_one();
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
        },
        NoCache,
        notify::Config::default(),
    ) {
        Ok(d) => Some(d),
        Err(e) => {
            if !quiet {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    error = %e,
                    "watcher: failed to create debouncer; reconciliation continues in degraded mode",
                );
            }
            None
        }
    }
}

/// Timing and churn-classification knobs for `run_pump`, bundled so the
/// pump loop stays under clippy's argument-count limit.
struct PumpConfig {
    reconcile_interval: Duration,
    failure_reminder: Duration,
    churn_warn_at: u32,
    churn_act_at: u32,
}
/// Runs the watcher pump loop: reconciles `pump` against the registry on
/// every registry-changed signal and on each reconcile tick, reloads
/// repo-layer config on tick, and drains `pending` into
/// `process_change_batch` after each iteration. Runs until `state`'s
/// shutdown token is cancelled.
async fn run_pump(
    state: DaemonState,
    mut pump: PumpState,
    wake: std::sync::Arc<tokio::sync::Notify>,
    pending: std::sync::Arc<std::sync::Mutex<PendingPaths>>,
    tracker: TaskTracker,
    config: PumpConfig,
    mut last_reconciled: u64,
) {
    let shutdown = state.shutdown_token().clone();

    let mut failures = FailureCoalescer::new(config.failure_reminder, MAX_FAILURE_SIGNATURES);
    let mut reload_failures = ReloadFailureMemo::new();
    let mut churn = ChurnTracker::new(config.churn_warn_at, config.churn_act_at);
    let mut reconcile_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + config.reconcile_interval,
        config.reconcile_interval,
    );
    reconcile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            // The registry has exactly one consumer of `changed()`: this
            // select loop. `WatchRegistry::signal_changed` uses
            // `notify_one()`, which carries a permit, so a signal fired
            // while this arm isn't being polled (this task not yet
            // scheduled, or busy in `pump.reconcile` / a batch below) is
            // still observed the next time this `select!` runs it, not
            // lost. The generation-gated reconcile below remains a
            // correctness backstop independent of that delivery, bounding
            // staleness to `watch.reconcile_interval_secs` even if this
            // arm were somehow never polled.
            () = state.watch_registry().changed().notified() => {
                pump.reconcile(&state, &tracker);
                last_reconciled = state.watch_registry().generation();
                state
                    .heartbeat()
                    .bump(super::heartbeat::TaskName::WatcherPump);
                continue;
            }
            _ = reconcile_tick.tick() => {
                for path in state.watch_registry().repo_layer_sources() {
                    reload_repo_layer(&state, &path, &tracker, &mut reload_failures);
                }
                state.watch_registry().mark_reconcile_due_all();
                pump.reconcile(&state, &tracker);
                last_reconciled = state.watch_registry().generation();
                state
                    .heartbeat()
                    .bump(super::heartbeat::TaskName::WatcherPump);
                continue;
            }
            // `notify_one()` carries a permit, so this is cancel-safe:
            // a wake that lands while this arm isn't being polled is
            // still observed the next time this select! runs it.
            () = wake.notified() => {}
            // Quiet-pump heartbeat: bumps the watchdog on a fixed
            // cadence even when no wake ever arrives, bounding how
            // stale the process looks to external liveness checks.
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                state
                    .heartbeat()
                    .bump(super::heartbeat::TaskName::WatcherPump);
                continue;
            }
        }
        state
            .heartbeat()
            .bump(super::heartbeat::TaskName::WatcherPump);
        // Correctness backstop (see the `Notify` arm above): reconciling
        // here on every `reconcile_tick` bounds how stale a missed
        // registry-changed signal can get to
        // `watch.reconcile_interval_secs`, independent of whether that
        // signal was ever observed. Skipped when the registry's
        // generation hasn't moved since the last pass -- nothing
        // changed, so refresh_roots + snapshot_roots would just repeat
        // prior work.
        let current_generation = state.watch_registry().generation();
        if current_generation != last_reconciled {
            pump.reconcile(&state, &tracker);
            last_reconciled = state.watch_registry().generation();
        }
        let (paths, overflow) = {
            let mut pending = pending.lock().expect("watch pending-paths mutex");
            (
                pending.paths.drain().collect::<Vec<_>>(),
                std::mem::take(&mut pending.overflow),
            )
        };
        if overflow {
            state.watch_registry().mark_reconcile_due_all();
            pump.reconcile(&state, &tracker);
            last_reconciled = state.watch_registry().generation();
        }
        if !paths.is_empty() {
            process_change_batch(&state, &pump.roots, paths, &mut failures, &mut churn).await;
        }
    }
}

/// Watch every corpus root the `WatchRegistry` knows about and spawn a task
/// that reindexes changed markdown files when a debouncer is available.
/// Reconciliation still catches up newly-registered roots when the watcher
/// backend cannot initialize. Seeds the boot baseline's corpora into
/// `state.watch_registry()`. Returns `None` only when baseline corpus
/// enumeration fails; then no pump exists and no degraded reconciliation
/// runs. A native backend failure still returns a handle, in degraded mode.
pub(crate) fn spawn_corpus_watcher(state: &DaemonState) -> Option<WatcherHandle> {
    spawn_corpus_watcher_inner(state, false)
}

/// Same as [`spawn_corpus_watcher`], but suppresses the "failed to create
/// debouncer" warning. For the boot-time capability probe in `server.rs`,
/// whose handle is aborted immediately and never reconciles in degraded
/// mode, so the warning would otherwise fire twice per boot for one
/// degraded condition.
pub(crate) fn spawn_corpus_watcher_probe(state: &DaemonState) -> Option<WatcherHandle> {
    spawn_corpus_watcher_inner(state, true)
}

fn spawn_corpus_watcher_inner(state: &DaemonState, quiet: bool) -> Option<WatcherHandle> {
    spawn_corpus_watcher_with(state, quiet, build_debouncer)
}

/// Seam for tests: swaps in a `debouncer_factory` other than [`build_debouncer`]
/// (e.g. one that always returns `None`) so a degraded pump can be exercised
/// without depending on the host's real notify backend.
fn spawn_corpus_watcher_with(
    state: &DaemonState,
    quiet: bool,
    debouncer_factory: impl FnOnce(
        &hallouminate_config::Config,
        &DaemonState,
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<std::sync::Mutex<PendingPaths>>,
        bool,
    ) -> Option<
        notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>,
    >,
) -> Option<WatcherHandle> {
    let cfg = state.baseline();
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

    // Seed every existing baseline corpus root into the registry; one
    // shared Arc<Config> across all baseline corpora (clone the Arc, not
    // the Config, per corpus).
    let baseline_cfg = std::sync::Arc::new(cfg.clone());
    for corpus in &corpora {
        let corpus_roots = watch_roots_for(corpus);
        state
            .watch_registry()
            .seed_baseline(corpus.clone(), baseline_cfg.clone(), corpus_roots);
    }

    let pending: std::sync::Arc<std::sync::Mutex<PendingPaths>> =
        std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let debouncer = debouncer_factory(cfg, state, wake.clone(), pending.clone(), quiet);
    let tracker = TaskTracker::new();

    // Reconciled once here (installing the baseline watches just seeded
    // above) before the pump task takes ownership of `pump`.
    let mut pump = PumpState {
        debouncer,
        installed: HashMap::new(),
        roots: Vec::new(),
        degraded_backend: HashSet::new(),
    };
    pump.reconcile(state, &tracker);
    let last_reconciled = state.watch_registry().generation();
    let pump_config = PumpConfig {
        reconcile_interval: Duration::from_secs(cfg.watch.effective_reconcile_interval_secs()),
        failure_reminder,
        churn_warn_at: cfg.daemon.churn_warn_at,
        churn_act_at: cfg.daemon.churn_act_at,
    };
    let state = state.clone();
    let tracker_for_task = tracker.clone();
    let task = tokio::spawn(run_pump(
        state,
        pump,
        wake,
        pending,
        tracker_for_task,
        pump_config,
        last_reconciled,
    ));

    Some(WatcherHandle { task, tracker })
}

/// Upper bound on distinct pending paths tracked between catch-up drains,
/// before `record_pending` starts marking `overflow` instead of inserting.
const MAX_PENDING_PATHS: usize = 4096;

#[derive(Default)]
struct PendingPaths {
    paths: std::collections::HashSet<PathBuf>,
    overflow: bool,
}

impl std::ops::Deref for PendingPaths {
    type Target = std::collections::HashSet<PathBuf>;

    fn deref(&self) -> &Self::Target {
        &self.paths
    }
}

impl std::ops::DerefMut for PendingPaths {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.paths
    }
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
    pending: &std::sync::Mutex<PendingPaths>,
    events: &[notify_debouncer_full::DebouncedEvent],
) {
    let mut pending = pending.lock().expect("watch pending-paths mutex");
    for event in events {
        if matches!(event.kind, notify::EventKind::Access(_)) {
            continue;
        }
        for path in &event.paths {
            if pending.paths.len() < MAX_PENDING_PATHS || pending.paths.contains(path) {
                pending.paths.insert(path.clone());
            } else {
                pending.overflow = true;
            }
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
/// Retains a not-yet-created root so recovery can install it when it appears.
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
        Some(WatchRoot {
            canonical_watched: canonicalize_or_passthrough(&root).into_path_buf(),
            watched: root,
            corpus: corpus.clone(),
            canonical_file_root: None,
            mode: RecursiveMode::Recursive,
        })
    }
}

fn watch_roots_for(corpus: &CorpusConfig) -> Vec<WatchRoot> {
    corpus
        .paths
        .iter()
        .filter_map(|raw| build_watch_root(corpus, raw))
        .collect()
}

/// Resolve the configuration a changed path is indexed under. Every
/// registration sharing the owner's `WatchRoot` (e.g. a repo-layer
/// registration overlapping the baseline's root) records the path as
/// pending work; the first match supplies the retained configuration used
/// to drive the reindex/prune. An unregistered owner, or a registration
/// removed between the snapshot and this lookup, falls back to the boot
/// baseline.
fn resolve_registration_config(
    state: &DaemonState,
    owner: &WatchRoot,
    mut matches: Vec<RegistrationId>,
    path: &Path,
) -> (CorpusConfig, std::sync::Arc<hallouminate_config::Config>) {
    matches.sort_by(|a, b| {
        a.source.cmp(&b.source).then_with(|| {
            a.corpus_key
                .canonical_root
                .cmp(&b.corpus_key.canonical_root)
        })
    });
    let Some(first_id) = matches.first().cloned() else {
        return (owner.corpus.clone(), state.baseline_arc());
    };
    for id in &matches {
        state
            .watch_registry()
            .record_pending(id, [path.to_path_buf()]);
    }
    let Some(resolved) = state.watch_registry().config_for(&first_id) else {
        return (owner.corpus.clone(), state.baseline_arc());
    };
    resolved
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
    churn: &mut ChurnTracker,
) {
    let _conn = state.enter_connection(WorkClass::Internal);
    for path in &paths {
        handle_changed_path(state, roots, path, failures, churn).await;
    }
    state.touch_activity(WorkClass::Internal);
}

/// Reindex (or prune) one changed path, resolving its owning registration(s)
/// once. More than one registration can share the same `WatchRoot` (e.g. a
/// repo-layer registration overlapping the baseline's root); every such
/// registration gets the path recorded as pending work, and the first
/// match's resolved config drives the reindex/prune.
async fn handle_changed_path(
    state: &DaemonState,
    roots: &[WatchRoot],
    path: &Path,
    failures: &mut FailureCoalescer,
    churn: &mut ChurnTracker,
) {
    let Some(owner) = owning_corpus(roots, path) else {
        return;
    };
    let matches = state.watch_registry().registrations_for_root(owner);
    let (corpus, cfg) = resolve_registration_config(state, owner, matches, path);
    let resources = match state.resources_for(&cfg).await {
        Ok(resources) => resources,
        Err(error) => {
            tracing::warn!(target: "hallouminate::daemon", error = %error, "watcher: resources unavailable");
            return;
        }
    };
    let store = resources.store.clone();
    // Stage 1 of the change gate (ADR daemon-rework-003, "git's algorithm,
    // not git's state"): compare the on-disk mtime against the last-indexed
    // snapshot before taking any lock or reading any bytes. Equal means the
    // event is a no-op (e.g. the access-event feedback loop that burned 200%
    // CPU) and is shed for the price of one stat + one snapshot row read.
    if mtime_matches_last_index(&store, &corpus, path).await {
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
        match index_single_file_with_content(&store, &registry, &corpus, path, &bytes, mtime).await
        {
            Ok(stats) => {
                let noop = stats.files_upserted == 0;
                state.record_watcher_reindex(noop);
                let effect = if noop {
                    ReindexEffect::NoOp
                } else {
                    ReindexEffect::Upserted
                };
                if let LadderOutcome::Action(action) = churn.record_reindex(effect, path) {
                    state.record_ladder_trip(action);
                    // ForceMaintenance reconciles index state without killing the watcher.
                    let s = state.clone();
                    tokio::spawn(async move {
                        // GC only runs on the scheduled tick, not the churn-triggered path -- see orphaned-root-gc age review.
                        let _ = s.run_maintenance_tick(false).await;
                    });
                }
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
        let canonical_root = match &owner.canonical_file_root {
            Some(file_root) => file_root,
            None => &owner.canonical_watched,
        };
        let corpus_key = CorpusKey {
            name: corpus.name.clone(),
            canonical_root: canonical_root.clone(),
        };
        let file_ref = delete_file_ref(owner, path);
        if let Some(file_ref_str) = file_ref.as_path().to_str()
            && let Err(e) = store.delete_file(&corpus_key, file_ref_str).await
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
async fn mtime_matches_last_index(store: &LanceStore, corpus: &CorpusConfig, path: &Path) -> bool {
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
    let Some(corpus_key) = corpus
        .corpus_key_for_path(path)
        .or_else(|| corpus.primary_corpus_key())
    else {
        return false;
    };
    match store.get_file_snapshot(&corpus_key, file_ref).await {
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

/// Find the baseline corpus that owns `path`: the deepest configured root that
/// is a prefix of `path` and whose membership rule accepts it. Deepest-first so
/// a nested file or directory corpus root wins over its watched parent root.
///
/// Membership mirrors `walker::scan`: a directory root accepts any descendant
/// the corpus' globs/exclude allow, while a **file-path** root (watched at its
/// parent) accepts only the exact declared file — never a sibling `.md` under
/// the same parent, which `scan` would never index.
fn owning_corpus<'r>(roots: &'r [WatchRoot], path: &Path) -> Option<&'r WatchRoot> {
    // Select the deepest geometric owner first. A rejected nested root must not
    // fall back to a broader root that would claim the same event.
    let mut owner: Option<(usize, &WatchRoot)> = None;
    for root in roots {
        if !path.starts_with(&root.canonical_watched) {
            continue;
        }
        if root
            .canonical_file_root
            .as_ref()
            .is_some_and(|file| file != path)
        {
            continue;
        }
        let configured_root = root
            .canonical_file_root
            .as_ref()
            .unwrap_or(&root.canonical_watched);
        let depth = configured_root.components().count();
        if owner.as_ref().is_none_or(|(current, _)| depth > *current) {
            owner = Some((depth, root));
        }
    }
    let (_, root) = owner?;

    match &root.canonical_file_root {
        // A file-path root watches its parent, but owns only its declared file.
        Some(file) if file != path => None,
        Some(_) | None => {
            let relative = path.strip_prefix(&root.canonical_watched).ok()?;
            ensure_corpus_allows_relative(&root.corpus, relative).ok()?;
            Some(root)
        }
    }
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
    use hallouminate_domain::indexer::ChunkStore;

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

    /// High thresholds so tests using this helper never cross warn/act by
    /// accident; use `ChurnTracker::new` directly to test escalation itself.
    fn disabled_churn() -> ChurnTracker {
        ChurnTracker::new(u32::MAX, u32::MAX)
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

    #[test]
    fn rejected_deepest_root_does_not_fall_back_to_broader_root() {
        let outer = corpus("outer", "/home/u/wiki", &["**/*.md"]);
        let inner = corpus("inner", "/home/u/wiki/private", &["**/*.txt"]);
        let roots = vec![
            watch_root("/home/u/wiki", outer, None),
            watch_root("/home/u/wiki/private", inner, None),
        ];

        assert!(
            owning_corpus(&roots, Path::new("/home/u/wiki/private/notes.md")).is_none(),
            "a rejected deepest root must not fall back to the outer root"
        );
    }

    #[test]
    fn file_root_applies_include_and_exclude_rules() {
        let cfg = corpus("config", "/home/u/.claude/CLAUDE.md", &["**/*.md"]);
        let mut cfg = cfg;
        cfg.exclude = vec!["CLAUDE.md".to_string()];
        let roots = vec![watch_root(
            "/home/u/.claude",
            cfg,
            Some("/home/u/.claude/CLAUDE.md"),
        )];

        assert!(
            owning_corpus(&roots, Path::new("/home/u/.claude/CLAUDE.md")).is_none(),
            "a file root must reject an event rejected by its selection rules"
        );
    }

    #[test]
    fn file_root_does_not_hide_directory_root_for_siblings() {
        let directory = corpus("directory", "/home/u/wiki", &["**/*.md"]);
        let file = corpus("file", "/home/u/wiki/CLAUDE.md", &["**/*.md"]);
        let roots = vec![
            watch_root("/home/u/wiki", directory, None),
            watch_root("/home/u/wiki", file, Some("/home/u/wiki/CLAUDE.md")),
        ];

        assert_eq!(
            name_of(owning_corpus(&roots, Path::new("/home/u/wiki/notes.md"))).as_deref(),
            Some("directory"),
            "a file root must not geometrically claim sibling events"
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

    /// A root-anchored include pattern (`docs/**/*.md`, not `**/docs/**/*.md`)
    /// must match relative to the corpus root: it admits `<root>/docs/a.md` but
    /// rejects `<root>/libs/docs/a.md`, even though the old absolute-path match
    /// would have accepted both (`**` in a leading position swallows any prefix,
    /// including `libs/`). Regresses the AC-6 relativization fix in
    /// `ensure_corpus_allows_file`.
    #[test]
    fn dir_root_honors_root_anchored_include_pattern() {
        let cfg = corpus("wiki", "/srv/wiki", &["docs/**/*.md"]);
        let roots = vec![watch_root("/srv/wiki", cfg, None)];
        assert_eq!(
            name_of(owning_corpus(&roots, Path::new("/srv/wiki/docs/a.md"))).as_deref(),
            Some("wiki"),
            "a root-anchored pattern must admit <root>/docs/a.md"
        );
        assert!(
            owning_corpus(&roots, Path::new("/srv/wiki/libs/docs/a.md")).is_none(),
            "a root-anchored pattern must reject <root>/libs/docs/a.md"
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

    /// A not-yet-created root remains registered for recovery.
    #[test]
    fn build_watch_root_retains_absent_root_for_recovery() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let absent = tmp.path().join("not-there");
        let cfg = corpus("ghost", absent.to_str().unwrap(), &["**/*.md"]);
        assert!(
            build_watch_root(&cfg, absent.to_str().unwrap()).is_some(),
            "an absent root remains registered for recovery"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_runtime_corpora_twice_is_a_no_op_that_does_not_advance_generation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");
        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir wiki");
        let corpora = vec![corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"])];

        register_runtime_corpora(&state, Some(tmp.path()), &corpora, &cfg).expect("first register");
        let generation_after_first = state.watch_registry().generation();

        register_runtime_corpora(&state, Some(tmp.path()), &corpora, &cfg)
            .expect("second register");

        assert_eq!(
            state.watch_registry().generation(),
            generation_after_first,
            "an unchanged re-registration must short-circuit before touching the registry",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_root_deletion_prunes_the_exact_declared_file_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let file = tmp.path().join("CLAUDE.md");
        std::fs::write(&file, "# Config\n\nbody\n").expect("write file root");
        let corpus = corpus("config", file.to_str().unwrap(), &["**/*.md"]);
        let owner = build_watch_root(&corpus, file.to_str().unwrap()).expect("file watch root");
        let event_path = owner
            .canonical_file_root
            .clone()
            .expect("file root keeps the declared file identity");
        let corpus_key = corpus.primary_corpus_key().expect("file corpus key");
        assert_eq!(corpus_key.canonical_root, event_path);
        let roots = vec![owner];
        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();

        handle_changed_path(&state, &roots, &event_path, &mut failures, &mut churn).await;
        let file_ref = event_path.to_str().expect("UTF-8 file root");
        let indexed = state
            .store()
            .get_file_snapshot(&corpus_key, file_ref)
            .await
            .expect("snapshot query")
            .expect("file root must be indexed under its exact declared key");
        assert_eq!(indexed.corpus_key, corpus_key);

        std::fs::remove_file(&event_path).expect("remove file root");
        handle_changed_path(&state, &roots, &event_path, &mut failures, &mut churn).await;
        assert!(
            state
                .store()
                .get_file_snapshot(&corpus_key, file_ref)
                .await
                .expect("snapshot query after delete")
                .is_none(),
            "file-root deletion must prune the exact declared-file identity",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlapping_directory_and_file_roots_choose_and_prune_the_file_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let directory = tmp.path().join("config");
        std::fs::create_dir_all(&directory).expect("mkdir config");
        let directory = directory.canonicalize().expect("canonicalize config");
        let file = directory.join("CLAUDE.md");
        std::fs::write(&file, "# Config\n\nbody\n").expect("write file root");

        let directory_corpus = corpus("directory", directory.to_str().unwrap(), &["**/*.md"]);
        let file_corpus = corpus("file", file.to_str().unwrap(), &["**/*.md"]);
        let directory_owner = build_watch_root(&directory_corpus, directory.to_str().unwrap())
            .expect("directory watch root");
        let file_owner =
            build_watch_root(&file_corpus, file.to_str().unwrap()).expect("file watch root");
        let event_path = file_owner
            .canonical_file_root
            .clone()
            .expect("file root keeps the declared file identity");
        let directory_key = directory_corpus
            .primary_corpus_key()
            .expect("directory corpus key");
        let file_key = file_corpus.primary_corpus_key().expect("file corpus key");
        let roots = vec![directory_owner, file_owner];
        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();

        assert_eq!(
            name_of(owning_corpus(&roots, &event_path)).as_deref(),
            Some("file"),
            "the deepest configured root must win even when its watcher uses the parent directory",
        );
        handle_changed_path(&state, &roots, &event_path, &mut failures, &mut churn).await;
        let file_ref = event_path.to_str().expect("UTF-8 file root");
        let indexed = state
            .store()
            .get_file_snapshot(&file_key, file_ref)
            .await
            .expect("file-root snapshot query")
            .expect("overlapping roots must index under the file-root identity");
        assert_eq!(indexed.corpus_key, file_key);
        assert!(
            state
                .store()
                .get_file_snapshot(&directory_key, file_ref)
                .await
                .expect("directory-root snapshot query")
                .is_none(),
            "configured order must not make the shallower directory own the file",
        );

        std::fs::remove_file(&event_path).expect("remove file root");
        assert_eq!(
            name_of(owning_corpus(&roots, &event_path)).as_deref(),
            Some("file"),
            "the absent file must still resolve to the deepest configured root for pruning",
        );
        handle_changed_path(&state, &roots, &event_path, &mut failures, &mut churn).await;
        assert!(
            state
                .store()
                .get_file_snapshot(&file_key, file_ref)
                .await
                .expect("file-root snapshot query after delete")
                .is_none(),
            "overlapping-root deletion must not leave a stale file-root row",
        );
        assert!(
            state
                .store()
                .get_file_snapshot(&directory_key, file_ref)
                .await
                .expect("directory-root snapshot query after delete")
                .is_none(),
            "overlapping-root deletion must not create or retain a directory-root row",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directory_root_deletion_keeps_the_directory_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).expect("mkdir corpus");
        let root = root.canonicalize().expect("canonicalize corpus");
        let file = root.join("note.md");
        std::fs::write(&file, "# Note\n\nbody\n").expect("write note");
        let corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        let owner =
            build_watch_root(&corpus, root.to_str().unwrap()).expect("directory watch root");
        let corpus_key = corpus.primary_corpus_key().expect("directory corpus key");
        assert_eq!(corpus_key.canonical_root, owner.canonical_watched);
        let roots = vec![owner];
        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();

        handle_changed_path(&state, &roots, &file, &mut failures, &mut churn).await;
        let file_ref = file.to_str().expect("UTF-8 file");
        assert!(
            state
                .store()
                .get_file_snapshot(&corpus_key, file_ref)
                .await
                .expect("snapshot query")
                .is_some(),
            "directory-root indexing must use the directory identity",
        );

        std::fs::remove_file(&file).expect("remove note");
        handle_changed_path(&state, &roots, &file, &mut failures, &mut churn).await;
        assert!(
            state
                .store()
                .get_file_snapshot(&corpus_key, file_ref)
                .await
                .expect("snapshot query after delete")
                .is_none(),
            "directory-root deletion must keep pruning by directory identity",
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
        let mut cfg = hallouminate_config::Config::default();
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
        let mut churn = disabled_churn();
        process_change_batch(&state, &roots, vec![deleted], &mut failures, &mut churn).await;
        assert_ne!(
            state.last_activity_secs(),
            u64::MAX,
            "batch processing must stamp the activity clock so idle-exit does \
             not fire immediately after a delete-branch write",
        );
    }

    /// Drives `pump` twice against `state`, asserting one catch-up pass is
    /// admitted for `id` and a second immediate reconcile does not admit a
    /// duplicate while that pass is still in flight.
    fn assert_admits_one_pass(
        state: &DaemonState,
        pump: &mut PumpState,
        tracker: &TaskTracker,
        id: &RegistrationId,
        message: &str,
    ) {
        pump.reconcile(state, tracker);
        assert_eq!(
            state.watch_registry().catch_up_state(id),
            Some(registry::CatchUpState::InFlight),
            "{message}",
        );
        pump.reconcile(state, tracker);
        assert_eq!(
            state.watch_registry().catch_up_state(id),
            Some(registry::CatchUpState::InFlight),
            "an immediate reconcile must not admit a duplicate pass",
        );
    }

    /// Regression for the "accepts registrations with zero baseline
    /// corpora" requirement: a `Config` with no `[[corpus]]` and no
    /// `[[repository]]` entries still starts the live watcher service
    /// instead of returning `None`, so runtime-discovered corpora can
    /// register later.
    #[tokio::test]
    async fn spawn_corpus_watcher_starts_with_zero_baseline_corpora() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");

        let handle = spawn_corpus_watcher(&state);

        assert!(
            handle.is_some(),
            "the watcher service must start even with no baseline corpora, \
             so runtime-discovered corpora can register later"
        );
    }

    /// A missing notify debouncer must leave reconciliation available. The
    /// first pass indexes the registration, an immediate second pass does not
    /// admit a duplicate while that pass is active, and a later reconciliation
    /// indexes a changed file as the degraded correctness backstop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_without_debouncer_indexes_without_hot_retry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.daemon.maintenance_interval_secs = 0;
        cfg.daemon.idle_exit_secs = 0;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let _coord = crate::debt::OBSERVED_HARD_COORD.read().await;
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).expect("mkdir wiki");
        let root = root.canonicalize().expect("canonicalize wiki");
        let file = root.join("note.md");
        std::fs::write(&file, "# First\n\nbody\n").expect("write initial note");
        let live_corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        let key = live_corpus.primary_corpus_key().expect("corpus key");
        register_runtime_corpora(&state, None, &[live_corpus], &cfg)
            .expect("register runtime corpus");
        let (id, _) = state
            .watch_registry()
            .snapshot_roots()
            .into_iter()
            .next()
            .expect("runtime registration root");

        let tracker = TaskTracker::new();
        let mut pump = PumpState {
            debouncer: None,
            installed: HashMap::new(),
            roots: Vec::new(),
            degraded_backend: HashSet::new(),
        };
        let first_permit = state
            .acquire_write_lane()
            .await
            .expect("acquire sole write-lane permit");
        assert_admits_one_pass(
            &state,
            &mut pump,
            &tracker,
            &id,
            "the unavailable debouncer must still admit one catch-up pass",
        );
        drop(first_permit);
        crate::test_support::wait_until(
            Duration::from_secs(5),
            "initial degraded catch-up must complete",
            || async {
                state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
            },
        )
        .await;
        pump.reconcile(&state, &tracker);
        assert_eq!(
            state.watch_registry().catch_up_state(&id),
            Some(registry::CatchUpState::Done),
            "an immediate reconcile after completion must not queue a retry",
        );
        let file_ref = file.to_str().unwrap();
        let first_hash = state
            .store()
            .get_file_snapshot(&key, file_ref)
            .await
            .expect("initial snapshot query")
            .expect("degraded reconciliation must index the initial file")
            .content_hash;

        std::fs::write(&file, "# Second\n\nchanged\n").expect("write changed note");
        set_mtime(&file, std::time::SystemTime::now() + Duration::from_secs(1));
        let second_permit = state
            .acquire_write_lane()
            .await
            .expect("reacquire sole write-lane permit");
        state.watch_registry().mark_reconcile_due_all();
        assert_admits_one_pass(
            &state,
            &mut pump,
            &tracker,
            &id,
            "a later reconciliation must admit changed work",
        );
        drop(second_permit);
        crate::test_support::wait_until(
            Duration::from_secs(5),
            "later degraded catch-up must complete",
            || async {
                state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
            },
        )
        .await;
        pump.reconcile(&state, &tracker);
        assert_eq!(
            state.watch_registry().catch_up_state(&id),
            Some(registry::CatchUpState::Done),
            "an immediate reconcile after completion must not queue a retry",
        );
        let second_snapshot = state
            .store()
            .get_file_snapshot(&key, file_ref)
            .await
            .expect("changed snapshot query")
            .expect("later reconciliation must retain the changed file");
        assert_ne!(
            second_snapshot.content_hash, first_hash,
            "later reconciliation must index changed content without notify",
        );
        assert!(
            matches!(
                state.watch_registry().observation(&id),
                Some(registry::Observation::Degraded { .. })
            ),
            "the registration must retain its degraded observation",
        );

        tracker.close();
        tracker.wait().await;
    }

    /// Split out of `reconcile_without_debouncer_indexes_without_hot_retry`:
    /// a later corpus registered at the same watch root while degraded must
    /// itself report a degraded observation, not silently inherit the first
    /// registration's status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_without_debouncer_degrades_a_later_shared_root_registration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.daemon.maintenance_interval_secs = 0;
        cfg.daemon.idle_exit_secs = 0;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let _coord = crate::debt::OBSERVED_HARD_COORD.read().await;
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).expect("mkdir wiki");
        let root = root.canonicalize().expect("canonicalize wiki");
        std::fs::write(root.join("note.md"), "# First\n\nbody\n").expect("write initial note");
        let live_corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        register_runtime_corpora(&state, None, &[live_corpus], &cfg)
            .expect("register runtime corpus");
        let (id, _) = state
            .watch_registry()
            .snapshot_roots()
            .into_iter()
            .next()
            .expect("runtime registration root");

        let tracker = TaskTracker::new();
        let mut pump = PumpState {
            debouncer: None,
            installed: HashMap::new(),
            roots: Vec::new(),
            degraded_backend: HashSet::new(),
        };
        pump.reconcile(&state, &tracker);
        crate::test_support::wait_until(
            Duration::from_secs(5),
            "initial degraded catch-up must complete",
            || async {
                state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
            },
        )
        .await;

        let shared = corpus("shared", root.to_str().unwrap(), &["**/*.md"]);
        let shared_id = RegistrationId {
            source: registry::ConfigSource::Baseline,
            corpus_key: shared.primary_corpus_key().expect("shared corpus key"),
        };
        register_runtime_corpora(&state, None, &[shared], &cfg)
            .expect("register later corpus at the same root");
        pump.reconcile(&state, &tracker);
        let Some(registry::Observation::Degraded { .. }) =
            state.watch_registry().observation(&shared_id)
        else {
            panic!("a later shared-root registration must report the unavailable watcher");
        };
        tracker.close();
        tracker.wait().await;
    }

    /// Regression test for finding 1: `spawn_corpus_watcher_with` must return
    /// a live pump even when the debouncer factory fails, and that pump must
    /// still catch up a registration via `reconcile`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_corpus_watcher_with_a_failing_debouncer_still_catches_up_via_reconcile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.daemon.maintenance_interval_secs = 0;
        cfg.daemon.idle_exit_secs = 0;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).expect("mkdir wiki");
        let root = root.canonicalize().expect("canonicalize wiki");
        std::fs::write(root.join("note.md"), "# First\n\nbody\n").expect("write initial note");
        let live_corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        let key = live_corpus.primary_corpus_key().expect("corpus key");

        let handle = spawn_corpus_watcher_with(&state, false, |_, _, _, _, _| None)
            .expect("degraded watcher must still start a pump");

        register_runtime_corpora(&state, None, &[live_corpus], &cfg)
            .expect("register runtime corpus");
        let (id, _) = state
            .watch_registry()
            .snapshot_roots()
            .into_iter()
            .next()
            .expect("runtime registration root");

        crate::test_support::wait_until(Duration::from_secs(5), "catch-up done", || async {
            state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
        })
        .await;

        let file_ref = root.join("note.md").to_str().unwrap().to_string();
        assert!(
            state
                .store()
                .get_file_snapshot(&key, &file_ref)
                .await
                .expect("snapshot query")
                .is_some(),
            "degraded pump must index the registration through run_pump/reconcile",
        );
        assert!(
            !handle.is_finished(),
            "the degraded pump task must remain alive after indexing"
        );

        handle.abort().await;
    }

    /// Regression test for finding 2: a registration whose resolved config
    /// overrides `storage.ground_dir` must have its catch-up pass write into
    /// the store resolved from *that* config, not the baseline store.
    #[tokio::test]
    async fn catch_up_writes_into_the_registered_config_store_not_the_baseline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("wiki");
        std::fs::create_dir_all(&root).expect("mkdir wiki");
        std::fs::write(root.join("a.md"), "# A\n\nbody\n").expect("write a.md");
        let baseline_ground = tmp.path().join("baseline-ground");
        let repo_ground = tmp.path().join("repo-ground");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = baseline_ground.to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let mut resolved_cfg = state.baseline().clone();
        resolved_cfg.storage.ground_dir = repo_ground.to_string_lossy().into_owned();

        let live_corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        let key = live_corpus.primary_corpus_key().expect("corpus key");
        register_runtime_corpora(&state, None, &[live_corpus], &resolved_cfg)
            .expect("register runtime corpus with the resolved config");
        let (id, _) = state
            .watch_registry()
            .snapshot_roots()
            .into_iter()
            .next()
            .expect("runtime registration root");

        let tracker = TaskTracker::new();
        let mut pump = PumpState {
            debouncer: None,
            installed: HashMap::new(),
            roots: Vec::new(),
            degraded_backend: HashSet::new(),
        };
        pump.reconcile(&state, &tracker);

        crate::test_support::wait_until(
            Duration::from_secs(5),
            "catch-up into the resolved config's store",
            || async {
                state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
            },
        )
        .await;

        let repo_res = state
            .resources_for(&resolved_cfg)
            .await
            .expect("repo-layer resources");
        let repo_files = repo_res
            .store
            .list_files(&key)
            .await
            .expect("list repo files");
        assert!(
            !repo_files.is_empty(),
            "catch-up must write into the store resolved from the registered config",
        );
        let baseline_files = state
            .store()
            .list_files(&key)
            .await
            .expect("list baseline files");
        assert!(
            baseline_files.is_empty(),
            "catch-up must not write into the baseline store when the registered config overrides storage",
        );

        tracker.close();
        tracker.wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_changed_path_records_watcher_reindex_counters() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
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
        let mut churn = disabled_churn();

        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 1, 0),
            "first reindex of a new file must count as a real (non-noop) reindex",
        );

        // Same file, unchanged content and mtime: the stage-1 mtime gate
        // (ADR daemon-rework-003) sheds the event before any read — a skip,
        // not a reindex, so no counter moves.
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
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
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 2, 1),
            "reindexing unchanged content must count as a noop reindex",
        );
    }

    /// #18: a path claimed by two registrations sharing the same `WatchRoot`
    /// (e.g. a repo-layer registration overlapping the baseline's root) must
    /// record pending work on both, not just the first match.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_changed_path_records_pending_on_every_registration_sharing_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let corpus_dir = corpus_dir.canonicalize().expect("canonicalize corpus dir");
        let note = corpus_dir.join("note.md");
        std::fs::write(
            &note,
            "# Note

body
",
        )
        .expect("write note");

        let corpus_cfg = corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]);
        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            corpus_cfg.clone(),
            None,
        )];
        let shared_cfg = std::sync::Arc::new(cfg);

        state.watch_registry().register(
            registry::ConfigSource::Baseline,
            corpus_cfg.clone(),
            shared_cfg.clone(),
            roots.clone(),
        );
        state.watch_registry().register(
            registry::ConfigSource::RepoLayer(corpus_dir.clone()),
            corpus_cfg,
            shared_cfg,
            roots.clone(),
        );

        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;

        let corpus_key = CorpusKey {
            name: "wiki".to_string(),
            canonical_root: corpus_dir.clone(),
        };
        let baseline_id = RegistrationId {
            source: registry::ConfigSource::Baseline,
            corpus_key: corpus_key.clone(),
        };
        let repo_id = RegistrationId {
            source: registry::ConfigSource::RepoLayer(corpus_dir),
            corpus_key,
        };
        assert!(
            !state
                .watch_registry()
                .pending(&baseline_id)
                .expect("baseline registration must exist")
                .is_idle(),
            "the baseline registration sharing the root must also see the pending path"
        );
        assert!(
            !state
                .watch_registry()
                .pending(&repo_id)
                .expect("repo-layer registration must exist")
                .is_idle(),
            "the repo-layer registration sharing the root must see the pending path"
        );
    }

    #[tokio::test]
    async fn resolve_registration_config_prefers_baseline_over_arbitrary_hashmap_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");

        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir corpus");
        let corpus_dir = corpus_dir.canonicalize().expect("canonicalize corpus dir");

        let baseline_corpus = corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.md"]);
        let repo_corpus = corpus("wiki", corpus_dir.to_str().unwrap(), &["**/*.txt"]);
        let roots = vec![watch_root(
            corpus_dir.to_str().unwrap(),
            baseline_corpus.clone(),
            None,
        )];
        let shared_cfg = std::sync::Arc::new(cfg);

        state.watch_registry().register(
            registry::ConfigSource::Baseline,
            baseline_corpus.clone(),
            shared_cfg.clone(),
            roots.clone(),
        );
        state.watch_registry().register(
            registry::ConfigSource::RepoLayer(corpus_dir.clone()),
            repo_corpus,
            shared_cfg,
            roots.clone(),
        );

        let corpus_key = CorpusKey {
            name: "wiki".to_string(),
            canonical_root: corpus_dir.clone(),
        };
        let baseline_id = RegistrationId {
            source: registry::ConfigSource::Baseline,
            corpus_key: corpus_key.clone(),
        };
        let repo_id = RegistrationId {
            source: registry::ConfigSource::RepoLayer(corpus_dir.clone()),
            corpus_key,
        };

        let path = corpus_dir.join("note.md");
        let (resolved_corpus, _resolved_cfg) =
            resolve_registration_config(&state, &roots[0], vec![repo_id, baseline_id], &path);

        assert_eq!(
            resolved_corpus.globs, baseline_corpus.globs,
            "matches must be sorted deterministically so the Baseline registration wins \
             regardless of HashMap iteration order"
        );
    }

    /// Churn wiring (G7, wiring task W3): consecutive zero-upsert reindexes
    /// driven through `handle_changed_path` must trip the act-tier ladder —
    /// recorded via `state.record_ladder_trip` as `ForceMaintenance` — and a
    /// real upsert must reset the streak. Once-per-streak refire suppression
    /// is pinned at unit level in churn.rs; this pins the wiring: config
    /// thresholds → tracker → `LadderOutcome::Action` arm → trip snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consecutive_noop_reindexes_trip_force_maintenance_and_reset_on_upsert() {
        use super::super::ladder::LadderAction;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        cfg.daemon.churn_warn_at = 2;
        cfg.daemon.churn_act_at = 3;
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
        // Mirrors the pump construction in `spawn_corpus_watcher`: thresholds
        // come from the baseline daemon config, not test-local constants.
        let baseline = state.baseline();
        let mut churn =
            ChurnTracker::new(baseline.daemon.churn_warn_at, baseline.daemon.churn_act_at);

        // A noop reindex = mtime moved (passes the stage-1 gate) but content
        // unchanged (indexer hash fast path upserts nothing).
        let bump_mtime = |path: &Path| {
            let bumped = std::fs::metadata(path)
                .expect("stat")
                .modified()
                .expect("mtime")
                + Duration::from_millis(10);
            set_mtime(path, bumped);
        };

        // Initial index: a real upsert; no trip.
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        assert_eq!(
            state.last_ladder_trip(),
            None,
            "a real upsert must not trip the ladder",
        );

        // Two noops: streak 2 < act_at 3 — warn tier at most, no trip.
        for _ in 0..2 {
            bump_mtime(&note);
            handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        }
        assert_eq!(
            state.last_ladder_trip(),
            None,
            "a noop streak below act_at must not trip the ladder",
        );

        // A real upsert (content rewritten) must reset the streak...
        std::fs::write(&note, "# Note\n\nrewritten body\n").expect("rewrite note");
        bump_mtime(&note);
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;

        // ...so two more noops stay below the threshold. Without the reset
        // the streak would be 4 >= act_at 3 here and this assertion fails.
        for _ in 0..2 {
            bump_mtime(&note);
            handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        }
        assert_eq!(
            state.last_ladder_trip(),
            None,
            "a real upsert must reset the noop streak; a trip here means reset-on-upsert is broken",
        );

        // Third consecutive noop reaches act_at: the act-tier trip is
        // recorded as ForceMaintenance.
        bump_mtime(&note);
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        let trip = state
            .last_ladder_trip()
            .expect("act threshold reached: the trip must be recorded on state");
        match trip.action {
            LadderAction::ForceMaintenance => {}
            other => panic!("churn escalation must force maintenance, got {other:?}"),
        }
        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 7, 5),
            "2 real + 5 noop reindexes must be counted (events stay 0: driven \
             directly, not through the pump)",
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
        let mut cfg = hallouminate_config::Config::default();
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
        let corpus_key = roots[0]
            .corpus
            .primary_corpus_key()
            .expect("wiki corpus key");
        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;

        let file_ref = canonicalize_or_passthrough(&note)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();
        let indexed = state
            .store()
            .get_file_snapshot(&corpus_key, &file_ref)
            .await
            .expect("snapshot query")
            .expect("initial index must store a snapshot");

        // Rewrite the content, then put the mtime back: from the outside the
        // file looks untouched, and only a content read could tell otherwise.
        std::fs::write(&note, "# Note\n\nrewritten body the gate must not see\n")
            .expect("rewrite note");
        set_mtime(&note, indexed_mtime);

        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;

        assert_eq!(
            state.watcher_counters_snapshot(),
            (0, 1, 0),
            "the skip must not count as a reindex",
        );
        let after = state
            .store()
            .get_file_snapshot(&corpus_key, &file_ref)
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
        let mut cfg = hallouminate_config::Config::default();
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
        let corpus_key = roots[0]
            .corpus
            .primary_corpus_key()
            .expect("wiki corpus key");
        let file_ref = canonicalize_or_passthrough(&note)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        // Baseline: the real file indexes and lands a snapshot row.
        let mut failures = disabled_coalescer();
        let mut churn = disabled_churn();
        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;
        let good = state
            .store()
            .get_file_snapshot(&corpus_key, &file_ref)
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

        handle_changed_path(&state, &roots, &note, &mut failures, &mut churn).await;

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
                .get_file_snapshot(&corpus_key, &secret_ref)
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
            .get_file_snapshot(&corpus_key, &file_ref)
            .await
            .expect("snapshot query")
            .expect("the snapshot must survive a rejected symlink reindex");
        assert_eq!(
            after.content_hash, good.content_hash,
            "the store must still hold the real in-corpus file's content, never \
             the outside secret's",
        );
    }

    #[test]
    fn record_pending_bounds_unique_paths_and_marks_overflow() {
        let pending: std::sync::Mutex<PendingPaths> =
            std::sync::Mutex::new(PendingPaths::default());
        let events: Vec<_> = (0..=MAX_PENDING_PATHS)
            .map(|index| {
                notify_debouncer_full::DebouncedEvent::new(
                    notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                        .add_path(PathBuf::from(format!("/srv/wiki/{index}.md"))),
                    std::time::Instant::now(),
                )
            })
            .collect();

        record_pending(&pending, &events);

        {
            let pending = pending.lock().expect("pending mutex");
            assert_eq!(pending.paths.len(), MAX_PENDING_PATHS);
            assert!(pending.overflow);
        }
        let other = std::sync::Mutex::new(PendingPaths::default());
        assert!(!other.lock().expect("other pending mutex").overflow);
    }

    /// `record_pending` coalesces duplicate paths within one batch and across
    /// multiple batches recorded before a drain — the fix for the unbounded
    /// channel: paths accumulate in a bounded shared set instead of every
    /// debounced batch queuing separately. `record_pending` does not filter
    /// by extension at all (only by `EventKind`), so an unsupported `.docx`
    /// path is coalesced and retained the same as any `.md` path.
    #[test]
    fn record_pending_coalesces_across_batches() {
        let pending: std::sync::Mutex<PendingPaths> =
            std::sync::Mutex::new(PendingPaths::default());
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
            std::collections::HashSet::from([a, b, ignored]),
            "pending must coalesce the duplicate .md path within a batch and \
             across batches, while retaining every mutation path"
        );
    }

    /// `record_pending` does not filter by file extension at all — that is
    /// the indexer's job via `format_from_extension` during actual reindex.
    /// It filters only by event *kind*, dropping `EventKind::Access` and
    /// admitting everything else. An uppercase `.MD`, a `.csv`, and even a
    /// known-unsupported `.docx` are all retained here.
    #[test]
    fn record_pending_is_extension_agnostic_filters_by_event_kind_only() {
        let pending: std::sync::Mutex<PendingPaths> =
            std::sync::Mutex::new(PendingPaths::default());
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
            std::collections::HashSet::from([uppercase_md, csv, unsupported]),
            "an uppercase .MD and a .csv must be admitted (matching \
             format_from_extension), while retaining every mutation path"
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
        let pending: std::sync::Mutex<PendingPaths> =
            std::sync::Mutex::new(PendingPaths::default());
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
        let pending: std::sync::Mutex<PendingPaths> =
            std::sync::Mutex::new(PendingPaths::default());
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

    #[test]
    fn first_failure_warns() {
        let mut memo = ReloadFailureMemo::new();
        let path = PathBuf::from("/repo/hallouminate.toml");
        assert!(memo.record_failure(&path, "boom"));
    }

    #[test]
    fn repeat_is_quiet() {
        let mut memo = ReloadFailureMemo::new();
        let path = PathBuf::from("/repo/hallouminate.toml");
        assert!(memo.record_failure(&path, "boom"));
        assert!(!memo.record_failure(&path, "boom"));
        assert!(memo.record_failure(&path, "different"));
    }

    #[test]
    fn recovery_clears() {
        let mut memo = ReloadFailureMemo::new();
        let path = PathBuf::from("/repo/hallouminate.toml");
        memo.record_failure(&path, "boom");
        assert!(memo.record_success(&path));
        assert!(!memo.record_success(&path));
    }
    /// The production construction seam uses `NoCache`, so recursive watches
    /// do not retain a file-ID entry for every path in dependency-like trees.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_debouncer_uses_no_cache_for_recursive_dependency_trees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");
        let tree = tmp.path().join("dependency-tree");
        std::fs::create_dir_all(tree.join("node_modules/@opentelemetry"))
            .expect("create dependency-like tree");
        let target = tree.join("node_modules/@opentelemetry/package.md");
        std::fs::write(&target, "# dependency\n").expect("write dependency file");
        let link = tmp.path().join("linked-dependency-tree");
        std::os::unix::fs::symlink(&tree, &link).expect("link dependency-like tree");
        std::os::unix::fs::symlink(tree.join("node_modules"), tree.join("linked-node-modules"))
            .expect("link dependency subtree");
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut debouncer =
            build_debouncer(&cfg, &state, wake, pending, false).expect("build watcher");

        debouncer
            .watch(&link, RecursiveMode::Recursive)
            .expect("watch symlink-heavy dependency-like tree");

        fn assert_no_cache<W, C>(_: &notify_debouncer_full::Debouncer<W, C>)
        where
            W: notify::Watcher,
            C: notify_debouncer_full::FileIdCache + 'static,
        {
            assert_eq!(
                std::any::TypeId::of::<C>(),
                std::any::TypeId::of::<NoCache>(),
                "the production watcher must use NoCache"
            );
        }

        assert_no_cache(&debouncer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_debouncer_forwards_native_file_lifecycle_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.debounce_ms = 50;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None).await.expect("open");
        let root = tmp.path().join("watch-root");
        std::fs::create_dir(&root).expect("create watch root");
        let root = root.canonicalize().expect("canonicalize watch root");
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
        let pending_for_test = pending.clone();
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut debouncer =
            build_debouncer(&cfg, &state, wake, pending, false).expect("build watcher");
        debouncer
            .watch(&root, RecursiveMode::Recursive)
            .expect("watch root");
        async fn wait_for_paths(
            pending: &std::sync::Arc<std::sync::Mutex<PendingPaths>>,
            expected: &[std::path::PathBuf],
        ) -> std::collections::HashSet<std::path::PathBuf> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut observed = std::collections::HashSet::new();
            loop {
                observed.extend(pending.lock().expect("pending mutex").paths.drain());
                if expected.iter().all(|path| observed.contains(path))
                    || std::time::Instant::now() >= deadline
                {
                    return observed;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let old = root.join("old.md");
        std::fs::write(&old, "one\n").expect("create file");
        assert!(
            wait_for_paths(&pending_for_test, std::slice::from_ref(&old))
                .await
                .contains(&old),
            "native create must reach pending"
        );
        std::fs::write(&old, "two\n").expect("edit file");
        assert!(
            wait_for_paths(&pending_for_test, std::slice::from_ref(&old))
                .await
                .contains(&old),
            "native edit must reach pending"
        );
        let renamed = root.join("renamed.md");
        std::fs::rename(&old, &renamed).expect("rename file");
        let rename_events =
            wait_for_paths(&pending_for_test, &[old.clone(), renamed.clone()]).await;
        assert!(
            rename_events.contains(&old),
            "native rename must report old path"
        );
        assert!(
            rename_events.contains(&renamed),
            "native rename must report new path"
        );
        let temp = root.join("atomic.tmp");
        std::fs::write(&temp, "three\n").expect("write atomic temp");
        std::fs::rename(&temp, &renamed).expect("atomic replace");
        // Only the atomic-replace *target* is a guaranteed observation: the temp
        // file exists for microseconds before it is renamed away, so a native
        // watcher with debouncing (esp. Linux inotify under load) may coalesce
        // its create/rename events and never surface the temp path. Asserting on
        // the transient temp is a flaky over-assertion; reindexing the target is
        // the behaviour that matters.
        let atomic_events = wait_for_paths(&pending_for_test, std::slice::from_ref(&renamed)).await;
        assert!(
            atomic_events.contains(&renamed),
            "atomic save must report target path"
        );
        std::fs::remove_file(&renamed).expect("delete file");
        assert!(
            wait_for_paths(&pending_for_test, std::slice::from_ref(&renamed))
                .await
                .contains(&renamed),
            "native delete must reach pending"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_watcher_indexes_descendant_mutations_before_reconciliation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.debounce_ms = 25;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let root = tmp.path().join("wiki");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested corpus root");
        let file = nested.join("note.md");
        let corpus = corpus("wiki", root.to_str().unwrap(), &["**/*.md"]);
        cfg.corpora.push(corpus.clone());
        let key = corpus.primary_corpus_key().expect("corpus key");
        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");
        let watcher = spawn_corpus_watcher(&state).expect("production watcher");
        std::fs::write(&file, "# first\n").expect("create descendant after watcher installation");
        // Stored file refs are canonical; macOS tempdirs are not.
        let file = std::fs::canonicalize(&file).expect("canonicalize descendant");

        async fn wait_for_snapshot(state: &DaemonState, key: &CorpusKey, file: &Path) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let file_ref = file.to_str().expect("UTF-8 test path");
            loop {
                if let Some(snapshot) = state
                    .store()
                    .get_file_snapshot(key, file_ref)
                    .await
                    .expect("snapshot query")
                {
                    return snapshot.content_hash;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "watcher did not index {file_ref}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        let old_hash = wait_for_snapshot(&state, &key, &file).await;
        std::fs::write(&file, "# second\nchanged descendant\n").expect("mutate descendant");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let file_ref = file.to_str().expect("UTF-8 test path");
            let snapshot = state
                .store()
                .get_file_snapshot(&key, file_ref)
                .await
                .expect("changed snapshot query");
            if snapshot
                .as_ref()
                .is_some_and(|s| s.content_hash != old_hash)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant mutation did not reach the index"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        state.shutdown_token().cancel();
        watcher.join().await;
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_heartbeat_advances_during_quiet_pump_sleep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None)
            .await
            .expect("open daemon state");
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let debouncer = build_debouncer(&cfg, &state, wake.clone(), pending.clone(), false)
            .expect("build production watcher");
        let tracker = TaskTracker::new();
        let pump = PumpState {
            debouncer: Some(debouncer),
            installed: HashMap::new(),
            roots: Vec::new(),
            degraded_backend: HashSet::new(),
        };
        let task = tokio::spawn(run_pump(
            state.clone(),
            pump,
            wake,
            pending,
            tracker,
            PumpConfig {
                reconcile_interval: Duration::from_secs(3600),
                failure_reminder: Duration::ZERO,
                churn_warn_at: u32::MAX,
                churn_act_at: u32::MAX,
            },
            state.watch_registry().generation(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(
            state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::WatcherPump)
                > 0,
            "quiet watcher pump must bump its heartbeat during the 60-second sleep"
        );
        state.shutdown_token().cancel();
        task.await.expect("watcher pump task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_parent_registration_preserves_recursive_descendant_delivery() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.debounce_ms = 25;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let root = tmp.path().join("shared");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("create shared parent");
        let file_root = root.join("CLAUDE.md");
        std::fs::write(&file_root, "# config\n").expect("write file root");
        let descendant = nested.join("note.md");
        let file_corpus = corpus("file-first", file_root.to_str().unwrap(), &["**/*.md"]);
        let directory_corpus = corpus("directory-second", root.to_str().unwrap(), &["**/*.md"]);
        cfg.corpora = vec![file_corpus, directory_corpus.clone()];
        let key = directory_corpus
            .primary_corpus_key()
            .expect("directory key");
        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");
        let watcher = spawn_corpus_watcher(&state).expect("production watcher");
        std::fs::write(&descendant, "# first\n")
            .expect("create descendant after watcher installation");
        // Stored file refs are canonical; macOS tempdirs are not.
        let descendant = std::fs::canonicalize(&descendant).expect("canonicalize descendant");
        let file_ref = descendant.to_str().expect("UTF-8 descendant path");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state
                .store()
                .get_file_snapshot(&key, file_ref)
                .await
                .expect("snapshot query")
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "recursive root did not catch up descendant"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let old_hash = state
            .store()
            .get_file_snapshot(&key, file_ref)
            .await
            .expect("baseline snapshot query")
            .expect("baseline descendant snapshot")
            .content_hash;
        std::fs::write(&descendant, "# second\nshared parent mutation\n")
            .expect("mutate descendant");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = state
                .store()
                .get_file_snapshot(&key, file_ref)
                .await
                .expect("changed snapshot query");
            if snapshot
                .as_ref()
                .is_some_and(|s| s.content_hash != old_hash)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "recursive shared-parent watch missed descendant mutation"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        state.shutdown_token().cancel();
        watcher.join().await;
    }

    #[tokio::test(start_paused = true)]
    async fn registry_change_and_reconcile_timer_each_bump_watcher_heartbeat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None)
            .await
            .expect("open daemon state");
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let debouncer = build_debouncer(&cfg, &state, wake.clone(), pending.clone(), false)
            .expect("build watcher");
        let task = tokio::spawn(run_pump(
            state.clone(),
            PumpState {
                debouncer: Some(debouncer),
                installed: HashMap::new(),
                roots: Vec::new(),
                degraded_backend: HashSet::new(),
            },
            wake,
            pending,
            TaskTracker::new(),
            PumpConfig {
                reconcile_interval: Duration::from_secs(10),
                failure_reminder: Duration::ZERO,
                churn_warn_at: u32::MAX,
                churn_act_at: u32::MAX,
            },
            state.watch_registry().generation(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        let after_timer = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::WatcherPump);
        assert!(
            after_timer > 0,
            "reconcile timer must bump watcher heartbeat before registry changes"
        );
        let root = tmp.path().join("runtime-root");
        std::fs::create_dir_all(&root).expect("create runtime root");
        let runtime = corpus("runtime", root.to_str().unwrap(), &["**/*.md"]);
        register_runtime_corpora(&state, None, &[runtime], &cfg).expect("register runtime corpus");
        tokio::task::yield_now().await;
        assert!(
            state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::WatcherPump)
                > after_timer,
            "registry change must bump watcher heartbeat after the timer observation"
        );
        state.shutdown_token().cancel();
        task.await.expect("watcher pump task");
    }

    /// Regression test for the lost-signal defect: `WatchRegistry` used
    /// `notify_waiters`, which has no permit memory, so a registry change
    /// fired before the pump task was ever polled was dropped instead of
    /// observed on the pump's first iteration. A long `reconcile_interval`
    /// means the generation-gated backstop would not paper over the loss
    /// within this test's timeout.
    #[tokio::test]
    async fn registry_signal_wakes_pump_before_it_is_first_polled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg.clone(), None)
            .await
            .expect("open daemon state");
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths::default()));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let debouncer = build_debouncer(&cfg, &state, wake.clone(), pending.clone(), false)
            .expect("build watcher");
        let task = tokio::spawn(run_pump(
            state.clone(),
            PumpState {
                debouncer: Some(debouncer),
                installed: HashMap::new(),
                roots: Vec::new(),
                degraded_backend: HashSet::new(),
            },
            wake,
            pending,
            TaskTracker::new(),
            PumpConfig {
                reconcile_interval: Duration::from_secs(3600),
                failure_reminder: Duration::ZERO,
                churn_warn_at: u32::MAX,
                churn_act_at: u32::MAX,
            },
            state.watch_registry().generation(),
        ));

        // No `.await` between the spawn above and the registration below:
        // on the current-thread test runtime the spawned pump task is not
        // polled until this test yields, so this reproduces a registry
        // signal firing while the pump is not yet parked on `changed()`.
        let root = tmp.path().join("runtime-root");
        std::fs::create_dir_all(&root).expect("create runtime root");
        let root = root.canonicalize().expect("canonicalize runtime root");
        std::fs::write(root.join("note.md"), "# note\n").expect("write seed file");
        let runtime_corpus = corpus("runtime", root.to_str().unwrap(), &["**/*.md"]);
        register_runtime_corpora(&state, None, &[runtime_corpus], &cfg)
            .expect("register runtime corpus");

        let id = RegistrationId {
            source: registry::ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "runtime".to_string(),
                canonical_root: root,
            },
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.watch_registry().catch_up_state(&id) == Some(registry::CatchUpState::Done)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("registry signal must wake the pump instead of waiting for the reconcile backstop");

        state.shutdown_token().cancel();
        task.await.expect("watcher pump task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_overflow_triggers_full_registration_reconciliation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let root = tmp.path().join("overflow-root");
        std::fs::create_dir_all(&root).expect("create overflow root");
        let file = root.join("missed.md");
        std::fs::write(&file, "# recovered\n").expect("write missed file");
        let configured = corpus("overflow", root.to_str().unwrap(), &["**/*.md"]);
        let key = configured.primary_corpus_key().expect("corpus key");
        let state = DaemonState::open(cfg.clone(), None)
            .await
            .expect("open daemon state");
        let roots = watch_roots_for(&configured);
        state
            .watch_registry()
            .seed_baseline(configured, std::sync::Arc::new(cfg.clone()), roots);
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingPaths {
            paths: std::collections::HashSet::new(),
            overflow: true,
        }));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let debouncer = build_debouncer(&cfg, &state, wake.clone(), pending.clone(), false)
            .expect("build watcher");
        let task = tokio::spawn(run_pump(
            state.clone(),
            PumpState {
                debouncer: Some(debouncer),
                installed: HashMap::new(),
                roots: Vec::new(),
                degraded_backend: HashSet::new(),
            },
            wake.clone(),
            pending,
            TaskTracker::new(),
            PumpConfig {
                reconcile_interval: Duration::from_secs(3600),
                failure_reminder: Duration::ZERO,
                churn_warn_at: u32::MAX,
                churn_act_at: u32::MAX,
            },
            state.watch_registry().generation(),
        ));
        tokio::task::yield_now().await;
        wake.notify_one();
        // Stored file refs are canonical; macOS tempdirs are not.
        let file = std::fs::canonicalize(&file).expect("canonicalize missed path");
        let file_ref = file.to_str().expect("UTF-8 missed path");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state
                .store()
                .get_file_snapshot(&key, file_ref)
                .await
                .expect("reconciliation snapshot query")
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "overflow did not trigger full registration catch-up"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        state.shutdown_token().cancel();
        task.await.expect("watcher pump task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_probe_stops_before_live_watcher_remains_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.debounce_ms = 25;
        cfg.watch.reconcile_interval_secs = Some(3600);
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let root = tmp.path().join("watch-root");
        std::fs::create_dir_all(&root).expect("create watch root");
        cfg.corpora
            .push(corpus("watch", root.to_str().unwrap(), &["**/*.md"]));
        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");

        let probe = spawn_corpus_watcher(&state).expect("startup capability probe watcher");
        let probe_task = probe.task.abort_handle();
        assert!(
            !probe_task.is_finished(),
            "startup probe must create a live pump before shutdown"
        );
        probe.abort().await;
        assert!(
            probe_task.is_finished(),
            "startup probe abort must await its pump task"
        );

        let live = spawn_corpus_watcher(&state).expect("supervised live watcher");
        let live_task = live.task.abort_handle();
        assert!(
            !live_task.is_finished(),
            "supervised watcher must remain live after probe shutdown"
        );
        live.abort().await;
        assert!(
            live_task.is_finished(),
            "live watcher shutdown must await its pump task"
        );
    }

    /// `WatcherHandle::abort` must stop the old pump before a replacement is
    /// started; otherwise both pumps would continue scheduling reconciliations.
    #[tokio::test(start_paused = true)]
    async fn watcher_abort_stops_pump_before_replacement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.watch.reconcile_interval_secs = Some(1);
        cfg.daemon.maintenance_interval_secs = 0;
        cfg.daemon.idle_exit_secs = 0;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");

        let first = spawn_corpus_watcher(&state).expect("initial watcher");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let previous = state
            .heartbeat()
            .epoch(super::super::heartbeat::TaskName::WatcherPump);
        assert!(
            previous > 0,
            "initial watcher must complete a reconcile tick before replacement",
        );

        first.abort().await;

        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::WatcherPump),
            previous,
            "an aborted watcher must not complete further pump cycles",
        );
        let replacement = spawn_corpus_watcher(&state).expect("replacement watcher");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            state
                .heartbeat()
                .epoch(super::super::heartbeat::TaskName::WatcherPump)
                > previous,
            "the replacement watcher must complete pump cycles",
        );

        state.shutdown_token().cancel();
        replacement.join().await;
    }

    /// Regression test for finding 1: a catch-up pass parked on the write
    /// lane (held externally) must observe `shutdown_token().cancel()`
    /// instead of hanging until the lane frees up. Mirrors the
    /// select!-wrapped `acquire_lane` closure built inline in
    /// `spawn_registration_catch_up`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catch_up_lane_wait_observes_shutdown_cancellation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("docs");
        std::fs::create_dir_all(&root).expect("mkdir docs");
        std::fs::write(root.join("a.md"), "# A\n\nbody\n").expect("write a.md");
        let mut cfg = hallouminate_config::Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().join("ground").to_string_lossy().into_owned();
        let corpus_cfg = corpus("docs", root.to_str().unwrap(), &["**/*.md"]);
        cfg.corpora.push(corpus_cfg.clone());
        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");

        // Hold the sole write-lane permit externally so `acquire_write_lane`
        // blocks once the scan/plan phase finds work to apply.
        let held_permit = state
            .write_lane()
            .try_acquire_owned()
            .expect("acquire the sole write-lane permit");

        let res = state
            .resources_for(state.baseline())
            .await
            .expect("resources");
        let registry = state.make_registry();
        let lane_state = state.clone();
        let shutdown = state.shutdown_token().clone();
        let task = tokio::spawn(async move {
            super::super::dispatch::catch_up_corpus(&res, &registry, &corpus_cfg, || {
                let lane_state = lane_state.clone();
                let shutdown = shutdown.clone();
                async move {
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => {
                            Err("daemon shutting down; catch-up cancelled before taking the write lane")
                        }
                        permit = lane_state.acquire_write_lane() => permit,
                    }
                }
            })
            .await
        });

        // Let the scan/plan phase run and reach the lane wait, blocked on
        // the externally-held permit.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        state.shutdown_token().cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled catch-up must not hang on the held write-lane permit")
            .expect("task join");
        assert!(
            outcome.is_err(),
            "cancelled catch-up must return an error instead of applying the plan"
        );
        drop(held_permit);
    }
}
