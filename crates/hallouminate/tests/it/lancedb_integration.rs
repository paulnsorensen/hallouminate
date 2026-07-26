//! Integration tests for `LanceStore` and `retrieve_signals` against a real
//! tempdir-backed LanceDB instance, using a deterministic fake embedder.
//!
//! Covers spec §8.1 #2, #3, #4, #6, #7, #8 from
//! `.cheese/specs/lancedb-rewrite.md`.

use crate::common::{
    LANCE_WRITE_LOCK, StubEmbedder, placeholder_prepared_file, prepared_file_with_chunks,
};
use hallouminate_adapters::{LanceStore, chunk_id_for};
use hallouminate_domain::common::CorpusKey;
use hallouminate_domain::indexer::ChunkStore;
use hallouminate_domain::search::search_fused;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

fn corpus_key(name: &str) -> CorpusKey {
    CorpusKey::from_configured_root(name, "/tmp")
}

async fn fresh_store() -> (tempfile::TempDir, LanceStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        LanceStore::open_or_create(dir.path(), MODEL, false, true, Some(Box::new(StubEmbedder)))
            .await
            .expect("open LanceStore");
    (dir, store)
}

// ── Spec §8.1 #2: Shrunk-file orphan drop ────────────────────────────────

#[tokio::test]
async fn re_index_with_fewer_chunks_drops_orphaned_ords() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let five = placeholder_prepared_file("/tmp/a.md", &corpus_key, 5);
    store.apply_batch(vec![five]).await.expect("apply 5 chunks");
    assert_eq!(store.count_rows().await.unwrap(), 5);

    let three = placeholder_prepared_file("/tmp/a.md", &corpus_key, 3);
    store
        .apply_batch(vec![three])
        .await
        .expect("apply 3 chunks");
    assert_eq!(
        store.count_rows().await.unwrap(),
        3,
        "shrunk file must orphan-drop ords 3..5"
    );
}

// ── Spec §8.1 #3: Atomic delete-by-file_ref ──────────────────────────────

#[tokio::test]
async fn delete_file_removes_all_chunks_for_that_file_only() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let a = placeholder_prepared_file("/tmp/a.md", &corpus_key, 3);
    let b = placeholder_prepared_file("/tmp/b.md", &corpus_key, 2);
    store.apply_batch(vec![a, b]).await.expect("apply both");
    assert_eq!(store.count_rows().await.unwrap(), 5);

    store
        .delete_file(&corpus_key, "/tmp/a.md")
        .await
        .expect("delete /tmp/a.md");
    assert_eq!(
        store.count_rows().await.unwrap(),
        2,
        "only /tmp/b.md should remain"
    );

    let snaps = store.list_files(&corpus_key).await.expect("list_files");
    assert!(
        !snaps.iter().any(|s| s.file_ref == "/tmp/a.md"),
        "a.md must be gone"
    );
    assert!(
        snaps.iter().any(|s| s.file_ref == "/tmp/b.md"),
        "b.md must remain"
    );
}

// ── fts_search: BM25-only sibling, corpus-scoped ─────────────────────────

#[tokio::test]
async fn fts_search_returns_lexical_hits_scoped_to_one_corpus() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let alpha = corpus_key("alpha");
    let beta = corpus_key("beta");

    // Same distinctive token in two corpora; apply_batch requires one corpus
    // per call, so seed them separately.
    let a = prepared_file_with_chunks(
        "/tmp/a.md",
        &alpha,
        100,
        "ha",
        vec!["zebrafish distinctive marker token"],
    );
    let b = prepared_file_with_chunks(
        "/tmp/b.md",
        &beta,
        100,
        "hb",
        vec!["zebrafish distinctive marker token"],
    );
    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    let signals = store
        .retrieve_signals(&alpha, "zebrafish", 10)
        .await
        .expect("retrieve_signals");
    assert!(
        !signals.fts.is_empty(),
        "must find the token in corpus alpha"
    );
    let file_refs: Vec<&String> = signals
        .fts
        .iter()
        .map(|id| &signals.hits.get(id).expect("hit for ranked chunk").file_ref)
        .collect();
    assert!(
        file_refs.iter().all(|f| f.as_str() == "/tmp/a.md"),
        "fts_search must not leak rows from corpus beta: {:?}",
        file_refs
    );
}

