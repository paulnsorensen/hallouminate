//! Single-flight directory walks for `ground` requests.
//!
//! Concurrent requests for the same corpus share one in-flight walk. The
//! entry leaves the map when the walk ends, so a later request always sees
//! the current files on disk. Every walk holds one coverage-gate permit, so
//! coverage checks and the lexical fallback share one concurrency limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use hallouminate_domain::common::CorpusConfig;
use hallouminate_domain::corpus::ScannedFile;
use tokio::sync::Semaphore;
use tokio::time::error::Elapsed;

/// Time one request waits for a corpus walk before it gives up on the files.
pub(crate) const SCAN_BUDGET: Duration = Duration::from_secs(2);

/// Walks one corpus on a blocking thread.
pub(crate) type Scanner =
    Arc<dyn Fn(&CorpusConfig) -> anyhow::Result<Vec<ScannedFile>> + Send + Sync>;

/// How one corpus walk ended.
#[derive(Clone)]
pub(crate) enum Walk {
    /// The walk finished and selected these files.
    Files(Arc<Vec<ScannedFile>>),
    /// The scanner returned an error.
    ScanFailed(Arc<str>),
    /// The walk task panicked or was cancelled.
    TaskFailed,
}

type InFlight = Shared<BoxFuture<'static, Walk>>;
type Registry = Arc<StdMutex<HashMap<CorpusConfig, InFlight>>>;

/// Removes the in-flight entry when the walk task ends, even on a panic.
struct EntryGuard {
    registry: Registry,
    corpus: CorpusConfig,
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        let Ok(mut inflight) = self.registry.lock() else {
            return;
        };
        inflight.remove(&self.corpus);
    }
}

pub(crate) struct ScanCache {
    budget: Duration,
    inflight: Registry,
}

impl ScanCache {
    pub(crate) fn new(budget: Duration) -> Self {
        Self {
            budget,
            inflight: Arc::default(),
        }
    }

    /// Joins or starts the walk of `corpus` and waits at most the budget.
    ///
    /// The walk keeps running after a timeout and holds its permit until it
    /// ends.
    pub(crate) async fn walk(
        &self,
        corpus: &CorpusConfig,
        gate: Arc<Semaphore>,
        scanner: Scanner,
    ) -> Result<Walk, Elapsed> {
        let flight = self.join_or_start(corpus, gate, scanner);
        tokio::time::timeout(self.budget, flight).await
    }

    /// Joins or starts the walk of `corpus` and waits for it to end.
    pub(crate) async fn walk_to_end(
        &self,
        corpus: &CorpusConfig,
        gate: Arc<Semaphore>,
        scanner: Scanner,
    ) -> Walk {
        self.join_or_start(corpus, gate, scanner).await
    }

    fn join_or_start(
        &self,
        corpus: &CorpusConfig,
        gate: Arc<Semaphore>,
        scanner: Scanner,
    ) -> InFlight {
        let mut inflight = self.inflight.lock().expect("scan cache lock");
        if let Some(flight) = inflight.get(corpus) {
            return flight.clone();
        }
        let guard = EntryGuard {
            registry: Arc::clone(&self.inflight),
            corpus: corpus.clone(),
        };
        let owned = corpus.clone();
        let task = tokio::spawn(async move {
            let walk = run_walk(owned, gate, scanner).await;
            drop(guard);
            walk
        });
        let flight = async move {
            let joined = task.await;
            match joined {
                Ok(walk) => walk,
                Err(error) => {
                    tracing::warn!(err = %error, "corpus walk task failed");
                    Walk::TaskFailed
                }
            }
        }
        .boxed()
        .shared();
        inflight.insert(corpus.clone(), flight.clone());
        flight
    }
}

async fn run_walk(corpus: CorpusConfig, gate: Arc<Semaphore>, scanner: Scanner) -> Walk {
    let permit = gate
        .acquire_owned()
        .await
        .expect("coverage gate remains open while daemon runs");
    let scanned = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        scanner(&corpus)
    })
    .await;
    match scanned {
        Ok(Ok(files)) => Walk::Files(Arc::new(files)),
        Ok(Err(error)) => {
            tracing::debug!(err = %error, "corpus walk failed");
            Walk::ScanFailed(Arc::from(format!("{error:#}")))
        }
        Err(error) => {
            tracing::warn!(err = %error, "corpus walk task failed");
            Walk::TaskFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use hallouminate_domain::common::{CorpusKey, FileRef, Mtime};

    use super::*;

    fn corpus(name: &str) -> CorpusConfig {
        CorpusConfig {
            name: name.to_string(),
            paths: vec![format!("/tmp/{name}")],
            ..CorpusConfig::default()
        }
    }

    fn one_file(corpus: &CorpusConfig) -> Vec<ScannedFile> {
        vec![ScannedFile {
            corpus_key: CorpusKey {
                name: corpus.name.clone(),
                canonical_root: PathBuf::from("/tmp"),
            },
            file: FileRef::new(PathBuf::from(format!("/tmp/{}/a.md", corpus.name))),
            mtime: Mtime(0),
        }]
    }

    fn file_count(walk: &Walk) -> Option<usize> {
        match walk {
            Walk::Files(files) => Some(files.len()),
            Walk::ScanFailed(_) | Walk::TaskFailed => None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_requests_for_one_corpus_walk_once() {
        let cache = Arc::new(ScanCache::new(Duration::from_secs(5)));
        let gate = Arc::new(Semaphore::new(6));
        let walks = Arc::new(AtomicUsize::new(0));
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(StdMutex::new(released));
        let scanner: Scanner = {
            let walks = Arc::clone(&walks);
            Arc::new(move |corpus| {
                walks.fetch_add(1, Ordering::SeqCst);
                let _ = released.lock().expect("release lock").recv();
                Ok(one_file(corpus))
            })
        };
        let target = corpus("shared");
        let mut requests = Vec::new();
        for _ in 0..3 {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            let scanner = Arc::clone(&scanner);
            let target = target.clone();
            requests.push(tokio::spawn(async move {
                cache.walk(&target, gate, scanner).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        release.send(()).expect("release the walk");
        for request in requests {
            let walk = request.await.expect("request task").expect("within budget");
            assert_eq!(file_count(&walk), Some(1));
        }
        assert_eq!(walks.load(Ordering::SeqCst), 1);

        release.send(()).expect("release the second walk");
        let again = cache.walk_to_end(&target, gate, scanner).await;
        assert_eq!(file_count(&again), Some(1));
        assert_eq!(
            walks.load(Ordering::SeqCst),
            2,
            "a finished walk must not serve a later request"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_corpus_times_out_without_delaying_another_corpus() {
        let cache = Arc::new(ScanCache::new(Duration::from_millis(200)));
        let gate = Arc::new(Semaphore::new(6));
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(StdMutex::new(released));
        let scanner: Scanner = Arc::new(move |corpus| {
            if corpus.name == "slow" {
                let _ = released.lock().expect("release lock").recv();
            }
            Ok(one_file(corpus))
        });
        let slow = corpus("slow");
        let fast = corpus("fast");

        let (slow_walk, fast_walk) = tokio::join!(
            cache.walk(&slow, Arc::clone(&gate), Arc::clone(&scanner)),
            cache.walk(&fast, Arc::clone(&gate), Arc::clone(&scanner)),
        );

        assert!(slow_walk.is_err(), "the slow walk must hit the budget");
        let fast_walk = fast_walk.expect("the fast corpus stays within budget");
        assert_eq!(file_count(&fast_walk), Some(1));
        release.send(()).expect("release the slow walk");
    }
}
