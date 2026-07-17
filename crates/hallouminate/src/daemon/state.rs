//! Shared daemon state: baseline configuration, per-request LanceDB resources,
//! mutation locks, the global write lane, and lifecycle accounting.
//!
//! Lock acquisition rule (enforced by every mutating dispatcher):
//!
//!   corpus lock → write_lane permit
//!
//! Never the other way around. The per-corpus mutex serializes everything
//! that touches one corpus's markdown + LanceDB rows so concurrent writes
//! to the same corpus see a coherent ordering. The global write-lane
//! semaphore (one permit) further serializes the actual on-disk mutation +
//! LanceDB commit so we never hit LanceDB's retry-limit warning around
//! many simultaneous writers.
//!
//! Stores, tokenizers, and embedders are cached by effective request config.
//! A baseline embedder that fails during startup remains retryable: the next
//! normal request for that resource key initializes and installs it in place.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::pressure::IoPressureProbe;
#[cfg(not(target_os = "linux"))]
use super::pressure::NoPressureSignal;
#[cfg(target_os = "linux")]
use super::pressure::PsiProbe;

use crate::config::Config;
use hallouminate_adapters::{EmbedBatch, Embedder, FastembedCrossencoder, LanceStore};
use hallouminate_domain::common::{HallouminateError, expand_tilde};
use hallouminate_domain::corpus::{Tokenizer, load_tokenizer, missing_roots};
use hallouminate_domain::indexer::{HandlerRegistry, SearchHit, index_corpus};
use hallouminate_domain::search::{Crossencoder, canonical_crossencoder_model};

use super::ladder::LadderAction;
use super::maintenance::{DeferReason, maintenance_loop};

const CHUNK_BUDGET_TOKENS: usize = 384;

/// Backup ground-store directories (`<ground>.bak-v{N}`, left behind by
/// `move_stale_store` on a schema-version rebuild) older than this are
/// pruned at daemon boot; they're recoverable-until-pruned, not permanent.
pub(crate) const STALE_BACKUP_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Monotonic seconds elapsed since process start (`Instant`-based), not
/// wall-clock Unix time — a clock step (NTP correction, manual clock change)
/// can't make idle accounting exit early or postpone exit until the clock
/// catches up.
fn monotonic_secs() -> u64 {
    PROCESS_START.get_or_init(Instant::now).elapsed().as_secs()
}

async fn init_embedder(
    model: &str,
    quantized: bool,
    cache_dir: PathBuf,
) -> anyhow::Result<Box<dyn EmbedBatch>> {
    let model = model.to_owned();
    tokio::task::spawn_blocking(move || Embedder::try_new(&model, quantized, &cache_dir))
        .await
        .map_err(|e| anyhow::anyhow!("embedder initialization task failed: {e}"))?
        .map(|embedder| Box::new(embedder) as Box<dyn EmbedBatch>)
        .map_err(|e| anyhow::anyhow!("init embedder: {e}"))
}

fn is_idle(last_use_secs: u64, now_secs: u64, idle_secs: u64) -> bool {
    now_secs.saturating_sub(last_use_secs) >= idle_secs
}

/// Map of key → per-key async mutex, created on first use. Two callers
/// holding the same key serialize on its mutex; distinct keys never collide.
/// Backs both the per-corpus write lock (keyed by corpus name) and the
/// per-`ResourceKey` build lock in `resources_for`. For corpus writes, every
/// mutating handler also takes the single-permit global `write_lane` (see
/// `DaemonStateInner.write_lane`), so cross-corpus writes still serialize at
/// the lane while reads through different corpora run freely.
struct KeyedLockMap<K> {
    inner: Mutex<HashMap<K, Arc<Mutex<()>>>>,
}

impl<K> Default for KeyedLockMap<K> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash> KeyedLockMap<K> {
    async fn lock<Q>(&self, key: &Q) -> OwnedMutexGuard<()>
    where
        Q: ToOwned<Owned = K> + ?Sized,
    {
        let mutex = {
            let mut map = self.inner.lock().await;
            map.entry(key.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }
}

/// Key identifying one distinct resource set: a `[storage].ground_dir` +
/// `[embeddings]` combination. Repo-layer config resolved per request
/// selects (or lazily builds) the `RequestResources` entry for its key, so
/// overriding any of these fields takes effect on the very next request
/// with no daemon restart.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ResourceKey {
    ground_dir: PathBuf,
    model: String,
    quantized: bool,
    enabled: bool,
}

impl ResourceKey {
    fn from_config(cfg: &Config) -> Self {
        Self {
            ground_dir: expand_tilde(&cfg.storage.ground_dir),
            model: cfg.embeddings.model.clone(),
            quantized: cfg.embeddings.quantized,
            enabled: cfg.embeddings.enabled,
        }
    }
}

/// Resources effective for one repo-layer config. Keyed cache entry —
/// mirrors the `crossencoders: Arc<Mutex<HashMap<..>>>` cache precedent.
pub struct RequestResources {
    pub store: Arc<LanceStore>,
    pub tokenizer: Tokenizer,
    pub embeddings_enabled: bool,
    pub ground_dir: PathBuf,
}

/// Owned daemon state. Cheap to clone (`Arc` inside); one instance lives for
/// the lifetime of the daemon process.
#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<DaemonStateInner>,
}

struct DaemonStateInner {
    /// Boot-time baseline, not the per-request effective config.
    ///
    /// Built once from XDG + `--config PATH` at daemon startup and frozen
    /// for the process lifetime. Per-request handling layers repo-discovery
    /// (`.hallouminate/config.toml` walk from the request's `cwd`) on top of
    /// this via `Config::resolve_for_cwd` in the dispatcher — the baseline
    /// never changes once the daemon is running.
    baseline: Config,
    /// Source path of the baseline (the XDG config path or the `--config
    /// PATH` override). Threaded into `resolve_for_cwd` so scalar-conflict
    /// diagnostics name the actual file that owns the baseline value, per
    /// AC #7 of `.cheese/specs/repo-config-discovery.md`. `None` when the
    /// daemon was booted without a known source (e.g. tests that construct
    /// a `Config` programmatically).
    baseline_xdg_path: Option<PathBuf>,
    /// Resources (store/tokenizer/embedder) for the boot baseline's
    /// `ResourceKey`. `watch.rs` and boot-time sweeps key off this instead
    /// of the per-request map since they have no per-request effective
    /// config to resolve.
    baseline_resources: Arc<RequestResources>,
    /// Per-request resource cache, keyed by `(ground_dir, model, quantized,
    /// enabled)`. A repo-layer config that overrides any of these fields
    /// gets its own entry lazily built by `resources_for` on first use, so
    /// the override takes effect on the very next request with no daemon
    /// restart — while requests sharing a key share one `LanceStore`/
    /// embedder/tokenizer set, never opening two store handles on the same
    /// directory.
    resources: Mutex<HashMap<ResourceKey, Arc<RequestResources>>>,
    /// Per-`ResourceKey` build lock. `resources_for` holds the matching key
    /// lock while opening a store so two requests never open the same ground
    /// dir concurrently (the single-open-per-ground-dir invariant), without
    /// holding the `resources` map lock across that async open.
    resource_build_locks: KeyedLockMap<ResourceKey>,
    corpus_locks: KeyedLockMap<String>,
    write_lane: Arc<Semaphore>,
    /// Lazy-loaded crossencoder rerankers, keyed by canonical model name.
    /// A per-model cache (rather than a single slot) so that repos
    /// selecting different `[search].crossencoder` models via repo-layer
    /// config each get their own loaded model instead of clobbering a
    /// shared one. Empty until the first `ground` request that resolves a
    /// configured model; the baseline model (if any) is pre-warmed at boot.
    crossencoders: Arc<Mutex<HashMap<String, FastembedCrossencoder>>>,
    /// Monotonic (`Instant`-based) seconds-since-process-start timestamp of
    /// completion (handle_connection) plus embedder/crossencoder acquire and
    /// guard drop. Idle-exit (server.rs) fires when this is quiet for
    /// `[daemon].idle_exit_secs` and no connection is active (ADR-003).
    last_activity_secs: Arc<AtomicU64>,
    /// Count of connection handlers in flight. Idle-exit defers while non-zero
    /// so the daemon never exits mid-request (ADR-003).
    active_connections: Arc<AtomicUsize>,
    /// Per-class activity clock (WorkClass::External), stored alongside
    /// `last_activity_secs` (the aggregate). Maintenance-defer eligibility
    /// reads this clock only (ADR daemon-rework-002) so internal housekeeping
    /// cannot starve maintenance; idle-exit keeps reading the aggregate.
    external_last_activity_secs: Arc<AtomicU64>,
    /// Per-class activity clock (WorkClass::Internal). Stamped by the same
    /// `touch_activity` path but read by no predicate today -- kept for
    /// curd 9's status snapshot.
    internal_last_activity_secs: Arc<AtomicU64>,
    /// Per-class in-flight count (WorkClass::External), stored alongside
    /// `active_connections` (the aggregate). Read by maintenance-defer
    /// eligibility (ADR daemon-rework-002); idle-exit reads the aggregate.
    active_external_connections: Arc<AtomicUsize>,
    /// Per-class in-flight count (WorkClass::Internal). Read by no predicate
    /// today -- kept for curd 9's status snapshot.
    active_internal_connections: Arc<AtomicUsize>,
    /// Consecutive-defer streak the maintenance loop is on right now
    /// (relocated from a loop-local in maintenance.rs so curd 9's status
    /// surface and the ladder (curd 1) can read it). Reset once per outer
    /// pass via `reset_defer_count`; the observable WARN-at-11 behaviour is
    /// unchanged, only the storage moved.
    defer_count: AtomicU32,
    /// Watcher activity counters (events admitted, reindex passes
    /// completed, and reindexes that upserted nothing) -- additive storage
    /// for curd 9's status surface.
    watcher_counters: WatcherCounters,
    /// Most recent ladder trip, if any (curd 1 writes this once the ladder
    /// is wired into a loop; curd 9's status surface reads it).
    last_ladder_trip: std::sync::Mutex<Option<LadderTrip>>,
    /// Supervisor owning the daemon's five long-lived loops (G5). Holds the
    /// escalation hook below; loops route through `supervisor().spawn(..)`
    /// at their existing spawn sites.
    supervisor: Arc<super::supervisor::Supervisor>,
    /// Per-task heartbeat epochs bumped by the supervised loops and polled
    /// by the watchdog (server.rs); snapshot surface for `daemon status`.
    heartbeat: Arc<super::heartbeat::HeartbeatRegistry>,
    /// Retained so shutdown drains maintenance before releasing the daemon flock.
    maintenance_task: Mutex<Option<JoinHandle<()>>>,
    /// Shutdown signal shared by the accept loop, the IPC `Shutdown`
    /// dispatcher, and the SIGINT/SIGTERM handlers. Cancelling it breaks the
    /// `serve_on_listener` select and triggers flock-drop + socket cleanup.
    shutdown: CancellationToken,
}

