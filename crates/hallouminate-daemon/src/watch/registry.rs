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
//! job — that lives in the call sites that will use this registry (a later
//! slice); this module only tracks state for whatever registrations its
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ConfigSource {
    Baseline,
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    RepoLayer(PathBuf),
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
    // Consumed by slice 4, which wires the provisioner and dispatch call sites
    // that will feed live per-path events into `record_pending`.
    #[allow(dead_code)]
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
    /// success. Purely observational: retry scheduling rides on
    /// `pending`/`catch_up`, not this field, so a failing pass can never
    /// busy-loop -- it becomes `Queued` exactly once per pass and waits its
    /// turn like any other dirty registration.
    last_error: Option<String>,
}

/// Result of a `register` call: whether this call created a new logical
/// registration or found an existing one for the same `(source, corpus
/// name)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Consumed by slice 4, which wires the provisioner and dispatch call sites.
#[allow(dead_code)]
pub(crate) enum RegisterOutcome {
    New,
    AlreadyRegistered,
    Conflict,
}

/// Retired registration data needed for fail-closed storage cleanup.
#[derive(Debug, Clone)]
pub(crate) struct RetiredRegistration {
    pub(crate) root: PathBuf,
    pub(crate) cfg: Arc<Config>,
}

/// Fair-scheduling and admission state shared by every registration.
/// `queue` holds ids that are `Queued`, in FIFO order; `active` is `true`
/// while some registration is `InFlight`, enforcing at most one running
/// catch-up pass across the whole daemon.
struct Inner {
    regs: HashMap<RegistrationId, Registration>,
    queue: VecDeque<RegistrationId>,
    active: bool,
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
                active: false,
            }),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("watch registry mutex poisoned")
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
            },
        );
        drop(guard);
        self.changed.notify_waiters();
    }

    /// Register a runtime-discovered (e.g. repo-layer) corpus. `catch_up`
    /// starts `Queued` so the live pump catches it up. Returns
    /// `AlreadyRegistered` for a repeat `(source, corpus name)` pair without
    /// disturbing the existing registration's state.
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
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
        for obsolete_id in obsolete {
            guard.regs.remove(&obsolete_id);
            guard.queue.retain(|queued| queued != &obsolete_id);
        }
        if let Some(existing) = guard.regs.get(&id) {
            if existing.corpus != corpus || *existing.cfg != *cfg || existing.roots != roots {
                guard.regs.remove(&id);
                guard.queue.retain(|queued| queued != &id);
            } else {
                return RegisterOutcome::AlreadyRegistered;
            }
        }
        let binding_conflict = guard.regs.iter().any(|(existing_id, existing)| {
            existing_id.corpus_key == id.corpus_key
                && existing_id.source != id.source
                && (existing.cfg.storage.ground_dir != cfg.storage.ground_dir
                    || existing.roots != roots)
        });
        if binding_conflict {
            return RegisterOutcome::Conflict;
        }
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
            },
        );
        guard.queue.push_back(id);
        drop(guard);
        self.changed.notify_waiters();
        RegisterOutcome::New
    }

    /// Notified whenever a registration is added or a catch-up pass becomes
    /// due; the pump races this future each loop iteration to reconcile
    /// promptly instead of only on its periodic backstop pass.
    pub(crate) fn changed(&self) -> &tokio::sync::Notify {
        &self.changed
    }

    pub(crate) fn refresh_roots<F>(&self, build: F)
    where
        F: Fn(&CorpusConfig) -> Vec<WatchRoot>,
    {
        let mut guard = self.lock();
        for registration in guard.regs.values_mut() {
            let roots = build(&registration.corpus);
            if registration.roots != roots {
                registration.roots = roots;
            }
        }
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
                .then_with(|| format!("{:?}", left.source).cmp(&format!("{:?}", right.source)))
        });
        roots
    }

    #[allow(dead_code)]
    /// Removes all registrations owned by `source` and cancels queued work.
    #[allow(dead_code)]
    pub(crate) fn remove_source(&self, source: &ConfigSource) -> usize {
        let mut guard = self.lock();
        let removed: std::collections::HashSet<RegistrationId> = guard
            .regs
            .keys()
            .filter(|id| &id.source == source)
            .cloned()
            .collect();
        for id in &removed {
            guard.regs.remove(id);
        }
        guard.queue.retain(|id| !removed.contains(id));
        drop(guard);
        if !removed.is_empty() {
            self.changed.notify_waiters();
        }
        removed.len()
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
            let conflict = guard.regs.iter().any(|(existing_id, existing)| {
                existing_id.source != source
                    && existing_id.corpus_key == id.corpus_key
                    && (existing.cfg.storage.ground_dir != cfg.storage.ground_dir
                        || existing.roots != *roots)
            });
            if conflict {
                return Err(format!(
                    "corpus {:?} has an incompatible watcher registration; use one root binding per corpus",
                    id.corpus_key.name
                ));
            }
        }
        let removed: HashSet<_> = guard
            .regs
            .keys()
            .filter(|id| id.source == source)
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
        guard.regs.retain(|id, _| id.source != source);
        guard.queue.retain(|id| !removed.contains(id));
        for (id, corpus, roots) in candidates {
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
                },
            );
            guard.queue.push_back(id);
        }
        drop(guard);
        self.changed.notify_waiters();
        Ok(retired)
    }

    pub(crate) fn repo_layer_sources(&self) -> Vec<PathBuf> {
        self.lock()
            .regs
            .keys()
            .filter_map(|id| match &id.source {
                ConfigSource::RepoLayer(path) => Some(path.clone()),
                ConfigSource::Baseline => None,
            })
            .collect()
    }
    /// Record paths as pending work for a registration's durable ledger,
    /// distinct from `watch/mod.rs`'s transient per-batch buffer, and queue
    /// it for a catch-up pass if it wasn't already due for one.
    // Consumed by slice 4, which wires the provisioner and dispatch call
    // sites that resolve a live event path to its owning registration id.
    #[allow(dead_code)]
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
        drop(guard);
        self.changed.notify_waiters();
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

    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn observation(&self, id: &RegistrationId) -> Option<Observation> {
        self.lock().regs.get(id).map(|reg| reg.observation.clone())
    }

    #[allow(dead_code)]
    pub(crate) fn pending(&self, id: &RegistrationId) -> Option<PendingWork> {
        self.lock().regs.get(id).map(|reg| reg.pending.clone())
    }

    #[allow(dead_code)]
    pub(crate) fn catch_up_state(&self, id: &RegistrationId) -> Option<CatchUpState> {
        self.lock().regs.get(id).map(|reg| reg.catch_up)
    }

    #[cfg(test)]
    fn recovery_warnings(&self) -> Vec<(String, String)> {
        let guard = self.lock();
        guard
            .regs
            .values()
            .filter_map(|reg| {
                let message = if let Some(error) = &reg.last_error {
                    Some(format!("failed reconciliation: {error}"))
                } else if matches!(reg.observation, Observation::Degraded { .. }) {
                    Some(
                        "watcher observation unavailable; reconciliation remains active"
                            .to_string(),
                    )
                } else if !reg.pending.is_idle() {
                    Some("reconciliation pending".to_string())
                } else {
                    None
                }?;
                Some((reg.corpus.name.clone(), message))
            })
            .collect()
    }

    /// Returns actionable recovery warnings for registrations with incomplete work.
    pub(crate) fn recovery_warnings_for(&self, queried: &[CorpusConfig]) -> Vec<(String, String)> {
        let names: HashSet<&str> = queried.iter().map(|corpus| corpus.name.as_str()).collect();
        let guard = self.lock();
        guard
            .regs
            .values()
            .filter(|reg| names.contains(reg.corpus.name.as_str()))
            .filter_map(|reg| {
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
                let message = if let Some(error) = &reg.last_error {
                    format!("root {root}; recovery state {state}; failed reconciliation: {error}")
                } else if let Observation::Degraded { error } = &reg.observation {
                    format!("root {root}; recovery state {state}; watcher backend error: {error}; retry will continue")
                } else if !reg.pending.is_idle() {
                    format!("root {root}; recovery state {state}; reconciliation pending")
                } else {
                    return None;
                };
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
                reg.last_error = None;
            }
            Self::mark_dirty_locked(&mut guard, &id);
        }
        drop(guard);
        self.changed.notify_waiters();
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

    /// Transitions one registration into the daemon-wide active pass slot.
    #[allow(dead_code)]
    pub(crate) fn begin_catch_up(&self, id: &RegistrationId) -> bool {
        let mut guard = self.lock();
        if guard.active {
            return false;
        }
        let Some(reg) = guard.regs.get_mut(id) else {
            return false;
        };
        if reg.last_error.is_some()
            || !matches!(
                reg.catch_up,
                CatchUpState::NotStarted | CatchUpState::Queued
            )
        {
            return false;
        }
        reg.catch_up = CatchUpState::InFlight;
        guard.active = true;
        guard.queue.retain(|queued| queued != id);
        true
    }

    /// Pops the next `Queued` id in FIFO order and starts it, respecting the
    /// same single-active-pass gate as `begin_catch_up`. This is what gives
    #[allow(dead_code)]
    /// registrations a fair turn: `finish_catch_up` appends a registration
    /// with follow-up work to the *back* of the queue rather than
    /// re-running it immediately, so a busy root can't starve a quieter one
    /// queued behind it.
    #[allow(dead_code)]
    pub(crate) fn begin_next_catch_up(&self) -> Option<RegistrationId> {
        let mut guard = self.lock();
        if guard.active {
            return None;
        }
        while let Some(id) = guard.queue.pop_front() {
            let Some(reg) = guard.regs.get_mut(&id) else {
                continue;
            };
            if reg.catch_up != CatchUpState::Queued {
                continue;
            }
            reg.catch_up = CatchUpState::InFlight;
            guard.active = true;
            return Some(id);
        }
        None
    }

    /// Marks catch-up finished for `id`, releasing the daemon-wide slot and
    /// returning (clearing) whatever pending work accumulated while it ran.
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
        guard.active = false;
        let Some(reg) = guard.regs.get_mut(id) else {
            return PendingWork::Idle;
        };
        let pending = match outcome {
            Ok(()) => {
                reg.last_error = None;
                let pending = std::mem::take(&mut reg.pending);
                if pending.is_idle() {
                    reg.catch_up = CatchUpState::Done;
                } else {
                    reg.catch_up = CatchUpState::Queued;
                    guard.queue.push_back(id.clone());
                }
                pending
            }
            Err(error) => {
                reg.last_error = Some(error);
                reg.pending = PendingWork::FullRoot;
                reg.catch_up = CatchUpState::Queued;
                guard.queue.push_back(id.clone());
                PendingWork::FullRoot
            }
        };
        drop(guard);
        self.changed.notify_waiters();
        pending
    }

    pub(crate) fn config_for(&self, id: &RegistrationId) -> Option<(CorpusConfig, Arc<Config>)> {
        self.lock()
            .regs
            .get(id)
            .map(|reg| (reg.corpus.clone(), reg.cfg.clone()))
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
        assert_eq!(registry.snapshot_roots().len(), 0);
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
            vec![],
        );
        let outcome2 = registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/repo")),
            corpus("wiki"),
            Arc::new(repo_cfg),
            vec![],
        );
        assert_eq!(outcome1, RegisterOutcome::New);
        assert_eq!(outcome2, RegisterOutcome::New);
    }

    #[test]
    fn seed_baseline_is_idempotent_and_preserves_pending_across_reseeding() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.seed_baseline(corpus("wiki"), cfg.clone(), vec![]);
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };
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
    fn mark_degraded_preserves_registration_and_pending() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };
        let path = PathBuf::from("/a/changed.md");
        registry.record_pending(&id, [path.clone()]);

        registry.mark_degraded(&id, "watch failed".into());

        assert_eq!(
            registry.observation(&id),
            Some(Observation::Degraded {
                error: "watch failed".into()
            })
        );
        assert_eq!(registry.snapshot_roots().len(), 0);
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
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };

        assert!(registry.begin_catch_up(&id));
        assert!(
            !registry.begin_catch_up(&id),
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
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };

        let paths = (0..=MAX_PENDING_PATHS).map(|i| PathBuf::from(format!("/a/{i}.md")));
        registry.record_pending(&id, paths);

        assert_eq!(
            registry.pending(&id),
            Some(PendingWork::FullRoot),
            "exceeding MAX_PENDING_PATHS must collapse to FullRoot, never drop paths"
        );
    }

    #[test]
    fn only_one_active_pass_admitted_daemon_wide() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("a"), cfg.clone(), vec![]);
        registry.register(ConfigSource::Baseline, corpus("b"), cfg, vec![]);
        let a = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "a".into(),
                canonical_root: PathBuf::new(),
            },
        };

        assert!(registry.begin_catch_up(&a));
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
        let a = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "a".into(),
                canonical_root: PathBuf::new(),
            },
        };
        let b = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "b".into(),
                canonical_root: PathBuf::new(),
            },
        };

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
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };

        assert!(registry.begin_catch_up(&id));
        let pending = registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            pending,
            PendingWork::FullRoot,
            "a failed pass must widen to a full retry, not vanish"
        );
        assert!(
            !registry.begin_catch_up(&id),
            "a failed pass must not immediately re-admit itself (no busy loop)"
        );

        // The next reconcile tick's admission cycle is what retries it.
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
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::new(),
            },
        };

        registry.record_pending(&id, [PathBuf::from("/a/changed.md")]);
        assert_eq!(
            registry.recovery_warnings(),
            vec![("wiki".into(), "reconciliation pending".into())]
        );

        assert!(registry.begin_catch_up(&id));
        registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            registry.recovery_warnings(),
            vec![("wiki".into(), "failed reconciliation: boom".into())]
        );

        registry.mark_reconcile_due_all();
        assert!(registry.begin_catch_up(&id));
        registry.finish_catch_up(&id, Ok(()));
        assert!(registry.recovery_warnings().is_empty());

        registry.mark_degraded(&id, "watch failed".into());
        assert_eq!(registry.recovery_warnings().len(), 1);
        registry.mark_watched(&id);
        assert!(registry.recovery_warnings().is_empty());
    }
}
