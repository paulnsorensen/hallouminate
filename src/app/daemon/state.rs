//! Shared daemon state: config, LanceStore handle, per-corpus locks, the
//! global write-lane semaphore, and a cached embedder + tokenizer.
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
//! The embedder and tokenizer are loaded once at daemon boot and shared
//! across requests. The embedder is wrapped in an async `Mutex` because
//! `Embedder::embed_batch` takes `&mut self` (it owns the fastembed runtime
//! handle); only one batch can run at a time per process today, so the
//! mutex matches the underlying constraint rather than introducing a new
//! one. Tokenizers are cheap to clone (`Arc` internally) and need no lock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::adapters::lance::LanceStore;
use crate::app::config::Config;
use crate::domain::common::{HallouminateError, expand_tilde};
use crate::domain::corpus::{load_tokenizer, missing_roots};
use crate::domain::embeddings::{EmbedBatch, Embedder};
use crate::domain::indexer::HandlerRegistry;
use crate::domain::indexer::index::index_corpus;
use crate::domain::search::{FastembedCrossencoder, canonical_crossencoder_model};

const CHUNK_BUDGET_TOKENS: usize = 384;

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_idle(last_use_secs: u64, now_secs: u64, idle_secs: u64) -> bool {
    now_secs.saturating_sub(last_use_secs) >= idle_secs
}