/// Both guards a mutating handler takes in the documented `corpus → write_lane`
/// order. Dropping it releases the write-lane permit first (LIFO drop order),
/// then the corpus lock; that matches the acquisition order's inverse and
/// keeps the per-corpus serial chain visible to the next waiter.
pub struct MutationGuard {
    // Drop order: `_permit` first, then `_corpus`. The fields are private to
    // make the order an invariant rather than a convention.
    _permit: OwnedSemaphorePermit,
    _corpus: OwnedMutexGuard<()>,
}

impl MutationGuard {
    /// Constructs a guard from its two held locks. `pub(super)` so only
    /// `backpressure::acquire` (the sole place that assembles a
    /// `MutationGuard`) can build one -- the private fields above stay an
    /// invariant, not a convention, for every other caller in the crate.
    pub(super) fn new(permit: OwnedSemaphorePermit, corpus: OwnedMutexGuard<()>) -> Self {
        Self {
            _permit: permit,
            _corpus: corpus,
        }
    }
}

/// Which subsystem is driving a connection/activity stamp (ADR daemon-
/// rework-002): client RPC handlers are `External`; watcher batches,
/// boot catch-up indexing, and the maintenance tick are `Internal`.
/// Maintenance-defer eligibility gates on `External` signals only --
/// housekeeping never defers housekeeping -- while idle-exit gates on both
/// classes via the aggregate counter/clock (ADR daemon-idle-exit-003).
/// Every `enter_connection` caller must name its class explicitly; the
/// `every_enter_connection_call_site_declares_a_work_class` source-pin
/// keeps a defaulted or wrapper call site from sneaking in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkClass {
    External,
    Internal,
}

/// Decrements the daemon's active-connection count when dropped. Held by a
/// connection handler task for its whole lifetime so idle-exit sees a non-zero
/// count for the duration of every in-flight request (ADR-003).
pub struct ConnectionGuard {
    active: Arc<AtomicUsize>,
    class_active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.class_active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Watcher activity counters (ADR daemon-rework seed 4): raw notify events
/// admitted per debounced batch, completed reindex passes, and reindexes
/// that upserted no rows (`ApplyStats::files_upserted == 0`).
#[derive(Debug, Default)]
struct WatcherCounters {
    events: AtomicU64,
    reindexes: AtomicU64,
    noop_reindexes: AtomicU64,
}

/// Snapshot of the most recent ladder trip: which escalation action fired
/// and when (monotonic seconds). Curd 1's future ladder wiring is the only
/// writer; this seed only provides storage + accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LadderTrip {
    pub(crate) action: LadderAction,
    pub(crate) at_secs: u64,
}

