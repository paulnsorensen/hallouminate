//! `WatchRegistry` — the durable, in-memory ledger of every logical watch
//! registration (baseline or runtime-discovered) and its live state:
//! whether `notify` is actually watching its roots, what paths changed since
//! the last catch-up pass, and whether a catch-up pass is currently running.
//!
//! One "logical registration" is keyed by `(source, corpus name)` — the same
//! corpus name registered from two different sources (e.g. the boot baseline
//! and a later repo-layer discovery) is two independent registrations, each
//! with its own roots, observation, and pending-work state. Re-registering an
//! already-known `(source, corpus name)` pair is a no-op that preserves the
//! existing registration's state (`RegisterOutcome::AlreadyRegistered`) —
//! callers never lose in-flight pending work or an in-progress catch-up by
//! registering the same logical corpus twice.
//!
//! Source-ownership and incompatible-binding rejection (what happens when two
//! *different* sources claim overlapping physical roots) is not this type's
//! job — `register_runtime_corpora` and `reload_repo_layer` in `watch/mod.rs`
//! own that; this module only tracks state for whatever registrations its
//! callers choose to make.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hallouminate_config::Config;
use hallouminate_domain::common::{CorpusConfig, CorpusKey};

use super::WatchRoot;

/// Who asked for a corpus to be watched. Distinguishes the boot baseline
/// (seeded once at watcher startup) from a later runtime discovery rooted at
/// a particular repo-layer config directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ConfigSource {
    Baseline,
    RepoLayer(PathBuf),
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Baseline => write!(f, "baseline"),
            ConfigSource::RepoLayer(path) => write!(f, "repo layer {}", path.display()),
        }
    }
}

/// Identifies one logical registration by source and semantic corpus identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RegistrationId {
    pub(crate) source: ConfigSource,
    pub(crate) corpus_key: CorpusKey,
}

fn corpus_key(corpus: &CorpusConfig, roots: &[WatchRoot]) -> CorpusKey {
    let root = roots
        .first()
        .map(|root| {
            root.canonical_file_root
                .clone()
                .unwrap_or_else(|| root.canonical_watched.clone())
        })
        .or_else(|| corpus.paths.first().map(PathBuf::from))
        .unwrap_or_default();
    CorpusKey {
        name: corpus.name.clone(),
        canonical_root: root,
    }
}

/// Whether the binding-relevant fields (ground_dir, embeddings model/
/// quantized/enabled) differ between two configs. Roots are compared
/// separately at each call site, since one call needs them ANDed in and the
/// other ORs them into a broader conflict check.
fn cfg_binding_differs(left: &Config, right: &Config) -> bool {
    left.storage.ground_dir != right.storage.ground_dir
        || left.embeddings.model != right.embeddings.model
        || left.embeddings.quantized != right.embeddings.quantized
        || left.embeddings.enabled != right.embeddings.enabled
}

fn registration_equivalent(
    existing: &Registration,
    corpus: &CorpusConfig,
    cfg: &Config,
    roots: &[WatchRoot],
) -> bool {
    existing.corpus == *corpus
        && existing.roots.as_slice() == roots
        && !cfg_binding_differs(&existing.cfg, cfg)
}

/// Whether a candidate `(id, cfg, roots)` binding conflicts with an existing
/// registration for the same corpus from a *different* source: same logical
/// corpus, incompatible storage/roots/embeddings binding. Returns the single
/// conflict message shared by every call site that rejects such a binding.
fn binding_conflict(
    guard: &Inner,
    id: &RegistrationId,
    cfg: &Config,
    roots: &[WatchRoot],
) -> Option<String> {
    guard.regs.iter().find_map(|(existing_id, existing)| {
        let conflicts = existing_id.corpus_key == id.corpus_key
            && existing_id.source != id.source
            && (existing.roots.as_slice() != roots || cfg_binding_differs(&existing.cfg, cfg));
        conflicts.then(|| {
            format!(
                "corpus {:?} has an incompatible watcher registration; use one root binding per corpus",
                id.corpus_key.name
            )
        })
    })
}
/// Whether a registration's roots are actually being watched by `notify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    Watched,
    Degraded { error: String },
}

/// Upper bound on the number of distinct paths a registration's
/// [`PendingWork::Paths`] tracks before collapsing to [`PendingWork::FullRoot`].
/// A registration touched this widely is cheaper to reconcile as "rescan
/// everything" than as a growing per-path set carried across catch-up
/// passes; overflow never drops a path, it only widens the retry.
const MAX_PENDING_PATHS: usize = 64;

/// Upper bound on the number of non-`Baseline` (i.e. runtime-discovered,
/// e.g. repo-layer) registrations the registry accepts. A single misbehaving
/// or maliciously large repo layer set must not let unbounded `register`
/// calls grow the ledger without limit; once at the cap, `register` reports
/// [`RegisterOutcome::LimitReached`] for a new corpus instead of inserting it.
const MAX_RUNTIME_REGISTRATIONS: usize = 256;

/// Paths that changed for a registration since its last completed catch-up
/// pass, distinct from the transient per-batch debounce-coalescing buffer in
/// `watch/mod.rs` (`record_pending`/`pending`): this is the durable ledger a
/// catch-up pass drains and, if it grew again mid-pass, drives a follow-up
/// pass against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PendingWork {
    #[default]
    Idle,
    Paths(HashSet<PathBuf>),
    FullRoot,
}

impl PendingWork {
    /// Adds `paths` to the pending set, collapsing to `FullRoot` once the
    /// distinct-path count would exceed `MAX_PENDING_PATHS`.
    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        match self {
            PendingWork::FullRoot => {}
            PendingWork::Idle => {
                let set: HashSet<PathBuf> = paths.into_iter().collect();
                if set.len() > MAX_PENDING_PATHS {
                    *self = PendingWork::FullRoot;
                } else if !set.is_empty() {
                    *self = PendingWork::Paths(set);
                }
            }
            PendingWork::Paths(existing) => {
                existing.extend(paths);
                if existing.len() > MAX_PENDING_PATHS {
                    *self = PendingWork::FullRoot;
                }
            }
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, PendingWork::Idle)
    }
}

/// Whether a registration's catch-up pass has run, is queued awaiting its
/// turn, or is actively running. `Queued` and `InFlight` are the two states
/// `WatchRegistry::begin_next_catch_up` cycles a registration through so
/// every registration gets a fair turn at the single daemon-wide catch-up
/// slot instead of one busy root monopolizing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CatchUpState {
    #[default]
    NotStarted,
    Queued,
    InFlight,
    Done,
}

struct Registration {
    corpus: CorpusConfig,
    cfg: Arc<Config>,
    roots: Vec<WatchRoot>,
    observation: Observation,
    pending: PendingWork,
    catch_up: CatchUpState,
    /// Set by `finish_catch_up` on a failed pass, cleared on the next
    /// success (`finish_catch_up(Ok)` is the sole clear site).
    last_error: Option<String>,
    /// Set by `finish_catch_up` on a failed pass; gates `begin_next_catch_up`
    /// from re-admitting this registration until `mark_reconcile_due_all`
    /// clears it on the next reconcile tick, preventing a failing pass from
    /// busy-looping.
    retry_on_tick: bool,
}

/// Result of a `register` call: whether this call created a new logical
/// registration or found an existing one for the same `(source, corpus
/// name)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    New,
    AlreadyRegistered,
    Conflict(String),
    LimitReached,
}