#[tokio::test]
async fn fts_search_on_empty_store_returns_no_hits() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");
    let signals = store
        .retrieve_signals(&corpus_key, "anything", 10)
        .await
        .expect("retrieve_signals on empty store");
    assert!(
        signals.fts.is_empty(),
        "empty store must yield zero FTS hits"
    );
    assert!(signals.hits.is_empty(), "empty store must yield zero hits");
}

// ── Spec §8.1 #4: Mtime-touch leaves chunks/embeddings alone ─────────────

#[tokio::test]
async fn touch_mtime_updates_only_mtime_column() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let pf = prepared_file_with_chunks(
        "/tmp/touch.md",
        &corpus_key,
        100,
        "hash-v1",
        vec!["text-one", "text-two"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    let before = store.count_rows().await.unwrap();
    assert_eq!(before, 2);

    store
        .touch_mtime(&corpus_key, "/tmp/touch.md", 999)
        .await
        .expect("touch_mtime");

    let after = store.count_rows().await.unwrap();
    assert_eq!(after, 2, "touch must not insert or remove rows");

    let snaps = store.list_files(&corpus_key).await.expect("list_files");
    let snap = snaps
        .iter()
        .find(|s| s.file_ref == "/tmp/touch.md")
        .expect("snapshot present");
    assert_eq!(snap.mtime_ms, 999, "mtime must have advanced");
    assert_eq!(
        snap.content_hash, "hash-v1",
        "content_hash must be untouched"
    );
}

// ── Spec §8.1 #6: Hybrid search returns results ──────────────────────────

#[tokio::test]
async fn retrieve_signals_returns_at_least_one_hit_for_indexed_corpus() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let pf = prepared_file_with_chunks(
        "/tmp/melange.md",
        &corpus_key,
        1,
        "h1",
        vec!["the spice melange flows on Arrakis"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    // Use the stub embedder to compute a query vector deterministically.
    let hits = search_fused(&store, &corpus_key, "spice", 5)
        .await
        .expect("retrieve_signals")
        .hits;
    assert!(
        !hits.is_empty(),
        "hybrid search must return hits for indexed corpus"
    );
    assert!(
        hits.iter().any(|h| h.file_ref == "/tmp/melange.md"),
        "result set must include the indexed file"
    );
}

// ── Spec §8.1 #7: Empty corpus → empty signal lists ───────────────────────

#[tokio::test]
async fn retrieve_signals_on_empty_corpus_returns_empty_signals() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");
    let hits = search_fused(&store, &corpus_key, "anything", 5)
        .await
        .expect("empty corpus must yield Ok, not error")
        .hits;
    assert!(hits.is_empty(), "empty corpus must yield zero hits");
}

// ── Spec §8.1 #8: Top hit for single-file corpus is that file ────────────

#[tokio::test]
async fn single_file_corpus_top_hit_is_that_file() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let pf = prepared_file_with_chunks(
        "/tmp/only.md",
        &corpus_key,
        1,
        "h1",
        vec!["unique_token_witness_me on the fury road"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    let hits = search_fused(&store, &corpus_key, "unique_token_witness_me", 5)
        .await
        .expect("retrieve_signals")
        .hits;
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(
        hits[0].file_ref, "/tmp/only.md",
        "top-1 must be the only file in the corpus"
    );
}

// ── Boundary: file_ref containing apostrophes survives SQL escaping ─────

#[tokio::test]
async fn file_ref_with_apostrophes_round_trips_through_apply_and_delete() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");
    let weird = "/tmp/o'brien's notes.md";
    let pf = placeholder_prepared_file(weird, &corpus_key, 2);
    store.apply_batch(vec![pf]).await.expect("apply weird name");
    assert_eq!(store.count_rows().await.unwrap(), 2);

    let snaps = store.list_files(&corpus_key).await.unwrap();
    assert!(snaps.iter().any(|s| s.file_ref == weird));

    store
        .touch_mtime(&corpus_key, weird, 4242)
        .await
        .expect("touch weird");
    let snaps2 = store.list_files(&corpus_key).await.unwrap();
    assert_eq!(
        snaps2
            .iter()
            .find(|s| s.file_ref == weird)
            .unwrap()
            .mtime_ms,
        4242
    );

    store
        .delete_file(&corpus_key, weird)
        .await
        .expect("delete weird");
    assert_eq!(store.count_rows().await.unwrap(), 0);
}

// ── Boundary: list_files filters by corpus ──────────────────────────────

#[tokio::test]
async fn list_files_returns_only_the_requested_corpus() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let alpha_key = corpus_key("alpha");
    let beta_key = corpus_key("beta");

    let a = placeholder_prepared_file("/tmp/a.md", &alpha_key, 2);
    let b = placeholder_prepared_file("/tmp/b.md", &beta_key, 2);
    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    let alpha = store.list_files(&alpha_key).await.unwrap();
    let beta = store.list_files(&beta_key).await.unwrap();

    assert_eq!(alpha.len(), 1, "alpha should see only its own file");
    assert_eq!(beta.len(), 1, "beta should see only its own file");
    assert!(alpha.iter().any(|s| s.file_ref == "/tmp/a.md"));
    assert!(beta.iter().any(|s| s.file_ref == "/tmp/b.md"));
}