impl DaemonState {
    pub async fn open(cfg: Config, xdg_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let ground_dir = expand_tilde(&cfg.storage.ground_dir);
        if let Some(parent) = ground_dir.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("create ground dir parent: {e}"))?;
        }
        let stale_version = match LanceStore::validate_existing_metadata(
            &ground_dir,
            &cfg.embeddings.model,
            cfg.embeddings.quantized,
            cfg.embeddings.enabled,
        ) {
            Ok(()) => None,
            Err(HallouminateError::StoreSchemaStale {
                found, expected, ..
            }) => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    %found,
                    %expected,
                    "ground store schema v{found} < expected v{expected}; rebuilding from source",
                );
                move_stale_store(&ground_dir, found).await?;
                Some(found)
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "validate ground dir {}: {e}",
                    ground_dir.display()
                ));
            }
        };

        // Model construction and ONNX session setup are synchronous and
        // CPU-heavy. Keep them off Tokio's async worker capacity.
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let embedder: Option<Box<dyn EmbedBatch>> = if cfg.embeddings.enabled {
            match init_embedder(
                &cfg.embeddings.model,
                cfg.embeddings.quantized,
                cache_dir.clone(),
            )
            .await
            {
                Ok(embedder) => Some(embedder),
                Err(e) => {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        model = %cfg.embeddings.model,
                        error = %e,
                        "embedder unavailable at startup; the next request will retry initialization",
                    );
                    None
                }
            }
        } else {
            None
        };
        let tokenizer = load_tokenizer(&cfg.embeddings.model)
            .map_err(|e| anyhow::anyhow!("load tokenizer for {}: {e}", cfg.embeddings.model))?;

        // Metadata validation happened before model ownership moved into the
        // store, so stale-schema recovery reuses this single ONNX session.
        let build_result: anyhow::Result<LanceStore> = async {
            let store = LanceStore::open_or_create(
                &ground_dir,
                &cfg.embeddings.model,
                cfg.embeddings.quantized,
                cfg.embeddings.enabled,
                embedder,
            )
            .await
            .map_err(|e| anyhow::anyhow!("open ground dir {}: {e}", ground_dir.display()))?;

            if stale_version.is_some() {
                let registry = HandlerRegistry::new(tokenizer.clone(), CHUNK_BUDGET_TOKENS);
                for corpus in cfg
                    .effective_corpora()
                    .map_err(|e| anyhow::anyhow!("rebuild: list corpora: {e}"))?
                {
                    let missing = missing_roots(&corpus);
                    if !missing.is_empty() {
                        tracing::warn!(
                            target: "hallouminate::daemon",
                            corpus = %corpus.name,
                            "rebuild: corpus root missing; skipped",
                        );
                        continue;
                    }
                    let stats = index_corpus(&corpus, &store, &registry)
                        .await
                        .map_err(|e| anyhow::anyhow!("rebuild: index {}: {e}", corpus.name))?;
                    tracing::info!(
                        target: "hallouminate::daemon",
                        corpus = %corpus.name,
                        files = stats.files_upserted,
                        chunks = stats.chunks_inserted,
                        "rebuild: reindexed",
                    );
                }
            }
            Ok(store)
        }
        .await;
        let store = match build_result {
            Ok(store) => store,
            Err(e) => {
                if let Some(found) = stale_version
                    && ground_dir.exists()
                {
                    let _ = tokio::fs::remove_dir_all(&ground_dir).await;
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        "rebuild failed; removed partial ground dir so next boot retries. \
                         Backup preserved at {}.bak-v{found}",
                        ground_dir.display(),
                    );
                }
                return Err(e);
            }
        };
        // Recoverable-until-pruned backups from a prior schema rebuild
        // (see `move_stale_store` above) accumulate on disk forever
        // otherwise. Tolerate failure — a stuck backup dir must never
        // block startup; the next boot's prune retries.
        if let Err(e) =
            prune_stale_backups(&ground_dir, SystemTime::now(), STALE_BACKUP_MAX_AGE).await
        {
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "failed to prune stale ground store backups",
            );
        }
        // Pre-warm the baseline crossencoder iff configured; tolerate
        // failure so a misconfigured model name (or offline first run)
        // doesn't brick the daemon. The cache stays empty for that model
        // and a later `crossencoder()` call retries the load. Per-request
        // repo-layer models are loaded lazily on first use, keyed by name.
        let mut crossencoders: HashMap<String, FastembedCrossencoder> = HashMap::new();
        if let Some(model) = cfg.search.crossencoder.as_deref() {
            match canonical_crossencoder_model(model)
                .map_err(anyhow::Error::from)
                .and_then(|canonical| {
                    FastembedCrossencoder::try_new(canonical, &cache_dir)
                        .map(|c| (canonical, c))
                        .map_err(anyhow::Error::from)
                }) {
                Ok((canonical, c)) => {
                    crossencoders.insert(canonical.to_string(), c);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        model = %model,
                        error = %e,
                        "crossencoder unavailable at startup; ground will skip rerank until reload",
                    );
                }
            }
        }
        let shutdown = CancellationToken::new();
        let crossencoders_arc = Arc::new(Mutex::new(crossencoders));
        let last_activity = Arc::new(AtomicU64::new(monotonic_secs()));
        let store = Arc::new(store);
        let write_lane = Arc::new(Semaphore::new(1));

        // #161's idle eviction is deleted (ADR-001): dropping the ONNX session
        // released nothing (the CPU BFCArena retains its extents), so each
        // evict->reload cycle stacked a fresh arena. Idle-exit (server.rs)
        // reclaims memory by exiting the whole process instead. The config
        // field still parses; warn when it was set to a non-default value so
        // operators migrate to `[daemon].idle_exit_secs`.
        if cfg.embeddings.idle_evict_secs
            != crate::config::EmbeddingsConfig::default().idle_evict_secs
        {
            tracing::warn!(
                target: "hallouminate::daemon",
                idle_evict_secs = cfg.embeddings.idle_evict_secs,
                "embeddings.idle_evict_secs is deprecated and does nothing; \
                 set [daemon].idle_exit_secs to control idle-exit instead",
            );
        }

        let baseline_key = ResourceKey {
            ground_dir: ground_dir.clone(),
            model: cfg.embeddings.model.clone(),
            quantized: cfg.embeddings.quantized,
            enabled: cfg.embeddings.enabled,
        };
        let baseline_resources = Arc::new(RequestResources {
            store,
            tokenizer,
            embeddings_enabled: cfg.embeddings.enabled,
            ground_dir,
        });
        let mut resources_map = HashMap::new();
        resources_map.insert(baseline_key, Arc::clone(&baseline_resources));

        let maintenance_interval_secs = cfg.daemon.maintenance_interval_secs;
        let restart_cap = cfg.daemon.restart_intensity_cap;
        let restart_window = Duration::from_secs(cfg.daemon.restart_intensity_window_secs);
        let heartbeat = Arc::new(super::heartbeat::HeartbeatRegistry::default());
        // Invented defaults, no existing analog in debt.rs; revisit if noisy.
        let ladder = super::ladder::Ladder {
            warn_at: 3,
            act_at: 5,
            action: LadderAction::WatchdogTrip,
        };
        let state = DaemonState {
            // `new_cyclic`: the escalation hook records trips into the very
            // `DaemonStateInner` being constructed (so `daemon status`
            // reports them); a `Weak` breaks the ownership cycle.
            inner: Arc::new_cyclic(|weak: &std::sync::Weak<DaemonStateInner>| {
                let weak = weak.clone();
                let escalate: super::supervisor::EscalationHook =
                    Arc::new(move |task, action| {
                        tracing::error!(
                            target: "hallouminate::daemon",
                            task = ?task,
                            action = ?action,
                            "supervised task exceeded the restart intensity cap",
                        );
                        if let Some(inner) = weak.upgrade() {
                            *inner
                                .last_ladder_trip
                                .lock()
                                .expect("ladder trip mutex poisoned") = Some(LadderTrip {
                                action,
                                at_secs: monotonic_secs(),
                            });
                        }
                    });
                DaemonStateInner {
                    baseline: cfg,
                    baseline_xdg_path: xdg_path,
                    baseline_resources,
                    resources: Mutex::new(resources_map),
                    resource_build_locks: KeyedLockMap::default(),
                    corpus_locks: KeyedLockMap::default(),
                    write_lane,
                    crossencoders: crossencoders_arc,
                    last_activity_secs: last_activity,
                    active_connections: Arc::new(AtomicUsize::new(0)),
                    external_last_activity_secs: Arc::new(AtomicU64::new(monotonic_secs())),
                    internal_last_activity_secs: Arc::new(AtomicU64::new(monotonic_secs())),
                    active_external_connections: Arc::new(AtomicUsize::new(0)),
                    active_internal_connections: Arc::new(AtomicUsize::new(0)),
                    defer_count: AtomicU32::new(0),
                    watcher_counters: WatcherCounters::default(),
                    last_ladder_trip: std::sync::Mutex::new(None),
                    supervisor: Arc::new(super::supervisor::Supervisor::new(
                        restart_cap,
                        restart_window,
                        ladder,
                        escalate,
                        shutdown.clone(),
                    )),
                    heartbeat,
                    maintenance_task: Mutex::new(None),
                    shutdown,
                }
            }),
        };

        // Low-frequency LanceDB maintenance tick (compaction + version
        // prune, see `LanceStore::maintain`). Runs under the write-lane
        // permit alone -- maintenance spans the whole table, not one
        // corpus, so there is no corpus lock to acquire first; taking only
        // the write lane still preserves the documented `corpus ->
        // write_lane` order (a lock that is never acquired can't be
        // acquired out of order). Deferred while active or under I/O
        // pressure (ADR-003); `maintenance_interval_secs == 0` disables it.
        if maintenance_interval_secs == 0 {
            tracing::info!(
                target: "hallouminate::daemon",
                "automatic maintenance disabled (daemon.maintenance_interval_secs = 0)",
            );
        } else {
            let loop_state = state.clone();
            let cancel = loop_state.shutdown_token().clone();
            let interval = Duration::from_secs(maintenance_interval_secs);
            #[cfg(target_os = "linux")]
            let probe: Arc<dyn IoPressureProbe> = Arc::new(PsiProbe);
            #[cfg(not(target_os = "linux"))]
            let probe: Arc<dyn IoPressureProbe> = Arc::new(NoPressureSignal);
            let maintenance_task = state.inner.supervisor.spawn(
                super::heartbeat::TaskName::Maintenance,
                move || {
                    maintenance_loop(loop_state.clone(), cancel.clone(), interval, probe.clone())
                },
            );
            *state.inner.maintenance_task.lock().await = Some(maintenance_task);
        }

        Ok(state)
    }

    /// The daemon-wide shutdown token. The accept loop selects on
    /// [`CancellationToken::cancelled`]; the IPC `Shutdown` dispatcher and the
    /// signal handlers call [`CancellationToken::cancel`].
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.inner.shutdown
    }

    pub(crate) async fn take_maintenance_task(&self) -> Option<JoinHandle<()>> {
        self.inner.maintenance_task.lock().await.take()
    }

    /// Supervisor owning the daemon's long-lived loops (G5 wiring).
    pub(crate) fn supervisor(&self) -> &Arc<super::supervisor::Supervisor> {
        &self.inner.supervisor
    }

    /// Heartbeat registry the supervised loops bump and the watchdog polls.
    pub(crate) fn heartbeat(&self) -> &Arc<super::heartbeat::HeartbeatRegistry> {
        &self.inner.heartbeat
    }

    /// Source path of the baseline config the daemon booted from — the XDG
    /// path when no `--config PATH` was given, or the `--config PATH` value
    /// itself. `None` when the baseline was constructed without a known
    /// source path (tests that build a `Config` programmatically). Threaded
    /// into `resolve_for_cwd` by the dispatcher so scalar-conflict messages
    /// can name the actual file.
    pub fn baseline_xdg_path(&self) -> Option<&Path> {
        self.inner.baseline_xdg_path.as_deref()
    }

    /// Boot-time baseline config (XDG layers + optional `--config PATH`).
    ///
    /// Frozen at `DaemonState::open` time. Per-request handling layers
    /// repo-discovery on top in the dispatcher via
    /// `Config::resolve_for_cwd`; callers that need the *effective* config
    /// for a request should use the resolved value, not this baseline.
    pub fn baseline(&self) -> &Config {
        &self.inner.baseline
    }

    pub fn store(&self) -> Arc<LanceStore> {
        self.inner.baseline_resources.store.clone()
    }

    pub fn ground_dir(&self) -> &std::path::Path {
        &self.inner.baseline_resources.ground_dir
    }

    /// Whether dense embeddings are enabled. Dispatchers branch on this to
    /// pass `Some(embedder)` (hybrid) or `None` (lexical-only) into `ground`
    /// and `index_corpus`. False means the embedder is permanently `None`.
    pub fn embeddings_enabled(&self) -> bool {
        self.inner.baseline_resources.embeddings_enabled
    }

    /// Per-request resource seam (B2+B3): resolve (or lazily build) the
    /// `RequestResources` for the effective config's `(ground_dir, model,
    /// quantized, enabled)` key. A repo-layer override of any of those
    /// fields (`[storage].ground_dir`, `[embeddings].model`,
    /// `[embeddings].enabled`) takes effect on the very next request — no
    /// daemon restart — while requests sharing a key share one
    /// `LanceStore`/embedder/tokenizer set so two `Arc<LanceStore>` handles
    /// never open on the same ground dir. Deliberately no stale-schema-
    /// rebuild handling here (that is boot-only, see `move_stale_store`); a
    /// per-request `ground_dir` hitting `HallouminateError::StoreSchemaStale`
    /// just surfaces as an `Err`, no worse than today's "can't point at a
    /// different ground_dir at all".
    pub async fn resources_for(&self, cfg: &Config) -> anyhow::Result<Arc<RequestResources>> {
        self.resources_for_with_initializer(cfg, |model, quantized, cache_dir| async move {
            init_embedder(&model, quantized, cache_dir).await
        })
        .await
    }

    async fn resources_for_with_initializer<F, Fut>(
        &self,
        cfg: &Config,
        initialize: F,
    ) -> anyhow::Result<Arc<RequestResources>>
    where
        F: FnOnce(String, bool, PathBuf) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Box<dyn EmbedBatch>>>,
    {
        let key = ResourceKey::from_config(cfg);
        if let Some(existing) = self.inner.resources.lock().await.get(&key)
            && (!cfg.embeddings.enabled || existing.store.embedder_available())
        {
            return Ok(Arc::clone(existing));
        }

        let _build = self.inner.resource_build_locks.lock(&key).await;
        if let Some(existing) = self.inner.resources.lock().await.get(&key).cloned() {
            if cfg.embeddings.enabled && !existing.store.embedder_available() {
                let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
                let retry = match initialize(
                    cfg.embeddings.model.clone(),
                    cfg.embeddings.quantized,
                    cache_dir,
                )
                .await
                {
                    Ok(embedder) => existing
                        .store
                        .install_embedder(embedder)
                        .map_err(anyhow::Error::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = retry {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        model = %cfg.embeddings.model,
                        error = %error,
                        "embedder retry failed; serving cached resources without embeddings",
                    );
                }
            }
            return Ok(existing);
        }

        let ground_dir = key.ground_dir.clone();
        if let Some(parent) = ground_dir.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("create ground dir parent: {e}"))?;
        }
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let embedder = if cfg.embeddings.enabled {
            Some(
                initialize(
                    cfg.embeddings.model.clone(),
                    cfg.embeddings.quantized,
                    cache_dir,
                )
                .await?,
            )
        } else {
            None
        };
        let tokenizer = load_tokenizer(&cfg.embeddings.model)
            .map_err(|e| anyhow::anyhow!("load tokenizer for {}: {e}", cfg.embeddings.model))?;
        let store = LanceStore::open_or_create(
            &ground_dir,
            &cfg.embeddings.model,
            cfg.embeddings.quantized,
            cfg.embeddings.enabled,
            embedder,
        )
        .await
        .map_err(|e| anyhow::anyhow!("open ground dir {}: {e}", ground_dir.display()))?;
        let resources = Arc::new(RequestResources {
            store: Arc::new(store),
            tokenizer,
            embeddings_enabled: cfg.embeddings.enabled,
            ground_dir: ground_dir.clone(),
        });
        self.inner
            .resources
            .lock()
            .await
            .insert(key, Arc::clone(&resources));
        Ok(resources)
    }

    /// Borrow the crossencoder for the model named by the per-request
    /// resolved config, loading it lazily on first use and caching it by
    /// canonical model name. Pass `None` (no model configured for this
    /// request) to skip reranking — returns `Ok(None)`. Returns `Err`
    /// when a configured model name is unknown or fails to load; the
    /// caller logs and falls back to fusion-only ranking. Resolving from
    /// the per-request `cfg.search.crossencoder` (not the baseline) is
    /// what lets repo-layer `[search].crossencoder` overrides take effect.
    pub async fn crossencoder(
        &self,
        model_name: Option<&str>,
    ) -> anyhow::Result<Option<CrossencoderGuard>> {
        let Some(model_name) = model_name else {
            return Ok(None);
        };
        // Canonicalize so config aliases (e.g. the corrected English
        // spelling of a typo'd upstream id) share one cache entry.
        let canonical = canonical_crossencoder_model(model_name)?;
        // Owned lock (not a borrowed `MutexGuard<'_, ...>`): #139's per-request
        // rerank timeout boxes this guard as `dyn Crossencoder` and moves it
        // into `spawn_blocking`, which requires 'static ownership.
        let mut guard = Arc::clone(&self.inner.crossencoders).lock_owned().await;
        if !guard.contains_key(canonical) {
            let cache_dir = expand_tilde(&self.inner.baseline.embeddings.cache_dir);
            let model = FastembedCrossencoder::try_new(canonical, &cache_dir)
                .map_err(|e| anyhow::anyhow!("init crossencoder ({canonical}): {e}"))?;
            guard.insert(canonical.to_string(), model);
        }
        self.inner
            .last_activity_secs
            .store(monotonic_secs(), Ordering::Relaxed);
        Ok(Some(CrossencoderGuard {
            guard,
            key: canonical.to_string(),
            last_use_secs: Arc::clone(&self.inner.last_activity_secs),
        }))
    }

    /// Bump the activity clock to now — called at request completion so
    /// idle-exit keys on real request throughput, not just embed use (ADR-003).
    pub fn touch_activity(&self, class: WorkClass) {
        let now = monotonic_secs();
        self.inner.last_activity_secs.store(now, Ordering::Relaxed);
        match class {
            WorkClass::External => self
                .inner
                .external_last_activity_secs
                .store(now, Ordering::Relaxed),
            WorkClass::Internal => self
                .inner
                .internal_last_activity_secs
                .store(now, Ordering::Relaxed),
        }
    }

    /// Register an active connection; the returned guard decrements the count
    /// on drop. Held for a connection handler's lifetime so idle-exit never
    /// fires mid-request (ADR-003).
    pub fn enter_connection(&self, class: WorkClass) -> ConnectionGuard {
        self.inner.active_connections.fetch_add(1, Ordering::SeqCst);
        let class_counter = match class {
            WorkClass::External => &self.inner.active_external_connections,
            WorkClass::Internal => &self.inner.active_internal_connections,
        };
        class_counter.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard {
            active: Arc::clone(&self.inner.active_connections),
            class_active: Arc::clone(class_counter),
        }
    }

    /// Current consecutive-defer streak (curd 9's status surface).
    #[allow(dead_code)]
    pub(crate) fn defer_count(&self) -> u32 {
        self.inner.defer_count.load(Ordering::Relaxed)
    }

    /// Reset the consecutive-defer streak to zero. Called once per outer
    /// maintenance-loop pass, before the inner defer-recheck loop starts.
    pub(super) fn reset_defer_count(&self) {
        self.inner.defer_count.store(0, Ordering::Relaxed);
    }

    /// Increment the consecutive-defer streak and return the new value.
    pub(super) fn increment_defer_count(&self) -> u32 {
        self.inner.defer_count.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record `count` raw notify events admitted from one debounced batch.
    pub(crate) fn record_watcher_events(&self, count: u64) {
        self.inner
            .watcher_counters
            .events
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Record one completed reindex pass; `noop` marks a pass that upserted
    /// no rows (`ApplyStats::files_upserted == 0`).
    pub(crate) fn record_watcher_reindex(&self, noop: bool) {
        self.inner
            .watcher_counters
            .reindexes
            .fetch_add(1, Ordering::Relaxed);
        if noop {
            self.inner
                .watcher_counters
                .noop_reindexes
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of `(events, reindexes, noop_reindexes)` (curd 9's status
    /// surface).
    #[allow(dead_code)]
    pub(crate) fn watcher_counters_snapshot(&self) -> (u64, u64, u64) {
        (
            self.inner.watcher_counters.events.load(Ordering::Relaxed),
            self.inner
                .watcher_counters
                .reindexes
                .load(Ordering::Relaxed),
            self.inner
                .watcher_counters
                .noop_reindexes
                .load(Ordering::Relaxed),
        )
    }

    /// Record a ladder trip (curd 1's future ladder wiring calls this when
    /// `Ladder::evaluate` returns `Action`). Overwrites any prior snapshot.
    #[allow(dead_code)]
    pub(crate) fn record_ladder_trip(&self, action: LadderAction) {
        let at_secs = monotonic_secs();
        *self
            .inner
            .last_ladder_trip
            .lock()
            .expect("ladder trip mutex poisoned") = Some(LadderTrip { action, at_secs });
    }

    /// The most recent ladder trip, if any (curd 9's status surface).
    #[allow(dead_code)]
    pub(crate) fn last_ladder_trip(&self) -> Option<LadderTrip> {
        *self
            .inner
            .last_ladder_trip
            .lock()
            .expect("ladder trip mutex poisoned")
    }

    /// Whether a due maintenance pass should defer against the real clock --
    /// see `maintenance_defer_reason_at`.
    pub(super) fn maintenance_defer_reason(
        &self,
        probe: &dyn IoPressureProbe,
    ) -> Option<DeferReason> {
        self.maintenance_defer_reason_at(probe, monotonic_secs())
    }

    /// Injectable-clock variant so tests drive a synthetic `now_secs`. Active
    /// (connection in flight, or activity within 60s) takes priority over
    /// I/O pressure so tests can distinguish the two defer reasons.
    ///
    /// Gates on `WorkClass::External` signals only (ADR daemon-rework-002):
    /// internal housekeeping (watcher batches, boot catch-up, the maintenance
    /// tick itself) never defers maintenance, so a busy watcher can no longer
    /// starve compaction the way the shared aggregate clock did.
    fn maintenance_defer_reason_at(
        &self,
        probe: &dyn IoPressureProbe,
        now_secs: u64,
    ) -> Option<DeferReason> {
        if self
            .inner
            .active_external_connections
            .load(Ordering::SeqCst)
            != 0
        {
            return Some(DeferReason::Active);
        }
        if !is_idle(
            self.inner
                .external_last_activity_secs
                .load(Ordering::Relaxed),
            now_secs,
            60,
        ) {
            return Some(DeferReason::Active);
        }
        if probe.elevated() {
            return Some(DeferReason::IoPressure);
        }
        None
    }

    /// Idle-exit predicate against the real clock: enabled, zero active
    /// connections, activity clock quiet for at least `idle_secs`.
    /// `idle_secs == 0` disables idle-exit.
    ///
    /// Reads the aggregate counter/clock, which every `WorkClass` stamps, so
    /// idle-exit gates on BOTH classes (ADR daemon-rework-002 preserving ADR
    /// daemon-idle-exit-003): housekeeping in flight defers teardown exactly
    /// like an external request -- never drop the flock mid-write.
    pub(crate) fn should_idle_exit(&self, idle_secs: u64) -> bool {
        self.should_idle_exit_at(idle_secs, monotonic_secs())
    }

    /// Injectable-clock variant so tests drive a synthetic `now_secs`.
    fn should_idle_exit_at(&self, idle_secs: u64, now_secs: u64) -> bool {
        if idle_secs == 0 {
            return false;
        }
        if self.inner.active_connections.load(Ordering::SeqCst) != 0 {
            return false;
        }
        is_idle(
            self.inner.last_activity_secs.load(Ordering::Relaxed),
            now_secs,
            idle_secs,
        )
    }

    /// Seconds remaining until the idle-exit deadline (last activity +
    /// `idle_exit_secs`): the full window right after activity, saturating to
    /// zero once the window has elapsed. The idle-exit watcher sleeps this
    /// long instead of polling on a fixed period, so exit lands within one
    /// short interval of the true deadline rather than overshooting by up to a
    /// whole period.
    pub(crate) fn secs_until_idle(&self, idle_exit_secs: u64) -> u64 {
        self.secs_until_idle_at(idle_exit_secs, monotonic_secs())
    }

    /// Injectable-clock variant so tests drive a synthetic `now_secs`.
    fn secs_until_idle_at(&self, idle_exit_secs: u64, now_secs: u64) -> u64 {
        let elapsed =
            now_secs.saturating_sub(self.inner.last_activity_secs.load(Ordering::Relaxed));
        idle_exit_secs.saturating_sub(elapsed)
    }

    /// Monotonic seconds-since-process-start of the most recent activity. Test accessor.
    #[cfg(test)]
    pub(crate) fn last_activity_secs(&self) -> u64 {
        self.inner.last_activity_secs.load(Ordering::Relaxed)
    }

    /// Force every activity clock (aggregate plus both per-class clocks) to
    /// an arbitrary value. Test-only: lets a cross-module test (e.g.
    /// watch.rs's batch-processing regression test) simulate a long-idle
    /// daemon without a real sleep.
    #[cfg(test)]
    pub(crate) fn set_last_activity_secs_for_test(&self, secs: u64) {
        self.inner.last_activity_secs.store(secs, Ordering::Relaxed);
        self.inner
            .external_last_activity_secs
            .store(secs, Ordering::Relaxed);
        self.inner
            .internal_last_activity_secs
            .store(secs, Ordering::Relaxed);
    }

    /// A freshly-constructed format-handler [`HandlerRegistry`] over the
    /// daemon's loaded tokenizer. Construction is cheap (the tokenizer is
    /// `Clone` and each handler is a thin wrapper), so handlers build one per
    /// call instead of reaching into shared state for it.
    pub fn make_registry(&self) -> HandlerRegistry {
        HandlerRegistry::new(
            self.inner.baseline_resources.tokenizer.clone(),
            CHUNK_BUDGET_TOKENS,
        )
    }

    /// Acquire the per-corpus async mutex. Call before any operation that
    /// reads-modifies-writes that corpus's filesystem or LanceDB rows.
    pub async fn lock_corpus(&self, corpus: &str) -> OwnedMutexGuard<()> {
        self.inner.corpus_locks.lock(corpus).await
    }

    /// Acquire the global write-lane permit. ALWAYS call after
    /// `lock_corpus` for the same operation to maintain the documented
    /// `corpus → write_lane` order and prevent deadlock.
    pub fn write_lane(&self) -> Arc<Semaphore> {
        self.inner.write_lane.clone()
    }

    /// Acquire the per-corpus mutex AND the global write-lane permit in the
    /// documented order. The returned `MutationGuard` releases both in the
    /// inverse order on drop. Replaces the open-coded
    /// `lock_corpus().await; write_lane().acquire_owned().await?` pattern
    /// every mutating handler used to repeat — fewer lines, no chance of
    /// flipping the order by accident.
    pub async fn acquire_mutation_guard(
        &self,
        corpus: &str,
    ) -> Result<MutationGuard, &'static str> {
        super::backpressure::acquire(self, corpus).await
    }
}

/// Move the stale ground store aside atomically so a fresh store can be
/// created in its place. The backup is named `<ground>.bak-v{found_version}`.
/// A pre-existing backup from a prior failed rebuild is overwritten.
async fn move_stale_store(ground_dir: &Path, found_version: u32) -> anyhow::Result<()> {
    let bak = ground_dir.with_file_name(format!(
        "{}.bak-v{found_version}",
        ground_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("ground"),
    ));
    if bak.exists() {
        tokio::fs::remove_dir_all(&bak).await?;
    }
    tokio::fs::rename(ground_dir, &bak).await?;
    // `rename(2)` preserves the source dir's mtime, so without this the
    // freshly-moved backup can already look >30d old and get pruned on the
    // same boot that created it. Stamp it to "now" so age is measured from
    // when it was set aside, not from the original store's last write.
    let stamp_target = bak.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&stamp_target)?.set_modified(SystemTime::now())
    })
    .await??;
    tracing::info!(
        target: "hallouminate::daemon",
        backup = %bak.display(),
        "moved stale ground store aside; recoverable until pruned",
    );
    Ok(())
}