/// Map of corpus name → per-corpus async mutex. Each corpus gets its own
/// `Mutex<()>` so unrelated corpora never collide *at the per-corpus lock
/// layer* — but every mutating handler also takes the single-permit global
/// `write_lane` (see `DaemonStateInner.write_lane`), so cross-corpus writes
/// still serialize at the lane while reads through different corpora run
/// freely.
#[derive(Default)]
struct CorpusLockMap {
    inner: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl CorpusLockMap {
    async fn lock(&self, corpus: &str) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.inner.lock().await;
            map.entry(corpus.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }
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
    store: Arc<LanceStore>,
    ground_dir: PathBuf,
    /// Whether dense embeddings are enabled for this daemon (mirrors
    /// `baseline.embeddings.enabled`). When false, the embedder is `None`
    /// permanently and every retrieval/index path runs lexical-only.
    embeddings_enabled: bool,
    corpus_locks: CorpusLockMap,
    write_lane: Arc<Semaphore>,
    embedder: Arc<Mutex<Option<Embedder>>>,
    /// Lazy-loaded crossencoder rerankers, keyed by canonical model name.
    /// A per-model cache (rather than a single slot) so that repos
    /// selecting different `[search].crossencoder` models via repo-layer
    /// config each get their own loaded model instead of clobbering a
    /// shared one. Empty until the first `ground` request that resolves a
    /// configured model; the baseline model (if any) is pre-warmed at boot.
    crossencoders: Arc<Mutex<HashMap<String, FastembedCrossencoder>>>,
    last_embed_use_secs: Arc<AtomicU64>,
    tokenizer: tokenizers::Tokenizer,
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
        // Build embedder + tokenizer BEFORE opening the store so we have them
        // available for a potential stale-store rebuild on the same boot.
        //
        // Embeddings are opt-in. When disabled, the embedder stays `None` for
        // the daemon's lifetime (no model download, no load) and every
        // retrieval/index path runs lexical-only. The tokenizer is still
        // loaded here — chunking needs it regardless of the embedding mode.
        //
        // When enabled, try to load the embedder eagerly so the first request
        // doesn't pay the load cost mid-call. Tolerate failure (e.g. offline
        // first run with no cached model) so the daemon can still serve
        // model-independent ops (`ping`, `list_corpora`, `list_files`,
        // `read_markdown`, `delete_markdown`); a later embedder() call will
        // retry the load and surface the error then.
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let mut embedder: Option<Embedder> = if cfg.embeddings.enabled {
            match Embedder::try_new(&cfg.embeddings.model, cfg.embeddings.quantized, &cache_dir) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!(
                        target: "hallouminate::daemon",
                        model = %cfg.embeddings.model,
                        error = %e,
                        "embedder unavailable at startup; will retry on first embedding request",
                    );
                    None
                }
            }
        } else {
            None
        };
        let tokenizer = load_tokenizer(&cfg.embeddings.model)
            .map_err(|e| anyhow::anyhow!("load tokenizer for {}: {e}", cfg.embeddings.model))?;

        let store = match LanceStore::open_or_create(
            &ground_dir,
            &cfg.embeddings.model,
            cfg.embeddings.quantized,
            cfg.embeddings.enabled,
        )
        .await
        {
            Ok(s) => s,
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
                // If anything in the rebuild fails, clean up the partially-created
                // fresh dir so the next boot re-enters the "no ground dir" path
                // and retries the rebuild, rather than opening an empty-but-valid store.
                let rebuild_result: anyhow::Result<LanceStore> = async {
                    let fresh = LanceStore::open_or_create(
                        &ground_dir,
                        &cfg.embeddings.model,
                        cfg.embeddings.quantized,
                        cfg.embeddings.enabled,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "rebuild: open fresh ground dir {}: {e}",
                            ground_dir.display()
                        )
                    })?;
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
                        let emb: Option<&mut dyn EmbedBatch> =
                            embedder.as_mut().map(|e| e as &mut dyn EmbedBatch);
                        let stats = index_corpus(&corpus, &fresh, emb, &registry)
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
                    Ok(fresh)
                }
                .await;
                match rebuild_result {
                    Ok(fresh) => fresh,
                    Err(e) => {
                        // Remove the fresh (empty/partial) ground dir so the next boot
                        // sees "no store" and retries the rebuild rather than booting
                        // with an empty index.
                        if ground_dir.exists() {
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
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "open ground dir {}: {e}",
                    ground_dir.display()
                ));
            }
        };
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
        let idle_evict_secs = cfg.embeddings.idle_evict_secs;
        let shutdown = CancellationToken::new();
        let embedder_arc = Arc::new(Mutex::new(embedder));
        let crossencoders_arc = Arc::new(Mutex::new(crossencoders));
        let last_embed_use = Arc::new(AtomicU64::new(unix_secs()));

        if idle_evict_secs > 0 {
            let embedder_ref = Arc::clone(&embedder_arc);
            let crossencoders_ref = Arc::clone(&crossencoders_arc);
            let last_use_ref = Arc::clone(&last_embed_use);
            let cancel = shutdown.clone();
            tracing::debug!(
                target: "hallouminate::daemon",
                idle_evict_secs,
                "idle evict task spawned",
            );
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(idle_evict_secs)) => {
                            if is_idle(last_use_ref.load(Ordering::Relaxed), unix_secs(), idle_evict_secs) {
                                let mut emb = embedder_ref.lock().await;
                                let mut xenc = crossencoders_ref.lock().await;
                                if is_idle(last_use_ref.load(Ordering::Relaxed), unix_secs(), idle_evict_secs) {
                                    let was_live = emb.is_some();
                                    *emb = None;
                                    let had_xenc = !xenc.is_empty();
                                    xenc.clear();
                                    if was_live || had_xenc {
                                        tracing::info!(
                                            target: "hallouminate::daemon",
                                            idle_secs = idle_evict_secs,
                                            "embedder evicted after idle; ORT BFCArena memory released",
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(DaemonState {
            inner: Arc::new(DaemonStateInner {
                embeddings_enabled: cfg.embeddings.enabled,
                baseline: cfg,
                baseline_xdg_path: xdg_path,
                store: Arc::new(store),
                ground_dir,
                corpus_locks: CorpusLockMap::default(),
                write_lane: Arc::new(Semaphore::new(1)),
                embedder: embedder_arc,
                crossencoders: crossencoders_arc,
                last_embed_use_secs: last_embed_use,
                tokenizer,
                shutdown,
            }),
        })
    }

    /// The daemon-wide shutdown token. The accept loop selects on
    /// [`CancellationToken::cancelled`]; the IPC `Shutdown` dispatcher and the
    /// signal handlers call [`CancellationToken::cancel`].
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.inner.shutdown
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
        self.inner.store.clone()
    }

    pub fn ground_dir(&self) -> &std::path::Path {
        &self.inner.ground_dir
    }

    /// Whether dense embeddings are enabled. Dispatchers branch on this to
    /// pass `Some(embedder)` (hybrid) or `None` (lexical-only) into `ground`
    /// and `index_corpus`. False means the embedder is permanently `None`.
    pub fn embeddings_enabled(&self) -> bool {
        self.inner.embeddings_enabled
    }

    /// Borrow the shared embedder for one call, loading it lazily on first
    /// use. Daemon boot tries an eager load (see `open`) but tolerates
    /// failure so model-independent ops (ping, list_corpora, list_files,
    /// read_markdown, delete_markdown) keep working offline. The first call
    /// that *needs* embedding pays the load cost (or surfaces a clean error
    /// when the model is unreachable).
    ///
    /// The fastembed runtime is `&mut`-only, so concurrent embed batches
    /// serialize behind this mutex; that matches the underlying constraint
    /// (one model handle per process) rather than introducing a new one.
    pub async fn embedder(&self) -> anyhow::Result<EmbedderGuard<'_>> {
        let mut guard = self.inner.embedder.lock().await;
        if guard.is_none() {
            let cache_dir = expand_tilde(&self.inner.baseline.embeddings.cache_dir);
            let embedder = Embedder::try_new(
                &self.inner.baseline.embeddings.model,
                self.inner.baseline.embeddings.quantized,
                &cache_dir,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "init embedder ({}): {e}",
                    self.inner.baseline.embeddings.model
                )
            })?;
            *guard = Some(embedder);
        }
        self.inner
            .last_embed_use_secs
            .store(unix_secs(), Ordering::Relaxed);
        Ok(EmbedderGuard {
            guard,
            last_use_secs: Arc::clone(&self.inner.last_embed_use_secs),
        })
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
    ) -> anyhow::Result<Option<CrossencoderGuard<'_>>> {
        let Some(model_name) = model_name else {
            return Ok(None);
        };
        // Canonicalize so config aliases (e.g. the corrected English
        // spelling of a typo'd upstream id) share one cache entry.
        let canonical = canonical_crossencoder_model(model_name)?;
        let mut guard = self.inner.crossencoders.lock().await;
        if !guard.contains_key(canonical) {
            let cache_dir = expand_tilde(&self.inner.baseline.embeddings.cache_dir);
            let model = FastembedCrossencoder::try_new(canonical, &cache_dir)
                .map_err(|e| anyhow::anyhow!("init crossencoder ({canonical}): {e}"))?;
            guard.insert(canonical.to_string(), model);
        }
        self.inner
            .last_embed_use_secs
            .store(unix_secs(), Ordering::Relaxed);
        Ok(Some(CrossencoderGuard {
            guard,
            key: canonical.to_string(),
            last_use_secs: Arc::clone(&self.inner.last_embed_use_secs),
        }))
    }

    /// Evict the embedder and crossencoders if idle for at least `idle_secs`
    /// seconds (comparing `now_secs` to the last-recorded embed use). Returns
    /// `true` when eviction conditions are met (whether or not anything was
    /// live), `false` when `idle_secs == 0` (disabled) or the last use is too
    /// recent. Exposed `pub(crate)` so tests can inject a synthetic `now_secs`
    /// without sleeping.
    #[cfg(test)]
    pub(crate) async fn evict_if_idle_at(&self, idle_secs: u64, now_secs: u64) -> bool {
        if idle_secs == 0 {
            return false;
        }
        let last = self.inner.last_embed_use_secs.load(Ordering::Relaxed);
        if !is_idle(last, now_secs, idle_secs) {
            return false;
        }
        let mut emb = self.inner.embedder.lock().await;
        let mut xenc = self.inner.crossencoders.lock().await;
        if !is_idle(
            self.inner.last_embed_use_secs.load(Ordering::Relaxed),
            now_secs,
            idle_secs,
        ) {
            return false;
        }
        let was_live = emb.is_some();
        *emb = None;
        let had_xenc = !xenc.is_empty();
        xenc.clear();
        if was_live || had_xenc {
            tracing::info!(
                target: "hallouminate::daemon",
                idle_secs,
                "embedder evicted after idle; ORT BFCArena memory released",
            );
        }
        true
    }

    /// Unix-second timestamp of the most recent `embedder()` or
    /// `crossencoder()` call. Exposed `pub(crate)` for tests.
    #[cfg(test)]
    pub(crate) fn last_embed_use_secs(&self) -> u64 {
        self.inner.last_embed_use_secs.load(Ordering::Relaxed)
    }

    /// A freshly-constructed format-handler [`HandlerRegistry`] over the
    /// daemon's loaded tokenizer. Construction is cheap (the tokenizer is
    /// `Clone` and each handler is a thin wrapper), so handlers build one per
    /// call instead of reaching into shared state for it.
    pub fn make_registry(&self) -> HandlerRegistry {
        HandlerRegistry::new(self.inner.tokenizer.clone(), CHUNK_BUDGET_TOKENS)
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
        let corpus = self.lock_corpus(corpus).await;
        let permit = self
            .write_lane()
            .acquire_owned()
            .await
            .map_err(|_| "write lane closed")?;
        Ok(MutationGuard {
            _permit: permit,
            _corpus: corpus,
        })
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
    tracing::info!(
        target: "hallouminate::daemon",
        backup = %bak.display(),
        "moved stale ground store aside; recoverable until pruned",
    );
    Ok(())
}

