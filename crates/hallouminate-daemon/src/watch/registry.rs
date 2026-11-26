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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hallouminate_config::Config;
use hallouminate_domain::common::{CorpusConfig, CorpusKey};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::state::corpus_lock_keys;

use super::{RootResolution, WatchRoot};

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

fn detached_matches(
    detached: &DetachedRegistration,
    id: &RegistrationId,
    cfg: &Config,
    roots: &[WatchRoot],
) -> bool {
    if detached.id.corpus_key != id.corpus_key
        || cfg_binding_differs(&detached.cfg, cfg)
        || detached.saved_roots.len() != roots.len()
    {
        return false;
    }
    let mut actual_roots = Vec::new();
    for root in roots {
        if root.resolution != RootResolution::Resolved {
            return false;
        }
        actual_roots.push(
            root.canonical_file_root
                .as_ref()
                .unwrap_or(&root.canonical_watched)
                .clone(),
        );
    }
    let mut saved_roots = detached.saved_roots.clone();
    saved_roots.sort();
    actual_roots.sort();
    saved_roots == actual_roots
}

fn reclaim_matching_detached(
    guard: &mut Inner,
    pending: &Mutex<VecDeque<PendingCleanup>>,
    id: &RegistrationId,
    cfg: &Config,
    roots: &[WatchRoot],
) {
    let matching = |detached: &DetachedRegistration| detached_matches(detached, id, cfg, roots);
    let ticketed = match &id.source {
        ConfigSource::RepoLayer(_) => guard
            .detached
            .iter()
            .position(|detached| detached._ticket.is_some() && matching(detached)),
        ConfigSource::Baseline => None,
    };
    if let Some(index) = ticketed.or_else(|| guard.detached.iter().position(matching)) {
        let detached = guard.detached.remove(index);
        detached.token.store(false, Ordering::SeqCst);
        pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|item| match item {
                PendingCleanup::Detached(token) => !Arc::ptr_eq(token, &detached.token),
                PendingCleanup::Retired(registration) => !registration
                    .ordinary_owner
                    .as_ref()
                    .is_some_and(|token| Arc::ptr_eq(token, &detached.token)),
                PendingCleanup::Deferred(_) => true,
            });
    }
}

fn stabilize_unresolved_roots(guard: &Inner, corpus: &CorpusConfig, roots: &mut [WatchRoot]) {
    for (index, root) in roots.iter_mut().enumerate() {
        if root.resolution != RootResolution::Unresolved {
            continue;
        }
        let mut prior: Option<&WatchRoot> = None;
        let mut ambiguous = false;
        for (id, registration) in &guard.regs {
            if id.corpus_key.name != corpus.name || registration.corpus.paths != corpus.paths {
                continue;
            }
            let Some(old) = registration.roots.get(index) else {
                continue;
            };
            if old.resolution != RootResolution::Resolved {
                continue;
            }
            if let Some(first) = prior
                && (first.watched != old.watched
                    || first.canonical_watched != old.canonical_watched
                    || first.canonical_file_root != old.canonical_file_root
                    || first.configured_file_root != old.configured_file_root
                    || first.mode != old.mode)
            {
                ambiguous = true;
                break;
            }
            prior = Some(old);
        }
        if !ambiguous && let Some(old) = prior {
            root.watched = old.watched.clone();
            root.canonical_watched = old.canonical_watched.clone();
            root.canonical_file_root = old.canonical_file_root.clone();
            root.configured_file_root = old.configured_file_root.clone();
            root.resolution = old.resolution;
            root.mode = old.mode;
        }
    }
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
    /// `retrying` distinguishes a per-root install failure (the pump keeps
    /// retrying on every reconcile) from native backend unavailability (no
    /// retry ever happens; only a daemon restart recovers).
    Degraded {
        error: String,
        retrying: bool,
    },
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
    /// `corpus_lock_keys(&corpus)`, computed when the registration is
    /// created so admission scans do not canonicalize under the registry lock.
    lock_keys: Vec<CorpusKey>,
    cfg: Arc<Config>,
    roots: Vec<WatchRoot>,
    binding: Arc<()>,
    retirement_due: bool,
    retirement_missing: bool,
    observation: Observation,
    pending: PendingWork,
    catch_up: CatchUpState,
    /// Set by `finish_catch_up` on a failed pass, cleared on the next
    /// success (`finish_catch_up(Ok)` is the sole clear site).
    last_error: Option<String>,
    /// Set when the first catch-up pass finishes, whether it succeeds or
    /// fails. A cold-start `ground` waits only while this is unset.
    reconciled_once: bool,
    /// Set by `finish_catch_up` on a failed pass; gates `begin_next_catch_up`
    /// from re-admitting this registration until `mark_reconcile_due_all`
    /// clears it on the next reconcile tick, preventing a failing pass from
    /// busy-looping.
    retry_on_tick: bool,
    /// Consecutive reconcile ticks that found this registration's root
    /// absent; `record_absent_roots` resets it when the root reappears.
    absent_ticks: u32,
    checkout_absent_ticks: u32,
    /// Consecutive identical reconcile failures `repeats_last_error` has
    /// suppressed; resets at each reminder.
    error_repeats: u32,
}

impl Registration {
    fn saved_roots(&self, id: &RegistrationId) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for root in &self.roots {
            match &root.canonical_file_root {
                Some(file) => roots.push(file.clone()),
                None => roots.push(root.canonical_watched.clone()),
            }
        }
        if roots.is_empty() {
            roots.push(id.corpus_key.canonical_root.clone());
        }
        roots
    }
}

/// Identical reconcile failures between repeated `warn` reminders.
const REPEAT_FAILURE_REMINDER_EVERY: u32 = 60;

/// Cap on concurrent storage-cleanup attempts.
const MAX_PENDING_CLEANUPS: usize = 64;

#[derive(Debug)]
pub(crate) struct RetirementTicket(Arc<AtomicUsize>);

impl RetirementTicket {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirementCause {
    ConfigRemoval,
    MissingRoot,
}

impl Drop for RetirementTicket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
/// `roots` holds canonical storage keys, and `lock_keys` is the corpus-lock
/// set cleanup takes before touching them.
#[derive(Debug)]
pub(crate) struct RetiredRegistration {
    pub(crate) corpus: String,
    pub(crate) root: PathBuf,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) lock_keys: Vec<CorpusKey>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) failed_attempts: u32,
    pub(crate) detached: Option<Arc<AtomicBool>>,
    pub(crate) ordinary_owner: Option<Arc<AtomicBool>>,
    pub(crate) log_missing_root: bool,
    pub(crate) waiting_roots: Vec<PathBuf>,
    pub(crate) _permit: Option<OwnedSemaphorePermit>,
    pub(crate) _ticket: Option<RetirementTicket>,
}

impl RetiredRegistration {
    /// Whether the current cleanup attempt should log at `warn`: the first
    /// failure, then one reminder every `REPEAT_FAILURE_REMINDER_EVERY`.
    pub(crate) fn warns_on_failure(&self) -> bool {
        self.failed_attempts
            .is_multiple_of(REPEAT_FAILURE_REMINDER_EVERY)
    }

    fn from_registration(
        id: &RegistrationId,
        registration: &Registration,
        retired_count: &Arc<AtomicUsize>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self::from_registration_with_permit(id, registration, retired_count, Some(permit))
    }

    fn from_registration_with_permit(
        id: &RegistrationId,
        registration: &Registration,
        retired_count: &Arc<AtomicUsize>,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Self {
        let roots = registration.saved_roots(id);
        Self {
            corpus: id.corpus_key.name.clone(),
            root: id.corpus_key.canonical_root.clone(),
            roots,
            lock_keys: registration.lock_keys.clone(),
            cfg: registration.cfg.clone(),
            failed_attempts: 0,
            detached: None,
            ordinary_owner: None,
            log_missing_root: false,
            waiting_roots: Vec::new(),
            _permit: permit,
            _ticket: match &id.source {
                ConfigSource::Baseline => None,
                ConfigSource::RepoLayer(_) => Some(RetirementTicket::new(retired_count)),
            },
        }
    }
}

fn attach_ordinary_owner(
    guard: &mut Inner,
    id: &RegistrationId,
    registration: &Registration,
    retired: &mut RetiredRegistration,
) {
    let mut independent_dirs = HashSet::new();
    for root in &registration.roots {
        if root.resolution == RootResolution::Resolved && root.canonical_file_root.is_none() {
            independent_dirs.insert(root.canonical_watched.clone());
        }
    }
    let token = Arc::new(AtomicBool::new(true));
    guard.detached.push(DetachedRegistration {
        id: id.clone(),
        cfg: registration.cfg.clone(),
        saved_roots: retired.roots.clone(),
        independent_dirs,
        binding: registration.binding.clone(),
        token: token.clone(),
        _ticket: retired._ticket.take(),
        retired: None,
        waiting_roots: Vec::new(),
        ready_roots: Vec::new(),
        absent_ticks: 0,
        rearmed_ticks: HashMap::new(),
        queued: false,
    });
    retired.ordinary_owner = Some(token);
    retired.log_missing_root = true;
}

struct DetachedRegistration {
    id: RegistrationId,
    cfg: Arc<Config>,
    saved_roots: Vec<PathBuf>,
    independent_dirs: HashSet<PathBuf>,
    binding: Arc<()>,
    token: Arc<AtomicBool>,
    _ticket: Option<RetirementTicket>,
    retired: Option<RetiredRegistration>,
    waiting_roots: Vec<PathBuf>,
    ready_roots: Vec<PathBuf>,
    absent_ticks: u32,
    rearmed_ticks: HashMap<PathBuf, u32>,
    queued: bool,
}

enum PendingCleanup {
    Retired(RetiredRegistration),
    Detached(Arc<AtomicBool>),
    Deferred(RegistrationId),
}

/// One registration's binding and corpus for an unlocked absence check.
pub(crate) struct RootProbe {
    pub(crate) id: RegistrationId,
    pub(crate) corpus: CorpusConfig,
    binding: Arc<()>,
    pub(crate) saved_roots: Vec<PathBuf>,
}

pub(crate) struct DetachedRootObservation {
    pub(crate) binding: Arc<()>,
    pub(crate) absences: Vec<(PathBuf, bool)>,
}

#[derive(Clone, Copy)]
pub(crate) enum RootPresence {
    Absent,
    CheckoutAbsent,
    Present,
}

pub(crate) struct RootObservation {
    pub(crate) probe: RootProbe,
    pub(crate) presence: RootPresence,
}

/// One admitted catch-up: a leader plus every compatible registration that
/// shares its pass. `valid` turns false when the leader registration is
/// released before its pass finishes.
struct ActiveGroup {
    leader: RegistrationId,
    lock_keys: Vec<CorpusKey>,
    members: Vec<RegistrationId>,
    valid: bool,
}

/// Fair-scheduling and admission state shared by every registration.
/// `queue` holds ids that are `Queued`, in FIFO order; `active` holds one
/// group per running catch-up pass, bounded by the limit passed to `begin_next_catch_up`. Two groups
/// never share a `CorpusKey`, which matches the per-key corpus lock; the same
/// corpus name under different roots (worktrees) runs concurrently.
struct Inner {
    regs: HashMap<RegistrationId, Registration>,
    queue: VecDeque<RegistrationId>,
    active: Vec<ActiveGroup>,
    detached: Vec<DetachedRegistration>,
    retry_sources: HashSet<PathBuf>,
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
    /// Woken after every catch-up pass settles, so a request can await its
    /// registrations' first reconciliation without polling.
    settled: tokio::sync::Notify,
    /// Set when the watcher pump could not be created; nothing admits
    /// catch-up passes then, so cold-start waits are skipped.
    pump_unavailable: std::sync::atomic::AtomicBool,
    /// Set while the boot `catch_up_index` task runs. Baseline registrations
    /// start `Done`, so this is the only signal that their index may lag.
    boot_catch_up_running: std::sync::atomic::AtomicBool,
    /// Retired registrations whose storage cleanup failed and must retry.
    pending_cleanups: Mutex<VecDeque<PendingCleanup>>,
    cleanup_slots: Arc<Semaphore>,
    retired_runtime: Arc<AtomicUsize>,
}

/// Whether the boot catch-up sweep is still reconciling baseline corpora.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootCatchUp {
    Idle,
    Running,
}

/// Clears the boot catch-up flag on drop, so no exit path of the sweep can
/// leave baseline results flagged as stale.
pub(crate) struct BootCatchUpGuard(Arc<WatchRegistry>);

impl Drop for BootCatchUpGuard {
    fn drop(&mut self) {
        self.0.boot_catch_up_running.store(false, Ordering::SeqCst);
    }
}