/// Prune backup ground-store directories (`<ground>.bak-v{N}`) older than
/// `max_age`, as measured from `now`. `now` is threaded in (rather than
/// read internally) so tests can make ages deterministic without a
/// filetime crate. Called once at daemon boot; failures are tolerated by
/// the caller (a stuck backup dir must never block startup).
async fn prune_stale_backups(
    ground_dir: &Path,
    now: SystemTime,
    max_age: Duration,
) -> anyhow::Result<()> {
    let parent = ground_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = format!(
        "{}.bak-v",
        ground_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("ground"),
    );
    let mut entries = match tokio::fs::read_dir(parent).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let metadata = match entry.metadata().await {
            Ok(metadata) => metadata,
            Err(e) => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    entry = %name,
                    error = %e,
                    "skipping stale-backup entry: failed to read metadata",
                );
                continue;
            }
        };
        if !metadata.is_dir() {
            continue;
        }
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(e) => {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    entry = %name,
                    error = %e,
                    "skipping stale-backup entry: failed to read modified time",
                );
                continue;
            }
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < max_age {
            continue;
        }
        let path = entry.path();
        if let Err(e) = tokio::fs::remove_dir_all(&path).await {
            tracing::warn!(
                target: "hallouminate::daemon",
                entry = %name,
                error = %e,
                "failed to remove stale ground store backup",
            );
            continue;
        }
        tracing::info!(
            target: "hallouminate::daemon",
            backup = %path.display(),
            age_days = age.as_secs() / 86_400,
            "pruned stale ground store backup",
        );
    }
    Ok(())
}

