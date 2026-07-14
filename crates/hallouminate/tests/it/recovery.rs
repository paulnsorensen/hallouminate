//! Crash-recovery tests. Simulate "process died mid-write" via a panicking
//! embedder inside a spawned tokio task — the panic surfaces as a
//! `JoinError`, and the LanceDB store on disk reflects pre-crash state thanks
//! to per-`merge_insert` atomicity.
//!
//! Covers spec §8.3 from `.cheese/specs/lancedb-rewrite.md`.

use std::fs;

use hallouminate_adapters::{EMBEDDING_DIM, EmbedBatch, EmbedRole, LanceStore};
use hallouminate_domain::common::{CorpusConfig, Result};
use hallouminate_domain::indexer::{ChunkStore, HandlerRegistry, index_corpus};
use text_splitter::Characters;

use crate::common::{LANCE_WRITE_LOCK, StubEmbedder, placeholder_prepared_file};

const MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Embedder that always panics. Used to simulate a crash before the
/// downstream `merge_insert` call.
struct PanickingEmbedder;

impl EmbedBatch for PanickingEmbedder {
    fn embed_batch(
        &mut self,
        _texts: &[String],
        _role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        panic!("simulated mid-apply crash");
    }
}

#[tokio::test]
async fn crash_in_embedder_leaves_store_at_pre_crash_state() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");

    // Phase 1: successfully apply file A
    {
        let store = LanceStore::open_or_create(
            store_dir.path(),
            MODEL,
            false,
            true,
            Some(Box::new(StubEmbedder)),
        )
        .await
        .expect("open initial");
        let pf_a = placeholder_prepared_file("/tmp/a.md", 3);
        store.apply_batch(vec![pf_a]).await.expect("apply A");
        assert_eq!(store.count_rows().await.unwrap(), 3);
        // store dropped at end of scope
    }

    // Phase 2: simulate a crashing apply via panicking embedder
    fs::write(corpus_dir.path().join("b.md"), "# B\n\nbody of b\n").unwrap();
    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
        global: false,
    };

    let store_path = store_dir.path().to_path_buf();
    let corpus_clone = corpus.clone();
    let crashed = tokio::task::spawn(async move {
        let store = LanceStore::open_or_create(
            &store_path,
            MODEL,
            false,
            true,
            Some(Box::new(PanickingEmbedder)),
        )
        .await
        .expect("reopen for crash");
        let registry = HandlerRegistry::new(Characters, 1500);
        let _ = index_corpus(&corpus_clone, &store, &registry).await;
    })
    .await;
    assert!(
        crashed.is_err(),
        "spawned task must have panicked (got Ok(_))"
    );

    // Phase 3: reopen the store; file A still present, no orphan partial writes
    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true, None)
        .await
        .expect("reopen after crash");
    let snaps = store.list_files("docs").await.expect("list_files");
    assert!(
        snaps.iter().any(|s| s.file_ref == "/tmp/a.md"),
        "file A must survive the crash"
    );
    assert_eq!(
        store.count_rows().await.unwrap(),
        3,
        "row count must equal pre-crash state — no partial chunks committed"
    );
}

#[tokio::test]
async fn re_run_after_crash_converges_to_correct_state() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    let file_a = corpus_dir
        .path()
        .canonicalize()
        .expect("canonical corpus dir")
        .join("a.md")
        .to_string_lossy()
        .into_owned();

    // Pre-crash: index file A
    {
        let store = LanceStore::open_or_create(
            store_dir.path(),
            MODEL,
            false,
            true,
            Some(Box::new(StubEmbedder)),
        )
        .await
        .expect("open");
        let pf_a = placeholder_prepared_file(&file_a, 2);
        store.apply_batch(vec![pf_a]).await.expect("apply A");
    }

    // Crash attempting to add B
    fs::write(corpus_dir.path().join("b.md"), "# B\n\nbeta body\n").unwrap();
    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
        global: false,
    };
    let store_path = store_dir.path().to_path_buf();
    let corpus_clone = corpus.clone();
    let crashed = tokio::task::spawn(async move {
        let store = LanceStore::open_or_create(
            &store_path,
            MODEL,
            false,
            true,
            Some(Box::new(PanickingEmbedder)),
        )
        .await
        .expect("reopen");
        let registry = HandlerRegistry::new(Characters, 1500);
        let _ = index_corpus(&corpus_clone, &store, &registry).await;
    })
    .await;
    assert!(crashed.is_err());

    // Recovery: re-run with a healthy embedder; convergence to final state
    let store = LanceStore::open_or_create(
        store_dir.path(),
        MODEL,
        false,
        true,
        Some(Box::new(StubEmbedder)),
    )
    .await
    .expect("recover");
    let registry = HandlerRegistry::new(Characters, 1500);
    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("recovered index_corpus");

    // File A is in DB but not on disk. The scan-based indexer will plan a
    // delete for it. So after recovery:
    //   - file A: deleted (not on disk)
    //   - file B: upserted (on disk, not in DB pre-recovery)
    assert!(stats.files_upserted >= 1, "B must upsert");
    assert!(stats.files_deleted >= 1, "A must be deleted (not on disk)");

    let snaps = store.list_files("docs").await.expect("list_files");
    assert!(snaps.iter().any(|s| s.file_ref.ends_with("b.md")));
    assert!(!snaps.iter().any(|s| s.file_ref == file_a));
}

#[tokio::test]
async fn multiple_independent_apply_batches_are_durable_across_opens() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    // Sanity check: every successful apply_batch must survive a reopen.
    let store_dir = tempfile::tempdir().expect("tempdir store");

    let cycles: &[&str] = &["/tmp/x.md", "/tmp/y.md", "/tmp/z.md"];
    for (i, file_ref) in cycles.iter().enumerate() {
        let store = LanceStore::open_or_create(
            store_dir.path(),
            MODEL,
            false,
            true,
            Some(Box::new(StubEmbedder)),
        )
        .await
        .expect("reopen cycle");
        let n_chunks = i + 1;
        let pf = placeholder_prepared_file(file_ref, n_chunks);
        store.apply_batch(vec![pf]).await.expect("apply cycle");
        // explicit drop via scope end
    }

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true, None)
        .await
        .expect("final reopen");
    let total = store.count_rows().await.unwrap();
    assert_eq!(total, 1 + 2 + 3, "all three batches must have persisted");
    let snaps = store.list_files("docs").await.expect("list");
    assert_eq!(snaps.len(), 3, "all three files snapshotted");
}
