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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hallouminate_config::Config;
use hallouminate_domain::common::CorpusConfig;

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

/// Identifies one logical registration: a source plus the corpus name it
/// registered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RegistrationId {
    pub(crate) source: ConfigSource,
    pub(crate) corpus_name: String,
}

/// Whether a registration's roots are actually being watched by `notify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    Watched,
    Degraded { error: String },
}

/// Paths that changed for a registration since its last completed catch-up
/// pass, distinct from the transient per-batch debounce-coalescing buffer in
/// `watch/mod.rs` (`record_pending`/`pending`): this is the durable ledger a
/// catch-up pass drains and, if it grew again mid-pass, drives a follow-up
/// pass against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PendingWork {
    #[default]
    Idle,
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    Paths(HashSet<PathBuf>),
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    FullRoot,
}

impl PendingWork {
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        match self {
            PendingWork::FullRoot => {}
            PendingWork::Idle => {
                let set: HashSet<PathBuf> = paths.into_iter().collect();
                if !set.is_empty() {
                    *self = PendingWork::Paths(set);
                }
            }
            PendingWork::Paths(existing) => existing.extend(paths),
        }
    }

    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, PendingWork::Idle)
    }
}

/// Whether a registration's initial catch-up pass (reconciling on-disk state
/// against the index for whatever may have changed before the registration
/// existed) has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CatchUpState {
    #[default]
    NotStarted,
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
}

/// The live-registration ledger backing the watcher pump. Lives on
/// `DaemonState` for the process lifetime; `changed()` lets the pump wake up
/// promptly when a new registration arrives instead of waiting for its
/// periodic reconcile pass.
pub(crate) struct WatchRegistry {
    inner: Mutex<HashMap<RegistrationId, Registration>>,
    changed: tokio::sync::Notify,
}

impl WatchRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RegistrationId, Registration>> {
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
            corpus_name: corpus.name.clone(),
        };
        let mut guard = self.lock();
        if guard.contains_key(&id) {
            return;
        }
        guard.insert(
            id,
            Registration {
                corpus,
                cfg,
                roots,
                observation: Observation::Watched,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::Done,
            },
        );
        drop(guard);
        self.changed.notify_waiters();
    }

    /// Register a runtime-discovered (e.g. repo-layer) corpus. `catch_up`
    /// starts `NotStarted` so the live pump catches it up. Returns
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
            corpus_name: corpus.name.clone(),
        };
        let mut guard = self.lock();
        if guard.contains_key(&id) {
            return RegisterOutcome::AlreadyRegistered;
        }
        guard.insert(
            id,
            Registration {
                corpus,
                cfg,
                roots,
                observation: Observation::Watched,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::NotStarted,
            },
        );
        drop(guard);
        self.changed.notify_waiters();
        RegisterOutcome::New
    }

    /// Notified whenever a registration is added; the pump races this future
    /// each loop iteration to reconcile promptly instead of only on its
    /// periodic backstop pass.
    pub(crate) fn changed(&self) -> &tokio::sync::Notify {
        &self.changed
    }

    /// Every root across every registration, flattened, paired with the
    /// registration that owns it.
    pub(crate) fn snapshot_roots(&self) -> Vec<(RegistrationId, WatchRoot)> {
        self.lock()
            .iter()
            .flat_map(|(id, reg)| reg.roots.iter().map(move |root| (id.clone(), root.clone())))
            .collect()
    }

    pub(crate) fn ids(&self) -> Vec<RegistrationId> {
        self.lock().keys().cloned().collect()
    }

    /// Record paths as pending work for a registration's durable ledger,
    /// distinct from `watch/mod.rs`'s transient per-batch buffer.
    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn record_pending(
        &self,
        id: &RegistrationId,
        paths: impl IntoIterator<Item = PathBuf>,
    ) {
        if let Some(reg) = self.lock().get_mut(id) {
            reg.pending.add_paths(paths);
        }
    }

    pub(crate) fn mark_degraded(&self, id: &RegistrationId, error: String) {
        if let Some(reg) = self.lock().get_mut(id) {
            reg.observation = Observation::Degraded { error };
        }
    }

    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn observation(&self, id: &RegistrationId) -> Option<Observation> {
        self.lock().get(id).map(|reg| reg.observation.clone())
    }

    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn pending(&self, id: &RegistrationId) -> Option<PendingWork> {
        self.lock().get(id).map(|reg| reg.pending.clone())
    }

    // Consumed by slice 4, which wires the provisioner and dispatch call sites.
    #[allow(dead_code)]
    pub(crate) fn catch_up_state(&self, id: &RegistrationId) -> Option<CatchUpState> {
        self.lock().get(id).map(|reg| reg.catch_up)
    }

    /// Transition `NotStarted` -> `InFlight`. Returns `false` for an unknown
    /// id or one whose catch-up already started or finished, so a caller
    /// never spawns two concurrent catch-up passes for the same id.
    pub(crate) fn begin_catch_up(&self, id: &RegistrationId) -> bool {
        let mut guard = self.lock();
        let Some(reg) = guard.get_mut(id) else {
            return false;
        };
        if reg.catch_up != CatchUpState::NotStarted {
            return false;
        }
        reg.catch_up = CatchUpState::InFlight;
        true
    }

    /// Marks catch-up `Done` and returns (clearing) whatever pending work
    /// accumulated while it ran. A non-`Idle` return means events arrived
    /// mid-catch-up and the caller must run another pass.
    pub(crate) fn finish_catch_up(&self, id: &RegistrationId) -> PendingWork {
        let mut guard = self.lock();
        let Some(reg) = guard.get_mut(id) else {
            return PendingWork::Idle;
        };
        reg.catch_up = CatchUpState::Done;
        std::mem::take(&mut reg.pending)
    }

    pub(crate) fn config_for(&self, id: &RegistrationId) -> Option<(CorpusConfig, Arc<Config>)> {
        self.lock()
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
        assert_eq!(registry.ids().len(), 1);
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
        assert_eq!(registry.ids().len(), 2);
    }

    #[test]
    fn seed_baseline_is_idempotent_and_preserves_pending_across_reseeding() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.seed_baseline(corpus("wiki"), cfg.clone(), vec![]);
        let id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_name: "wiki".into(),
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
            corpus_name: "wiki".into(),
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
        assert_eq!(registry.ids(), vec![id.clone()]);
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
            corpus_name: "wiki".into(),
        };

        assert!(registry.begin_catch_up(&id));
        assert!(
            !registry.begin_catch_up(&id),
            "catch-up must not start twice"
        );

        let path = PathBuf::from("/a/mid-flight.md");
        registry.record_pending(&id, [path.clone()]);

        assert_eq!(
            registry.finish_catch_up(&id),
            PendingWork::Paths(HashSet::from([path])),
            "events recorded during catch-up must be returned so the caller re-runs"
        );
        assert_eq!(
            registry.finish_catch_up(&id),
            PendingWork::Idle,
            "a follow-up finish with no new events must report Idle"
        );
    }
}