/// Owned guard around the lazily-loaded embedder. Derefs to `Embedder` so
/// existing call sites (`ground`, `index_corpus`, `apply`) keep their
/// `&mut Embedder` signatures unchanged — only the *acquisition* shape
/// (Result instead of infallible) differs.
pub struct EmbedderGuard<'a> {
    guard: MutexGuard<'a, Option<Embedder>>,
    last_use_secs: Arc<AtomicU64>,
}

impl std::ops::Deref for EmbedderGuard<'_> {
    type Target = Embedder;
    fn deref(&self) -> &Embedder {
        // SAFETY-of-correctness: `embedder()` populates `Some(...)` before
        // handing the guard out, and the guard holds the mutex so no one
        // else can swap it back to `None`.
        self.guard.as_ref().expect("embedder loaded")
    }
}

impl std::ops::DerefMut for EmbedderGuard<'_> {
    fn deref_mut(&mut self) -> &mut Embedder {
        self.guard.as_mut().expect("embedder loaded")
    }
}

impl Drop for EmbedderGuard<'_> {
    fn drop(&mut self) {
        self.last_use_secs.store(unix_secs(), Ordering::Relaxed);
    }
}

/// Owned guard around the lazily-loaded crossencoder, mirroring
/// `EmbedderGuard`. Derefs to `FastembedCrossencoder` so callers can
/// pass `&mut *guard` directly into anything that wants
/// `&mut dyn Crossencoder`.
pub struct CrossencoderGuard<'a> {
    guard: MutexGuard<'a, HashMap<String, FastembedCrossencoder>>,
    /// Canonical model name; the key into `guard` that `crossencoder()`
    /// inserted before handing the guard out.
    key: String,
    last_use_secs: Arc<AtomicU64>,
}