/// Retired registration data needed for fail-closed storage cleanup.
#[derive(Debug, Clone)]
pub(crate) struct RetiredRegistration {
    pub(crate) root: PathBuf,
    pub(crate) cfg: Arc<Config>,
}

/// Fair-scheduling and admission state shared by every registration.
/// `queue` holds ids that are `Queued`, in FIFO order; at most one
/// registration may be `InFlight` at a time, enforcing at most one running
/// catch-up pass across the whole daemon.
struct Inner {
    regs: HashMap<RegistrationId, Registration>,
    queue: VecDeque<RegistrationId>,
    active_group: Vec<RegistrationId>,
    active_leader: Option<RegistrationId>,
    active_leader_valid: bool,
    /// Bumped on every mutation that already notifies `changed`, plus every
    /// `refresh_roots` re-key. Lets the pump's per-iteration reconcile skip
    /// work when nothing in the registry moved since its last pass.
    generation: u64,
}

/// The live-registration ledger backing the watcher pump. Lives on
/// `DaemonState` for the process lifetime; `changed()` lets the pump wake up
/// promptly when a new registration arrives instead of waiting for its
/// periodic reconcile pass.
pub(crate) struct WatchRegistry {
    inner: Mutex<Inner>,
    changed: tokio::sync::Notify,
}