// ── Multi-corpus apply_batch rejects mixed-corpus batches ───────────────

#[tokio::test]
async fn apply_batch_rejects_mixed_corpus_batches() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let alpha = corpus_key("alpha");
    let beta = corpus_key("beta");
    let a = placeholder_prepared_file("/tmp/a.md", &alpha, 1);
    let b = placeholder_prepared_file("/tmp/b.md", &beta, 1);
    let err = store
        .apply_batch(vec![a, b])
        .await
        .expect_err("mixed corpus batch must error");
    assert!(
        err.to_string().contains("same corpus"),
        "error should explain single-corpus invariant: {err}"
    );
}

// ── Multi-corpus isolation: shared file_ref keeps independent rows ──────

#[tokio::test]
async fn same_file_ref_in_two_corpora_keeps_independent_rows() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let alpha = corpus_key("alpha");
    let beta = corpus_key("beta");
    let shared = "/tmp/shared.md";

    let a = prepared_file_with_chunks(shared, &alpha, 1, "h1", vec!["alpha-only token"]);
    let b = prepared_file_with_chunks(shared, &beta, 1, "h1", vec!["beta-only token"]);

    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    // Two corpora × one chunk each = 2 rows total. If the merge key were
    // chunk_id alone, the second apply would have overwritten the first.
    assert_eq!(store.count_rows().await.unwrap(), 2);

    // Deleting from `alpha` must not touch `beta`'s row.
    store
        .delete_file(&alpha, shared)
        .await
        .expect("delete alpha row");
    assert_eq!(store.count_rows().await.unwrap(), 1);
    let beta = store.list_files(&beta).await.unwrap();
    assert!(beta.iter().any(|s| s.file_ref == shared));
}

// ── Multi-corpus isolation: retrieve_signals stays inside its corpus ────