impl WatchRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                regs: HashMap::new(),
                queue: VecDeque::new(),
                active: Vec::new(),
                detached: Vec::new(),
                retry_sources: HashSet::new(),
                generation: 0,
            }),
            changed: tokio::sync::Notify::new(),
            settled: tokio::sync::Notify::new(),
            pump_unavailable: std::sync::atomic::AtomicBool::new(false),
            boot_catch_up_running: std::sync::atomic::AtomicBool::new(false),
            pending_cleanups: Mutex::new(VecDeque::new()),
            cleanup_slots: Arc::new(Semaphore::new(MAX_PENDING_CLEANUPS)),
            retired_runtime: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Marks the boot catch-up sweep as running. Call before spawning it.
    pub(crate) fn mark_boot_catch_up_running(&self) {
        self.boot_catch_up_running.store(true, Ordering::SeqCst);
    }

    /// Returns a guard that marks the boot catch-up sweep finished on drop.
    pub(crate) fn boot_catch_up_guard(self: &Arc<Self>) -> BootCatchUpGuard {
        BootCatchUpGuard(Arc::clone(self))
    }

    pub(crate) fn boot_catch_up(&self) -> BootCatchUp {
        if self.boot_catch_up_running.load(Ordering::SeqCst) {
            BootCatchUp::Running
        } else {
            BootCatchUp::Idle
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
                lock_keys: corpus_lock_keys(&corpus),
                corpus,
                cfg,
                roots,
                binding: Arc::new(()),
                retirement_due: false,
                retirement_missing: false,
                observation: Observation::Watched,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::Done,
                last_error: None,
                reconciled_once: true,
                retry_on_tick: false,
                absent_ticks: 0,
                checkout_absent_ticks: 0,
                error_repeats: 0,
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
        mut roots: Vec<WatchRoot>,
    ) -> RegisterOutcome {
        let mut guard = self.lock();
        stabilize_unresolved_roots(&guard, &corpus, &mut roots);
        let id = RegistrationId {
            source,
            corpus_key: corpus_key(&corpus, &roots),
        };
        let obsolete: Vec<RegistrationId> = guard
            .regs
            .iter()
            .filter(|(existing, reg)| {
                existing.source == id.source
                    && existing.corpus_key.name == id.corpus_key.name
                    && existing.corpus_key != id.corpus_key
                    && !reg.retirement_due
            })
            .map(|(existing, _)| existing.clone())
            .collect();
        let unchanged = guard.regs.get(&id).is_some_and(|existing| {
            !existing.retirement_due && registration_equivalent(existing, &corpus, &cfg, &roots)
        });
        if unchanged {
            let existing = guard.regs.get_mut(&id).expect("checked above");
            existing.lock_keys = corpus_lock_keys(&corpus);
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
                    .any(|existing| matches!(existing.source, ConfigSource::RepoLayer(_)))
                || guard.detached.iter().any(|detached| {
                    detached._ticket.is_some() && detached_matches(detached, &id, &cfg, &roots)
                });
            if runtime_count + self.retired_runtime.load(Ordering::SeqCst)
                >= MAX_RUNTIME_REGISTRATIONS
                && !replaces_runtime
            {
                return RegisterOutcome::LimitReached;
            }
        }
        for obsolete_id in obsolete {
            guard.regs.remove(&obsolete_id);
            guard.queue.retain(|queued| queued != &obsolete_id);
            release_admission(&mut guard, &obsolete_id);
        }
        // Backend unavailability is a pump-lifetime condition, not a property
        // of the replaced registration. A changed re-registration keeps that
        // observation so the pump's per-id cache and the registry stay in
        // agreement; every other observation resets and the pump re-derives it.
        if guard.regs.get(&id).is_some_and(|reg| reg.retirement_due) {
            self.cancel_deferred(&id);
        }
        let observation = match guard.regs.remove(&id).map(|reg| reg.observation) {
            Some(Observation::Degraded {
                retrying: false,
                error,
            }) => Observation::Degraded {
                retrying: false,
                error,
            },
            _ => Observation::Watched,
        };
        release_admission(&mut guard, &id);
        guard.queue.retain(|queued| queued != &id);
        reclaim_matching_detached(&mut guard, &self.pending_cleanups, &id, &cfg, &roots);
        guard.regs.insert(
            id.clone(),
            Registration {
                lock_keys: corpus_lock_keys(&corpus),
                corpus,
                cfg,
                roots,
                binding: Arc::new(()),
                retirement_due: false,
                retirement_missing: false,
                observation,
                pending: PendingWork::Idle,
                catch_up: CatchUpState::Queued,
                last_error: None,
                reconciled_once: false,
                retry_on_tick: false,
                absent_ticks: 0,
                checkout_absent_ticks: 0,
                error_repeats: 0,
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
        let guard = self.lock();
        let mut roots = roots.to_vec();
        stabilize_unresolved_roots(&guard, corpus, &mut roots);
        guard.regs.iter().any(|(id, existing)| {
            id.source == *source
                && id.corpus_key.name == corpus.name
                && !existing.retirement_due
                && registration_equivalent(existing, corpus, cfg, &roots)
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
        let snapshot: Vec<(RegistrationId, CorpusConfig, Vec<WatchRoot>, Arc<()>)> = {
            let guard = self.lock();
            guard
                .regs
                .iter()
                .filter(|(_, registration)| !registration.retirement_due)
                .map(|(id, registration)| {
                    (
                        id.clone(),
                        registration.corpus.clone(),
                        registration.roots.clone(),
                        Arc::clone(&registration.binding),
                    )
                })
                .collect()
        };
        let mut built = Vec::new();
        for (id, corpus, previous, binding) in snapshot {
            let mut roots = build(&corpus);
            let mut fully_resolved = true;
            for root in &roots {
                if root.resolution != RootResolution::Resolved {
                    fully_resolved = false;
                    break;
                }
            }
            for (index, root) in roots.iter_mut().enumerate() {
                if root.resolution == RootResolution::Unresolved
                    && let Some(old) = previous.get(index)
                {
                    *root = old.clone();
                }
            }
            built.push((id, roots, binding, fully_resolved));
        }

        let mut guard = self.lock();
        for (old_id, roots, binding, fully_resolved) in built {
            let Some(current) = guard.regs.get(&old_id) else {
                continue;
            };
            if current.retirement_due || !Arc::ptr_eq(&current.binding, &binding) {
                continue;
            }
            if current.roots == roots {
                if fully_resolved && !roots.is_empty() {
                    let cfg = Arc::clone(&current.cfg);
                    reclaim_matching_detached(
                        &mut guard,
                        &self.pending_cleanups,
                        &old_id,
                        &cfg,
                        &roots,
                    );
                }
                continue;
            }
            let new_key = corpus_key(&current.corpus, &roots);
            if new_key != old_id.corpus_key {
                let new_id = RegistrationId {
                    source: old_id.source.clone(),
                    corpus_key: new_key,
                };
                let Some(mut registration) = guard.regs.remove(&old_id) else {
                    continue;
                };
                registration.roots = roots;
                registration.binding = Arc::new(());
                registration.absent_ticks = 0;
                registration.checkout_absent_ticks = 0;
                if registration.catch_up == CatchUpState::InFlight {
                    registration.catch_up = CatchUpState::Queued;
                    release_admission(&mut guard, &old_id);
                    guard.queue.push_back(new_id.clone());
                }
                if fully_resolved && !registration.roots.is_empty() {
                    reclaim_matching_detached(
                        &mut guard,
                        &self.pending_cleanups,
                        &new_id,
                        &registration.cfg,
                        &registration.roots,
                    );
                }
                guard.regs.insert(new_id.clone(), registration);
                for queued in &mut guard.queue {
                    if *queued == old_id {
                        *queued = new_id.clone();
                    }
                }
                guard.generation += 1;
            } else if let Some(registration) = guard.regs.get_mut(&old_id) {
                registration.roots = roots;
                registration.binding = Arc::new(());
                registration.absent_ticks = 0;
                registration.checkout_absent_ticks = 0;
                if fully_resolved && !registration.roots.is_empty() {
                    let cfg = Arc::clone(&registration.cfg);
                    let roots = registration.roots.clone();
                    reclaim_matching_detached(
                        &mut guard,
                        &self.pending_cleanups,
                        &old_id,
                        &cfg,
                        &roots,
                    );
                }
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
            .filter(|(_, reg)| !reg.retirement_due && reg.roots.iter().any(|owned| owned == root))
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
            .filter(|(_, reg)| !reg.retirement_due)
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
        let built: Vec<_> = corpora
            .into_iter()
            .map(|corpus| {
                let roots = build_roots(&corpus);
                (corpus, roots)
            })
            .collect();
        let mut guard = self.lock();
        let mut candidates = Vec::new();
        for (corpus, mut roots) in built {
            stabilize_unresolved_roots(&guard, &corpus, &mut roots);
            let id = RegistrationId {
                source: source.clone(),
                corpus_key: corpus_key(&corpus, &roots),
            };
            candidates.push((id, corpus, roots));
        }
        for (id, _, roots) in &candidates {
            if let Some(message) = binding_conflict(&guard, id, &cfg, roots) {
                return Err(message);
            }
        }
        let candidate_ids: HashSet<_> = candidates.iter().map(|(id, ..)| id.clone()).collect();
        let over_limit = match &source {
            ConfigSource::Baseline => false,
            ConfigSource::RepoLayer(_) => {
                let live = guard
                    .regs
                    .keys()
                    .filter(|id| matches!(id.source, ConfigSource::RepoLayer(_)))
                    .count();
                let mut added = 0;
                let mut reusable = HashSet::new();
                let mut seen = HashSet::new();
                for (id, _, roots) in &candidates {
                    if !seen.insert(id.clone()) || guard.regs.contains_key(id) {
                        continue;
                    }
                    added += 1;
                    for (index, detached) in guard.detached.iter().enumerate() {
                        if !reusable.contains(&index)
                            && detached._ticket.is_some()
                            && detached_matches(detached, id, &cfg, roots)
                        {
                            reusable.insert(index);
                            break;
                        }
                    }
                }
                live + self.retired_runtime.load(Ordering::SeqCst) + added - reusable.len()
                    > MAX_RUNTIME_REGISTRATIONS
            }
        };
        let removed: HashSet<_> = guard
            .regs
            .keys()
            .filter(|id| id.source == source && !candidate_ids.contains(*id))
            .cloned()
            .collect();
        let mut retired = Vec::new();
        if over_limit {
            let mut pending = self
                .pending_cleanups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for id in &removed {
                if let Some(registration) = guard.regs.get_mut(id) {
                    registration.retirement_missing = false;
                    if !registration.retirement_due {
                        registration.retirement_due = true;
                        release_admission(&mut guard, id);
                        pending.push_back(PendingCleanup::Deferred(id.clone()));
                    }
                }
            }
            guard.queue.retain(|id| !removed.contains(id));
            if let ConfigSource::RepoLayer(path) = &source
                && guard.retry_sources.len() < MAX_RUNTIME_REGISTRATIONS
            {
                guard.retry_sources.insert(path.clone());
            }
        } else {
            for id in &removed {
                if let Some(cleanup) =
                    self.retire_or_defer(&mut guard, id, RetirementCause::ConfigRemoval)
                {
                    retired.push(cleanup);
                }
            }
            guard.queue.retain(|id| !removed.contains(id));
            if let ConfigSource::RepoLayer(path) = &source {
                guard.retry_sources.remove(path);
            }
        }
        for (id, corpus, roots) in candidates {
            if over_limit
                && !guard.regs.contains_key(&id)
                && !guard.detached.iter().any(|detached| {
                    detached._ticket.is_some() && detached_matches(detached, &id, &cfg, &roots)
                })
            {
                continue;
            }
            if guard
                .regs
                .get(&id)
                .is_some_and(|existing| existing.retirement_due)
            {
                self.cancel_deferred(&id);
            }
            if let Some(existing) = guard.regs.get_mut(&id)
                && registration_equivalent(existing, &corpus, &cfg, &roots)
            {
                existing.lock_keys = corpus_lock_keys(&corpus);
                existing.corpus = corpus;
                existing.cfg = cfg.clone();
                if existing.retirement_due {
                    existing.retirement_due = false;
                    existing.retirement_missing = false;
                    existing.catch_up = CatchUpState::Queued;
                    guard.queue.push_back(id);
                }
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
            reclaim_matching_detached(&mut guard, &self.pending_cleanups, &id, &cfg, &roots);
            guard.regs.insert(
                id.clone(),
                Registration {
                    lock_keys: corpus_lock_keys(&corpus),
                    corpus,
                    cfg: cfg.clone(),
                    roots,
                    binding: Arc::new(()),
                    retirement_due: false,
                    retirement_missing: false,
                    observation: Observation::Watched,
                    pending: PendingWork::Idle,
                    catch_up: CatchUpState::Queued,
                    last_error: None,
                    reconciled_once: false,
                    retry_on_tick: false,
                    absent_ticks: 0,
                    checkout_absent_ticks: 0,
                    error_repeats: 0,
                },
            );
            guard.queue.push_back(id);
        }
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
        self.settled.notify_waiters();
        if over_limit {
            Err("runtime watcher registration limit reached; retaining source for retry".into())
        } else {
            Ok(retired)
        }
    }

    pub(crate) fn repo_layer_sources(&self) -> Vec<PathBuf> {
        let guard = self.lock();
        let mut paths: std::collections::BTreeSet<PathBuf> =
            guard.retry_sources.iter().cloned().collect();
        for id in guard.regs.keys() {
            if let ConfigSource::RepoLayer(path) = &id.source {
                paths.insert(path.clone());
            }
        }
        for detached in &guard.detached {
            if let ConfigSource::RepoLayer(path) = &detached.id.source {
                paths.insert(path.clone());
            }
        }
        paths.into_iter().collect()
    }

    pub(crate) fn forget_retry_source(&self, path: &Path) {
        self.lock().retry_sources.remove(path);
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
        if let Some(reg) = guard.regs.get_mut(id)
            && !reg.retirement_due
        {
            reg.pending.add_paths(paths);
        }
        Self::mark_dirty_locked(&mut guard, id);
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
    }

    pub(crate) fn mark_degraded(&self, id: &RegistrationId, error: String) {
        if let Some(reg) = self.lock().regs.get_mut(id) {
            reg.observation = Observation::Degraded {
                error,
                retrying: true,
            };
        }
    }

    /// Marks `id` degraded because the native watcher backend never
    /// initialized. Unlike [`WatchRegistry::mark_degraded`], this state
    /// never clears itself on a retry: recovery needs a daemon restart.
    pub(crate) fn mark_backend_unavailable(&self, id: &RegistrationId) {
        if let Some(reg) = self.lock().regs.get_mut(id) {
            reg.observation = Observation::Degraded {
                error: "notify debouncer unavailable".to_owned(),
                retrying: false,
            };
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

    /// Whether `error` equals the error that `id`'s previous catch-up pass
    /// recorded, so the caller can log a repeated failure below `warn`.
    pub(crate) fn repeats_last_error(&self, id: &RegistrationId, error: &str) -> bool {
        let mut guard = self.lock();
        let Some(reg) = guard.regs.get_mut(id) else {
            return false;
        };
        if reg.last_error.as_deref() != Some(error) {
            reg.error_repeats = 0;
            return false;
        }
        reg.error_repeats = reg.error_repeats.saturating_add(1);
        if reg.error_repeats >= REPEAT_FAILURE_REMINDER_EVERY {
            reg.error_repeats = 0;
            return false;
        }
        true
    }

    fn cancel_deferred(&self, id: &RegistrationId) {
        self.pending_cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|item| match item {
                PendingCleanup::Deferred(queued) => queued != id,
                PendingCleanup::Retired(_) | PendingCleanup::Detached(_) => true,
            });
    }

    fn retire_or_defer(
        &self,
        guard: &mut Inner,
        id: &RegistrationId,
        cause: RetirementCause,
    ) -> Option<RetiredRegistration> {
        let mut pending = self
            .pending_cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.is_empty()
            && let Ok(permit) = self.cleanup_slots.clone().try_acquire_owned()
        {
            let registration = guard.regs.remove(id)?;
            release_admission(guard, id);
            let mut retired = RetiredRegistration::from_registration(
                id,
                &registration,
                &self.retired_runtime,
                permit,
            );
            if cause == RetirementCause::MissingRoot {
                attach_ordinary_owner(guard, id, &registration, &mut retired);
            }
            return Some(retired);
        }
        if let Some(registration) = guard.regs.get_mut(id) {
            registration.retirement_missing = cause == RetirementCause::MissingRoot;
            if !registration.retirement_due {
                registration.retirement_due = true;
                release_admission(guard, id);
                pending.push_back(PendingCleanup::Deferred(id.clone()));
            }
        }
        None
    }

    /// Returns failed cleanup to the FIFO without retaining an execution permit.
    pub(crate) fn park_failed_cleanup(&self, mut registration: RetiredRegistration) {
        registration._permit = None;
        let Some(token) = registration
            .detached
            .as_ref()
            .or(registration.ordinary_owner.as_ref())
            .cloned()
        else {
            self.pending_cleanups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(PendingCleanup::Retired(registration));
            return;
        };
        let mut guard = self.lock();
        let Some(index) = guard
            .detached
            .iter()
            .position(|detached| Arc::ptr_eq(&detached.token, &token))
        else {
            return;
        };
        let detached = &mut guard.detached[index];
        if !registration.waiting_roots.is_empty() {
            for root in &registration.waiting_roots {
                if detached.independent_dirs.contains(root) {
                    detached.rearmed_ticks.insert(root.clone(), 0);
                }
            }
            detached
                .waiting_roots
                .append(&mut registration.waiting_roots);
            detached.absent_ticks = 0;
            detached.binding = Arc::new(());
        }
        registration.roots.append(&mut detached.ready_roots);
        if !registration.roots.is_empty() {
            detached.queued = true;
            self.pending_cleanups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(PendingCleanup::Detached(token));
        }
        let finished = detached.waiting_roots.is_empty() && registration.roots.is_empty();
        if finished {
            guard.detached.remove(index);
        } else {
            detached.retired = Some(registration);
        }
    }

    /// Admits retries and deferred retirements in FIFO order, up to the task cap.
    pub(crate) fn take_pending_cleanups(&self) -> Vec<RetiredRegistration> {
        let mut guard = self.lock();
        let mut pending = self
            .pending_cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut admitted = Vec::new();
        let mut removed = HashSet::new();
        while let Some(item) = pending.pop_front() {
            let Ok(permit) = self.cleanup_slots.clone().try_acquire_owned() else {
                pending.push_front(item);
                break;
            };
            match item {
                PendingCleanup::Retired(mut registration) => {
                    registration._permit = Some(permit);
                    admitted.push(registration);
                }
                PendingCleanup::Detached(token) => {
                    let Some(detached) = guard
                        .detached
                        .iter_mut()
                        .find(|detached| Arc::ptr_eq(&detached.token, &token))
                    else {
                        continue;
                    };
                    detached.queued = false;
                    let Some(mut registration) = detached.retired.take() else {
                        continue;
                    };
                    registration._permit = Some(permit);
                    admitted.push(registration);
                }
                PendingCleanup::Deferred(id) => {
                    let Some(registration) = guard.regs.get(&id) else {
                        continue;
                    };
                    if !registration.retirement_due {
                        continue;
                    }
                    let registration = guard.regs.remove(&id).expect("checked above");
                    release_admission(&mut guard, &id);
                    let mut retired = RetiredRegistration::from_registration(
                        &id,
                        &registration,
                        &self.retired_runtime,
                        permit,
                    );
                    if registration.retirement_missing {
                        attach_ordinary_owner(&mut guard, &id, &registration, &mut retired);
                    }
                    admitted.push(retired);
                    removed.insert(id);
                }
            }
        }
        if !removed.is_empty() {
            guard.queue.retain(|id| !removed.contains(id));
            guard.generation += 1;
        }
        drop(pending);
        drop(guard);
        if !removed.is_empty() {
            self.signal_changed();
            self.settled.notify_waiters();
        }
        admitted
    }

    /// Copies each registration's binding and corpus for an unlocked probe.
    /// The caller expands declared paths after releasing the registry lock.
    pub(crate) fn root_probes(&self) -> Vec<RootProbe> {
        let guard = self.lock();
        let mut probes = Vec::new();
        for (id, reg) in &guard.regs {
            if reg.retirement_due && !reg.retirement_missing {
                continue;
            }
            probes.push(RootProbe {
                id: id.clone(),
                corpus: reg.corpus.clone(),
                saved_roots: reg.saved_roots(id),
                binding: reg.binding.clone(),
            });
        }
        probes
    }

    /// Whether a live registration owns this exact physical root in the same store.
    pub(crate) fn live_claims_retired_root(&self, cfg: &Config, root: &Path) -> bool {
        let guard = self.lock();
        for (id, registration) in &guard.regs {
            if registration.cfg.storage.ground_dir != cfg.storage.ground_dir {
                continue;
            }
            for saved in registration.saved_roots(id) {
                if saved == root {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(test)]
    pub(super) fn retired_runtime_count(&self) -> usize {
        self.retired_runtime.load(Ordering::SeqCst)
    }

    pub(crate) fn detached_probes(&self) -> Vec<(Arc<()>, Vec<PathBuf>)> {
        let guard = self.lock();
        let mut probes = Vec::new();
        for detached in &guard.detached {
            if !detached.waiting_roots.is_empty() {
                probes.push((detached.binding.clone(), detached.waiting_roots.clone()));
            }
        }
        probes
    }

    pub(crate) fn record_detached_absence(
        &self,
        observations: &[DetachedRootObservation],
        threshold: u32,
    ) {
        let mut guard = self.lock();
        let mut ready = Vec::new();
        let mut claims = HashSet::new();
        for (id, registration) in &guard.regs {
            for root in registration.saved_roots(id) {
                claims.insert((registration.cfg.storage.ground_dir.clone(), root));
            }
        }
        for detached in &mut guard.detached {
            let Some(DetachedRootObservation { absences, .. }) = observations
                .iter()
                .find(|observation| Arc::ptr_eq(&observation.binding, &detached.binding))
            else {
                continue;
            };
            let mut eligible = Vec::new();
            if detached.rearmed_ticks.is_empty() {
                let absent = detached.waiting_roots.iter().all(|root| {
                    absences
                        .iter()
                        .any(|(path, absent)| path == root && *absent)
                });
                if absent {
                    detached.absent_ticks = detached.absent_ticks.saturating_add(1);
                } else {
                    detached.absent_ticks = 0;
                }
                if detached.absent_ticks >= threshold {
                    detached.absent_ticks = 0;
                    eligible.append(&mut detached.waiting_roots);
                }
            } else {
                for (root, ticks) in &mut detached.rearmed_ticks {
                    let absent = !claims
                        .contains(&(detached.cfg.storage.ground_dir.clone(), root.clone()))
                        && absences
                            .iter()
                            .find(|(path, _)| path == root)
                            .is_some_and(|(_, absent)| *absent);
                    if absent {
                        *ticks = ticks.saturating_add(1);
                    } else {
                        *ticks = 0;
                    }
                }
                let all_absent = detached.waiting_roots.iter().all(|root| {
                    absences
                        .iter()
                        .any(|(path, absent)| path == root && *absent)
                });
                if all_absent {
                    detached.absent_ticks = detached.absent_ticks.saturating_add(1);
                } else {
                    detached.absent_ticks = 0;
                }
                let group_ready = detached.absent_ticks >= threshold;
                if group_ready {
                    detached.absent_ticks = 0;
                }
                detached.waiting_roots.retain(|root| {
                    let ready = if detached.independent_dirs.contains(root) {
                        detached
                            .rearmed_ticks
                            .get(root)
                            .is_some_and(|ticks| *ticks >= threshold)
                    } else {
                        group_ready
                    };
                    if ready {
                        eligible.push(root.clone());
                    }
                    !ready
                });
                for root in &eligible {
                    detached.rearmed_ticks.remove(root);
                }
            }
            if !eligible.is_empty() {
                if let Some(retired) = detached.retired.as_mut() {
                    retired.roots.append(&mut eligible);
                    if !detached.queued {
                        detached.queued = true;
                        ready.push(detached.token.clone());
                    }
                } else {
                    detached.ready_roots.append(&mut eligible);
                }
            }
        }
        if ready.is_empty() {
            return;
        }
        let mut pending = self
            .pending_cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for token in ready {
            pending.push_back(PendingCleanup::Detached(token));
        }
        guard.generation += 1;
        drop(pending);
        drop(guard);
        self.signal_changed();
    }

    /// Applies only observations whose binding still matches the registration.
    /// A stale or missing observation neither ages nor revives a replacement.
    pub(crate) fn record_absent_roots(
        &self,
        observations: &HashMap<RegistrationId, RootObservation>,
        threshold: u32,
    ) -> Vec<RetiredRegistration> {
        let mut guard = self.lock();
        let mut expired = Vec::new();
        let mut detached = Vec::new();
        let mut revived = Vec::new();
        for (id, reg) in &mut guard.regs {
            let Some(observed) = observations.get(id) else {
                continue;
            };
            if !Arc::ptr_eq(&reg.binding, &observed.probe.binding) {
                continue;
            }
            match observed.presence {
                RootPresence::Present => {
                    reg.absent_ticks = 0;
                    reg.checkout_absent_ticks = 0;
                    if reg.retirement_missing {
                        reg.retirement_due = false;
                        reg.retirement_missing = false;
                        reg.catch_up = CatchUpState::Queued;
                        revived.push(id.clone());
                    }
                    continue;
                }
                RootPresence::Absent => {
                    reg.checkout_absent_ticks = 0;
                }
                RootPresence::CheckoutAbsent => {
                    reg.absent_ticks = 0;
                    if reg.retirement_missing {
                        reg.retirement_due = false;
                        reg.retirement_missing = false;
                        reg.catch_up = CatchUpState::Queued;
                        revived.push(id.clone());
                    }
                }
            }
            if reg.retirement_due {
                continue;
            }
            match observed.presence {
                RootPresence::Absent => {
                    reg.absent_ticks = reg.absent_ticks.saturating_add(1);
                    if reg.absent_ticks >= threshold {
                        expired.push(id.clone());
                    }
                }
                RootPresence::CheckoutAbsent => {
                    reg.checkout_absent_ticks = reg.checkout_absent_ticks.saturating_add(1);
                    if reg.checkout_absent_ticks >= threshold {
                        detached.push(id.clone());
                    }
                }
                RootPresence::Present => {}
            }
        }
        for id in &revived {
            self.cancel_deferred(id);
            guard.queue.push_back(id.clone());
        }
        if expired.is_empty() && detached.is_empty() && revived.is_empty() {
            return Vec::new();
        }
        let mut retired = Vec::new();
        let mut expired_ids = HashSet::new();
        for id in detached {
            let Some(registration) = guard.regs.remove(&id) else {
                continue;
            };
            let mut retired = RetiredRegistration::from_registration_with_permit(
                &id,
                &registration,
                &self.retired_runtime,
                None,
            );
            let saved_roots = retired.roots.clone();
            let ticket = retired._ticket.take();
            retired.roots.clear();
            let mut independent_dirs = HashSet::new();
            for root in &registration.roots {
                if root.resolution == RootResolution::Resolved && root.canonical_file_root.is_none()
                {
                    independent_dirs.insert(root.canonical_watched.clone());
                }
            }
            let token = Arc::new(AtomicBool::new(true));
            retired.detached = Some(token.clone());
            guard.detached.push(DetachedRegistration {
                id: id.clone(),
                cfg: retired.cfg.clone(),
                saved_roots: saved_roots.clone(),
                independent_dirs,
                binding: registration.binding,
                token,
                _ticket: ticket,
                retired: Some(retired),
                waiting_roots: saved_roots,
                ready_roots: Vec::new(),
                absent_ticks: 0,
                rearmed_ticks: HashMap::new(),
                queued: false,
            });
            release_admission(&mut guard, &id);
            expired_ids.insert(id);
        }
        for id in expired {
            if let Some(cleanup) =
                self.retire_or_defer(&mut guard, &id, RetirementCause::MissingRoot)
            {
                retired.push(cleanup);
            }
            expired_ids.insert(id);
        }
        guard.queue.retain(|id| !expired_ids.contains(id));
        guard.generation += 1;
        drop(guard);
        self.signal_changed();
        self.settled.notify_waiters();
        retired
    }

    /// Count of registrations currently observed as [`Observation::Degraded`]
    /// — i.e. `notify` never established (or lost) native watching for their
    /// roots, so they run on periodic reconciliation only. Surfaced on
    /// `daemon status` so degraded watching isn't invisible.
    pub(crate) fn degraded_count(&self) -> usize {
        self.lock()
            .regs
            .values()
            .filter(|reg| matches!(reg.observation, Observation::Degraded { .. }))
            .count()
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
        let boot_catch_up = self.boot_catch_up();
        let guard = self.lock();
        guard
            .regs
            .iter()
            .filter(|(id, reg)| id.source == *source && names.contains(reg.corpus.name.as_str()))
            .filter_map(|(id, reg)| {
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
                if let Observation::Degraded { error, retrying } = &reg.observation {
                    if *retrying {
                        clauses.push(format!(
                            "watcher backend error: {error}; retry will continue"
                        ));
                    } else {
                        clauses.push(
                            "native watcher unavailable; periodic reconciliation only".to_string(),
                        );
                    }
                }
                if !reg.pending.is_idle() {
                    clauses.push("reconciliation pending".to_string());
                }
                match (&id.source, boot_catch_up) {
                    (ConfigSource::Baseline, BootCatchUp::Running) => clauses
                        .push("boot reconciliation in progress; results may be stale".to_string()),
                    (ConfigSource::Baseline, BootCatchUp::Idle)
                    | (ConfigSource::RepoLayer(_), BootCatchUp::Running)
                    | (ConfigSource::RepoLayer(_), BootCatchUp::Idle) => {}
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
        let ids: Vec<RegistrationId> = guard
            .regs
            .iter()
            .filter(|(_, reg)| !reg.retirement_due)
            .map(|(id, _)| id.clone())
            .collect();
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
        if !reg.retirement_due
            && matches!(reg.catch_up, CatchUpState::NotStarted | CatchUpState::Done)
        {
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
    pub(crate) fn begin_next_catch_up(&self, limit: usize) -> Option<RegistrationId> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.active.len() >= limit {
            return None;
        }
        let mut gated: Vec<RegistrationId> = Vec::new();
        let mut deferred: Vec<RegistrationId> = Vec::new();
        let admitted = loop {
            let Some(id) = inner.queue.pop_front() else {
                break None;
            };
            let Some(reg) = inner.regs.get(&id) else {
                continue;
            };
            if reg.retirement_due || reg.catch_up != CatchUpState::Queued {
                continue;
            }
            if reg.retry_on_tick {
                gated.push(id);
                continue;
            }
            let lock_keys = reg.lock_keys.clone();
            if active_locks_overlap(&inner.active, &lock_keys) {
                deferred.push(id);
                continue;
            }
            let leader_corpus = reg.corpus.clone();
            let leader_roots = reg.roots.clone();
            let leader_cfg = reg.cfg.clone();
            let Some(leader) = inner.regs.get_mut(&id) else {
                continue;
            };
            leader.catch_up = CatchUpState::InFlight;
            let mut members = vec![id.clone()];
            let group_members: Vec<RegistrationId> = inner
                .queue
                .iter()
                .filter(|candidate| {
                    inner.regs.get(*candidate).is_some_and(|member| {
                        !member.retirement_due
                            && member.catch_up == CatchUpState::Queued
                            && member.corpus == leader_corpus
                            && member.roots == leader_roots
                            && !cfg_binding_differs(&member.cfg, &leader_cfg)
                    })
                })
                .cloned()
                .collect();
            for member_id in group_members {
                if let Some(member) = inner.regs.get_mut(&member_id) {
                    member.catch_up = CatchUpState::InFlight;
                }
                inner.queue.retain(|candidate| candidate != &member_id);
                members.push(member_id);
            }
            inner.active.push(ActiveGroup {
                leader: id.clone(),
                lock_keys,
                members,
                valid: true,
            });
            break Some(id);
        };
        for id in deferred.into_iter().rev() {
            inner.queue.push_front(id);
        }
        inner.queue.extend(gated);
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
        let led_group = guard
            .active
            .iter()
            .position(|group| group.leader == *id)
            .map(|position| guard.active.remove(position));
        let members: Vec<RegistrationId> = if let Some(group) = led_group {
            if group.members.is_empty() {
                if group.valid && guard.regs.contains_key(id) {
                    vec![id.clone()]
                } else {
                    Vec::new()
                }
            } else {
                group.members
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
                // Another group member shares the leader's pass. Its admission
                // slot is released here. On success its own pending work stays;
                // on failure it records the error and requeues a full pass.
                match &outcome {
                    Ok(()) => {
                        reg.reconciled_once = true;
                        reg.last_error = None;
                        if reg.pending.is_idle() {
                            reg.catch_up = CatchUpState::Done;
                        } else {
                            reg.catch_up = CatchUpState::Queued;
                            guard.queue.push_back(member_id.clone());
                        }
                    }
                    Err(error) => {
                        reg.last_error = Some(error.clone());
                        reg.retry_on_tick = true;
                        reg.pending = PendingWork::FullRoot;
                        reg.catch_up = CatchUpState::Queued;
                        guard.queue.push_back(member_id.clone());
                    }
                }
                continue;
            }
            reg.reconciled_once = true;
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
        self.settled.notify_waiters();
        returned
    }

    /// Reports whether any registration under `source` for a corpus in
    /// `names` has not yet finished its first pass and still has one queued
    /// or in flight. Routine passes on a registration that already
    /// reconciled once do not count, and neither does a failed first pass.
    fn first_reconcile_pending(&self, source: &ConfigSource, names: &HashSet<&str>) -> bool {
        let guard = self.lock();
        for (id, reg) in &guard.regs {
            if id.source != *source
                || !names.contains(reg.corpus.name.as_str())
                || reg.retirement_due
                || reg.reconciled_once
                || reg.last_error.is_some()
            {
                continue;
            }
            match reg.catch_up {
                CatchUpState::Queued | CatchUpState::InFlight => return true,
                CatchUpState::NotStarted | CatchUpState::Done => {}
            }
        }
        false
    }

    /// Records that the watcher pump failed to start. With no pump, no
    /// catch-up pass is ever admitted, so a cold-start wait cannot succeed.
    pub(crate) fn mark_pump_unavailable(&self) {
        self.pump_unavailable.store(true, Ordering::SeqCst);
    }

    /// Records that the watcher pump started.
    pub(crate) fn mark_pump_available(&self) {
        self.pump_unavailable.store(false, Ordering::SeqCst);
    }

    pub(crate) fn pump_unavailable(&self) -> bool {
        self.pump_unavailable.load(Ordering::SeqCst)
    }

    /// Waits until no registration of `queried` under `source` has an
    /// unfinished first pass, or until `timeout` elapses. Returns `true`
    /// when the registrations settled and `false` on timeout or when the
    /// watcher pump is not running. Holds no registry lock while it waits.
    pub(crate) async fn wait_first_reconcile(
        &self,
        source: &ConfigSource,
        queried: &[CorpusConfig],
        timeout: std::time::Duration,
    ) -> bool {
        let mut names: HashSet<&str> = HashSet::with_capacity(queried.len());
        for corpus in queried {
            names.insert(corpus.name.as_str());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.first_reconcile_pending(source, &names) {
                return true;
            }
            if self.pump_unavailable() {
                return false;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return !self.first_reconcile_pending(source, &names);
            }
        }
    }

    pub(crate) fn config_for(&self, id: &RegistrationId) -> Option<(CorpusConfig, Arc<Config>)> {
        self.lock()
            .regs
            .get(id)
            .map(|reg| (reg.corpus.clone(), reg.cfg.clone()))
    }
}

/// Reports whether any in-flight group already holds one of `lock_keys`.
fn active_locks_overlap(active: &[ActiveGroup], lock_keys: &[CorpusKey]) -> bool {
    for group in active {
        for key in &group.lock_keys {
            if lock_keys.contains(key) {
                return true;
            }
        }
    }
    false
}

/// Drops `id` from the in-flight admission group, invalidating the leader slot
/// when `id` was leading it so the next `begin_next_catch_up` can re-admit.
fn release_admission(guard: &mut Inner, id: &RegistrationId) {
    for group in &mut guard.active {
        if let Some(position) = group.members.iter().position(|active| active == id) {
            group.members.remove(position);
        }
        if group.leader == *id {
            group.valid = false;
        }
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
            configured_file_root: None,
            resolution: RootResolution::Resolved,
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
    fn retired_root_claim_requires_same_path_and_ground_store() {
        let registry = WatchRegistry::new();
        let mut current = Config::default();
        current.storage.ground_dir = "/ground-b".into();
        assert_eq!(
            registry.register(
                ConfigSource::Baseline,
                corpus("current"),
                Arc::new(current.clone()),
                vec![root("/target", corpus("current"))],
            ),
            RegisterOutcome::New
        );
        let mut retired = current.clone();
        retired.storage.ground_dir = "/ground-a".into();
        assert!(!registry.live_claims_retired_root(&retired, Path::new("/target")));
        assert!(!registry.live_claims_retired_root(&current, Path::new("/other")));
        assert!(registry.live_claims_retired_root(&current, Path::new("/target")));
    }

    fn detached_registry_at_cap() -> (WatchRegistry, Arc<Config>, Vec<WatchRoot>) {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/old/.hallouminate/config.toml"));
        let selected = corpus("wiki");
        let mut file_root = root("/parent", selected.clone());
        file_root.canonical_file_root = Some(PathBuf::from("/parent/file.md"));
        file_root.mode = notify::RecursiveMode::NonRecursive;
        let roots = vec![
            root("/target", selected.clone()),
            file_root,
            root("/secondary", selected.clone()),
        ];
        assert_eq!(
            registry.register(source.clone(), selected, cfg.clone(), roots.clone()),
            RegisterOutcome::New
        );
        for index in 0..MAX_RUNTIME_REGISTRATIONS - 1 {
            assert_eq!(
                registry.register(
                    ConfigSource::RepoLayer(PathBuf::from(format!(
                        "/other-{index}/.hallouminate/config.toml"
                    ))),
                    corpus(&format!("other-{index}")),
                    cfg.clone(),
                    vec![],
                ),
                RegisterOutcome::New
            );
        }
        let probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id.source == source)
            .expect("old source probe");
        let id = probe.id.clone();
        let observations = HashMap::from([(
            id,
            RootObservation {
                probe,
                presence: RootPresence::CheckoutAbsent,
            },
        )]);
        for _ in 0..3 {
            assert!(registry.record_absent_roots(&observations, 3).is_empty());
        }
        assert_eq!(registry.detached_probes().len(), 1);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 1);
        assert_eq!(registry.lock().regs.len(), MAX_RUNTIME_REGISTRATIONS - 1);
        assert!(
            registry
                .repo_layer_sources()
                .iter()
                .any(|path| path == Path::new("/old/.hallouminate/config.toml"))
        );
        (registry, cfg, roots)
    }

    #[test]
    fn detached_slot_reuses_only_exact_full_storage_binding_at_cap() {
        let (registry, cfg, roots) = detached_registry_at_cap();
        let source = ConfigSource::RepoLayer(PathBuf::from("/new/.hallouminate/config.toml"));
        let mut partial = roots.clone();
        partial[1] = root("/parent", corpus("wiki"));
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg.clone(), partial),
            RegisterOutcome::LimitReached
        );
        let mut changed = (*cfg).clone();
        changed.storage.ground_dir = "/different-ground".into();
        assert_eq!(
            registry.register(
                source.clone(),
                corpus("wiki"),
                Arc::new(changed),
                roots.clone()
            ),
            RegisterOutcome::LimitReached
        );
        assert_eq!(registry.detached_probes().len(), 1);
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg, roots),
            RegisterOutcome::New
        );
        assert!(registry.detached_probes().is_empty());
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert_eq!(registry.lock().regs.len(), MAX_RUNTIME_REGISTRATIONS);
        assert!(
            !registry
                .repo_layer_sources()
                .iter()
                .any(|path| path == Path::new("/old/.hallouminate/config.toml"))
        );
    }

    #[test]
    fn ticketless_baseline_retirement_cannot_credit_a_runtime_slot() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let selected = corpus("wiki");
        let roots = vec![root("/target", selected.clone())];
        registry.seed_baseline(selected.clone(), cfg.clone(), roots.clone());
        let probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id.source == ConfigSource::Baseline)
            .expect("baseline probe");
        let observations = HashMap::from([(
            probe.id.clone(),
            RootObservation {
                probe,
                presence: RootPresence::Absent,
            },
        )]);
        let mut retired = Vec::new();
        for _ in 0..3 {
            retired.extend(registry.record_absent_roots(&observations, 3));
        }
        assert_eq!(retired.len(), 1);
        assert!(retired[0]._ticket.is_none());
        for index in 0..MAX_RUNTIME_REGISTRATIONS {
            assert_eq!(
                registry.register(
                    ConfigSource::RepoLayer(PathBuf::from(format!("/other-{index}/config.toml"))),
                    corpus(&format!("other-{index}")),
                    cfg.clone(),
                    vec![],
                ),
                RegisterOutcome::New
            );
        }
        let source = ConfigSource::RepoLayer(PathBuf::from("/new/config.toml"));
        assert_eq!(
            registry.register(source.clone(), selected.clone(), cfg.clone(), roots.clone()),
            RegisterOutcome::LimitReached,
            "a Baseline owner cannot transfer a runtime slot"
        );
        assert!(
            registry
                .replace_source(source, vec![selected], cfg, |_| roots.clone())
                .is_err()
        );
        assert_eq!(registry.lock().regs.len(), MAX_RUNTIME_REGISTRATIONS);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert!(
            retired[0]
                .ordinary_owner
                .as_ref()
                .expect("baseline owner")
                .load(Ordering::SeqCst)
        );
    }

    #[test]
    fn ordinary_runtime_retirement_transfers_exact_slot_at_cap() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let selected = corpus("wiki");
        let roots = vec![root("/target", selected.clone())];
        let source = ConfigSource::RepoLayer(PathBuf::from("/old/config.toml"));
        assert_eq!(
            registry.register(source.clone(), selected.clone(), cfg.clone(), roots.clone()),
            RegisterOutcome::New
        );
        for index in 0..MAX_RUNTIME_REGISTRATIONS - 1 {
            assert_eq!(
                registry.register(
                    ConfigSource::RepoLayer(PathBuf::from(format!("/other-{index}/config.toml"))),
                    corpus(&format!("other-{index}")),
                    cfg.clone(),
                    vec![],
                ),
                RegisterOutcome::New
            );
        }
        let probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id.source == source)
            .expect("runtime probe");
        let observations = HashMap::from([(
            probe.id.clone(),
            RootObservation {
                probe,
                presence: RootPresence::Absent,
            },
        )]);
        let mut retired = Vec::new();
        for _ in 0..3 {
            retired.extend(registry.record_absent_roots(&observations, 3));
        }
        assert_eq!(retired.len(), 1);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry.register(
                ConfigSource::RepoLayer(PathBuf::from("/new/config.toml")),
                selected,
                cfg,
                roots,
            ),
            RegisterOutcome::New,
        );
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert_eq!(registry.lock().regs.len(), MAX_RUNTIME_REGISTRATIONS);
        assert!(
            !retired[0]
                .ordinary_owner
                .as_ref()
                .expect("runtime owner")
                .load(Ordering::SeqCst)
        );
    }

    #[test]
    fn pr_786_register_reclaims_permuted_secondary_roots_at_cap() {
        let (registry, cfg, roots) = detached_registry_at_cap();
        let source = ConfigSource::RepoLayer(PathBuf::from("/new/.hallouminate/config.toml"));
        let mut permuted = roots.clone();
        permuted.swap(1, 2);
        let mut duplicated = permuted.clone();
        duplicated[1] = duplicated[2].clone();
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg.clone(), duplicated),
            RegisterOutcome::LimitReached,
            "duplicate keys cannot replace a missing physical root"
        );
        let mut unresolved = permuted.clone();
        unresolved[1].resolution = RootResolution::Unresolved;
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg.clone(), unresolved),
            RegisterOutcome::LimitReached,
        );
        let mut changed_file = permuted.clone();
        changed_file[2] = root("/parent", corpus("wiki"));
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg.clone(), changed_file),
            RegisterOutcome::LimitReached,
            "the watched parent cannot replace the exact file key"
        );
        let mut changed_primary = permuted.clone();
        changed_primary[0] = root("/new-primary", corpus("wiki"));
        assert_eq!(
            registry.register(source.clone(), corpus("wiki"), cfg.clone(), changed_primary),
            RegisterOutcome::LimitReached,
        );
        let mut changed_cfg = (*cfg).clone();
        changed_cfg.embeddings.model = "different-model".into();
        assert_eq!(
            registry.register(
                source.clone(),
                corpus("wiki"),
                Arc::new(changed_cfg),
                permuted.clone()
            ),
            RegisterOutcome::LimitReached,
        );
        assert_eq!(registry.detached_probes().len(), 1);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry.register(
                source.clone(),
                corpus("wiki"),
                cfg.clone(),
                permuted.clone()
            ),
            RegisterOutcome::New,
            "the same physical roots reuse the detached ticket at cap"
        );
        assert!(registry.detached_probes().is_empty());
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert!(registry.is_registered_unchanged(&source, &corpus("wiki"), &cfg, &permuted));
        assert!(!registry.is_registered_unchanged(&source, &corpus("wiki"), &cfg, &roots));
    }

    #[test]
    fn pr_786_reload_reclaims_permuted_secondary_roots_at_cap() {
        let (registry, cfg, roots) = detached_registry_at_cap();
        let source = ConfigSource::RepoLayer(PathBuf::from("/reload/.hallouminate/config.toml"));
        let mut permuted = roots.clone();
        permuted.swap(1, 2);
        registry
            .replace_source(source.clone(), vec![corpus("wiki")], cfg.clone(), |_| {
                permuted.clone()
            })
            .expect("permuted reload reuses the detached ticket at cap");
        assert!(registry.detached_probes().is_empty());
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert!(registry.is_registered_unchanged(&source, &corpus("wiki"), &cfg, &permuted));
    }

    #[test]
    fn detached_slot_reloads_exact_match_even_with_unmatched_overflow() {
        let (registry, cfg, roots) = detached_registry_at_cap();
        let source = ConfigSource::RepoLayer(PathBuf::from("/new/.hallouminate/config.toml"));
        assert!(
            registry
                .replace_source(
                    source.clone(),
                    vec![corpus("wiki"), corpus("overflow")],
                    cfg.clone(),
                    |corpus| {
                        if corpus.name == "wiki" {
                            roots.clone()
                        } else {
                            vec![root("/overflow", corpus.clone())]
                        }
                    },
                )
                .is_err()
        );
        assert!(registry.detached_probes().is_empty());
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert_eq!(registry.lock().regs.len(), MAX_RUNTIME_REGISTRATIONS);
        assert!(registry.is_registered_unchanged(&source, &corpus("wiki"), &cfg, &roots));
        assert!(!registry.is_registered_unchanged(
            &source,
            &corpus("overflow"),
            &cfg,
            &[root("/overflow", corpus("overflow"))]
        ));
    }

    #[test]
    fn detached_obligations_with_same_id_keep_distinct_storage() {
        let registry = WatchRegistry::new();
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate/config.toml"));
        let roots = vec![root("/target", corpus("wiki"))];
        for ground in ["/ground-a", "/ground-b"] {
            let mut cfg = Config::default();
            cfg.storage.ground_dir = ground.into();
            assert_eq!(
                registry.register(source.clone(), corpus("wiki"), Arc::new(cfg), roots.clone()),
                RegisterOutcome::New
            );
            let probe = registry.root_probes().pop().expect("active probe");
            let observations = HashMap::from([(
                probe.id.clone(),
                RootObservation {
                    probe,
                    presence: RootPresence::CheckoutAbsent,
                },
            )]);
            for _ in 0..3 {
                assert!(registry.record_absent_roots(&observations, 3).is_empty());
            }
        }
        let guard = registry.lock();
        assert_eq!(guard.detached.len(), 2);
        let mut grounds = Vec::new();
        for detached in &guard.detached {
            grounds.push(detached.cfg.storage.ground_dir.clone());
        }
        grounds.sort();
        assert_eq!(grounds, ["/ground-a", "/ground-b"]);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn detached_waiting_root_rearms_while_failed_sibling_is_active() {
        let (registry, cfg, roots) = detached_registry_at_cap();
        let (binding, saved) = registry.detached_probes().pop().expect("detached roots");
        for _ in 0..3 {
            registry.record_detached_absence(
                &[DetachedRootObservation {
                    binding: binding.clone(),
                    absences: saved.iter().cloned().map(|root| (root, true)).collect(),
                }],
                3,
            );
        }
        let mut old = registry.take_pending_cleanups().pop().expect("old cleanup");
        old.roots = vec![saved[1].clone()];
        old.waiting_roots = vec![saved[0].clone()];
        registry.park_failed_cleanup(old);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 1);

        let mut failed = registry
            .take_pending_cleanups()
            .pop()
            .expect("failed sibling retry");
        assert_eq!(failed.roots, vec![saved[1].clone()]);
        let (binding, waiting) = registry.detached_probes().pop().expect("waiting root");
        assert_eq!(waiting, vec![saved[0].clone()]);
        for _ in 0..3 {
            registry.record_detached_absence(
                &[DetachedRootObservation {
                    binding: binding.clone(),
                    absences: waiting.iter().cloned().map(|root| (root, true)).collect(),
                }],
                3,
            );
        }
        failed.roots.clear();
        registry.park_failed_cleanup(failed);
        let ready = registry
            .take_pending_cleanups()
            .pop()
            .expect("returned root must rearm independently");
        assert_eq!(ready.roots, vec![saved[0].clone()]);

        let source = ConfigSource::RepoLayer(PathBuf::from("/new/.hallouminate/config.toml"));
        assert_eq!(
            registry.register(source, corpus("wiki"), cfg, roots),
            RegisterOutcome::New
        );
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
        assert!(
            !ready
                .detached
                .as_ref()
                .expect("detached token")
                .load(Ordering::SeqCst)
        );
        registry.park_failed_cleanup(ready);
        assert!(registry.take_pending_cleanups().is_empty());
    }

    #[test]
    fn pr_786_rearmed_directory_counts_once_and_does_not_release_file() {
        let (registry, _cfg, _roots) = detached_registry_at_cap();
        let (binding, saved) = registry.detached_probes().pop().expect("detached roots");
        for _ in 0..3 {
            let absences = saved.iter().cloned().map(|root| (root, true)).collect();
            registry.record_detached_absence(
                &[DetachedRootObservation {
                    binding: binding.clone(),
                    absences,
                }],
                3,
            );
        }
        let mut old = registry
            .take_pending_cleanups()
            .pop()
            .expect("initial cleanup");
        old.roots.clear();
        old.waiting_roots = vec![
            saved[0].clone(),
            saved[0].clone(),
            saved[1].clone(),
            saved[2].clone(),
        ];
        registry.park_failed_cleanup(old);
        let (binding, waiting) = registry.detached_probes().pop().expect("rearmed roots");
        assert_eq!(waiting.len(), 4);
        let absent_target = vec![
            (saved[0].clone(), true),
            (saved[1].clone(), false),
            (saved[2].clone(), false),
        ];
        for _ in 0..2 {
            registry.record_detached_absence(
                &[DetachedRootObservation {
                    binding: binding.clone(),
                    absences: absent_target.clone(),
                }],
                3,
            );
        }
        assert!(registry.take_pending_cleanups().is_empty());
        let present_target = vec![
            (saved[0].clone(), false),
            (saved[1].clone(), false),
            (saved[2].clone(), false),
        ];
        registry.record_detached_absence(
            &[DetachedRootObservation {
                binding: binding.clone(),
                absences: present_target,
            }],
            3,
        );
        for _ in 0..2 {
            registry.record_detached_absence(
                &[DetachedRootObservation {
                    binding: binding.clone(),
                    absences: absent_target.clone(),
                }],
                3,
            );
        }
        assert!(registry.take_pending_cleanups().is_empty());
        registry.record_detached_absence(
            &[DetachedRootObservation {
                binding,
                absences: absent_target,
            }],
            3,
        );
        let ready = registry.take_pending_cleanups();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].roots, vec![saved[0].clone(), saved[0].clone()]);
        let (_, waiting) = registry
            .detached_probes()
            .pop()
            .expect("other roots remain");
        assert_eq!(waiting, vec![saved[1].clone(), saved[2].clone()]);
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 1);
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
    fn over_limit_reload_revives_declared_deferred_registration() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        let selected = corpus("wiki");
        registry.register(source.clone(), selected.clone(), cfg.clone(), vec![]);
        let held: Vec<_> = (0..MAX_PENDING_CLEANUPS)
            .map(|_| {
                registry
                    .cleanup_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("slot")
            })
            .collect();
        assert!(
            registry
                .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
                .expect("defer removal")
                .is_empty()
        );
        for index in 0..MAX_RUNTIME_REGISTRATIONS - 1 {
            assert_eq!(
                registry.register(
                    ConfigSource::RepoLayer(PathBuf::from(format!("/other-{index}"))),
                    corpus(&format!("other-{index}")),
                    cfg.clone(),
                    vec![],
                ),
                RegisterOutcome::New
            );
        }
        assert!(
            registry
                .replace_source(
                    source.clone(),
                    vec![selected.clone(), corpus("over-cap")],
                    cfg.clone(),
                    |_| vec![],
                )
                .is_err()
        );
        assert!(registry.is_registered_unchanged(&source, &selected, &cfg, &[]));
        drop(held);
        assert!(registry.take_pending_cleanups().is_empty());
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

        assert_eq!(registry.begin_next_catch_up(1), Some(baseline_id.clone()));
        assert_eq!(registry.begin_next_catch_up(1), None);
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

    fn register_baseline(registry: &WatchRegistry, name: &str) -> RegistrationId {
        registry.register(
            ConfigSource::Baseline,
            corpus(name),
            Arc::new(Config::default()),
            vec![root(&format!("/{name}"), corpus(name))],
        );
        registry
            .snapshot_roots()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| id.corpus_key.name == name)
            .expect("registered id")
    }

    #[test]
    fn catch_up_concurrency_admits_up_to_the_limit_at_once() {
        let registry = WatchRegistry::new();
        let a = register_baseline(&registry, "a");
        let b = register_baseline(&registry, "b");
        let c = register_baseline(&registry, "c");

        let first = registry.begin_next_catch_up(2).expect("first admitted");
        let second = registry.begin_next_catch_up(2).expect("second admitted");
        assert_eq!(registry.begin_next_catch_up(2), None, "limit reached");
        assert_eq!(
            [first.clone(), second.clone()]
                .into_iter()
                .filter(|id| *id == c)
                .count(),
            0,
            "FIFO order admits the first two roots",
        );
        assert_ne!(first, second);
        let waiting = [&a, &b, &c]
            .into_iter()
            .find(|id| **id != first && **id != second)
            .expect("one waits");
        assert_eq!(registry.catch_up_state(waiting), Some(CatchUpState::Queued));

        registry.finish_catch_up(&first, Ok(()));
        assert_eq!(registry.begin_next_catch_up(2).as_ref(), Some(waiting));
        assert_eq!(registry.begin_next_catch_up(2), None);
    }

    #[test]
    fn catch_up_concurrency_of_one_matches_serial_admission() {
        let registry = WatchRegistry::new();
        let a = register_baseline(&registry, "a");
        register_baseline(&registry, "b");

        let first = registry.begin_next_catch_up(1).expect("first admitted");
        assert_eq!(registry.begin_next_catch_up(1), None);
        registry.finish_catch_up(&first, Ok(()));
        let second = registry.begin_next_catch_up(1).expect("second admitted");
        assert_ne!(first, second);
        assert!(first == a || second == a);
    }

    fn corpus_at(name: &str, paths: &[&str]) -> CorpusConfig {
        CorpusConfig {
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            ..corpus(name)
        }
    }

    #[test]
    fn catch_up_runs_worktrees_of_one_corpus_name_concurrently() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let main = corpus_at("wiki", &["/no-such-main"]);
        let worktree = corpus_at("wiki", &["/no-such-worktree"]);
        registry.register(
            ConfigSource::Baseline,
            main.clone(),
            cfg.clone(),
            vec![root("/no-such-main", main)],
        );
        registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/no-such-worktree/.hallouminate")),
            worktree.clone(),
            cfg,
            vec![root("/no-such-worktree", worktree)],
        );

        let first = registry.begin_next_catch_up(2).expect("first admitted");
        let second = registry.begin_next_catch_up(2).expect("second admitted");
        assert_ne!(first, second, "distinct roots run in flight together");
        assert_eq!(registry.begin_next_catch_up(2), None);
    }

    #[test]
    fn catch_up_never_admits_registrations_sharing_a_corpus_key() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let one = corpus_at("wiki", &["/no-such-a"]);
        let overlapping = corpus_at("wiki", &["/no-such-b", "/no-such-a"]);
        registry.register(
            ConfigSource::Baseline,
            one.clone(),
            cfg.clone(),
            vec![root("/no-such-a", one)],
        );
        registry.register(
            ConfigSource::RepoLayer(PathBuf::from("/no-such-b/.hallouminate")),
            overlapping.clone(),
            cfg,
            vec![root("/no-such-b", overlapping)],
        );
        let other = register_baseline(&registry, "other");

        let first = registry.begin_next_catch_up(3).expect("first admitted");
        let second = registry.begin_next_catch_up(3).expect("second admitted");
        assert_eq!(second, other, "key-sharing registration is deferred");
        assert_eq!(registry.begin_next_catch_up(3), None);
        registry.finish_catch_up(&first, Ok(()));
        assert!(registry.begin_next_catch_up(3).is_some());
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

        assert_eq!(registry.begin_next_catch_up(1), Some(baseline_id.clone()));
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

        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));
        registry
            .replace_source(source, vec![], Arc::new(Config::default()), |_| vec![])
            .unwrap();
        assert_eq!(registry.begin_next_catch_up(1), None);
        assert_eq!(registry.finish_catch_up(&leader, Ok(())), PendingWork::Idle);
        assert_eq!(registry.catch_up_state(&member), None);
        assert_eq!(registry.begin_next_catch_up(1), Some(other));
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
        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));

        registry
            .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
            .unwrap();
        let replacement = corpus("wiki");
        let replacement_roots = vec![root("/repo", replacement.clone())];
        assert_eq!(
            registry.register(source, replacement, cfg, replacement_roots),
            RegisterOutcome::New
        );
        assert_eq!(registry.begin_next_catch_up(1), None);
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
    fn identical_reconcile_failures_re_warn_at_the_reminder_cadence() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        let wiki = corpus("wiki");
        let wiki_root = root("/repo", wiki.clone());
        registry.register(source.clone(), wiki, cfg, vec![wiki_root]);
        let id = RegistrationId {
            source,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/repo"),
            },
        };
        assert!(!registry.repeats_last_error(&id, "boom"));
        registry.finish_catch_up(&id, Err("boom".into()));

        let mut warned = 0;
        for _ in 0..REPEAT_FAILURE_REMINDER_EVERY * 2 {
            if !registry.repeats_last_error(&id, "boom") {
                warned += 1;
            }
        }
        assert_eq!(
            warned, 2,
            "a long identical failure run must re-warn periodically"
        );
    }

    #[test]
    fn pending_cleanups_retain_every_admitted_registration() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for index in 0..=MAX_PENDING_CLEANUPS {
            registry.park_failed_cleanup(RetiredRegistration {
                corpus: format!("c{index}"),
                root: PathBuf::from("/gone"),
                roots: vec![PathBuf::from("/gone")],
                lock_keys: Vec::new(),
                cfg: cfg.clone(),
                failed_attempts: 0,
                detached: None,
                ordinary_owner: None,
                log_missing_root: false,
                waiting_roots: Vec::new(),
                _permit: None,
                _ticket: None,
            });
        }
        let mut active = registry.take_pending_cleanups();
        assert_eq!(active.len(), MAX_PENDING_CLEANUPS);
        assert_eq!(active[0].corpus, "c0");
        assert_eq!(active[MAX_PENDING_CLEANUPS - 1].corpus, "c63");
        assert!(registry.cleanup_slots.clone().try_acquire_owned().is_err());
        assert!(registry.take_pending_cleanups().is_empty());

        registry.park_failed_cleanup(active.remove(0));
        let next = registry.take_pending_cleanups();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].corpus, "c64", "new work precedes the retry");
        drop(next);
        drop(active);
        let retry = registry.take_pending_cleanups();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].corpus, "c0", "failed work stays retryable");
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
            registry.begin_next_catch_up(1),
            Some(RegistrationId {
                source: ConfigSource::Baseline,
                corpus_key: CorpusKey {
                    name: "wiki".into(),
                    canonical_root: PathBuf::from("/repo"),
                },
            })
        );
        assert_eq!(registry.begin_next_catch_up(1), None);
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
                error: "watch failed".into(),
                retrying: true,
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
    fn recovery_warning_for_install_failure_says_retry_will_continue() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        let queried = [corpus("wiki")];

        registry.mark_degraded(&id, "watch failed".into());

        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; watcher backend error: watch failed; retry will continue".into()
            )]
        );
    }

    #[test]
    fn recovery_warning_for_backend_unavailable_says_periodic_reconciliation_only() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");
        let queried = [corpus("wiki")];

        registry.mark_backend_unavailable(&id);

        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; native watcher unavailable; periodic reconciliation only".into()
            )]
        );
    }

    #[test]
    fn changed_re_registration_keeps_backend_unavailable_but_resets_install_failure() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let id = baseline_id("wiki");
        let changed = CorpusConfig {
            globs: vec!["*.md".into()],
            ..corpus("wiki")
        };

        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.mark_backend_unavailable(&id);
        registry.register(ConfigSource::Baseline, changed.clone(), cfg.clone(), vec![]);
        assert_eq!(
            registry.observation(&id),
            Some(Observation::Degraded {
                error: "notify debouncer unavailable".into(),
                retrying: false,
            }),
            "backend unavailability outlives a changed re-registration"
        );

        registry.mark_degraded(&id, "watch failed".into());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        assert_eq!(
            registry.observation(&id),
            Some(Observation::Watched),
            "a per-root install failure resets; the pump re-attempts the install"
        );
    }

    #[test]
    fn catch_up_lifecycle_reports_events_that_arrived_mid_flight() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        assert_eq!(
            registry.begin_next_catch_up(1),
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

        assert_eq!(registry.begin_next_catch_up(1), Some(a.clone()));
        assert_eq!(
            registry.begin_next_catch_up(1),
            None,
            "a second pass must not be admitted while one is active"
        );

        registry.finish_catch_up(&a, Ok(()));
        assert!(
            registry.begin_next_catch_up(1).is_some(),
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
        assert_eq!(registry.begin_next_catch_up(1), Some(a.clone()));
        // A follow-up arrives for `a` mid-flight; `a` cannot re-queue itself
        // (it's InFlight, not Queued) until its pass finishes.
        registry.record_pending(&a, [PathBuf::from("/a/mid.md")]);
        // Finishing `a` with pending work re-queues it at the *back*: [b, a].
        registry.finish_catch_up(&a, Ok(()));

        assert_eq!(
            registry.begin_next_catch_up(1),
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

        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        let pending = registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            pending,
            PendingWork::FullRoot,
            "a failed pass must widen to a full retry, not vanish"
        );
        assert_eq!(
            registry.begin_next_catch_up(1),
            None,
            "a failed pass must not immediately re-admit itself (no busy loop)"
        );

        // The next reconcile tick's admission cycle is what retries it.
        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.begin_next_catch_up(1),
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
    fn repeated_pass_failure_is_recognized_until_a_success_clears_it() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let id = baseline_id("wiki");

        assert!(!registry.repeats_last_error(&id, "walk error"));
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        registry.finish_catch_up(&id, Err("walk error".into()));
        assert!(registry.repeats_last_error(&id, "walk error"));
        assert!(!registry.repeats_last_error(&id, "other error"));

        registry.mark_reconcile_due_all();
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        registry.finish_catch_up(&id, Ok(()));
        assert!(!registry.repeats_last_error(&id, "walk error"));
    }

    #[test]
    fn absent_root_count_resets_on_return_and_retires_at_threshold() {
        let registry = WatchRegistry::new();
        let a = register_baseline(&registry, "a");
        let b = register_baseline(&registry, "b");
        let probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id == a)
            .expect("a probe");
        let only_a = HashMap::from([(
            a.clone(),
            RootObservation {
                probe,
                presence: RootPresence::Absent,
            },
        )]);
        let present_probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id == a)
            .expect("present probe");
        let present_a = HashMap::from([(
            a.clone(),
            RootObservation {
                probe: present_probe,
                presence: RootPresence::Present,
            },
        )]);

        assert!(registry.record_absent_roots(&only_a, 3).is_empty());
        assert!(registry.record_absent_roots(&only_a, 3).is_empty());
        assert!(registry.record_absent_roots(&present_a, 3).is_empty());
        assert!(registry.record_absent_roots(&only_a, 3).is_empty());
        assert!(registry.record_absent_roots(&only_a, 3).is_empty());
        assert!(
            registry.observation(&a).is_some(),
            "a reset count must restart from zero"
        );

        let generation = registry.generation();
        let retired = registry.record_absent_roots(&only_a, 3);
        let names: Vec<&str> = retired.iter().map(|r| r.corpus.as_str()).collect();
        assert_eq!(names, vec!["a"]);
        assert_eq!(retired[0].root, a.corpus_key.canonical_root);
        assert!(registry.observation(&a).is_none());
        assert!(registry.observation(&b).is_some());
        assert_eq!(registry.catch_up_state(&a), None);
        assert!(registry.generation() > generation);
        assert_ne!(registry.begin_next_catch_up(2), Some(a));
    }

    #[test]
    fn pr_786_retirement_followup_stale_probe_does_not_age_replacement() {
        let registry = WatchRegistry::new();
        let id = register_baseline(&registry, "wiki");
        let old_probe = registry.root_probes();
        let replacement = corpus("wiki");
        assert_eq!(
            registry.register(
                ConfigSource::Baseline,
                replacement.clone(),
                Arc::new(Config::default()),
                vec![
                    root("/wiki", replacement.clone()),
                    root("/new", replacement)
                ],
            ),
            RegisterOutcome::New
        );
        let stale = HashMap::from([(
            old_probe[0].id.clone(),
            RootObservation {
                probe: old_probe.into_iter().next().expect("old probe"),
                presence: RootPresence::Absent,
            },
        )]);
        let current_probe = registry
            .root_probes()
            .into_iter()
            .next()
            .expect("current probe");
        let current = HashMap::from([(
            current_probe.id.clone(),
            RootObservation {
                probe: current_probe,
                presence: RootPresence::Absent,
            },
        )]);
        assert!(registry.record_absent_roots(&stale, 3).is_empty());
        assert!(registry.record_absent_roots(&current, 3).is_empty());
        assert!(registry.record_absent_roots(&current, 3).is_empty());
        assert!(
            registry.observation(&id).is_some(),
            "replacement needs its own three ticks"
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

        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        registry.finish_catch_up(&id, Err("boom".into()));
        assert_eq!(
            registry.recovery_warnings_for(&ConfigSource::Baseline, &queried),
            vec![(
                "wiki".into(),
                "root <no root>; recovery state queued; failed reconciliation: boom; reconciliation pending".into()
            )]
        );

        registry.mark_reconcile_due_all();
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
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
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
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
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
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
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
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

        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        registry.finish_catch_up(
            &id,
            Err("registration vanished before catch-up could start".into()),
        );

        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.begin_next_catch_up(1),
            Some(id),
            "the active slot must be released even when the admitted registration vanished"
        );
    }

    #[test]
    fn refresh_roots_rekeys_registration_when_canonical_root_changes() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let mut selected = corpus("wiki");
        let configured = temp.path().join("absent");
        selected.paths = vec![configured.to_string_lossy().into_owned()];
        registry.register(ConfigSource::Baseline, selected, cfg, vec![]);
        let old_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: configured,
            },
        };
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
    fn pr_786_refresh_rekey_reclaims_exact_detached_binding() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let selected = corpus("wiki");
        let old_source = ConfigSource::RepoLayer(PathBuf::from("/old/.hallouminate/config.toml"));
        let saved = vec![
            root("/target", selected.clone()),
            root("/secondary", selected.clone()),
        ];
        assert_eq!(
            registry.register(
                old_source.clone(),
                selected.clone(),
                cfg.clone(),
                saved.clone()
            ),
            RegisterOutcome::New
        );
        let probe = registry
            .root_probes()
            .into_iter()
            .find(|probe| probe.id.source == old_source)
            .expect("old source probe");
        let observations = HashMap::from([(
            probe.id.clone(),
            RootObservation {
                probe,
                presence: RootPresence::CheckoutAbsent,
            },
        )]);
        for _ in 0..3 {
            assert!(registry.record_absent_roots(&observations, 3).is_empty());
        }
        assert_eq!(registry.detached_probes().len(), 1);

        let source = ConfigSource::RepoLayer(PathBuf::from("/new/.hallouminate/config.toml"));
        let partial = vec![
            root("/temporary", selected.clone()),
            root("/secondary", selected.clone()),
        ];
        assert_eq!(
            registry.register(source.clone(), selected.clone(), cfg, partial),
            RegisterOutcome::New
        );
        assert_eq!(registry.detached_probes().len(), 1);
        registry.refresh_roots(|_| saved.clone());
        let roots = registry.snapshot_roots();
        assert_eq!(roots.len(), 2);
        for (id, _) in roots {
            assert_eq!(id.corpus_key.canonical_root, PathBuf::from("/target"));
        }
        assert!(registry.detached_probes().is_empty());
        assert_eq!(registry.retired_runtime.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_roots_rekey_of_in_flight_registration_releases_the_slot() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg, vec![]);
        let old_id = baseline_id("wiki");
        assert_eq!(registry.begin_next_catch_up(1), Some(old_id.clone()));

        registry.refresh_roots(|c| vec![root("/newly/resolved", c.clone())]);

        let new_id = RegistrationId {
            source: ConfigSource::Baseline,
            corpus_key: CorpusKey {
                name: "wiki".into(),
                canonical_root: PathBuf::from("/newly/resolved"),
            },
        };
        assert_eq!(
            registry.begin_next_catch_up(1),
            None,
            "the re-keyed registration must wait for the old in-flight pass"
        );
        assert_eq!(registry.finish_catch_up(&old_id, Ok(())), PendingWork::Idle);
        assert_eq!(registry.begin_next_catch_up(1), Some(new_id));
    }

    #[test]
    fn pr_786_refresh_roots_does_not_apply_stale_same_id_snapshot() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let c = temp.path().join("c");
        for path in [&a, &b, &c] {
            std::fs::create_dir(path).expect("create root");
        }
        let old = corpus_at("wiki", &[a.to_str().expect("a"), b.to_str().expect("b")]);
        let replacement = corpus_at("wiki", &[a.to_str().expect("a"), c.to_str().expect("c")]);
        assert_eq!(
            registry.register(
                ConfigSource::Baseline,
                old.clone(),
                cfg.clone(),
                super::super::watch_roots_for(&old),
            ),
            RegisterOutcome::New
        );
        registry.refresh_roots(|snapshot| {
            assert_eq!(snapshot.paths, old.paths);
            assert_eq!(
                registry.register(
                    ConfigSource::Baseline,
                    replacement.clone(),
                    cfg.clone(),
                    super::super::watch_roots_for(&replacement),
                ),
                RegisterOutcome::New
            );
            super::super::watch_roots_for(snapshot)
        });
        let actual: Vec<PathBuf> = registry
            .snapshot_roots()
            .into_iter()
            .map(|(_, root)| root.canonical_watched)
            .collect();
        assert_eq!(
            actual,
            vec![a, c],
            "replacement roots must survive stale refresh"
        );
        assert!(!actual.contains(&b), "removed root must not remain routed");
    }

    #[cfg(unix)]
    #[test]
    fn pr_786_refresh_roots_does_not_rekey_stale_replacement() {
        use std::os::unix::fs::symlink;

        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let b = temp.path().join("b");
        let c = temp.path().join("c");
        for path in [&first, &second, &b, &c] {
            std::fs::create_dir(path).expect("create root");
        }
        let alias = temp.path().join("a");
        symlink(&first, &alias).expect("first alias");
        let old = corpus_at(
            "wiki",
            &[alias.to_str().expect("alias"), b.to_str().expect("b")],
        );
        let replacement = corpus_at(
            "wiki",
            &[alias.to_str().expect("alias"), c.to_str().expect("c")],
        );
        assert_eq!(
            registry.register(
                ConfigSource::Baseline,
                old.clone(),
                cfg.clone(),
                super::super::watch_roots_for(&old),
            ),
            RegisterOutcome::New
        );
        registry.refresh_roots(|snapshot| {
            assert_eq!(snapshot.paths, old.paths);
            assert_eq!(
                registry.register(
                    ConfigSource::Baseline,
                    replacement.clone(),
                    cfg.clone(),
                    super::super::watch_roots_for(&replacement),
                ),
                RegisterOutcome::New
            );
            std::fs::remove_file(&alias).expect("remove first alias");
            symlink(&second, &alias).expect("retarget alias");
            super::super::watch_roots_for(snapshot)
        });
        let roots = registry.snapshot_roots();
        assert_eq!(roots.len(), 2, "replacement keeps both roots");
        assert_eq!(roots[0].0.corpus_key.canonical_root, first);
        assert_eq!(roots[1].0.corpus_key.canonical_root, first);
        let actual: Vec<PathBuf> = roots
            .into_iter()
            .map(|(_, root)| root.canonical_watched)
            .collect();
        assert_eq!(
            actual,
            vec![first, c],
            "stale rekey must not move replacement"
        );
        assert!(!actual.contains(&b), "removed root must not remain routed");
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
    fn config_replacement_defers_excess_retirements_without_losing_them() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        let corpora: Vec<_> = (0..=MAX_PENDING_CLEANUPS)
            .map(|index| corpus(&format!("c{index}")))
            .collect();
        registry
            .replace_source(source.clone(), corpora, cfg.clone(), |_| vec![])
            .expect("register");

        let retired = registry
            .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
            .expect("first replacement");
        assert_eq!(retired.len(), MAX_PENDING_CLEANUPS);
        assert_eq!(registry.repo_layer_sources(), vec![PathBuf::from("/repo")]);
        let blocked = registry
            .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
            .expect("blocked replacement");
        assert!(
            blocked.is_empty(),
            "held cleanup permits must defer retirement"
        );
        drop(retired);
        let last = registry.take_pending_cleanups();
        assert_eq!(
            last.len(),
            1,
            "the omitted registration must retire on retry"
        );
        assert!(registry.repo_layer_sources().is_empty());
        assert!(registry.take_pending_cleanups().is_empty());
    }

    #[test]
    fn rediscovery_keeps_an_obsolete_deferred_cleanup() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        let selected = corpus("wiki");
        let old_root = root("/old", selected.clone());
        registry.register(
            source.clone(),
            selected.clone(),
            cfg.clone(),
            vec![old_root],
        );
        let held: Vec<_> = (0..MAX_PENDING_CLEANUPS)
            .map(|_| {
                registry
                    .cleanup_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("cleanup slot")
            })
            .collect();
        assert!(
            registry
                .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
                .expect("defer old root")
                .is_empty()
        );
        assert!(
            registry.snapshot_roots().is_empty(),
            "a deferred removal must stop watching"
        );
        assert_eq!(
            registry.begin_next_catch_up(1),
            None,
            "a deferred removal must not reindex"
        );

        let outcome = registry.register(
            source.clone(),
            selected.clone(),
            cfg,
            vec![root("/new", selected)],
        );
        assert_eq!(outcome, RegisterOutcome::New);
        drop(held);
        let retired = registry.take_pending_cleanups();
        assert_eq!(
            retired.len(),
            1,
            "rediscovery must not erase the old cleanup"
        );
        assert_eq!(retired[0].roots[0], PathBuf::from("/old"));
        assert_eq!(
            registry.snapshot_roots().len(),
            1,
            "the new root must stay watched"
        );
    }

    #[test]
    fn same_id_rediscovery_revives_a_deferred_registration() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        registry.register(source.clone(), corpus("wiki"), cfg.clone(), vec![]);
        let held: Vec<_> = (0..MAX_PENDING_CLEANUPS)
            .map(|_| {
                registry
                    .cleanup_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("cleanup slot")
            })
            .collect();
        assert!(
            registry
                .replace_source(source.clone(), vec![], cfg.clone(), |_| vec![])
                .expect("defer root")
                .is_empty()
        );
        assert_eq!(registry.begin_next_catch_up(1), None);
        let outcome = registry.register(source, corpus("wiki"), cfg, vec![]);
        assert_eq!(
            outcome,
            RegisterOutcome::New,
            "rediscovery must queue a fresh pass"
        );
        assert_eq!(
            registry.begin_next_catch_up(1),
            Some(RegistrationId {
                source: ConfigSource::RepoLayer(PathBuf::from("/repo")),
                corpus_key: CorpusKey {
                    name: "wiki".into(),
                    canonical_root: PathBuf::new()
                }
            })
        );
        drop(held);
        assert!(
            registry.take_pending_cleanups().is_empty(),
            "revived root must not retire"
        );
    }

    #[test]
    fn missing_root_retirement_defers_excess_without_dropping_registrations() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for index in 0..=MAX_PENDING_CLEANUPS {
            let name = format!("c{index}");
            registry.register(ConfigSource::Baseline, corpus(&name), cfg.clone(), vec![]);
        }
        let mut absent = HashMap::new();
        for probe in registry.root_probes() {
            absent.insert(
                probe.id.clone(),
                RootObservation {
                    probe,
                    presence: RootPresence::Absent,
                },
            );
        }
        let retired = registry.record_absent_roots(&absent, 1);
        assert_eq!(retired.len(), MAX_PENDING_CLEANUPS);
        assert!(registry.record_absent_roots(&absent, 1).is_empty());
        drop(retired);
        let last = registry.take_pending_cleanups();
        assert_eq!(
            last.len(),
            1,
            "the deferred registration must retire on a later tick"
        );
        assert!(registry.root_probes().is_empty());
    }

    #[test]
    fn deferred_missing_root_revives_before_cleanup_admission() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for index in 0..=MAX_PENDING_CLEANUPS {
            let name = format!("c{index}");
            registry.register(ConfigSource::Baseline, corpus(&name), cfg.clone(), vec![]);
        }
        let mut absent = HashMap::new();
        for probe in registry.root_probes() {
            absent.insert(
                probe.id.clone(),
                RootObservation {
                    probe,
                    presence: RootPresence::Absent,
                },
            );
        }
        let retired = registry.record_absent_roots(&absent, 1);
        let probes = registry.root_probes();
        assert_eq!(probes.len(), 1, "only one missing root should wait");
        let deferred = probes[0].id.clone();
        let present = HashMap::from([(
            deferred.clone(),
            RootObservation {
                probe: probes.into_iter().next().expect("deferred probe"),
                presence: RootPresence::Present,
            },
        )]);
        assert!(registry.record_absent_roots(&present, 1).is_empty());
        drop(retired);
        assert!(registry.take_pending_cleanups().is_empty());
        assert!(
            registry
                .root_probes()
                .iter()
                .any(|probe| probe.id == deferred)
        );
    }

    #[test]
    fn deferred_missing_root_revives_when_checkout_moves_but_saved_root_lives() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        for index in 0..=MAX_PENDING_CLEANUPS {
            let name = format!("c{index}");
            registry.register(
                ConfigSource::RepoLayer(PathBuf::from(format!(
                    "/repo-{index}/.hallouminate/config.toml"
                ))),
                corpus(&name),
                cfg.clone(),
                vec![],
            );
        }
        let mut absent = HashMap::new();
        for probe in registry.root_probes() {
            absent.insert(
                probe.id.clone(),
                RootObservation {
                    probe,
                    presence: RootPresence::Absent,
                },
            );
        }
        let retired = registry.record_absent_roots(&absent, 1);
        let probes = registry.root_probes();
        assert_eq!(probes.len(), 1);
        let probe = probes.into_iter().next().expect("deferred probe");
        let id = probe.id.clone();
        let checkout = HashMap::from([(
            id.clone(),
            RootObservation {
                probe,
                presence: RootPresence::CheckoutAbsent,
            },
        )]);
        assert!(registry.record_absent_roots(&checkout, 3).is_empty());
        drop(retired);
        assert!(registry.take_pending_cleanups().is_empty());
        assert_eq!(registry.catch_up_state(&id), Some(CatchUpState::Queued));
        assert_eq!(registry.root_probes().len(), 1);
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

    #[test]
    fn degraded_count_reflects_registrations_marked_degraded() {
        // WHY: `daemon status` derives degraded_watch_roots from this count
        // — it must move from 0 to 1 the moment a registration is marked
        // degraded, and back down once recovered, or status silently hides
        // periodic-reconciliation-only mode.
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.register(ConfigSource::Baseline, corpus("other"), cfg, vec![]);
        assert_eq!(registry.degraded_count(), 0);

        let id = baseline_id("wiki");
        registry.mark_degraded(&id, "watch failed".into());
        assert_eq!(registry.degraded_count(), 1);

        registry.mark_watched(&id);
        assert_eq!(registry.degraded_count(), 0);
    }

    fn wait_targets() -> HashSet<&'static str> {
        HashSet::from(["wiki"])
    }

    #[test]
    fn first_pass_counts_as_pending_until_it_finishes_ok_or_err() {
        for outcome in [Ok(()), Err("scan failed".to_string())] {
            let registry = WatchRegistry::new();
            registry.register(
                ConfigSource::Baseline,
                corpus("wiki"),
                Arc::new(Config::default()),
                vec![],
            );
            let id = baseline_id("wiki");
            assert!(registry.first_reconcile_pending(&ConfigSource::Baseline, &wait_targets()));
            assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
            assert!(registry.first_reconcile_pending(&ConfigSource::Baseline, &wait_targets()));
            registry.finish_catch_up(&id, outcome);
            assert!(!registry.first_reconcile_pending(&ConfigSource::Baseline, &wait_targets()));
        }
    }

    /// WHY: a routine pass on a root that already reconciled once must not
    /// make `ground` wait; only the first reconciliation is a cold start.
    #[tokio::test]
    async fn routine_pass_on_warm_root_does_not_block_waiters() {
        let registry = WatchRegistry::new();
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![],
        );
        let id = baseline_id("wiki");
        assert_eq!(registry.begin_next_catch_up(1), Some(id.clone()));
        registry.finish_catch_up(&id, Ok(()));
        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.catch_up_state(&id),
            Some(CatchUpState::Queued),
            "the routine pass is queued"
        );

        let started = std::time::Instant::now();
        let settled = registry
            .wait_first_reconcile(
                &ConfigSource::Baseline,
                &[corpus("wiki")],
                std::time::Duration::from_secs(30),
            )
            .await;
        assert!(settled);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// WHY: a group member shares the leader's pass; without its own
    /// `reconciled_once`, later routine passes would park every waiter.
    #[tokio::test]
    async fn group_member_counts_as_reconciled_after_leader_finishes_ok() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let member_source = ConfigSource::RepoLayer(PathBuf::from("/repo-member"));
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.register(member_source.clone(), corpus("wiki"), cfg, vec![]);
        let leader = baseline_id("wiki");
        let member = RegistrationId {
            source: member_source.clone(),
            corpus_key: leader.corpus_key.clone(),
        };
        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));
        assert_eq!(
            registry.catch_up_state(&member),
            Some(CatchUpState::InFlight),
            "the member shares the leader's pass"
        );
        registry.finish_catch_up(&leader, Ok(()));
        registry.mark_reconcile_due_all();
        assert_eq!(
            registry.catch_up_state(&member),
            Some(CatchUpState::Queued),
            "the routine pass is queued"
        );

        let started = std::time::Instant::now();
        let settled = registry
            .wait_first_reconcile(
                &member_source,
                &[corpus("wiki")],
                std::time::Duration::from_secs(30),
            )
            .await;
        assert!(settled);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// WHY: a member whose shared pass failed must not look settled and
    /// clean; it keeps the failure and retries like the leader.
    #[test]
    fn failed_group_leader_marks_members_failed_and_requeues() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let member_source = ConfigSource::RepoLayer(PathBuf::from("/repo-member"));
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.register(member_source.clone(), corpus("wiki"), cfg, vec![]);
        let leader = baseline_id("wiki");
        let member = RegistrationId {
            source: member_source,
            corpus_key: leader.corpus_key.clone(),
        };
        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));

        registry.finish_catch_up(&leader, Err("scan failed".into()));

        assert_eq!(registry.catch_up_state(&member), Some(CatchUpState::Queued));
        assert_eq!(registry.pending(&member), Some(PendingWork::FullRoot));
        let guard = registry.lock();
        let reg = guard.regs.get(&member).expect("member registered");
        assert_eq!(reg.last_error.as_deref(), Some("scan failed"));
        assert!(reg.retry_on_tick);
        assert!(guard.queue.contains(&member), "member is requeued");
    }

    #[test]
    fn successful_group_pass_clears_member_last_error() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let member_source = ConfigSource::RepoLayer(PathBuf::from("/repo-member"));
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.register(member_source.clone(), corpus("wiki"), cfg, vec![]);
        let leader = baseline_id("wiki");
        let member = RegistrationId {
            source: member_source,
            corpus_key: leader.corpus_key.clone(),
        };
        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));
        registry.finish_catch_up(&leader, Err("scan failed".into()));
        registry.mark_reconcile_due_all();
        let next = registry
            .begin_next_catch_up(1)
            .expect("the tick readmits the failed group");

        registry.finish_catch_up(&next, Ok(()));

        let guard = registry.lock();
        for id in [&leader, &member] {
            let reg = guard.regs.get(id).expect("registered");
            assert_eq!(
                reg.last_error, None,
                "a successful shared pass clears {id:?}"
            );
        }
    }

    /// WHY: seeded baseline registrations start `Done`, so only the boot
    /// flag tells a baseline ground that the sweep still runs.
    #[test]
    fn baseline_ground_warns_during_boot_catch_up() {
        let registry = Arc::new(WatchRegistry::new());
        registry.seed_baseline(corpus("wiki"), Arc::new(Config::default()), vec![]);
        let repo_source = ConfigSource::RepoLayer(PathBuf::from("/repo/.hallouminate"));
        registry.register(
            repo_source.clone(),
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![],
        );
        let queried = [corpus("wiki")];
        assert!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &queried)
                .is_empty()
        );

        registry.mark_boot_catch_up_running();
        let guard = registry.boot_catch_up_guard();

        assert_eq!(registry.boot_catch_up(), BootCatchUp::Running);
        let warnings = registry.recovery_warnings_for(&ConfigSource::Baseline, &queried);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].1.contains("boot reconciliation in progress"),
            "{warnings:?}"
        );
        let repo_warnings = registry.recovery_warnings_for(&repo_source, &queried);
        assert!(
            repo_warnings
                .iter()
                .all(|(_, message)| !message.contains("boot reconciliation")),
            "{repo_warnings:?}"
        );

        drop(guard);
    }

    #[test]
    fn baseline_warning_clears_when_boot_catch_up_guard_drops() {
        let registry = Arc::new(WatchRegistry::new());
        registry.seed_baseline(corpus("wiki"), Arc::new(Config::default()), vec![]);
        registry.mark_boot_catch_up_running();
        drop(registry.boot_catch_up_guard());

        assert_eq!(registry.boot_catch_up(), BootCatchUp::Idle);
        assert!(
            registry
                .recovery_warnings_for(&ConfigSource::Baseline, &[corpus("wiki")])
                .is_empty()
        );
    }

    /// WHY: a cold wait on a group member ends when the shared pass fails,
    /// and the member's source reports the failed reconciliation.
    #[tokio::test]
    async fn cold_wait_returns_with_warning_after_shared_pass_failure() {
        let registry = WatchRegistry::new();
        let cfg = Arc::new(Config::default());
        let member_source = ConfigSource::RepoLayer(PathBuf::from("/repo-member"));
        registry.register(ConfigSource::Baseline, corpus("wiki"), cfg.clone(), vec![]);
        registry.register(member_source.clone(), corpus("wiki"), cfg, vec![]);
        let leader = baseline_id("wiki");
        assert_eq!(registry.begin_next_catch_up(1), Some(leader.clone()));
        registry.finish_catch_up(&leader, Err("scan failed".into()));

        let started = std::time::Instant::now();
        let settled = registry
            .wait_first_reconcile(
                &member_source,
                &[corpus("wiki")],
                std::time::Duration::from_secs(30),
            )
            .await;

        assert!(settled);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let warnings = registry.recovery_warnings_for(&member_source, &[corpus("wiki")]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].1.contains("failed reconciliation: scan failed"),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn waiter_for_cold_root_times_out_when_pump_is_live() {
        let registry = WatchRegistry::new();
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![],
        );
        let settled = registry
            .wait_first_reconcile(
                &ConfigSource::Baseline,
                &[corpus("wiki")],
                std::time::Duration::from_millis(100),
            )
            .await;
        assert!(!settled, "a never-reconciled root keeps the waiter parked");
    }

    /// WHY: with no pump nothing admits catch-ups, so waiting the full bound
    /// only delays the answer.
    #[tokio::test]
    async fn waiter_returns_at_once_when_pump_is_unavailable() {
        let registry = WatchRegistry::new();
        registry.register(
            ConfigSource::Baseline,
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![],
        );
        registry.mark_pump_unavailable();
        let started = std::time::Instant::now();
        let settled = registry
            .wait_first_reconcile(
                &ConfigSource::Baseline,
                &[corpus("wiki")],
                std::time::Duration::from_secs(30),
            )
            .await;
        assert!(!settled);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn replacing_a_source_wakes_waiters_on_removed_registrations() {
        let registry = Arc::new(WatchRegistry::new());
        let source = ConfigSource::RepoLayer(PathBuf::from("/repo"));
        registry.register(
            source.clone(),
            corpus("wiki"),
            Arc::new(Config::default()),
            vec![],
        );
        let waiter = {
            let registry = registry.clone();
            let source = source.clone();
            tokio::spawn(async move {
                registry
                    .wait_first_reconcile(
                        &source,
                        &[corpus("wiki")],
                        std::time::Duration::from_secs(30),
                    )
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        registry
            .replace_source(source, vec![], Arc::new(Config::default()), |_| vec![])
            .unwrap();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiter wakes without waiting the full bound")
            .expect("waiter task");
        assert!(settled);
    }
}