impl WatchRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                regs: HashMap::new(),
                queue: VecDeque::new(),
                active_group: Vec::new(),
                active_leader: None,
                active_leader_valid: false,
                generation: 0,
            }),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Seed a boot-baseline registration. `catch_up` starts `Done`: the
    /// boot-time `catch_up_index` supervised task (server.rs) already
    /// indexes every baseline corpus, so re-running catch-up here would only
    /// duplicate that work. Idempotent — re-seeding an already-present
    /// `(Baseline, name)` id is a no-op that leaves the existing
    /// registration's pending/observation state untouched.
    pub(crate) fn seed_baseline(
        &self,
        corpus: CorpusConfig,
        cfg: Arc<Config>,
        roots: Vec<WatchRoot>,
    ) {
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: corpus_key(&corpus, &roots),
        };
        let mut guard = self.lock();
        if guard.regs.contains_key(&id) {
            return;
        }
        guard.regs.insert(
            id,
            Registration {
                corpus,
                cfg,
                roots,
                observation: Observation::Watched,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::Done,
                last_error: None,
                retry_on_tick: false,
            },
        );
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
    }

    /// Register a runtime-discovered (e.g. repo-layer) corpus. `catch_up`
    /// starts `Queued` so the live pump catches it up. Returns
    /// `AlreadyRegistered` for a repeat `(source, corpus name)` pair without
    /// disturbing the existing registration's state.
    pub(crate) fn register(
        &self,
        source: ConfigSource,
        corpus: CorpusConfig,
        cfg: Arc<Config>,
        roots: Vec<WatchRoot>,
    ) -> RegisterOutcome {
        let id = RegistrationId {
            source,
            corpus_key: corpus_key(&corpus, &roots),
        };
        let mut guard = self.lock();
        let obsolete: Vec<RegistrationId> = guard
            .regs
            .keys()
            .filter(|existing| {
                existing.source == id.source
                    && existing.corpus_key.name == id.corpus_key.name
                    && existing.corpus_key != id.corpus_key
            })
            .cloned()
            .collect();
        let unchanged = guard
            .regs
            .get(&id)
            .is_some_and(|existing| registration_equivalent(existing, &corpus, &cfg, &roots));
        if unchanged {
            let existing = guard.regs.get_mut(&id).expect("checked above");
            existing.corpus = corpus;
            existing.cfg = cfg;
            return RegisterOutcome::AlreadyRegistered;
        }
        if let Some(message) = binding_conflict(&guard, &id, &cfg, &roots) {
            return RegisterOutcome::Conflict(message);
        }
        let is_baseline = match &id.source {
            ConfigSource::Baseline => true,
            ConfigSource::RepoLayer(_) => false,
        };
        if !is_baseline {
            let runtime_count = guard
                .regs
                .keys()
                .filter(|existing| {
                    let existing_is_baseline = match &existing.source {
                        ConfigSource::Baseline => true,
                        ConfigSource::RepoLayer(_) => false,
                    };
                    !existing_is_baseline
                })
                .count();
            let replaces_runtime = guard.regs.contains_key(&id)
                || obsolete
                    .iter()
                    .any(|existing| matches!(existing.source, ConfigSource::RepoLayer(_)));
            if runtime_count >= MAX_RUNTIME_REGISTRATIONS && !replaces_runtime {
                return RegisterOutcome::LimitReached;
            }
        }
        for obsolete_id in obsolete {
            guard.regs.remove(&obsolete_id);
            guard.queue.retain(|queued| queued != &obsolete_id);
            release_admission(&mut guard, &obsolete_id);
        }
        guard.regs.remove(&id);
        release_admission(&mut guard, &id);
        guard.queue.retain(|queued| queued != &id);
        guard.regs.insert(
            id.clone(),
            Registration {
                corpus,
                cfg,
                roots,
                observation: Observation::Watched,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::Queued,
                last_error: None,
                retry_on_tick: false,
            },
        );
        guard.queue.push_back(id);
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
        RegisterOutcome::New
    }

    /// Whether `(source, corpus.name)` already has an equivalent registration.
    ///
    /// Equivalence includes corpus paths, selection rules, watched roots,
    /// storage settings, and embedding settings.
    pub(crate) fn is_registered_unchanged(
        &self,
        source: &ConfigSource,
        corpus: &CorpusConfig,
        cfg: &Config,
        roots: &[WatchRoot],
    ) -> bool {
        self.lock().regs.iter().any(|(id, existing)| {
            id.source == *source
                && id.corpus_key.name == corpus.name
                && registration_equivalent(existing, corpus, cfg, roots)
        })
    }

    /// Wakes the pump so it reconciles promptly instead of waiting for its
    /// periodic backstop pass. Uses `notify_one` (permit-carrying), not
    /// `notify_waiters`: the registry has exactly one consumer (the watcher
    /// pump's `run_pump` select loop), and a signal fired while that loop is
    /// busy elsewhere must be observed on its next iteration rather than
    /// lost, which `notify_waiters` would do because it has no permit
    /// memory for a task that isn't parked on `.notified()` at the moment
    /// the signal fires.
    pub(crate) fn changed(&self) -> &tokio::sync::Notify {
        &self.changed
    }

    fn signal_changed(&self) {
        self.changed.notify_one();
    }

    /// Monotonic counter bumped on every registry mutation that notifies
    /// `changed` (plus `refresh_roots` re-keys). The pump uses this to skip
    /// a redundant reconcile pass when nothing moved since its last one.
    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(crate) fn refresh_roots<F>(&self, build: F)
    where
        F: Fn(&CorpusConfig) -> Vec<WatchRoot>,
    {
        let snapshot: Vec<(RegistrationId, CorpusConfig)> = {
            let guard = self.lock();
            guard
                .regs
                .iter()
                .map(|(id, registration)| (id.clone(), registration.corpus.clone()))
                .collect()
        };
        let built: Vec<(RegistrationId, Vec<WatchRoot>)> = snapshot
            .into_iter()
            .map(|(id, corpus)| {
                let roots = build(&corpus);
                (id, roots)
            })
            .collect();

        let mut guard = self.lock();
        let stale: Vec<(RegistrationId, RegistrationId, Vec<WatchRoot>)> = built
            .iter()
            .filter_map(|(id, roots)| {
                let registration = guard.regs.get(id)?;
                let new_key = corpus_key(&registration.corpus, roots);
                if new_key == id.corpus_key {
                    return None;
                }
                Some((
                    id.clone(),
                    RegistrationId {
                        source: id.source.clone(),
                        corpus_key: new_key,
                    },
                    roots.clone(),
                ))
            })
            .collect();
        for (old_id, new_id, roots) in stale {
            let Some(mut registration) = guard.regs.remove(&old_id) else {
                continue;
            };
            registration.roots = roots;
            if registration.catch_up == CatchUpState::InFlight {
                registration.catch_up = CatchUpState::Queued;
                release_admission(&mut guard, &old_id);
                guard.queue.push_back(new_id.clone());
            }
            guard.regs.insert(new_id.clone(), registration);
            for queued in guard.queue.iter_mut() {
                if *queued == old_id {
                    *queued = new_id.clone();
                }
            }
            guard.generation += 1;
        }
        for (id, roots) in &built {
            if let Some(registration) = guard.regs.get_mut(id)
                && registration.roots != *roots
            {
                registration.roots = roots.clone();
            }
        }
    }

    /// Every registration owning `root`, resolved under one lock without
    /// cloning every registration's roots (unlike `snapshot_roots`, which
    /// materializes and sorts the whole flattened root list).
    pub(crate) fn registrations_for_root(&self, root: &WatchRoot) -> Vec<RegistrationId> {
        self.lock()
            .regs
            .iter()
            .filter(|(_, reg)| reg.roots.iter().any(|owned| owned == root))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every root across every registration, flattened, paired with the
    /// registration that owns it.
    pub(crate) fn snapshot_roots(&self) -> Vec<(RegistrationId, WatchRoot)> {
        let mut roots: Vec<_> = self
            .lock()
            .regs
            .iter()
            .flat_map(|(id, reg)| reg.roots.iter().map(move |root| (id.clone(), root.clone())))
            .collect();
        roots.sort_by(|(left, _), (right, _)| {
            left.corpus_key
                .name
                .cmp(&right.corpus_key.name)
                .then_with(|| {
                    left.corpus_key
                        .canonical_root
                        .cmp(&right.corpus_key.canonical_root)
                })
                .then_with(|| left.source.cmp(&right.source))
        });
        roots
    }

    pub(crate) fn replace_source(
        &self,
        source: ConfigSource,
        corpora: Vec<CorpusConfig>,
        cfg: Arc<Config>,
        build_roots: impl Fn(&CorpusConfig) -> Vec<WatchRoot>,
    ) -> Result<Vec<RetiredRegistration>, String> {
        let candidates: Vec<_> = corpora
            .into_iter()
            .map(|corpus| {
                let roots = build_roots(&corpus);
                let id = RegistrationId {
                    source: source.clone(),
                    corpus_key: corpus_key(&corpus, &roots),
                };
                (id, corpus, roots)
            })
            .collect();
        let mut guard = self.lock();
        for (id, _, roots) in &candidates {
            if let Some(message) = binding_conflict(&guard, id, &cfg, roots) {
                return Err(message);
            }
        }
        let candidate_ids: HashSet<_> = candidates.iter().map(|(id, ..)| id.clone()).collect();
        let removed: HashSet<_> = guard
            .regs
            .keys()
            .filter(|id| id.source == source && !candidate_ids.contains(*id))
            .cloned()
            .collect();
        let retired = removed
            .iter()
            .filter_map(|id| {
                guard.regs.get(id).map(|registration| RetiredRegistration {
                    root: id.corpus_key.canonical_root.clone(),
                    cfg: registration.cfg.clone(),
                })
            })
            .collect();
        for id in &removed {
            guard.regs.remove(id);
            release_admission(&mut guard, id);
        }
        guard.queue.retain(|id| !removed.contains(id));
        for (id, corpus, roots) in candidates {
            if let Some(existing) = guard.regs.get_mut(&id)
                && registration_equivalent(existing, &corpus, &cfg, &roots)
            {
                existing.corpus = corpus;
                existing.cfg = cfg.clone();
                continue;
            }
            if guard
                .regs
                .get(&id)
                .is_some_and(|existing| existing.catch_up == CatchUpState::InFlight)
            {
                release_admission(&mut guard, &id);
            }
            guard.queue.retain(|queued| queued != &id);
            guard.regs.insert(
                id.clone(),
                Registration {
                    corpus,
                    cfg: cfg.clone(),
                    roots,
                    observation: Observation::Watched,
                    pending: PendingWork::Idle,
                    catch_up: CatchUpState::Queued,
                    last_error: None,
                    retry_on_tick: false,
                },
            );
            guard.queue.push_back(id);
        }
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
        Ok(retired)
    }

    pub(crate) fn repo_layer_sources(&self) -> Vec<PathBuf> {
        let paths: std::collections::BTreeSet<PathBuf> = self
            .lock()
            .regs
            .keys()
            .filter_map(|id| match &id.source {
                ConfigSource::RepoLayer(path) => Some(path.clone()),
                ConfigSource::Baseline => None,
            })
            .collect();
        paths.into_iter().collect()
    }
    /// Record paths as pending work for a registration's durable ledger,
    /// distinct from `watch/mod.rs`'s transient per-batch buffer, and queue
    /// it for a catch-up pass if it wasn't already due for one.
    pub(crate) fn record_pending(
        &self,
        id: &RegistrationId,
        paths: impl IntoIterator<Item = PathBuf>,
    ) {
        let mut guard = self.lock();
        if let Some(reg) = guard.regs.get_mut(id) {
            reg.pending.add_paths(paths);
        }
        Self::mark_dirty_locked(&mut guard, id);
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
    }

    pub(crate) fn mark_degraded(&self, id: &RegistrationId, error: String) {
        if let Some(reg) = self.lock().regs.get_mut(id) {
            reg.observation = Observation::Degraded { error };
        }
    }

    pub(crate) fn mark_watched(&self, id: &RegistrationId) {
        if let Some(reg) = self.lock().regs.get_mut(id) {
            reg.observation = Observation::Watched;
        }
    }

    pub(crate) fn observation(&self, id: &RegistrationId) -> Option<Observation> {
        self.lock().regs.get(id).map(|reg| reg.observation.clone())
    }

    /// Whether `id`'s last catch-up pass failed and hasn't been superseded
    /// by a successful one yet; used to log a recovery transition instead
    /// of staying silent once the pass finally succeeds.
    pub(crate) fn has_error(&self, id: &RegistrationId) -> bool {
        self.lock()
            .regs
            .get(id)
            .is_some_and(|reg| reg.last_error.is_some())
    }

    #[cfg(test)]
    pub(crate) fn pending(&self, id: &RegistrationId) -> Option<PendingWork> {
        self.lock().regs.get(id).map(|reg| reg.pending.clone())
    }

    #[cfg(test)]
    pub(crate) fn catch_up_state(&self, id: &RegistrationId) -> Option<CatchUpState> {
        self.lock().regs.get(id).map(|reg| reg.catch_up)
    }

    /// Returns actionable recovery warnings for registrations with incomplete
    /// work, scoped to `source` so a ground call only ever sees warnings for
    /// its own registrations, never another workspace's root path or error.
    pub(crate) fn recovery_warnings_for(
        &self,
        source: &ConfigSource,
        queried: &[CorpusConfig],
    ) -> Vec<(String, String)> {
        let names: HashSet<&str> = queried.iter().map(|corpus| corpus.name.as_str()).collect();
        let guard = self.lock();
        guard
            .regs
            .iter()
            .filter(|(id, reg)| id.source == *source && names.contains(reg.corpus.name.as_str()))
            .filter_map(|(_, reg)| {
                let root = reg
                    .roots
                    .first()
                    .map(|root| root.canonical_watched.display().to_string())
                    .or_else(|| reg.corpus.paths.first().cloned())
                    .unwrap_or_else(|| "<no root>".to_string());
                let state = match reg.catch_up {
                    CatchUpState::NotStarted => "not-started",
                    CatchUpState::Queued => "queued",
                    CatchUpState::InFlight => "in-flight",
                    CatchUpState::Done => "done",
                };
                let mut clauses = Vec::new();
                if let Some(error) = &reg.last_error {
                    clauses.push(format!("failed reconciliation: {error}"));
                }
                if let Observation::Degraded { error } = &reg.observation {
                    clauses.push(format!(
                        "watcher backend error: {error}; retry will continue"
                    ));
                }
                if !reg.pending.is_idle() {
                    clauses.push("reconciliation pending".to_string());
                }
                // A queued or in-flight pass with no dirty paths and no
                // error is still incomplete work: the caller cannot rely on
                // the index, or on a notify watch, until it finishes.
                if clauses.is_empty() && reg.catch_up != CatchUpState::Done {
                    clauses.push("reconciliation not complete".to_string());
                }
                if clauses.is_empty() {
                    return None;
                }
                let message = format!(
                    "root {root}; recovery state {state}; {}",
                    clauses.join("; ")
                );
                Some((reg.corpus.name.clone(), message))
            })
            .collect()
    }

    /// Queue every retained registration whose catch-up isn't already
    /// queued or running, so the next admission cycle re-reconciles it even
    /// when no filesystem event was ever recorded for it. This is the
    /// periodic reconcile timer's entry point: recovering a `notify` event
    /// the OS silently dropped. A registration already `Queued` or
    /// `InFlight` is left alone -- ticking must not pile up duplicate work
    /// on a pass that is already pending or running.
    pub(crate) fn mark_reconcile_due_all(&self) {
        let mut guard = self.lock();
        let ids: Vec<RegistrationId> = guard.regs.keys().cloned().collect();
        for id in ids {
            if let Some(reg) = guard.regs.get_mut(&id) {
                reg.retry_on_tick = false;
            }
            Self::mark_dirty_locked(&mut guard, &id);
        }
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
    }

    /// Promotes `NotStarted`/`Done` to `Queued` and appends `id` to the
    /// fair-share queue. A no-op when `id` is unknown, already `Queued`
    /// (avoids a duplicate queue entry), or `InFlight` (its follow-up rides
    /// on `pending` and gets queued when the running pass finishes).
    fn mark_dirty_locked(guard: &mut Inner, id: &RegistrationId) {
        let Some(reg) = guard.regs.get_mut(id) else {
            return;
        };
        if matches!(reg.catch_up, CatchUpState::NotStarted | CatchUpState::Done) {
            reg.catch_up = CatchUpState::Queued;
            guard.queue.push_back(id.clone());
        }
    }

    /// Pops the next `Queued` id in FIFO order and starts it, respecting the
    /// single-active-pass gate. This is what gives registrations a fair
    /// turn: `finish_catch_up` appends a registration with follow-up work to
    /// the *back* of the queue rather than re-running it immediately, so a
    /// busy root can't starve a quieter one queued behind it. A registration
    /// with `retry_on_tick` set (a previous pass failed) is skipped and
    /// re-queued rather than admitted, so the reconcile tick's
    /// `mark_reconcile_due_all` -- not immediate re-admission -- is what
    /// retries a failing registration; this is what keeps a persistently
    /// failing pass from busy-looping.
    pub(crate) fn begin_next_catch_up(&self) -> Option<RegistrationId> {
        let mut guard = self.lock();
        if guard.active_leader.is_some() {
            return None;
        }
        let mut gated: Vec<RegistrationId> = Vec::new();
        let admitted = loop {
            let Some(id) = guard.queue.pop_front() else {
                break None;
            };
            let Some(reg) = guard.regs.get_mut(&id) else {
                continue;
            };
            if reg.catch_up != CatchUpState::Queued {
                continue;
            }
            if reg.retry_on_tick {
                gated.push(id);
                continue;
            }
            let Some(leader) = guard.regs.get(&id) else {
                continue;
            };
            let leader_corpus = leader.corpus.clone();
            let leader_roots = leader.roots.clone();
            let leader_cfg = leader.cfg.clone();
            guard
                .regs
                .get_mut(&id)
                .expect("queued registration must exist")
                .catch_up = CatchUpState::InFlight;
            guard.active_leader = Some(id.clone());
            guard.active_leader_valid = true;
            guard.active_group.push(id.clone());
            let group_members: Vec<RegistrationId> = guard
                .queue
                .iter()
                .filter(|candidate| {
                    guard.regs.get(*candidate).is_some_and(|member| {
                        member.catch_up == CatchUpState::Queued
                            && member.corpus == leader_corpus
                            && member.roots == leader_roots
                            && !cfg_binding_differs(&member.cfg, &leader_cfg)
                    })
                })
                .cloned()
                .collect();
            for member_id in group_members {
                if let Some(member) = guard.regs.get_mut(&member_id) {
                    member.catch_up = CatchUpState::InFlight;
                }
                guard.queue.retain(|candidate| candidate != &member_id);
                guard.active_group.push(member_id);
            }
            break Some(id);
        };
        guard.queue.extend(gated);
        admitted
    }

    /// Marks catch-up finished for `id`, releasing the daemon-wide slot and
    /// returning (clearing) whatever pending work accumulated while `id` ran.
    /// The returned `PendingWork` is always `id`'s own. When `id` leads a
    /// group of compatible registrations, finishing releases the group's
    /// shared admission slot and re-queues every member that has pending
    /// work, but each member keeps its own pending/`last_error` state and
    /// none of it is folded into the return value.
    ///
    /// A non-`Idle` return means events arrived mid-catch-up (or the pass
    /// itself failed) and `id` has already been re-queued for a follow-up
    /// pass; it does not re-run inline here, so a failing pass cannot
    /// busy-loop -- it waits its turn like any other queued registration.
    ///
    /// `outcome` is `Err` when the catch-up pass itself failed (the
    /// previously-swallowed `catch_up_corpus` error). A failure escalates
    /// pending work to `FullRoot`: a partial pass leaves no reliable record
    /// of which paths it already covered, so the safe retry is "reconcile
    /// the whole root again".
    pub(crate) fn finish_catch_up(
        &self,
        id: &RegistrationId,
        outcome: Result<(), String>,
    ) -> PendingWork {
        let mut guard = self.lock();
        let members: Vec<RegistrationId> = if guard.active_leader.as_ref() == Some(id) {
            let leader_valid = guard.active_leader_valid;
            guard.active_leader = None;
            guard.active_leader_valid = false;
            if guard.active_group.is_empty() {
                if leader_valid && guard.regs.contains_key(id) {
                    vec![id.clone()]
                } else {
                    Vec::new()
                }
            } else {
                std::mem::take(&mut guard.active_group)
            }
        } else if guard.regs.contains_key(id) {
            vec![id.clone()]
        } else {
            return PendingWork::Idle;
        };
        let mut returned = PendingWork::Idle;
        for member_id in members {
            let Some(reg) = guard.regs.get_mut(&member_id) else {
                continue;
            };
            if member_id != *id {
                // A group member other than the one `finish_catch_up` was
                // called for didn't itself just run a catch-up pass; only
                // its admission slot is released here, not its logical
                // state (pending/last_error stay per registration).
                if reg.pending.is_idle() {
                    reg.catch_up = CatchUpState::Done;
                } else {
                    reg.catch_up = CatchUpState::Queued;
                    guard.queue.push_back(member_id.clone());
                }
                continue;
            }
            let pending = match &outcome {
                Ok(()) => {
                    reg.last_error = None;
                    let pending = std::mem::take(&mut reg.pending);
                    if pending.is_idle() {
                        reg.catch_up = CatchUpState::Done;
                    } else {
                        reg.catch_up = CatchUpState::Queued;
                        guard.queue.push_back(member_id.clone());
                    }
                    pending
                }
                Err(error) => {
                    reg.last_error = Some(error.clone());
                    reg.retry_on_tick = true;
                    reg.pending = PendingWork::FullRoot;
                    reg.catch_up = CatchUpState::Queued;
                    guard.queue.push_back(member_id.clone());
                    PendingWork::FullRoot
                }
            };
            returned = pending;
        }
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
        returned
    }

    pub(crate) fn config_for(&self, id: &RegistrationId) -> Option<(CorpusConfig, Arc<Config>)> {
        self.lock()
            .regs
            .get(id)
            .map(|reg| (reg.corpus.clone(), reg.cfg.clone()))
    }
}