#[tokio::test]
async fn retrieve_signals_returns_only_hits_from_requested_corpus() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let alpha = corpus_key("alpha");
    let beta = corpus_key("beta");

    let a = prepared_file_with_chunks(
        "/tmp/alpha.md",
        &alpha,
        1,
        "h1",
        vec!["unique_alpha_marker on the sand"],
    );
    let b = prepared_file_with_chunks(
        "/tmp/beta.md",
        &beta,
        1,
        "h1",
        vec!["unique_alpha_marker on the road"],
    );
    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    let hits_alpha = search_fused(&store, &alpha, "unique_alpha_marker", 5)
        .await
        .expect("alpha search")
        .hits;
    let hits_beta = search_fused(&store, &beta, "unique_alpha_marker", 5)
        .await
        .expect("beta search")
        .hits;

    assert!(
        hits_alpha.iter().all(|h| h.file_ref == "/tmp/alpha.md"),
        "alpha search leaked cross-corpus: {:?}",
        hits_alpha.iter().map(|h| &h.file_ref).collect::<Vec<_>>()
    );
    assert!(
        hits_beta.iter().all(|h| h.file_ref == "/tmp/beta.md"),
        "beta search leaked cross-corpus: {:?}",
        hits_beta.iter().map(|h| &h.file_ref).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn same_name_roots_are_isolated_through_public_lance_api() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let root_a = tempfile::tempdir().expect("root a");
    let root_b = tempfile::tempdir().expect("root b");
    let key_a =
        CorpusKey::from_configured_root("docs", root_a.path().to_str().expect("UTF-8 root a"));
    let key_b =
        CorpusKey::from_configured_root("docs", root_b.path().to_str().expect("UTF-8 root b"));
    let file_ref = "/virtual/shared/page.md";

    let a = prepared_file_with_chunks(
        file_ref,
        &key_a,
        10,
        "a-v1",
        vec!["sharedquery root-a-one", "sharedquery root-a-two"],
    );
    let b = prepared_file_with_chunks(
        file_ref,
        &key_b,
        20,
        "b-v1",
        vec!["sharedquery root-b-one", "sharedquery root-b-two"],
    );
    store.apply_batch(vec![a]).await.expect("apply root a");
    store.apply_batch(vec![b]).await.expect("apply root b");

    let files_a = store.list_files(&key_a).await.expect("list root a");
    let files_b = store.list_files(&key_b).await.expect("list root b");
    assert_eq!(files_a.len(), 1);
    assert_eq!(files_b.len(), 1);
    assert_eq!(files_a[0].corpus_key, key_a);
    assert_eq!(files_a[0].file_ref, file_ref);
    assert_eq!(files_b[0].corpus_key, key_b);
    assert_eq!(files_b[0].file_ref, file_ref);

    let signals_a = store
        .retrieve_signals(&key_a, "sharedquery", 10)
        .await
        .expect("search root a");
    let signals_b = store
        .retrieve_signals(&key_b, "sharedquery", 10)
        .await
        .expect("search root b");
    assert_eq!(signals_a.hits.len(), 2);
    for hit in signals_a.hits.values() {
        assert_eq!(hit.corpus_key, key_a);
        assert_eq!(hit.file_ref, file_ref);
        assert!(hit.text.starts_with("sharedquery root-a-"));
    }
    assert_eq!(signals_b.hits.len(), 2);
    for hit in signals_b.hits.values() {
        assert_eq!(hit.corpus_key, key_b);
        assert_eq!(hit.file_ref, file_ref);
        assert!(hit.text.starts_with("sharedquery root-b-"));
    }

    store
        .touch_mtime(&key_a, file_ref, 99)
        .await
        .expect("touch root a");
    assert_eq!(store.list_files(&key_a).await.unwrap()[0].mtime_ms, 99);
    let files_b = store
        .list_files(&key_b)
        .await
        .expect("list root b after touch");
    assert_eq!(files_b.len(), 1);
    assert_eq!(files_b[0].corpus_key, key_b);
    assert_eq!(files_b[0].file_ref, file_ref);
    assert_eq!(files_b[0].mtime_ms, 20);
    assert_eq!(files_b[0].content_hash, "b-v1");
    let signals_b = store
        .retrieve_signals(&key_b, "sharedquery", 10)
        .await
        .expect("search root b after touch");
    assert_eq!(signals_b.hits.len(), 2);
    for hit in signals_b.hits.values() {
        assert_eq!(hit.corpus_key, key_b);
        assert_eq!(hit.file_ref, file_ref);
        assert!(hit.text.starts_with("sharedquery root-b-"));
    }
    let stats_a = store
        .corpus_chunk_stats(&key_a)
        .await
        .expect("stats root a after touch");
    let stats_b = store
        .corpus_chunk_stats(&key_b)
        .await
        .expect("stats root b after touch");
    assert_eq!((stats_a.indexed_files, stats_a.total_chunks), (1, 2));
    assert_eq!((stats_b.indexed_files, stats_b.total_chunks), (1, 2));

    let replacement = prepared_file_with_chunks(
        file_ref,
        &key_a,
        100,
        "a-v2",
        vec!["sharedquery root-a-replacement"],
    );
    store
        .apply_batch(vec![replacement])
        .await
        .expect("replace root a");
    let files_a = store
        .list_files(&key_a)
        .await
        .expect("list root a after replacement");
    assert_eq!(files_a.len(), 1);
    assert_eq!(files_a[0].corpus_key, key_a);
    assert_eq!(files_a[0].file_ref, file_ref);
    assert_eq!(files_a[0].content_hash, "a-v2");
    let signals_a = store
        .retrieve_signals(&key_a, "sharedquery", 10)
        .await
        .expect("search root a after replacement");
    assert_eq!(signals_a.hits.len(), 1);
    let hit_a = signals_a.hits.values().next().expect("single hit");
    assert_eq!(hit_a.corpus_key, key_a);
    assert_eq!(hit_a.file_ref, file_ref);
    assert_eq!(hit_a.text, "sharedquery root-a-replacement");
    let stats_a = store
        .corpus_chunk_stats(&key_a)
        .await
        .expect("stats root a after replacement");
    assert_eq!((stats_a.indexed_files, stats_a.total_chunks), (1, 1));

    let files_b = store
        .list_files(&key_b)
        .await
        .expect("list root b after replacement");
    assert_eq!(files_b.len(), 1);
    assert_eq!(files_b[0].corpus_key, key_b);
    assert_eq!(files_b[0].file_ref, file_ref);
    assert_eq!(files_b[0].mtime_ms, 20);
    assert_eq!(files_b[0].content_hash, "b-v1");
    let signals_b = store
        .retrieve_signals(&key_b, "sharedquery", 10)
        .await
        .expect("search root b after replacement");
    assert_eq!(signals_b.hits.len(), 2);
    for hit in signals_b.hits.values() {
        assert_eq!(hit.corpus_key, key_b);
        assert_eq!(hit.file_ref, file_ref);
        assert!(hit.text.starts_with("sharedquery root-b-"));
    }
    let stats_b = store
        .corpus_chunk_stats(&key_b)
        .await
        .expect("stats root b after replacement");
    assert_eq!((stats_b.indexed_files, stats_b.total_chunks), (1, 2));

    store
        .delete_file(&key_a, file_ref)
        .await
        .expect("delete root a");
    assert!(store.list_files(&key_a).await.unwrap().is_empty());
    let signals_a = store
        .retrieve_signals(&key_a, "sharedquery", 10)
        .await
        .expect("search root a after deletion");
    assert!(signals_a.fts.is_empty());
    assert!(signals_a.hits.is_empty());
    let stats_a = store
        .corpus_chunk_stats(&key_a)
        .await
        .expect("stats root a after deletion");
    assert_eq!((stats_a.indexed_files, stats_a.total_chunks), (0, 0));

    let files_b = store
        .list_files(&key_b)
        .await
        .expect("list root b after deletion");
    assert_eq!(files_b.len(), 1);
    assert_eq!(files_b[0].corpus_key, key_b);
    assert_eq!(files_b[0].file_ref, file_ref);
    assert_eq!(files_b[0].mtime_ms, 20);
    assert_eq!(files_b[0].content_hash, "b-v1");
    let signals_b = store
        .retrieve_signals(&key_b, "sharedquery", 10)
        .await
        .expect("search root b after deletion");
    assert_eq!(signals_b.hits.len(), 2);
    for hit in signals_b.hits.values() {
        assert_eq!(hit.corpus_key, key_b);
        assert_eq!(hit.file_ref, file_ref);
        assert!(hit.text.starts_with("sharedquery root-b-"));
    }
    let stats_b = store
        .corpus_chunk_stats(&key_b)
        .await
        .expect("stats root b after deletion");
    assert_eq!((stats_b.indexed_files, stats_b.total_chunks), (1, 2));
}

// ── Bonus: chunk_id determinism end-to-end through apply_batch ──────────

#[tokio::test]
async fn apply_batch_uses_deterministic_chunk_ids_so_reapply_is_idempotent() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let (_dir, store) = fresh_store().await;
    let corpus_key = corpus_key("docs");

    let pf = placeholder_prepared_file("/tmp/idem.md", &corpus_key, 4);
    store.apply_batch(vec![pf]).await.expect("first apply");
    assert_eq!(store.count_rows().await.unwrap(), 4);

    let pf2 = placeholder_prepared_file("/tmp/idem.md", &corpus_key, 4);
    store
        .apply_batch(vec![pf2])
        .await
        .expect("idempotent reapply");
    assert_eq!(
        store.count_rows().await.unwrap(),
        4,
        "reapplying identical content must not duplicate rows"
    );

    // chunk_ids are derived from (file_ref, ord) so 0..4 are the same ids
    let _ = chunk_id_for("/tmp/idem.md", 0);
}