/// Owned guard around the lazily-loaded crossencoder. Derefs to
/// `FastembedCrossencoder` so callers can
/// pass `&mut *guard` directly into anything that wants
/// `&mut dyn Crossencoder`. Holds an `OwnedMutexGuard` (not a borrowed
/// `MutexGuard<'a, ...>`) so it can be boxed as `Box<dyn Crossencoder>` and
/// moved into `spawn_blocking` for the #139 per-request rerank timeout.
pub struct CrossencoderGuard {
    guard: OwnedMutexGuard<HashMap<String, FastembedCrossencoder>>,
    /// Canonical model name; the key into `guard` that `crossencoder()`
    /// inserted before handing the guard out.
    key: String,
    last_use_secs: Arc<AtomicU64>,
}

impl std::ops::Deref for CrossencoderGuard {
    type Target = FastembedCrossencoder;
    fn deref(&self) -> &FastembedCrossencoder {
        // `crossencoder()` inserts `key` before constructing the guard,
        // and the guard holds the lock, so the entry can't vanish.
        self.guard.get(&self.key).expect("crossencoder loaded")
    }
}

impl std::ops::DerefMut for CrossencoderGuard {
    fn deref_mut(&mut self) -> &mut FastembedCrossencoder {
        self.guard.get_mut(&self.key).expect("crossencoder loaded")
    }
}

impl Drop for CrossencoderGuard {
    fn drop(&mut self) {
        self.last_use_secs
            .store(monotonic_secs(), Ordering::Relaxed);
    }
}

/// Lets a `CrossencoderGuard` be boxed as `Box<dyn Crossencoder>` and moved
/// into `spawn_blocking` for the #139 per-request rerank timeout, instead of
/// call sites unwrapping it to a borrowed `&mut dyn Crossencoder`.
impl Crossencoder for CrossencoderGuard {
    fn rerank(
        &mut self,
        query: &str,
        hits: &mut [SearchHit],
    ) -> hallouminate_domain::common::Result<()> {
        (**self).rerank(query, hits)
    }
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState")
            .field("ground_dir", &self.inner.baseline_resources.ground_dir)
            .field("model", &self.inner.baseline.embeddings.model)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::super::maintenance::{MaintenanceTick, jittered_sleep_secs};
    use super::*;
    use hallouminate_adapters::{EMBEDDING_DIM, EmbedRole, MaintenanceStats};

