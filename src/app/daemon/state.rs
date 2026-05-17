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
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use crate::adapters::lance::LanceStore;
use crate::app::config::Config;
use crate::domain::common::expand_tilde;
use crate::domain::corpus::{MarkdownChunker, load_tokenizer};
use crate::domain::embeddings::Embedder;

const CHUNK_BUDGET_TOKENS: usize = 384;

/// Map of corpus name → per-corpus async mutex. Each corpus gets its own
/// `Mutex<()>` so unrelated corpora never block each other.
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
    cfg: Config,
    store: Arc<LanceStore>,
    ground_dir: PathBuf,
    corpus_locks: CorpusLockMap,
    write_lane: Arc<Semaphore>,
    embedder: Arc<Mutex<Embedder>>,
    tokenizer: tokenizers::Tokenizer,
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
    pub async fn open(cfg: Config) -> anyhow::Result<Self> {
        let ground_dir = expand_tilde(&cfg.storage.ground_dir);
        if let Some(parent) = ground_dir.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("create ground dir parent: {e}"))?;
        }
        let store = LanceStore::open_or_create(&ground_dir, &cfg.embeddings.model)
            .await
            .map_err(|e| anyhow::anyhow!("open ground dir {}: {e}", ground_dir.display()))?;
        // Load the embedder + tokenizer once at boot. A daemon whose whole
        // job is to amortize LanceDB ownership across CLI/MCP processes
        // should not pay the model-load cost per request.
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)
            .map_err(|e| anyhow::anyhow!("init embedder ({}): {e}", cfg.embeddings.model))?;
        let tokenizer = load_tokenizer(&cfg.embeddings.model)
            .map_err(|e| anyhow::anyhow!("load tokenizer for {}: {e}", cfg.embeddings.model))?;
        Ok(DaemonState {
            inner: Arc::new(DaemonStateInner {
                cfg,
                store: Arc::new(store),
                ground_dir,
                corpus_locks: CorpusLockMap::default(),
                write_lane: Arc::new(Semaphore::new(1)),
                embedder: Arc::new(Mutex::new(embedder)),
                tokenizer,
            }),
        })
    }

    pub fn cfg(&self) -> &Config {
        &self.inner.cfg
    }

    pub fn store(&self) -> Arc<LanceStore> {
        self.inner.store.clone()
    }

    pub fn ground_dir(&self) -> &std::path::Path {
        &self.inner.ground_dir
    }

    /// Borrow the shared embedder for one call. The fastembed runtime is
    /// `&mut`-only, so concurrent embed batches serialize behind this mutex.
    /// That matches the underlying constraint (one model handle per process)
    /// rather than introducing a new one.
    pub async fn embedder(&self) -> MutexGuard<'_, Embedder> {
        self.inner.embedder.lock().await
    }

    /// A freshly-constructed `MarkdownChunker` over the daemon's loaded
    /// tokenizer. Construction is cheap (the tokenizer is `Clone` and the
    /// chunker is a thin wrapper), so handlers build one per call instead of
    /// reaching into shared state for it.
    pub fn make_chunker(&self) -> MarkdownChunker<tokenizers::Tokenizer> {
        MarkdownChunker::new(self.inner.tokenizer.clone(), CHUNK_BUDGET_TOKENS)
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

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState")
            .field("ground_dir", &self.inner.ground_dir)
            .field("model", &self.inner.cfg.embeddings.model)
            .finish()
    }
}