impl std::ops::Deref for CrossencoderGuard<'_> {
    type Target = FastembedCrossencoder;
    fn deref(&self) -> &FastembedCrossencoder {
        // `crossencoder()` inserts `key` before constructing the guard,
        // and the guard holds the lock, so the entry can't vanish.
        self.guard.get(&self.key).expect("crossencoder loaded")
    }
}

impl std::ops::DerefMut for CrossencoderGuard<'_> {
    fn deref_mut(&mut self) -> &mut FastembedCrossencoder {
        self.guard.get_mut(&self.key).expect("crossencoder loaded")
    }
}

impl Drop for CrossencoderGuard<'_> {
    fn drop(&mut self) {
        self.last_use_secs.store(unix_secs(), Ordering::Relaxed);
    }
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState")
            .field("ground_dir", &self.inner.ground_dir)
            .field("model", &self.inner.baseline.embeddings.model)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn evict_if_idle_at_returns_false_when_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        assert!(
            !state.evict_if_idle_at(0, u64::MAX).await,
            "idle_secs=0 means disabled; must not evict",
        );
    }

    #[tokio::test]
    async fn evict_if_idle_at_returns_false_when_recently_used() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_embed_use_secs();
        // now_secs is only 1 s ahead — not enough for a 300 s idle threshold.
        assert!(
            !state.evict_if_idle_at(300, last + 1).await,
            "1 s elapsed < 300 s idle; must not evict",
        );
    }

    #[tokio::test]
    async fn evict_if_idle_at_returns_true_when_idle_threshold_exceeded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_embed_use_secs();
        // now_secs is 301 s ahead — exceeds the 300 s idle threshold.
        assert!(
            state.evict_if_idle_at(300, last + 301).await,
            "301 s elapsed >= 300 s idle; must evict",
        );
    }

    #[tokio::test]
    async fn evict_if_idle_at_exact_boundary_evicts() {
        // The condition is `elapsed >= idle_secs` (inclusive). Verifies `>`
        // was not accidentally used instead.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_embed_use_secs();
        assert!(
            state.evict_if_idle_at(300, last + 300).await,
            "elapsed == idle_secs (>= threshold); must evict",
        );
    }

    #[tokio::test]
    async fn evict_if_idle_at_one_second_below_threshold_does_not_evict() {
        // Symmetric boundary: elapsed = idle_secs - 1 is strictly less.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        let last = state.last_embed_use_secs();
        assert!(
            !state.evict_if_idle_at(300, last + 299).await,
            "elapsed = idle_secs - 1 (< threshold); must not evict",
        );
    }
    #[tokio::test]
    async fn evict_if_idle_at_rechecks_after_waiting_for_embedder_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        let state = DaemonState::open(cfg, None).await.expect("open");
        state.inner.last_embed_use_secs.store(1, Ordering::Relaxed);

        let held = state.inner.embedder.lock().await;
        let task_state = state.clone();
        let eviction = tokio::spawn(async move { task_state.evict_if_idle_at(300, 1000).await });
        state
            .inner
            .last_embed_use_secs
            .store(999, Ordering::Relaxed);
        drop(held);

        assert!(
            !eviction.await.expect("eviction task joins"),
            "eviction must re-check idleness after waiting for an active guard",
        );
    }

    #[tokio::test]
    async fn embedder_guard_updates_last_use_on_drop() {
        let last_use_secs = Arc::new(AtomicU64::new(1));
        let guard = Mutex::new(None);
        let guard = guard.lock().await;
        let before_drop = unix_secs();

        drop(EmbedderGuard {
            guard,
            last_use_secs: Arc::clone(&last_use_secs),
        });

        let observed = last_use_secs.load(Ordering::Relaxed);
        assert!(
            observed >= before_drop,
            "drop should stamp embedder use at or after guard lifetime start: observed {observed}, before {before_drop}",
        );
    }

    #[tokio::test]
    async fn crossencoder_guard_updates_last_use_on_drop() {
        let last_use_secs = Arc::new(AtomicU64::new(1));
        let guard = Mutex::new(HashMap::new());
        let guard = guard.lock().await;
        let before_drop = unix_secs();

        drop(CrossencoderGuard {
            guard,
            key: String::new(),
            last_use_secs: Arc::clone(&last_use_secs),
        });

        let observed = last_use_secs.load(Ordering::Relaxed);
        assert!(
            observed >= before_drop,
            "drop should stamp crossencoder use at or after guard lifetime start: observed {observed}, before {before_drop}",
        );
    }
}