/// Drops `id` from the in-flight admission group, invalidating the leader slot
/// when `id` was leading it so the next `begin_next_catch_up` can re-admit.
fn release_admission(guard: &mut Inner, id: &RegistrationId) {
    if let Some(position) = guard.active_group.iter().position(|active| active == id) {
        guard.active_group.remove(position);
    }
    if guard.active_leader.as_ref() == Some(id) {
        guard.active_leader_valid = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(name: &str) -> CorpusConfig {
        CorpusConfig {
            name: name.into(),
            paths: vec![],
            globs: vec![],
            exclude: vec![],
            global: false,
        }
    }

    fn root(watched: &str, corpus: CorpusConfig) -> WatchRoot {
        WatchRoot {
            watched: PathBuf::from(watched),
            canonical_watched: PathBuf::from(watched),
            corpus,
            canonical_file_root: None,
            mode: notify::RecursiveMode::Recursive,
        }
    }

    fn baseline_id(name: &str) -> RegistrationId {
        RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: name.into(),
                canonical_root: PathBuf::new(),
            },
        }
    }

    #[test]
    fn register_twice_same_source_and_name_is_idempotent() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let outcome1 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            vec![root("/a", corpus("wiki"))],
        );
        let outcome2 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg,
            vec![root("/a", corpus("wiki"))],
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        assert_eq!(outcome2, RegisterOutcome::AlreadyRegistered);
        assert_eq!(registry.snapshot_roots().len(), 1);
    }

    #[test]
    fn same_id_replacement_is_admitted_at_runtime_cap() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for i in 0..MAX_RUNTIME_REGISTRATIONS {
            registry.register(
                ConfigSource::RepoLayer(PathBuf::from(format!("/repo-{i}"))),
                corpus(&format!("corpus-{i}")),
                cfg.clone(),
                vec![],
            );
        }
        let outcome = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo-0")),
            corpus("corpus-0"),
            Arc::new(Config::default()),
            vec![root("/replacement", corpus("corpus-0"))],
        );
        assert_eq!(outcome, RegisterOutcome::New);
    }

    #[test]
    fn runtime_registration_limit_rejects_beyond_cap_without_advancing_generation() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for i in 0..MAX_RUNTIME_REGISTRATIONS {
            let outcome = registry.register(
                ConfigSource::RepoLayer(PathBuf::from(format!("/repo-{i}"))),
                corpus(&format!("corpus-{i}")),
                cfg.clone(),
                vec![],
            );
            assert_eq!(outcome, RegisterOutcome::New);
        }
        let generation_before = registry.generation();

        let outcome = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo-over-cap")),
            corpus("corpus-over-cap"),
            cfg,
            vec![],
        );

        assert_eq!(outcome, RegisterOutcome::LimitReached);
        assert_eq!(registry.generation(), generation_before);
    }

    #[test]
    fn register_same_name_different_sources_yields_two_registrations() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let outcome1 =
            registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        let outcome2 = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/a")),
            corpus("wiki"),
            cfg,
            vec![],
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        assert_eq!(outcome2, RegisterOutcome::New);
        let repo_id = RegistrationId {
            source: ConfigSource::RepoLayer(PathBuf::from("/a")),
            corpus_key: corpus_key(&corpus("wiki"), &[]),
        };
        assert!(registry.config_for(&baseline_id("wiki")).is_some());
        assert!(registry.config_for(&repo_id).is_some());
    }

    #[test]
    fn compatible_cross_source_registration_ignores_unrelated_config_fields() {
        let registry = WatchRegistry::new();
        let mut baseline_cfg = Config::default();
        baseline_cfg.search.limit_default = 10;
        let mut repo_cfg = baseline_cfg.clone();
        repo_cfg.search.limit_default = 100;
        let outcome1 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(baseline_cfg),
            vec![root("/a", corpus("wiki"))],
        );
        let outcome2 = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo")),
            corpus("wiki"),
            Arc::new(repo_cfg),
            vec![root("/a", corpus("wiki"))],
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        assert_eq!(outcome2, RegisterOutcome::New);

        let key = corpus_key(&corpus("wiki"), &[root("/a", corpus("wiki"))]);
        let baseline_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: key.clone(),
        };
        let repo_id = RegistrationId {
            source: ConfigSource::RepoLayer(PathBuf::from("/repo")),
            corpus_key: key,
        };
        let (_, baseline_resolved) = registry
            .config_for(&baseline_id)
            .expect("baseline id must resolve independently");
        let (_, repo_resolved) = registry
            .config_for(&repo_id)
            .expect("repo id must resolve independently");
        assert_eq!(baseline_resolved.search.limit_default, 10);
        assert_eq!(repo_resolved.search.limit_default, 100);
    }

    #[test]
    fn cross_source_conflict_on_differing_embeddings_model() {
        let registry = WatchRegistry::new();
        let mut baseline_cfg = Config::default();
        baseline_cfg.embeddings.model = "model-a".into();
        let mut repo_cfg = baseline_cfg.clone();
        repo_cfg.embeddings.model = "model-b".into();
        let roots = vec![root("/a", corpus("wiki"))];
        let outcome1 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(baseline_cfg),
            roots.clone(),
        );
        let outcome2 = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo")),
            corpus("wiki"),
            Arc::new(repo_cfg),
            roots,
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        assert_eq!(
            outcome2,
            RegisterOutcome::Conflict(
                "corpus \"wiki\" has an incompatible watcher registration; use one root binding per corpus"
                    .to_string()
            )
        );
    }

    #[test]
    fn rejected_replacement_preserves_previous_registration_and_pending_work() {
        let registry = WatchRegistry::new();
        let mut baseline_config = Config::default();
        baseline_config.embeddings.model = "model-a".into();
        let cfg = Arc::new(baseline_config);
        let repo_source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            vec![root("/b", corpus("wiki"))],
        );
        registry.register(
            repo_source.clone(),
            corpus("wiki"),
            cfg.clone(),
            vec![root("/a", corpus("wiki"))],
        );
        let old_id = RegistrationId {
            source: repo_source.clone(),
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/a"),
            },
        };
        registry.record_pending(&old_id, [PathBuf::from("/a/changed.md")]);

        let mut replacement_config = (*cfg).clone();
        replacement_config.embeddings.model = "model-b".into();
        let outcome = registry.register(
            repo_source,
            corpus("wiki"),
            Arc::new(replacement_config),
            vec![root("/b", corpus("wiki"))],
        );

        assert!(matches!(outcome, RegisterOutcome::Conflict(_)));
        assert_eq!(
            registry.pending(&old_id),
            Some(PendingWork::Paths(HashSet::from([PathBuf::from(
                "/a/changed.md"
            )]))),
            "a rejected replacement must preserve the old registration state"
        );
    }

    #[test]
    fn seed_baseline_is_idempotent_and_preserves_pending_across_reseeding() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.seed_baseline(corpus("wiki"), cfg.clone(), vec![]);
        let id = baseline_id("wiki");
        let path = PathBuf::from("/a/changed.md");
        registry.record_pending(&id, [path.clone()]);

        // Simulate a pump restart re-seeding the same baseline registration.
        registry.seed_baseline(corpus("wiki"), cfg, vec![]);

        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::Paths(HashSet::from([path]))),
            "re-seeding an existing baseline registration must not reset its pending work"
        );
    }

    #[test]
    fn compatible_registrations_share_one_admission_and_finish_independently() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let roots = vec![root("/repo", corpus("wiki"))];
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            roots.clone(),
        );
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate"));
        registry.register(source.clone(), corpus("wiki"), cfg, roots);
        let baseline_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        let source_id = RegistrationId {
            source,
            corpus_key: baseline_id.corpus_key.clone(),
        };

        assert_eq!(registry.begin_next_catch_up(), Some(baseline_id.clone()));
        assert_eq!(registry.begin_next_catch_up(), None);
        registry.record_pending(&source_id, [PathBuf::from("/repo/mid.md")]);
        registry.finish_catch_up(&baseline_id, Ok(()));

        assert_eq!(
            registry.catch_up_state(&baseline_id),
            Some(CatchUpState::Done)
        );
        assert_eq!(
            registry.catch_up_state(&source_id),
            Some(CatchUpState::Queued)
        );
        assert_eq!(
            registry.pending(&source_id),
            Some(PendingWork::Paths(HashSet::from([PathBuf::from(
                "/repo/mid.md"
            )])))
        );
    }

    #[test]
    fn group_leader_finish_returns_only_its_own_pending() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let roots = vec![root("/repo", corpus("wiki"))];
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            roots.clone(),
        );
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate"));
        registry.register(source.clone(), corpus("wiki"), cfg, roots);
        let baseline_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        let source_id = RegistrationId {
            source,
            corpus_key: baseline_id.corpus_key.clone(),
        };

        assert_eq!(registry.begin_next_catch_up(), Some(baseline_id.clone()));
        registry.record_pending(&baseline_id, [PathBuf::from("/repo/leader.md")]);
        registry.record_pending(&source_id, [PathBuf::from("/repo/member.md")]);

        assert_eq!(
            registry.finish_catch_up(&baseline_id, Ok(())),
            PendingWork::Paths(HashSet::from([PathBuf::from("/repo/leader.md")]))
        );
        assert_eq!(registry.pending(&baseline_id), Some(PendingWork::Idle));
        assert_eq!(
            registry.pending(&source_id),
            Some(PendingWork::Paths(HashSet::from([PathBuf::from(
                "/repo/member.md"
            )])))
        );
    }

    #[test]
    fn retired_group_member_keeps_slot_until_leader_finishes() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let roots = vec![root("/repo", corpus("wiki"))];
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            roots.clone(),
        );
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate"));
        registry.register(source.clone(), corpus("wiki"), cfg.clone(), roots);
        registry.register(ConfigSource::Baseline, corpus("other"), cfg, vec![]);
        let leader = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        let member = RegistrationId {
            source: source.clone(),
            corpus_key: leader.corpus_key.clone(),
        };
        let other = baseline_id("other");

        assert_eq!(registry.begin_next_catch_up(), Some(leader.clone()));
        registry
            .replace_source(source, vec![], Arc::new(Config::default()), |_| vec![])
            .unwrap();
        assert_eq!(registry.begin_next_catch_up(), None);
        assert_eq!(registry.finish_catch_up(&leader, Ok(())), PendingWork::Idle);
        assert_eq!(registry.catch_up_state(&member), None);
        assert_eq!(registry.begin_next_catch_up(), Some(other));
    }

    #[test]
    fn replaced_group_member_does_not_receive_old_leader_failure() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let roots = vec![root("/repo", corpus("wiki"))];
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            roots.clone(),
        );
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate"));
        registry.register(source.clone(), corpus("wiki"), cfg.clone(), roots.clone());
        let leader = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        let member = RegistrationId {
            source: source.clone(),
            corpus_key: leader.corpus_key.clone(),
        };
        assert_eq!(registry.begin_next_catch_up(), Some(leader.clone()));

        registry
            .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
            .unwrap();
        let replacement = corpus("wiki");
        let replacement_roots = vec![root("/repo", replacement.clone())];
        assert_eq!(
            registry.register(source, replacement, cfg, replacement_roots),
            RegisterOutcome::New
        );
        assert_eq!(registry.begin_next_catch_up(), None);
        registry.finish_catch_up(&leader, Err("old failure".into()));
        assert_eq!(registry.catch_up_state(&member), Some(CatchUpState::Queued));
        assert_eq!(registry.pending(&member), Some(PendingWork::Idle));
    }

    #[test]
    fn same_source_selection_change_replaces_registration_state() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        let initial = corpus("wiki");
        let initial_root = root("/repo", initial.clone());
        assert_eq!(
            registry.register(source.clone(), initial, cfg.clone(), vec![initial_root]),
            RegisterOutcome::New
        );
        let id = RegistrationId {
            source: source.clone(),
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        registry.record_pending(&id, [PathBuf::from("/repo/old.md")]);
        let mut changed = corpus("wiki");
        changed.globs = vec!["**/*.rs".into()];
        let changed_root = root("/repo", changed.clone());
        assert_eq!(
            registry.register(source, changed, cfg, vec![changed_root]),
            RegisterOutcome::New
        );
        assert_eq!(registry.pending(&id), Some(PendingWork::Idle));
    }

    #[test]
    fn incompatible_selection_rules_do_not_share_admission() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let roots = vec![root("/repo", corpus("wiki"))];
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            cfg.clone(),
            roots.clone(),
        );
        let mut selected = corpus("wiki");
        selected.globs = vec!["**/*.rs".into()];
        let selected_roots = vec![root("/repo", selected.clone())];
        let outcome = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate")),
            selected,
            cfg,
            selected_roots,
        );

        assert_eq!(
            outcome,
            RegisterOutcome::Conflict(
                "corpus \"wiki\" has an incompatible watcher registration; use one root binding per corpus"
                    .to_string()
            )
        );
        assert_eq!(
            registry.begin_next_catch_up(),
            Some(RegistrationId {
                source: ConfigSource::Baseline,
                corpus_key: CorpusKey {
                    name: "wiki".into(),
                    canonical_root: PathBuf::from("/repo"),
                },
            })
        );
        assert_eq!(registry.begin_next_catch_up(), None);
    }

    #[test]
    fn mark_degraded_preserves_registration_and_pending() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        let path = PathBuf::from("/a/changed.md");
        registry.record_pending(&id, [path.clone()]);

        registry.mark_degraded(&id, "watch failed".into());

        assert_eq!(
            registry.observation(&id),
            Some(Observation::Degraded {
                error: "watch failed".into()
            })
        );
        assert!(
            registry.config_for(&id).is_some(),
            "marking a registration degraded must not drop its config binding"
        );
        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::Paths(HashSet::from([path]))),
            "marking a registration degraded must not disturb its pending work"
        );
    }

    #[test]
    fn catch_up_lifecycle_reports_events_that_arrived_mid_flight() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        assert_eq!(
            registry.begin_next_catch_up(),
            None,
            "catch-up must not start twice"
        );

        let path = PathBuf::from("/a/mid-flight.md");
        registry.record_pending(&id, [path.clone()]);

        assert_eq!(
            registry.finish_catch_up(&id, Ok(())),
            PendingWork::Paths(HashSet::from([path])),
            "events recorded during catch-up must be returned so the caller re-runs"
        );
        assert_eq!(
            registry.finish_catch_up(&id, Ok(())),
            PendingWork::Idle,
            "a follow-up finish with no new events must report Idle"
        );
    }

    #[test]
    fn path_limit_overflow_collapses_to_full_root() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        let paths = (0..=MAX_PENDING_PATHS).map(|i| PathBuf::from(format!("/a/{i}.md")));
        registry.record_pending(&id, paths);

        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::FullRoot),
            "exceeding MAX_PENDING_PATHS must collapse to FullRoot, never drop paths"
        );
    }

    #[test]
    fn path_limit_accumulates_incrementally_and_collapses_one_past_the_limit() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        let all_paths: Vec<PathBuf> = (0..MAX_PENDING_PATHS)
            .map(|i| PathBuf::from(format!("/a/{i}.md")))
            .collect();
        for chunk in all_paths.chunks(4) {
            registry.record_pending(&id, chunk.to_vec());
        }
        let Some(PendingWork::Paths(set)) = registry.pending(&id) else {
            panic!("expected Paths at the limit");
        };
        assert_eq!(set.len(), MAX_PENDING_PATHS);

        registry.record_pending(&id, [PathBuf::from("/a/one-past.md")]);
        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::FullRoot),
            "one path past the limit must collapse to FullRoot"
        );
    }

    #[test]
    fn only_one_active_pass_admitted_daemon_wide() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("a"), cfg.clone(), vec![]);
        registry.register(ConfigSource::Baseline, corpus("b"), cfg, vec![]);
        let a = baseline_id("a");

        assert_eq!(registry.begin_next_catch_up(), Some(a.clone()));
        assert_eq!(
            registry.begin_next_catch_up(),
            None,
            "a second pass must not be admitted while one is active"
        );

        registry.finish_catch_up(&a, Ok(()));
        assert!(
            registry.begin_next_catch_up().is_some(),
            "the slot frees up once the active pass finishes"
        );
    }

    #[test]
    fn round_robin_lets_b_start_before_as_follow_up() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("a"), cfg.clone(), vec![]);
        registry.register(ConfigSource::Baseline, corpus("b"), cfg, vec![]);
        let a = baseline_id("a");
        let b = baseline_id("b");

        // Queue is [a, b] from registration. Start a's pass.
        assert_eq!(registry.begin_next_catch_up(), Some(a.clone()));
        // A follow-up arrives for `a` mid-flight; `a` cannot re-queue itself
        // (it's InFlight, not Queued) until its pass finishes.
        registry.record_pending(&a, [PathBuf::from("/a/mid.md")]);
        // Finishing `a` with pending work re-queues it at the *back*: [b, a].
        registry.finish_catch_up(&a, Ok(()));

        assert_eq!(
            registry.begin_next_catch_up(),
            Some(b),
            "b must start before a's follow-up, since a was requeued behind it"
        );
    }

    #[test]
    fn failed_pass_stays_pending_without_busy_looping() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        let pending = registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            pending,
            PendingWork::FullRoot,
            "a failed pass must widen to a full retry, not vanish"
        );
        assert_eq!(
            registry.begin_next_catch_up(),
            None,
            "a failed pass must not immediately re-admit itself (no busy loop)"
        );

        // The next reconcile tick's admission cycle is what retries it.
        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.begin_next_catch_up(),
            Some(id.clone()),
            "the reconcile tick's admission must retry the failed registration"
        );
        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::FullRoot),
            "pending work stays FullRoot until a pass succeeds"
        );
    }

    #[test]
    fn recovery_warnings_clear_after_successful_recovery() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        let queried = [corpus("wiki")];

        registry.record_pending(&id, [PathBuf::from("/a/changed.md")]);
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; reconciliation pending".into()
            )]
        );

        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; failed reconciliation: boom; reconciliation pending".into()
            )]
        );

        registry.mark_reconcile_due_all();
        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        registry.finish_catch_up(&id, Ok(()));
        // The failed pass left FullRoot pending, so the successful pass
        // conservatively re-queues one more pass; the warning stays until
        // that pass finishes.
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; reconciliation not complete".into()
            )]
        );
        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        registry.finish_catch_up(&id, Ok(()));
        assert!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &queried)
                .is_empty()
        );

        registry.mark_degraded(&id, "watch failed".into());
        assert_eq!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &queried)
                .len(),
            1
        );
        registry.mark_watched(&id);
        assert!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &queried)
                .is_empty()
        );
    }

    #[test]
    fn recovery_warnings_for_scoped_to_caller_source() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        let repo_source = ConfigSource::RepoLayer(PathBuf::from("/b"));
        registry.register(repo_source.clone(), corpus("wiki"), cfg, vec![]);
        let baseline_id = baseline_id("wiki");
        let repo_id = RegistrationId {
            source: repo_source.clone(),
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };
        registry.record_pending(&repo_id, [PathBuf::from("/b/changed.md")]);
        let queried = [corpus("wiki")];

        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; reconciliation not complete".into()
            )],
            "Baseline must not see RepoLayer(/b)'s pending-work warning"
        );
        assert_eq!(
            registry.recovery_warnings_for(&repo_source, &queried).len(),
            1
        );
        assert!(registry.pending(&baseline_id).unwrap().is_idle());
    }

    #[test]
    fn recovery_warnings_report_incomplete_catch_up_without_pending_paths() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        let queried = [corpus("wiki")];
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; reconciliation not complete".into()
            )],
            "a freshly registered corpus has not been reconciled yet"
        );
        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state in-flight; reconciliation not complete".into()
            )]
        );
        registry.finish_catch_up(&id, Ok(()));
        assert!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &queried)
                .is_empty(),
            "a completed pass with no dirty paths or errors has nothing to report"
        );
    }

    #[test]
    fn recovery_warnings_compose_every_applicable_clause() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        registry.finish_catch_up(&id, Err("boom".into()));
        registry.mark_degraded(&id, "watch failed".into());
        let queried = [corpus("wiki")];

        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; failed reconciliation: boom; watcher backend error: watch failed; retry will continue; reconciliation pending".into()
            )]
        );
    }

    #[test]
    fn vanished_registration_releases_active_slot_on_failed_admission() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        assert_eq!(registry.begin_next_catch_up(), Some(id.clone()));
        registry.finish_catch_up(
            &id,
            Err("registration vanished before catch-up could start".into()),
        );

        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.begin_next_catch_up(),
            Some(id),
            "the active slot must be released even when the admitted registration vanished"
        );
    }

    #[test]
    fn refresh_roots_rekeys_registration_when_canonical_root_changes() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let old_id = baseline_id("wiki");
        registry.record_pending(&old_id, [PathBuf::from("/a/changed.md")]);

        registry.refresh_roots(|c| vec![root("/newly/resolved", c.clone())]);

        let new_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/newly/resolved"),
            },
        };
        assert_eq!(
            registry.pending(&old_id),
            None,
            "the stale id must no longer resolve"
        );
        assert_eq!(
            registry.pending(&new_id),
            Some(PendingWork::Paths(HashSet::from([PathBuf::from(
                "/a/changed.md"
            )]))),
            "re-keying must preserve pending work under the new id"
        );
    }

    #[test]
    fn refresh_roots_rekey_of_in_flight_registration_releases_the_slot() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let old_id = baseline_id("wiki");
        assert_eq!(registry.begin_next_catch_up(), Some(old_id.clone()));

        registry.refresh_roots(|c| vec![root("/newly/resolved", c.clone())]);

        let new_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/newly/resolved"),
            },
        };
        assert_eq!(
            registry.begin_next_catch_up(),
            None,
            "the re-keyed registration must wait for the old in-flight pass"
        );
        assert_eq!(registry.finish_catch_up(&old_id, Ok(())), PendingWork::Idle);
        assert_eq!(registry.begin_next_catch_up(), Some(new_id));
    }

    #[test]
    fn registrations_for_root_returns_every_registration_sharing_the_root() {
        let registry = WatchRegistry::new();
        let shared_root = root("/repo", corpus("wiki"));
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![shared_root.clone()],
        );
        registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate")),
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![shared_root.clone()],
        );

        let mut owners = registry.registrations_for_root(&shared_root);
        owners.sort_by(|a, b| a.source.cmp(&b.source));

        assert_eq!(
            owners,
            vec![
                RegistrationId {
                    source: ConfigSource::Baseline,
                    corpus_key: CorpusKey {
                        name: "wiki".into(),
                        canonical_root: PathBuf::from("/repo"),
                    },
                },
                RegistrationId {
                    source: ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate")),
                    corpus_key: CorpusKey {
                        name: "wiki".into(),
                        canonical_root: PathBuf::from("/repo"),
                    },
                },
            ]
        );
    }

    #[test]
    fn refresh_roots_build_closure_can_call_generation_without_deadlocking() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);

        registry.refresh_roots(|c| {
            registry.generation();
            vec![root("/resolved", c.clone())]
        });

        assert_eq!(
            registry.snapshot_roots()[0].1.canonical_watched,
            PathBuf::from("/resolved")
        );
    }

    #[test]
    fn same_source_unrelated_field_drift_keeps_state_and_reports_already_registered() {
        let registry = WatchRegistry::new();
        let mut cfg1 = Config::default();
        cfg1.search.limit_default = 10;
        let outcome1 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(cfg1.clone()),
            vec![],
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        let id = baseline_id("wiki");
        registry.record_pending(&id, [PathBuf::from("/a/changed.md")]);

        let mut cfg2 = cfg1.clone();
        cfg2.search.limit_default = 100;
        let outcome2 = registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(cfg2),
            vec![],
        );

        assert_eq!(
            outcome2,
            RegisterOutcome::AlreadyRegistered,
            "an unrelated config field must not force a re-create"
        );
        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::Paths(HashSet::from([PathBuf::from(
                "/a/changed.md"
            )]))),
            "pending work must survive a re-registration that only touched unrelated fields"
        );
    }

    #[test]
    fn replace_source_preserves_unchanged_registrations_and_retires_only_removed() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        registry
            .replace_source(
                source.clone(),
                vec![corpus("a"), corpus("b")],
                cfg.clone(),
                |_| vec![],
            )
            .unwrap();
        let id_a = RegistrationId {
            source: source.clone(),
            corpus_key: CorpusKey {
                name: "a".into(),
                canonical_root: PathBuf::new(),
            },
        };
        let id_b = RegistrationId {
            source: source.clone(),
            corpus_key: CorpusKey {
                name: "b".into(),
                canonical_root: PathBuf::new(),
            },
        };
        registry.record_pending(&id_a, [PathBuf::from("/a/changed.md")]);
        registry.finish_catch_up(&id_a, Err("boom".into()));

        let retired = registry
            .replace_source(
                source.clone(),
                vec![corpus("a"), corpus("b")],
                cfg.clone(),
                |_| vec![],
            )
            .unwrap();
        assert!(
            retired.is_empty(),
            "no registration was removed, so nothing should retire"
        );
        assert_eq!(
            registry.pending(&id_a),
            Some(PendingWork::FullRoot),
            "dirty state on an unchanged registration must survive a replace_source pass"
        );
        assert_eq!(registry.catch_up_state(&id_b), Some(CatchUpState::Queued));

        let retired = registry
            .replace_source(source, vec![corpus("a")], cfg, |_| vec![])
            .unwrap();
        assert_eq!(
            retired.len(),
            1,
            "removing corpus b's candidate must retire exactly that one registration"
        );
        assert!(registry.pending(&id_b).is_none());
        assert_eq!(
            registry.pending(&id_a),
            Some(PendingWork::FullRoot),
            "the still-present registration must remain untouched"
        );
    }

    #[test]
    fn repo_layer_sources_dedups_by_path() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        registry
            .replace_source(source, vec![corpus("a"), corpus("b")], cfg, |_| vec![])
            .unwrap();
        assert_eq!(
            registry.repo_layer_sources(),
            vec![PathBuf::from("/repo")],
            "two corpora under one RepoLayer source must yield one path"
        );
    }

    #[test]
    fn generation_advances_on_mutation_and_is_stable_on_reads() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let gen0 = registry.generation();

        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let gen1 = registry.generation();
        assert!(gen1 > gen0, "register must bump the generation");

        let id = baseline_id("wiki");
        registry.record_pending(&id, [PathBuf::from("/a/changed.md")]);
        let gen2 = registry.generation();
        assert!(gen2 > gen1, "record_pending must bump the generation");

        registry.finish_catch_up(&id, Ok(()));
        let gen3 = registry.generation();
        assert!(gen3 > gen2, "finish_catch_up must bump the generation");

        let gen4 = registry.generation();
        assert_eq!(gen4, gen3, "read-only calls must not move the generation");
        let _ = registry.pending(&id);
        let _ = registry.observation(&id);
        assert_eq!(
            registry.generation(),
            gen3,
            "pending/observation reads must not move the generation"
        );
    }
}
