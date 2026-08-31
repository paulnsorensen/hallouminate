//! Provisioning subsystem: the background loop that catches up newly
//! discovered corpus roots (repo-layer overrides, worktree wikis) so a
//! `ground` request against an unseen corpus doesn't block on indexing it
//! inline (mirrors `maintenance.rs`'s background loop).

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use hallouminate_config::Config;
use hallouminate_domain::common::{CorpusConfig, CorpusKey};
use hallouminate_domain::indexer::HandlerRegistry;

use super::state::{CHUNK_BUDGET_TOKENS, DaemonState};

/// One corpus to catch up, paired with the request-resolved config that
/// discovered it (repo-layer overrides included) — `provision_corpus` must
/// resolve resources from this config, not the boot baseline, or a
/// repo-layer `storage.ground_dir`/`embeddings` override indexes into the
/// wrong store.
struct ProvisionRequest {
    corpus: CorpusConfig,
    cfg: Arc<Config>,
}

pub(crate) struct Provisioner {
    seen: std::sync::Mutex<HashSet<CorpusKey>>,
    tx: mpsc::UnboundedSender<ProvisionRequest>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<ProvisionRequest>>>,
}

impl Provisioner {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            seen: std::sync::Mutex::new(HashSet::new()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Pre-seed the seen-set with corpora boot catch-up (`catch_up_index`)
    /// already covers, so `observe` only enqueues request-resolved corpora
    /// (e.g. a git worktree's repo-layer wiki root) that boot never indexed.
    pub(super) fn seed(&self, corpora: &[CorpusConfig]) {
        let mut seen = self
            .seen
            .lock()
            .expect("provisioner seen-set mutex poisoned");
        for corpus in corpora {
            for key in corpus.corpus_keys() {
                seen.insert(key);
            }
        }
    }

    pub(crate) fn observe(&self, corpora: &[CorpusConfig], cfg: &Config) {
        let mut seen = self
            .seen
            .lock()
            .expect("provisioner seen-set mutex poisoned");
        let mut shared_cfg: Option<Arc<Config>> = None;
        for corpus in corpora {
            let mut unseen = false;
            for key in corpus.corpus_keys() {
                if seen.insert(key) {
                    unseen = true;
                }
            }
            if unseen {
                let cfg = shared_cfg.get_or_insert_with(|| Arc::new(cfg.clone()));
                let _ = self.tx.send(ProvisionRequest {
                    corpus: corpus.clone(),
                    cfg: Arc::clone(cfg),
                });
            }
        }
    }

    fn clear_seen(&self, key: &CorpusKey) {
        self.seen
            .lock()
            .expect("provisioner seen-set mutex poisoned")
            .remove(key);
    }
}

pub(super) async fn provisioning_loop(state: DaemonState) {
    let cancel = state.shutdown_token().clone();
    let mut rx_guard = state.provisioner().rx.lock().await;
    let Some(rx) = rx_guard.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        let request = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            recv = rx.recv() => match recv {
                Some(request) => request,
                None => return,
            },
        };
        let ProvisionRequest { corpus, cfg } = request;
        provision_corpus(&state, &corpus, &cfg).await;
        state
            .heartbeat()
            .bump(super::heartbeat::TaskName::Provision);
    }
}

async fn provision_corpus(state: &DaemonState, corpus: &CorpusConfig, cfg: &Config) {
    let _guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                corpus = %corpus.name,
                error = %e,
                "provisioning: could not acquire mutation guard; will retry on next ground",
            );
            clear_seen_keys(state, corpus);
            return;
        }
    };
    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                corpus = %corpus.name,
                error = %e,
                "provisioning: could not resolve resources; will retry on next ground",
            );
            clear_seen_keys(state, corpus);
            return;
        }
    };
    let registry = HandlerRegistry::new(res.tokenizer.clone(), CHUNK_BUDGET_TOKENS);
    match super::dispatch::catch_up_corpus(&res, &registry, corpus).await {
        Ok(Some(stats)) => tracing::info!(
            target: "hallouminate::daemon",
            corpus = %corpus.name,
            files_upserted = stats.files_upserted,
            files_touched = stats.files_touched,
            files_deleted = stats.files_deleted,
            "provisioning: indexed newly discovered corpus",
        ),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                corpus = %corpus.name,
                error = %e,
                "provisioning: pass failed; will retry on next ground",
            );
            clear_seen_keys(state, corpus);
        }
    }
}

fn clear_seen_keys(state: &DaemonState, corpus: &CorpusConfig) {
    for key in corpus.corpus_keys() {
        state.provisioner().clear_seen(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observe_does_not_enqueue_a_corpus_seeded_from_baseline() {
        let corpus = CorpusConfig {
            name: "docs".to_string(),
            paths: vec!["/tmp/hallouminate-test-baseline-docs".to_string()],
            globs: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            global: false,
        };
        let provisioner = Provisioner::new();
        provisioner.seed(std::slice::from_ref(&corpus));

        provisioner.observe(std::slice::from_ref(&corpus), &Config::default());

        let mut rx = provisioner.rx.lock().await.take().expect("receiver");
        assert!(
            rx.try_recv().is_err(),
            "observe must not enqueue a corpus whose key was pre-seeded from baseline",
        );
    }

    #[tokio::test]
    async fn observe_enqueues_a_corpus_whose_key_is_not_in_baseline() {
        let baseline_corpus = CorpusConfig {
            name: "docs".to_string(),
            paths: vec!["/tmp/hallouminate-test-baseline-docs".to_string()],
            globs: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            global: false,
        };
        let unseen_corpus = CorpusConfig {
            name: "repo:worktree:wiki".to_string(),
            paths: vec!["/tmp/hallouminate-test-worktree-wiki".to_string()],
            globs: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            global: false,
        };
        let provisioner = Provisioner::new();
        provisioner.seed(std::slice::from_ref(&baseline_corpus));

        provisioner.observe(std::slice::from_ref(&unseen_corpus), &Config::default());

        let mut rx = provisioner.rx.lock().await.take().expect("receiver");
        let enqueued = rx
            .try_recv()
            .expect("observe must enqueue a corpus whose key was not seeded from baseline");
        assert_eq!(enqueued.corpus.name, "repo:worktree:wiki");
    }
}
