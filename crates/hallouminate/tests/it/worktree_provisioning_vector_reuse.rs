//! AC-6 end-to-end test for the worktree index provisioning spec
//! (`.cheese/specs/worktree-index-provisioning.md`): provisioning a fresh
//! root byte-identical to an already-indexed root completes with zero
//! embedder invocations for the identical files, and the provisioned root's
//! corpus is searchable afterward.
//!
//! Runs against the domain crust (`index_corpus` — the same scan/plan/apply
//! pipeline `catch_up_corpus` runs from the provisioning task) and a real
//! `LanceStore`, mirroring `cross_repo_union.rs`'s conventions, so it needs
//! no daemon, socket, or model download.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hallouminate_adapters::{EMBEDDING_DIM, EmbedBatch, EmbedRole, LanceStore};
use hallouminate_domain::common::{CorpusConfig, Result};
use hallouminate_domain::ground::{GroundOpts, ground};
use hallouminate_domain::indexer::{HandlerRegistry, index_corpus};
use text_splitter::Characters;

use crate::common::{LANCE_WRITE_LOCK, StubEmbedder};

const MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Wraps `StubEmbedder`, counting `embed_batch` invocations via a shared
/// counter (mirrors `cross_repo_union.rs`'s `CountingEmbedder`).
struct CountingEmbedder {
    calls: Arc<AtomicUsize>,
}

impl EmbedBatch for CountingEmbedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        StubEmbedder.embed_batch(texts, role)
    }
}

fn seed_root(dir: &Path, body: &str) {
    fs::create_dir_all(dir).expect("mkdir root");
    fs::write(dir.join("a.md"), body).expect("write a.md");
}

fn corpus_for_root(root: &Path) -> CorpusConfig {
    CorpusConfig {
        name: "docs".to_string(),
        paths: vec![root.to_string_lossy().into_owned()],
        globs: vec!["**/*.md".to_string()],
        exclude: Vec::new(),
        global: false,
    }
}

#[tokio::test]
async fn provisioning_a_byte_identical_root_reuses_vectors_with_zero_embedder_calls() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");

    let root_a = parent.path().join("root-a");
    let root_b = parent.path().join("root-b");
    let body = "# Doc\n\nThe distinctive token zphyxnort lives here.\n";
    seed_root(&root_a, body);
    seed_root(&root_b, body);

    let calls = Arc::new(AtomicUsize::new(0));
    let store = LanceStore::open_or_create(
        store_dir.path(),
        MODEL,
        false,
        true,
        Some(Box::new(CountingEmbedder {
            calls: calls.clone(),
        })),
    )
    .await
    .expect("open store");
    let registry = HandlerRegistry::new(Characters, 1500);

    let corpus_a = corpus_for_root(&root_a);
    index_corpus(&corpus_a, &store, &registry)
        .await
        .expect("index root a");
    let calls_after_a = calls.load(Ordering::SeqCst);
    assert!(calls_after_a > 0, "root a must embed on its first index");

    let corpus_b = corpus_for_root(&root_b);
    index_corpus(&corpus_b, &store, &registry)
        .await
        .expect("provision byte-identical root b");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_after_a,
        "provisioning a byte-identical root must invoke the embedder zero times for the identical files"
    );

    let resp = ground(
        "distinctive token",
        &corpus_b,
        &store,
        None,
        GroundOpts::default(),
    )
    .await
    .expect("ground over provisioned root b");
    assert!(
        resp.docs.keys().any(|path| path.ends_with("a.md")),
        "search over the newly provisioned root's corpus must surface a.md, got {:?}",
        resp.docs.keys().collect::<Vec<_>>(),
    );
}
