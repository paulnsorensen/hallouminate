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
//! new registrations get `debouncer.watch()`'d and catch-up'd without a
//! watcher restart. `register_runtime_corpora` (called from
//! `dispatch::handle_ground`) registers request-resolved repo-layer corpora,
//! and `reload_repo_layer` (driven by the reconcile tick) re-resolves and
//! replaces a repo-layer source's registrations as its config changes.
//!
//! Concurrency (spec Risk): every reindex takes the same per-corpus lock +
//! global write-lane (`acquire_mutation_guard`) that `handle_index` /
//! `handle_add_markdown` take, so a watch-triggered reindex never races the
//! daemon's own writes.

mod registry;

pub(crate) use registry::{ConfigSource, WatchRegistry};

use std::collections::HashMap;
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

/// Owns the background debouncer + event-pump task. Dropping it stops the
/// watcher: the pump task owns the debouncer directly, so aborting/dropping
/// this handle's `_task` drops the debouncer transitively, which releases
/// every physical `notify` watch. Registration teardown rides on this drop —
/// there is no separate explicit "unregister everything" step.
pub struct WatcherHandle {
    _task: tokio::task::JoinHandle<()>,
    tracker: TaskTracker,
}

impl WatcherHandle {
    /// Await the pump task; used by the supervisor factory so a watcher
    /// restart rebuilds the whole debouncer + pump pair. Holds `self` (and
    /// so the debouncer, owned by the task) alive until the pump future
    /// completes.
    pub(crate) async fn join(self) {
        let result = self._task.await;
        self.tracker.close();
        self.tracker.wait().await;
        if let Err(join_err) = result {
            tracing::error!(target: "hallouminate::daemon", error = %join_err, "watcher: pump task ended abnormally");
            if join_err.is_panic() {
                std::panic::resume_unwind(join_err.into_panic());
            }
        }
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

/// Bundles the watcher pump's live-reconcile state: the installed
/// debouncer, the set of paths currently `debouncer.watch()`'d, and the
/// flattened current root list used by `process_change_batch`.
struct PumpState {
    debouncer: notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>,
    installed: std::collections::HashSet<PathBuf>,
    roots: Vec<WatchRoot>,
}

impl PumpState {
    /// Reconcile the live debouncer against the registry's current
    /// registrations: newly-registered roots get `debouncer.watch()`'d (or
    /// `mark_degraded` on failure), any registration whose catch-up hasn't
    /// started gets one spawned, and `self.roots` is refreshed to the
    /// flattened current root list for the caller's subsequent
    /// `process_change_batch` call.
    fn reconcile(&mut self, state: &DaemonState, tracker: &TaskTracker) {
        let registry = state.watch_registry();
        registry.refresh_roots(watch_roots_for);
        let snapshot = registry.snapshot_roots();
        let desired: std::collections::HashSet<PathBuf> = snapshot
            .iter()
            .map(|(_, root)| root.watched.clone())
            .collect();
        let obsolete: Vec<PathBuf> = self.installed.difference(&desired).cloned().collect();
        for path in obsolete {
            if let Err(error) = self.debouncer.unwatch(&path) {
                tracing::debug!(target: "hallouminate::daemon", path = %path.display(), error = %error, "watcher: obsolete watch removal failed");
            }
            self.installed.remove(&path);
        }
        self.roots = snapshot.iter().map(|(_, r)| r.clone()).collect();
        for (id, root) in &snapshot {
            if self.installed.contains(&root.watched) {
                continue;
            }
            let was_degraded = match registry.observation(id) {
                Some(registry::Observation::Degraded { .. }) => true,
                Some(registry::Observation::Watched) => false,
                None => false,
            };
            match self.debouncer.watch(&root.watched, root.mode) {
                Ok(()) => {
                    self.installed.insert(root.watched.clone());
                    registry.mark_watched(id);
                    if was_degraded {
                        tracing::info!(target: "hallouminate::daemon", corpus = %id.corpus_key.name, root = %root.watched.display(), "watcher: watch install recovered");
                    }
                }
                Err(error) => {
                    if !was_degraded {
                        tracing::warn!(target: "hallouminate::daemon", corpus = %id.corpus_key.name, root = %root.watched.display(), error = %error, "watcher: watch install failed; reconciliation continues");
                    }
                    registry.mark_degraded(id, error.to_string());
                }
            }
        }
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
        // Take the same per-corpus lock and global write-lane, in the same
        // order, that `handle_index` and `provisioner::provision_corpus`
        // take. A catch-up pass rewrites the corpus' rows, so without the
        // guard it races an explicit `index` or an `add_markdown` write.
        let outcome = match state.acquire_mutation_guard(&corpus.name).await {
            Ok(_guard) => match state.resources_for(&cfg).await {
                Ok(res) => {
                    let reg = state.make_registry();
                    match super::dispatch::catch_up_corpus(&res, &reg, &corpus).await {
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
            },
            Err(e) => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    corpus = %corpus.name,
                    error = %e,
                    "watcher: no mutation guard for registration catch-up; retry on recovery",
                );
                Err(e.to_string())
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
) -> Result<registry::ConfigSource, String> {
    let source = repo_path
        .map(|path| registry::ConfigSource::RepoLayer(path.to_path_buf()))
        .unwrap_or(registry::ConfigSource::Baseline);
    let cfg = std::sync::Arc::new(cfg.clone());
    for corpus in corpora {
        if state
            .watch_registry()
            .is_registered_unchanged(&source, corpus, &cfg)
        {
            continue;
        }
        let roots = watch_roots_for(corpus);
        match state
            .watch_registry()
            .register(source.clone(), corpus.clone(), cfg.clone(), roots)
        {
            registry::RegisterOutcome::Conflict(message) => return Err(message),
            registry::RegisterOutcome::LimitReached => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    corpus = %corpus.name,
                    "watcher: registration limit reached; corpus not watched",
                );
            }
            registry::RegisterOutcome::New | registry::RegisterOutcome::AlreadyRegistered => {}
        }
    }
    Ok(source)
}

/// Builds the notify debouncer that reindexes changed markdown files: each
/// debounced batch is folded into `pending` and `wake` is notified so the
/// pump loop picks it up on its next iteration. Returns `None` only when
/// the watcher backend itself fails to initialize.
fn build_debouncer(
    cfg: &hallouminate_config::Config,
    state: &DaemonState,
    wake: std::sync::Arc<tokio::sync::Notify>,
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
) -> Option<notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>> {
    let debounce = Duration::from_millis(cfg.watch.debounce_ms);
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
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "watcher: failed to create debouncer; auto-reindex disabled",
            );
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
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
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
            // `Notify::notify_waiters()` has no permit memory, so a
            // signal that arrives while nothing is `.await`ing it here is
            // lost. That is acceptable because the generation-gated
            // reconcile below runs on every `reconcile_tick`, which
            // bounds how stale a lost signal can get to
            // `watch.reconcile_interval_secs` -- that reconcile is the
            // correctness backstop; this arm is purely a latency
            // improvement for the common case.
            () = state.watch_registry().changed().notified() => {
                pump.reconcile(&state, &tracker);
                last_reconciled = state.watch_registry().generation();
                continue;
            }
            _ = reconcile_tick.tick() => {
                for path in state.watch_registry().repo_layer_sources() {
                    reload_repo_layer(&state, &path, &tracker, &mut reload_failures);
                }
                state.watch_registry().mark_reconcile_due_all();
                pump.reconcile(&state, &tracker);
                last_reconciled = state.watch_registry().generation();
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
        let paths: Vec<PathBuf> = {
            let mut set = pending.lock().expect("watch pending-paths mutex");
            set.drain().collect()
        };
        if !paths.is_empty() {
            process_change_batch(&state, &pump.roots, paths, &mut failures, &mut churn).await;
        }
    }
}

/// Watch every corpus root the `WatchRegistry` knows about and spawn a task
/// that reindexes changed markdown files (debounced by `cfg.watch.debounce_ms`)
/// and reconciles newly-registered roots. Seeds the boot baseline's corpora
/// into `state.watch_registry()` before installing any watches. Returns
/// `None` only when the watcher backend itself fails to initialize — an
/// empty baseline root set is not a failure, since runtime registrations may
/// arrive later.
pub fn spawn_corpus_watcher(state: &DaemonState) -> Option<WatcherHandle> {
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

    let pending: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());

    let debouncer = build_debouncer(cfg, state, wake.clone(), pending.clone())?;

    // Reconciled once here (installing the baseline watches just seeded
    // above) before the pump task takes ownership of `pump`.
    let mut pump = PumpState {
        debouncer,
        installed: std::collections::HashSet::new(),
        roots: Vec::new(),
    };
    let tracker = TaskTracker::new();
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

    Some(WatcherHandle {
        _task: task,
        tracker,
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
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut debouncer = build_debouncer(&cfg, &state, wake, pending).expect("build watcher");

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
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let pending_for_test = pending.clone();
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut debouncer = build_debouncer(&cfg, &state, wake, pending).expect("build watcher");
        debouncer
            .watch(&root, RecursiveMode::Recursive)
            .expect("watch root");
        async fn wait_for_paths(
            pending: &std::sync::Arc<
                std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
            >,
            expected: &[std::path::PathBuf],
        ) -> std::collections::HashSet<std::path::PathBuf> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut observed = std::collections::HashSet::new();
            loop {
                observed.extend(pending.lock().expect("pending mutex").drain());
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
        let atomic_events = wait_for_paths(&pending_for_test, std::slice::from_ref(&renamed)).await;
        assert!(
            atomic_events.contains(&renamed),
            "atomic save must report target path"
        );
        assert!(
            !atomic_events.contains(&temp),
            "unsupported atomic temp path must not reach pending"
        );
        std::fs::remove_file(&renamed).expect("delete file");
        assert!(
            wait_for_paths(&pending_for_test, std::slice::from_ref(&renamed))
                .await
                .contains(&renamed),
            "native delete must reach pending"
        );
    }
}