    use std::fmt;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        strings: HashMap<String, String>,
        numbers: HashMap<String, u64>,
    }

    #[derive(Clone, Default)]
    struct EventCapture(Arc<std::sync::Mutex<Vec<CapturedEvent>>>);

    impl EventCapture {
        fn maintenance_events(&self) -> Vec<CapturedEvent> {
            let events = self.0.lock().expect("capture lock");
            let mut maintenance = Vec::new();
            for event in events.iter() {
                if event.strings.contains_key("maintenance_event") {
                    maintenance.push(event.clone());
                }
            }
            maintenance
        }
    }

    impl<S> Layer<S> for EventCapture
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut captured = CapturedEvent::default();
            event.record(&mut captured);
            self.0.lock().expect("capture lock").push(captured);
        }
    }

    impl Visit for CapturedEvent {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.numbers.insert(field.name().to_owned(), value);
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            let value = u64::try_from(value).expect("maintenance numeric fields are non-negative");
            self.numbers.insert(field.name().to_owned(), value);
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.strings
                .insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.strings
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    fn event_stages(events: &[CapturedEvent]) -> Vec<&str> {
        let mut stages = Vec::new();
        for event in events {
            stages.push(
                event
                    .strings
                    .get("maintenance_event")
                    .expect("maintenance event stage")
                    .as_str(),
            );
        }
        stages
    }

    fn assert_correlated(events: &[CapturedEvent]) {
        let id = events[0]
            .numbers
            .get("maintenance_id")
            .expect("first maintenance id");
        for event in events {
            assert_eq!(
                event.numbers.get("maintenance_id"),
                Some(id),
                "every lifecycle event must carry the same maintenance id",
            );
        }
    }

    fn assert_terminal_durations(event: &CapturedEvent) {
        for field in ["queue_wait_ms", "maintenance_ms", "total_ms"] {
            assert!(
                event.numbers.contains_key(field),
                "terminal event must carry numeric {field}: {event:?}",
            );
        }
    }

    /// Covers AC #9 (daemon half): the baseline accessor returns the config
    /// that was passed into `open`, unchanged. The dispatcher layers
    /// repo-discovery on top per-request via `resolve_for_cwd`; the
    /// baseline itself is frozen at boot.
    #[tokio::test]
    async fn baseline_returns_the_configured_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let expected_model = cfg.embeddings.model.clone();

        let state = DaemonState::open(cfg, None)
            .await
            .expect("open daemon state");

        assert_eq!(state.baseline().embeddings.model, expected_model);
    }

    #[tokio::test]
    async fn should_idle_exit_is_false_when_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        assert!(
            !state.should_idle_exit_at(0, u64::MAX),
            "idle_secs=0 disables idle-exit; must never fire",
        );
    }

    #[tokio::test]
    async fn should_idle_exit_is_false_when_recently_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_activity_secs();
        assert!(
            !state.should_idle_exit_at(300, last + 1),
            "1 s elapsed < 300 s idle; must not exit",
        );
    }

    #[tokio::test]
    async fn should_idle_exit_is_true_when_idle_and_no_connections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_activity_secs();
        // Inclusive boundary and just past it both fire.
        assert!(
            state.should_idle_exit_at(300, last + 300),
            "elapsed == idle_secs (>= threshold); must exit",
        );
        assert!(
            state.should_idle_exit_at(300, last + 301),
            "elapsed > idle_secs; must exit",
        );
    }

    #[tokio::test]
    async fn should_idle_exit_is_false_one_second_below_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_activity_secs();
        assert!(
            !state.should_idle_exit_at(300, last + 299),
            "elapsed = idle_secs - 1 (< threshold); must not exit",
        );
    }

    #[tokio::test]
    async fn secs_until_idle_counts_down_to_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_activity_secs();
        // Full window remains the instant activity lands.
        assert_eq!(
            state.secs_until_idle_at(300, last),
            300,
            "no time elapsed since activity; the full window remains",
        );
        // Partway through the window, only the remainder is left.
        assert_eq!(
            state.secs_until_idle_at(300, last + 100),
            200,
            "100 s elapsed of a 300 s window; 200 s remain",
        );
        // Saturates to zero once the window has fully elapsed, and stays there
        // well past it (no underflow).
        assert_eq!(
            state.secs_until_idle_at(300, last + 300),
            0,
            "window exactly elapsed; deadline reached",
        );
        assert_eq!(
            state.secs_until_idle_at(300, last + 10_000),
            0,
            "well past the window; saturates to zero, never underflows",
        );
    }

    #[tokio::test]
    async fn active_connection_defers_idle_exit_even_when_clock_is_idle() {
        // ADR-003: idle-exit must never fire while a connection is in flight,
        // no matter how quiet the activity clock is.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_activity_secs();

        let guard = state.enter_connection(WorkClass::External);
        assert!(
            !state.should_idle_exit_at(300, last + 10_000),
            "an active connection must defer idle-exit even when long idle",
        );
        drop(guard);
        assert!(
            state.should_idle_exit_at(300, last + 10_000),
            "once the connection count returns to zero, idle-exit fires",
        );
    }

    #[tokio::test]
    async fn internal_connection_defers_idle_exit_even_when_clock_is_idle() {
        // ADR-003 via ADR daemon-rework-002: idle-exit gates on BOTH work
        // classes. A live Internal guard (e.g. a LanceDB write inside a
        // watcher batch) must block teardown just like an External request.
        let state = test_state().await;
        let last = state.last_activity_secs();
        let guard = state.enter_connection(WorkClass::Internal);
        assert!(
            !state.should_idle_exit_at(300, last + 10_000),
            "an active Internal connection must defer idle-exit even when long idle",
        );
        drop(guard);
        assert!(
            state.should_idle_exit_at(300, last + 10_000),
            "once the Internal guard drops, idle-exit fires",
        );
    }

    #[tokio::test]
    async fn internal_touch_activity_defers_idle_exit() {
        // Idle-exit gates on both classes: Internal activity (the watcher's
        // clock stamp after a batch) must reset the idle window even though it
        // no longer defers maintenance.
        let state = test_state().await;
        state.set_last_activity_secs_for_test(1);
        assert!(
            state.should_idle_exit_at(300, 1000),
            "clock stale at 1 s; now=1000 is well past idle",
        );
        state.touch_activity(WorkClass::Internal);
        assert!(
            !state.should_idle_exit_at(300, state.last_activity_secs() + 1),
            "Internal activity must stamp the idle clock (both classes gate idle-exit)",
        );
    }

    #[tokio::test]
    async fn maintenance_tick_emits_correlated_structured_lifecycle() {
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        state.set_last_activity_secs_for_test(u64::MAX / 2);
        let idle_clock_before_tick = state.last_activity_secs();

        let tick = state.run_maintenance_tick().await;

        assert_eq!(tick, MaintenanceTick::Continue);
        assert_eq!(
            state.last_activity_secs(),
            idle_clock_before_tick,
            "maintenance must not stamp the idle-activity clock (ADR-002)",
        );
        let events = capture.maintenance_events();
        assert_eq!(
            event_stages(&events),
            [
                "started",
                "write_lane_acquired",
                "compaction_started",
                "compaction_finished",
                "prune_started",
                "prune_finished",
                "finished",
            ],
        );
        assert_correlated(&events);
        let terminal = events.last().expect("terminal maintenance event");
        assert_eq!(
            terminal.strings.get("outcome").map(String::as_str),
            Some("success"),
        );
        assert_terminal_durations(terminal);
        for field in [
            "fragments_removed",
            "fragments_added",
            "old_versions_pruned",
        ] {
            assert!(
                terminal.numbers.contains_key(field),
                "success event must retain numeric {field}: {terminal:?}",
            );
        }
        for field in ["fragments_removed", "fragments_added"] {
            assert!(
                events[3].numbers.contains_key(field),
                "compaction event must retain numeric {field}: {:?}",
                events[3],
            );
        }
        assert!(
            events[5].numbers.contains_key("old_versions_pruned"),
            "prune event must retain numeric old_versions_pruned: {:?}",
            events[5],
        );
        assert!(
            !terminal.numbers.contains_key("bytes_read")
                && !terminal.numbers.contains_key("bytes_written"),
            "unmeasured byte counters must not be emitted",
        );
    }

    #[tokio::test]
    async fn maintenance_tick_emits_failure_outcome() {
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let state = test_state().await;

        let tick = state
            .run_maintenance_tick_with(|_| async {
                Err(HallouminateError::Config(
                    "forced maintenance failure".to_owned(),
                ))
            })
            .await;

        assert_eq!(tick, MaintenanceTick::Continue);
        let events = capture.maintenance_events();
        assert_eq!(
            event_stages(&events),
            ["started", "write_lane_acquired", "finished"],
        );
        assert_correlated(&events);
        let terminal = events.last().expect("terminal maintenance event");
        assert_eq!(
            terminal.strings.get("outcome").map(String::as_str),
            Some("failure"),
        );
        assert_terminal_durations(terminal);
        assert!(terminal.strings.contains_key("error"));
    }

    #[tokio::test]
    async fn maintenance_tick_emits_cancellation_outcome_when_aborted_while_queued() {
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let state = test_state().await;
        let lane = state.write_lane();
        let _permit = lane.acquire().await.expect("write lane");
        let tick_state = state.clone();
        let task = tokio::spawn(async move { tick_state.run_maintenance_tick().await });

        for _ in 0..100 {
            let events = capture.maintenance_events();
            if event_stages(&events) == ["started"] {
                break;
            }
            tokio::task::yield_now().await;
        }
        task.abort();
        let join = task.await;
        assert!(join.expect_err("aborted task").is_cancelled());

        let events = capture.maintenance_events();
        assert_eq!(event_stages(&events), ["started", "finished"]);
        assert_correlated(&events);
        let terminal = events.last().expect("terminal maintenance event");
        assert_eq!(
            terminal.strings.get("outcome").map(String::as_str),
            Some("cancelled"),
        );
        assert_terminal_durations(terminal);
    }

    #[tokio::test]
    async fn maintenance_tick_emits_shutdown_outcome_when_cancelled_while_queued() {
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let state = test_state().await;
        let lane = state.write_lane();
        let _permit = lane.acquire().await.expect("write lane");
        let tick_state = state.clone();
        let task = tokio::spawn(async move { tick_state.run_maintenance_tick().await });

        for _ in 0..100 {
            let events = capture.maintenance_events();
            if event_stages(&events) == ["started"] {
                break;
            }
            tokio::task::yield_now().await;
        }
        state.shutdown_token().cancel();
        let tick = task.await.expect("maintenance task");

        assert_eq!(tick, MaintenanceTick::Stop);
        let events = capture.maintenance_events();
        assert_eq!(event_stages(&events), ["started", "finished"]);
        assert_correlated(&events);
        let terminal = events.last().expect("terminal maintenance event");
        assert_eq!(
            terminal.strings.get("outcome").map(String::as_str),
            Some("shutdown"),
        );
        assert_terminal_durations(terminal);
    }

    #[tokio::test]
    async fn maintenance_tick_emits_shutdown_outcome_when_cancelled_while_running() {
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let state = test_state().await;
        let lane = state.write_lane();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let tick_state = state.clone();
        let task = tokio::spawn(async move {
            tick_state
                .run_maintenance_tick_with(|_| async move {
                    started_tx.send(()).expect("maintenance started");
                    finish_rx.await.expect("finish maintenance");
                    Ok(MaintenanceStats {
                        fragments_removed: None,
                        fragments_added: None,
                        old_versions_pruned: None,
                    })
                })
                .await
        });

        started_rx.await.expect("maintenance start signal");
        state.shutdown_token().cancel();
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "shutdown must not cancel in-flight maintenance",
        );
        assert!(
            lane.try_acquire().is_err(),
            "write lane must remain held until in-flight maintenance completes",
        );
        finish_tx.send(()).expect("finish signal");
        let tick = task.await.expect("maintenance task");

        assert_eq!(tick, MaintenanceTick::Stop);
        let events = capture.maintenance_events();
        assert_eq!(
            event_stages(&events),
            ["started", "write_lane_acquired", "finished"],
        );
        assert_correlated(&events);
        let terminal = events.last().expect("terminal maintenance event");
        assert_eq!(
            terminal.strings.get("outcome").map(String::as_str),
            Some("shutdown"),
        );
        assert_terminal_durations(terminal);
    }

    async fn test_state() -> DaemonState {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        DaemonState::open(cfg, None).await.expect("open")
    }

    #[tokio::test]
    async fn touch_activity_advances_the_idle_clock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        state.inner.last_activity_secs.store(1, Ordering::Relaxed);
        assert!(
            state.should_idle_exit_at(300, 1000),
            "clock stale at 1 s; now=1000 is well past idle",
        );
        state.touch_activity(WorkClass::External);
        assert!(
            !state.should_idle_exit_at(300, state.last_activity_secs() + 1),
            "touch_activity must reset the clock so a fresh now is not idle",
        );
    }

    #[tokio::test]
    async fn enter_connection_and_touch_activity_track_per_class_state() {
        let state = test_state().await;
        let baseline = state.last_activity_secs();

        let ext = state.enter_connection(WorkClass::External);
        assert_eq!(
            state
                .inner
                .active_external_connections
                .load(Ordering::SeqCst),
            1
        );
        assert_eq!(
            state
                .inner
                .active_internal_connections
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            state.inner.active_connections.load(Ordering::SeqCst),
            1,
            "aggregate must still count both classes",
        );

        let int = state.enter_connection(WorkClass::Internal);
        assert_eq!(
            state
                .inner
                .active_internal_connections
                .load(Ordering::SeqCst),
            1
        );
        assert_eq!(state.inner.active_connections.load(Ordering::SeqCst), 2);

        state.touch_activity(WorkClass::External);
        assert!(
            state
                .inner
                .external_last_activity_secs
                .load(Ordering::Relaxed)
                >= baseline
        );
        assert_eq!(
            state
                .inner
                .internal_last_activity_secs
                .load(Ordering::Relaxed),
            baseline,
            "External touch must not stamp the Internal clock",
        );

        state.touch_activity(WorkClass::Internal);
        assert!(
            state
                .inner
                .internal_last_activity_secs
                .load(Ordering::Relaxed)
                >= baseline
        );

        drop(ext);
        assert_eq!(
            state
                .inner
                .active_external_connections
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            state.inner.active_connections.load(Ordering::SeqCst),
            1,
            "aggregate reflects only the dropped guard",
        );

        drop(int);
        assert_eq!(
            state
                .inner
                .active_internal_connections
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(state.inner.active_connections.load(Ordering::SeqCst), 0);

        // Existing predicate (aggregate-based) must be unaffected by class split.
        assert!(!state.should_idle_exit_at(300, baseline + 1));
    }

    /// Source-pin (ADR daemon-rework-002): every `enter_connection` call site
    /// must name its `WorkClass` with an explicit literal. Misclassifying (or
    /// defaulting) a caller re-opens the maintenance-starvation incident this
    /// split fixed, so a future zero-arg wrapper or `impl Default for
    /// WorkClass` cannot sneak past review: this scan fails the build instead.
    #[test]
    fn every_enter_connection_call_site_declares_a_work_class() {
        fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read_dir under src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);

        let needle = ".enter_connection";
        let mut call_sites = 0usize;
        for path in files {
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let mut start = 0;
            while let Some(found) = text[start..].find(needle) {
                let after = &text[start + found + needle.len()..];
                start += found + needle.len();
                // Only method calls count; prose mentions have no `(`.
                let Some(args) = after.trim_start().strip_prefix('(') else {
                    continue;
                };
                call_sites += 1;
                assert!(
                    args.trim_start().starts_with("WorkClass::"),
                    "{path:?}: `enter_connection` call site must pass an explicit \
                     `WorkClass::` literal (ADR daemon-rework-002); found: {:?}",
                    after.chars().take(40).collect::<String>(),
                );
            }
        }
        assert!(
            call_sites >= 4,
            "expected at least the four production call sites (server, watcher, \
             boot catch-up, maintenance tick); scan found {call_sites} -- did the \
             scan root move?",
        );
    }

    #[tokio::test]
    async fn defer_count_resets_and_increments() {
        let state = test_state().await;
        assert_eq!(state.defer_count(), 0);
        assert_eq!(state.increment_defer_count(), 1);
        assert_eq!(state.increment_defer_count(), 2);
        assert_eq!(state.defer_count(), 2);
        state.reset_defer_count();
        assert_eq!(state.defer_count(), 0);
    }

    #[tokio::test]
    async fn watcher_counters_track_events_and_reindex_outcomes() {
        let state = test_state().await;
        state.record_watcher_events(3);
        state.record_watcher_reindex(false);
        state.record_watcher_reindex(true);
        assert_eq!(state.watcher_counters_snapshot(), (3, 2, 1));
    }

    #[tokio::test]
    async fn ladder_trip_storage_records_and_overwrites_the_latest_action() {
        let state = test_state().await;
        assert_eq!(state.last_ladder_trip(), None);

        state.record_ladder_trip(LadderAction::ForceMaintenance);
        assert_eq!(
            state.last_ladder_trip().expect("trip recorded").action,
            LadderAction::ForceMaintenance,
        );

        state.record_ladder_trip(LadderAction::WatchdogTrip);
        assert_eq!(
            state.last_ladder_trip().expect("trip recorded").action,
            LadderAction::WatchdogTrip,
        );
    }

    /// Regression for the correctness finding fixed alongside this test:
    /// idle accounting must key off `Instant`-based monotonic ticks, not
    /// wall-clock Unix seconds, so an NTP correction or manual clock change
    /// can't make the daemon exit immediately after activity or postpone
    /// exit until the clock catches up. A process-relative monotonic clock
    /// reads small (seconds since this test binary started); a wall-clock
    /// Unix-seconds reading is always > 1.7 billion (2024+). If this ever
    /// regresses to `unix_secs()`-style wall time, `monotonic_secs()` jumps
    /// to the same huge magnitude and this assertion fails.
    #[test]
    fn idle_clock_is_monotonic_not_wall_clock() {
        let wall_clock_secs = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_secs();
        let monotonic = monotonic_secs();
        assert!(
            monotonic < wall_clock_secs / 2,
            "idle clock must be process-relative (Instant-based), not wall-clock \
             Unix seconds: monotonic={monotonic}, wall_clock={wall_clock_secs}",
        );
    }

    #[tokio::test]
    async fn crossencoder_guard_updates_last_use_on_drop() {
        let last_use_secs = Arc::new(AtomicU64::new(1));
        let before_drop = monotonic_secs();

        drop(CrossencoderGuard {
            guard: Arc::new(Mutex::new(HashMap::new())).lock_owned().await,
            key: String::new(),
            last_use_secs: Arc::clone(&last_use_secs),
        });

        let observed = last_use_secs.load(Ordering::Relaxed);
        assert!(
            observed >= before_drop,
            "drop should stamp crossencoder use at or after guard lifetime start: observed {observed}, before {before_drop}",
        );
    }

    #[tokio::test]
    async fn prune_stale_backups_removes_dirs_at_or_past_max_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ground_dir = tmp.path().join("ground");
        tokio::fs::create_dir_all(&ground_dir)
            .await
            .expect("create ground dir");
        let stale = tmp.path().join("ground.bak-v1");
        let unrelated = tmp.path().join("other-dir");
        tokio::fs::create_dir_all(&stale)
            .await
            .expect("create stale backup");
        tokio::fs::create_dir_all(&unrelated)
            .await
            .expect("create unrelated dir");

        let max_age = Duration::from_secs(1);
        let now = SystemTime::now() + Duration::from_secs(10);

        prune_stale_backups(&ground_dir, now, max_age)
            .await
            .expect("prune");

        assert!(!stale.exists(), "backup past max_age must be pruned");
        assert!(
            unrelated.exists(),
            "dirs that don't match the backup prefix must be left alone",
        );
    }

    #[tokio::test]
    async fn prune_stale_backups_keeps_dirs_within_max_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ground_dir = tmp.path().join("ground");
        tokio::fs::create_dir_all(&ground_dir)
            .await
            .expect("create ground dir");
        let fresh = tmp.path().join("ground.bak-v2");
        tokio::fs::create_dir_all(&fresh)
            .await
            .expect("create fresh backup");

        prune_stale_backups(&ground_dir, SystemTime::now(), STALE_BACKUP_MAX_AGE)
            .await
            .expect("prune");

        assert!(fresh.exists(), "backup younger than max_age must survive");
    }

    #[tokio::test]
    async fn prune_stale_backups_keeps_non_numeric_suffix_even_if_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ground_dir = tmp.path().join("ground");
        tokio::fs::create_dir_all(&ground_dir)
            .await
            .expect("create ground dir");
        // Shares the `<base>.bak-v` prefix but the suffix isn't all digits;
        // must never be treated as a pruneable version backup.
        let lookalike = tmp.path().join("ground.bak-vault");
        tokio::fs::create_dir_all(&lookalike)
            .await
            .expect("create lookalike dir");

        let max_age = Duration::from_secs(1);
        let now = SystemTime::now() + Duration::from_secs(31 * 24 * 60 * 60);

        prune_stale_backups(&ground_dir, now, max_age)
            .await
            .expect("prune");

        assert!(
            lookalike.exists(),
            "non-numeric-suffix dir must survive even when older than max_age",
        );
    }

    #[tokio::test]
    async fn move_stale_store_stamps_backup_mtime_to_now() {
        // `rename(2)` preserves the source dir's mtime, so a store idle >30d
        // would already look prunable the instant it's moved aside — the
        // whole point of the recovery window collapses. Give the source an
        // artificially old mtime, move it, and confirm `prune_stale_backups`
        // does NOT delete the fresh backup.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ground_dir = tmp.path().join("ground");
        tokio::fs::create_dir_all(&ground_dir)
            .await
            .expect("create ground dir");

        let old_mtime = SystemTime::now() - Duration::from_secs(60 * 24 * 60 * 60);
        let dir = ground_dir.clone();
        tokio::task::spawn_blocking(move || std::fs::File::open(&dir)?.set_modified(old_mtime))
            .await
            .expect("join")
            .expect("backdate ground dir mtime");

        move_stale_store(&ground_dir, 1).await.expect("move");

        let bak = tmp.path().join("ground.bak-v1");
        prune_stale_backups(&ground_dir, SystemTime::now(), STALE_BACKUP_MAX_AGE)
            .await
            .expect("prune");

        assert!(
            bak.exists(),
            "backup just moved aside must survive its own boot's prune, \
             even though the source dir's mtime was 60d old",
        );
    }

    /// C0 regression (state.rs unit half): `resources_for` must key its
    /// cache on `storage.ground_dir` alone, without going through the
    /// config-merge layer at all (hermetic — no repo-discovery, no scalar-
    /// conflict guard). Two effective `Config`s differing only in
    /// `ground_dir` must resolve to two distinct `RequestResources`, each
    /// rooted at its own tempdir; the same config queried twice must return
    /// the identical cached `Arc` rather than opening a second store.
    #[tokio::test]
    async fn resources_for_keys_on_ground_dir() {
        let tmp_a = tempfile::tempdir().expect("tempdir a");
        let tmp_b = tempfile::tempdir().expect("tempdir b");

        let mut cfg_a = Config::default();
        cfg_a.embeddings.enabled = false;
        cfg_a.storage.ground_dir = tmp_a.path().to_string_lossy().into_owned();

        let mut cfg_b = cfg_a.clone();
        cfg_b.storage.ground_dir = tmp_b.path().to_string_lossy().into_owned();

        let state = DaemonState::open(cfg_a.clone(), None)
            .await
            .expect("open daemon state");

        let res_a1 = state
            .resources_for(&cfg_a)
            .await
            .expect("resources_for cfg_a (first call)");
        let res_a2 = state
            .resources_for(&cfg_a)
            .await
            .expect("resources_for cfg_a (second call)");
        assert!(
            Arc::ptr_eq(&res_a1, &res_a2),
            "same config must resolve the same cached RequestResources Arc, \
             not rebuild or reopen a second store",
        );

        let res_b = state
            .resources_for(&cfg_b)
            .await
            .expect("resources_for cfg_b");
        assert!(
            !Arc::ptr_eq(&res_a1, &res_b),
            "a different ground_dir must key a distinct RequestResources entry",
        );
        assert_eq!(
            res_a1.ground_dir,
            tmp_a.path(),
            "cfg_a's resources must be rooted at tmp_a's ground_dir",
        );
        assert_eq!(
            res_b.ground_dir,
            tmp_b.path(),
            "cfg_b's resources must be rooted at tmp_b's ground_dir, not cfg_a's",
        );
        assert!(
            tmp_b.path().join("meta.toml").exists(),
            "resources_for must open (and initialize) a store at the new \
             ground_dir on first use",
        );
    }

    /// C0 regression: concurrent `resources_for` calls sharing one key must
    /// build the entry exactly once. The per-key build lock serializes the
    /// racing tasks so only the first opens the store; the rest re-check the
    /// cache and reuse it. A naive "drop the map lock, then insert" fix would
    /// let two tasks open a second `LanceStore` on the same ground dir,
    /// breaking the single-open-per-ground-dir invariant — this asserts every
    /// racer resolves to the identical `Arc`.
    #[tokio::test]
    async fn resources_for_builds_once_under_concurrent_same_key_calls() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();

        let state = DaemonState::open(cfg.clone(), None)
            .await
            .expect("open daemon state");

        // Race the builders on a ground_dir the boot-built baseline does NOT
        // own: the fresh key has no cache entry, so all 16 calls contend on
        // the build path. (Evicting the baseline entry instead would leave
        // its live store holding the ground dir's single-owner flock — #204 —
        // and the rebuild would correctly refuse.)
        let tmp_race = tempfile::tempdir().expect("tempdir race");
        cfg.storage.ground_dir = tmp_race.path().to_string_lossy().into_owned();

        let mut handles = Vec::new();
        for _ in 0..16 {
            let state = state.clone();
            let cfg = cfg.clone();
            handles.push(tokio::spawn(async move {
                state.resources_for(&cfg).await.expect("resources_for")
            }));
        }

        let mut resolved = Vec::new();
        for h in handles {
            resolved.push(h.await.expect("join resources_for task"));
        }

        let first = &resolved[0];
        for (i, res) in resolved.iter().enumerate() {
            assert!(
                Arc::ptr_eq(first, res),
                "racer {i} resolved a different RequestResources Arc — the \
                 store was opened more than once for one ground dir",
            );
        }
    }
    struct ZeroEmbedder;

    impl EmbedBatch for ZeroEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            _role: EmbedRole,
        ) -> hallouminate_domain::common::Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(vec![[0.0; EMBEDDING_DIM]; texts.len()])
        }
    }

    #[tokio::test]
    async fn cached_enabled_resource_retries_transient_embedder_failure() {
        let baseline_dir = tempfile::tempdir().expect("baseline tempdir");
        let mut baseline = Config::default();
        baseline.embeddings.enabled = false;
        baseline.storage.ground_dir = baseline_dir.path().to_string_lossy().into_owned();
        let state = DaemonState::open(baseline, None)
            .await
            .expect("open daemon state");

        let retry_dir = tempfile::tempdir().expect("retry tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = true;
        cfg.storage.ground_dir = retry_dir.path().to_string_lossy().into_owned();
        let store = LanceStore::open_or_create(
            retry_dir.path(),
            &cfg.embeddings.model,
            cfg.embeddings.quantized,
            true,
            None,
        )
        .await
        .expect("open enabled store without embedder");
        let cached = Arc::new(RequestResources {
            store: Arc::new(store),
            tokenizer: load_tokenizer(&cfg.embeddings.model).expect("tokenizer"),
            embeddings_enabled: true,
            ground_dir: retry_dir.path().to_path_buf(),
        });
        state
            .inner
            .resources
            .lock()
            .await
            .insert(ResourceKey::from_config(&cfg), Arc::clone(&cached));

        let first = state
            .resources_for_with_initializer(&cfg, |_, _, _| async {
                Err(anyhow::anyhow!("transient initialization failure"))
            })
            .await
            .expect("cached resources remain usable after retry failure");
        assert!(Arc::ptr_eq(&cached, &first));
        assert!(!first.store.embedder_available());

        let retried = state
            .resources_for_with_initializer(&cfg, |_, _, _| async {
                Ok(Box::new(ZeroEmbedder) as Box<dyn EmbedBatch>)
            })
            .await
            .expect("second request retries initialization");
        assert!(Arc::ptr_eq(&cached, &retried));
        assert!(retried.store.embedder_available());

        let normal = state
            .resources_for_with_initializer(&cfg, |_, _, _| async {
                panic!("ready cache hit must not initialize again")
            })
            .await
            .expect("normal request reuses repaired resource");
        assert!(Arc::ptr_eq(&retried, &normal));
    }

    // --- Test probe double: mutable atomic pressure signal for live-loop tests ---
    struct TestProbe(std::sync::atomic::AtomicBool);

    impl TestProbe {
        fn new(v: bool) -> Self {
            Self(std::sync::atomic::AtomicBool::new(v))
        }

        fn set(&self, v: bool) {
            self.0.store(v, Ordering::SeqCst);
        }
    }

    impl IoPressureProbe for TestProbe {
        fn elevated(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    // --- Decision matrix: maintenance_defer_reason_at ---

    #[tokio::test]
    async fn defer_reason_idle_and_clear_defers_nothing() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let probe = TestProbe::new(false);
        // Idle + no pressure means the caller would run a maintenance pass;
        // the pass itself is covered by the existing run_maintenance_tick tests.
        assert_eq!(state.maintenance_defer_reason_at(&probe, 1000), None);
    }

    #[tokio::test]
    async fn defer_reason_external_connection_defers_as_active() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let _conn = state.enter_connection(WorkClass::External);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::Active),
        );
    }

    #[tokio::test]
    async fn defer_reason_recent_activity_defers_as_active() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(970);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::Active),
        );
    }

    #[tokio::test]
    async fn defer_reason_external_activity_defers_as_active() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        state
            .inner
            .external_last_activity_secs
            .store(970, Ordering::Relaxed);
        state.inner.last_activity_secs.store(970, Ordering::Relaxed);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::Active),
        );
    }

    #[tokio::test]
    async fn defer_reason_internal_connection_does_not_defer_maintenance() {
        // ADR daemon-rework-002: housekeeping never defers housekeeping. An
        // in-flight Internal guard (watcher batch, boot catch-up, maintenance
        // tick) must leave the daemon maintenance-eligible.
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let _conn = state.enter_connection(WorkClass::Internal);
        let probe = TestProbe::new(false);
        assert_eq!(state.maintenance_defer_reason_at(&probe, 1000), None);
    }

    #[tokio::test]
    async fn defer_reason_internal_connection_with_io_pressure_defers_as_io_pressure() {
        // With the class split, internal work must not mask the real defer
        // reason: an Internal guard plus elevated PSI reads as IoPressure
        // (pre-split it misread as Active), so the ladder and logs see why
        // maintenance actually waited.
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let _conn = state.enter_connection(WorkClass::Internal);
        let probe = TestProbe::new(true);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::IoPressure),
        );
    }

    #[tokio::test]
    async fn defer_reason_internal_activity_does_not_defer_maintenance() {
        // The 2026-07-17 incident: the watcher stamping the shared clock kept
        // the daemon permanently "Active" and starved maintenance. Internal
        // activity alone must not read as Active.
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        state
            .inner
            .internal_last_activity_secs
            .store(970, Ordering::Relaxed);
        state.inner.last_activity_secs.store(970, Ordering::Relaxed);
        let probe = TestProbe::new(false);
        assert_eq!(state.maintenance_defer_reason_at(&probe, 1000), None);
    }

    #[tokio::test]
    async fn internal_housekeeping_alone_is_maintenance_eligible_but_defers_idle_exit() {
        // Acceptance (daemon-rework curd 5): when internal housekeeping is the
        // only activity, the daemon is maintenance-eligible while idle-exit
        // still waits for the housekeeping to finish (ADR-003 preserved).
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let conn = state.enter_connection(WorkClass::Internal);
        state
            .inner
            .internal_last_activity_secs
            .store(970, Ordering::Relaxed);
        state.inner.last_activity_secs.store(970, Ordering::Relaxed);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            None,
            "internal-only housekeeping must leave maintenance eligible",
        );
        assert!(
            !state.should_idle_exit_at(300, 100_000),
            "an in-flight Internal guard must defer idle-exit",
        );
        drop(conn);
        assert!(
            !state.should_idle_exit_at(300, 1000),
            "recent Internal activity (clock at 970) must defer idle-exit at now=1000",
        );
        assert!(
            state.should_idle_exit_at(300, 970 + 300),
            "once housekeeping completes and the window elapses, idle-exit fires",
        );
    }

    #[tokio::test]
    async fn defer_reason_io_pressure_defers_and_clears() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let probe = TestProbe::new(true);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::IoPressure),
        );
        probe.set(false);
        assert_eq!(state.maintenance_defer_reason_at(&probe, 1000), None);
    }

    #[tokio::test]
    async fn defer_reason_active_takes_priority_over_io_pressure() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let _conn = state.enter_connection(WorkClass::External);
        let probe = TestProbe::new(true);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 1000),
            Some(DeferReason::Active),
        );
    }

    // --- Boundary: is_idle at exactly the 60s activity window edge ---

    #[tokio::test]
    async fn defer_reason_idle_boundary_59s_is_still_active() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 59),
            Some(DeferReason::Active),
            "59s since last activity must not count as idle",
        );
    }

    #[tokio::test]
    async fn defer_reason_idle_boundary_60s_is_idle() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let probe = TestProbe::new(false);
        assert_eq!(
            state.maintenance_defer_reason_at(&probe, 60),
            None,
            "60s since last activity must count as idle (inclusive boundary)",
        );
    }

    #[tokio::test]
    async fn defer_reason_idle_boundary_61s_is_idle() {
        let state = test_state().await;
        state.set_last_activity_secs_for_test(0);
        let probe = TestProbe::new(false);
        assert_eq!(state.maintenance_defer_reason_at(&probe, 61), None);
    }

    // --- Live loop: paused-time, clock-free defer via active_connections ---

    #[tokio::test(start_paused = true)]
    async fn maintenance_loop_warns_after_eleven_consecutive_defers() {
        // Shared OBSERVED slot: an ambient Hard recorded by a concurrent
        // test would skip the defer path this test asserts on.
        let _coord = crate::daemon::debt::OBSERVED_HARD_COORD.read().await;
        let state = test_state().await;
        let _conn = state.enter_connection(WorkClass::External);
        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));

        // Let the freshly spawned task register its interval sleep timer
        // against the current paused clock before advancing it -- otherwise
        // `advance` moves the clock before the timer exists and the sleep
        // computes its deadline from the already-advanced time.
        tokio::task::yield_now().await;

        // Past interval + max jitter (100 + 10): enters the recheck loop, where
        // the first defer check fires synchronously (consecutive_defers == 1).
        tokio::time::advance(Duration::from_secs(111)).await;
        tokio::task::yield_now().await;

        // W2's heartbeat cadence slices the interval sleep into <=60s
        // chunks, so 60s advances are consumed by chunk completions as well
        // as defer rechecks -- an iteration-indexed schedule no longer maps
        // 1:1 to defer counts. Drive the clock and pin the FIRST captured
        // count-11 event instead: the warn threshold is `> 10`, so a warn
        // firing any earlier would surface tagged with a smaller count.
        let mut saw_eleven = false;
        for _ in 0..30 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
            let warned = {
                let events = capture.0.lock().expect("capture lock");
                events
                    .iter()
                    .any(|e| e.numbers.get("consecutive_defers").copied() == Some(11))
            };
            if warned {
                saw_eleven = true;
                break;
            }
        }
        assert!(
            saw_eleven,
            "expected a warn event with consecutive_defers == 11 within the advance budget"
        );

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_loop_shuts_down_promptly_during_interval_sleep() {
        let state = test_state().await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));

        // Cancel wins the biased select against the interval sleep, so no
        // time advance is needed for the task to complete.
        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_loop_shuts_down_promptly_during_recheck_sleep() {
        let state = test_state().await;
        let _conn = state.enter_connection(WorkClass::External);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(maintenance_loop(
            state.clone(),
            cancel.clone(),
            Duration::from_secs(100),
            Arc::new(TestProbe::new(false)),
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(111)).await;
        tokio::task::yield_now().await;

        cancel.cancel();
        task.await.expect("maintenance_loop task");
    }

    #[tokio::test]
    async fn maintenance_interval_zero_disables_the_background_task() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        cfg.daemon.maintenance_interval_secs = 0;

        let capture = EventCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let state = DaemonState::open(cfg, None).await.expect("open");
        assert!(state.take_maintenance_task().await.is_none());

        let events = capture.0.lock().expect("capture lock");
        let disabled = events.iter().any(|e| {
            e.strings
                .get("message")
                .map(|m| m.contains("disabled"))
                .unwrap_or(false)
        });
        assert!(
            disabled,
            "expected a log message mentioning maintenance is disabled"
        );
    }

    // --- Jitter ---

    #[test]
    fn jittered_sleep_secs_stays_within_interval_plus_ten_percent() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let jittered = jittered_sleep_secs(100);
            assert!(
                (100..=110).contains(&jittered),
                "jittered_sleep_secs(100) = {jittered} out of [100, 110]",
            );
            seen.insert(jittered);
        }
        assert!(
            seen.contains(&100) && seen.contains(&110),
            "expected both closed-interval endpoints reachable across 1000 iterations, got {seen:?}",
        );
    }

    #[test]
    fn jittered_sleep_secs_zero_interval_returns_zero() {
        assert_eq!(jittered_sleep_secs(0), 0);
    }
    #[test]
    fn jittered_sleep_secs_below_ten_adds_no_jitter() {
        assert_eq!(jittered_sleep_secs(5), 5);
    }
}
