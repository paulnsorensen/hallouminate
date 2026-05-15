//! Integration tests for `LanceStore` and `hybrid_search` against a real
//! tempdir-backed LanceDB instance, using a deterministic fake embedder.
//!
//! Covers spec §8.1 #2, #3, #4, #6, #7, #8 from
//! `.cheese/specs/lancedb-rewrite.md`.

use std::path::PathBuf;

use hallouminate::adapters::lance::{chunk_id_for, LanceStore};
use hallouminate::domain::common::FileRef;
use hallouminate::domain::search::hybrid_search;

mod common;
use common::{placeholder_prepared_file, prepared_file_with_chunks, StubEmbedder};

const MODEL: &str = "BAAI/bge-small-en-v1.5";

async fn fresh_store() -> (tempfile::TempDir, LanceStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LanceStore::open_or_create(dir.path(), MODEL)
        .await
        .expect("open LanceStore");
    (dir, store)
}

// ── Spec §8.1 #2: Shrunk-file orphan drop ────────────────────────────────

#[tokio::test]
async fn re_index_with_fewer_chunks_drops_orphaned_ords() {
    let (_dir, store) = fresh_store().await;

    let five = placeholder_prepared_file("/tmp/a.md", 5);
    store.apply_batch(vec![five]).await.expect("apply 5 chunks");
    assert_eq!(store.count_rows().await.unwrap(), 5);

    let three = placeholder_prepared_file("/tmp/a.md", 3);
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
    let (_dir, store) = fresh_store().await;

    let a = placeholder_prepared_file("/tmp/a.md", 3);
    let b = placeholder_prepared_file("/tmp/b.md", 2);
    store.apply_batch(vec![a, b]).await.expect("apply both");
    assert_eq!(store.count_rows().await.unwrap(), 5);

    store
        .delete_file("docs", "/tmp/a.md")
        .await
        .expect("delete /tmp/a.md");
    assert_eq!(
        store.count_rows().await.unwrap(),
        2,
        "only /tmp/b.md should remain"
    );

    let snaps = store.list_files("docs").await.expect("list_files");
    let a_key = FileRef::new(PathBuf::from("/tmp/a.md"));
    let b_key = FileRef::new(PathBuf::from("/tmp/b.md"));
    assert!(!snaps.contains_key(&a_key), "a.md must be gone");
    assert!(snaps.contains_key(&b_key), "b.md must remain");
}

// ── Spec §8.1 #4: Mtime-touch leaves chunks/embeddings alone ─────────────

#[tokio::test]
async fn touch_mtime_updates_only_mtime_column() {
    let (_dir, store) = fresh_store().await;

    let pf = prepared_file_with_chunks(
        "/tmp/touch.md",
        "docs",
        100,
        "hash-v1",
        vec!["text-one", "text-two"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    let before = store.count_rows().await.unwrap();
    assert_eq!(before, 2);

    store
        .touch_mtime("docs", "/tmp/touch.md", 999)
        .await
        .expect("touch_mtime");

    let after = store.count_rows().await.unwrap();
    assert_eq!(after, 2, "touch must not insert or remove rows");

    let snaps = store.list_files("docs").await.expect("list_files");
    let snap = snaps
        .get(&FileRef::new(PathBuf::from("/tmp/touch.md")))
        .expect("snapshot present");
    assert_eq!(snap.mtime_ms, 999, "mtime must have advanced");
    assert_eq!(
        snap.content_hash, "hash-v1",
        "content_hash must be untouched"
    );
}

// ── Spec §8.1 #6: Hybrid search returns results ──────────────────────────

#[tokio::test]
async fn hybrid_search_returns_at_least_one_hit_for_indexed_corpus() {
    let (_dir, store) = fresh_store().await;

    let pf = prepared_file_with_chunks(
        "/tmp/melange.md",
        "docs",
        1,
        "h1",
        vec!["the spice melange flows on Arrakis"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    // Use the stub embedder to compute a query vector deterministically.
    let mut emb = StubEmbedder;
    use hallouminate::domain::embeddings::EmbedBatch;
    let qv = emb
        .embed_batch(&["spice melange".into()])
        .expect("embed query")[0];

    let hits = hybrid_search(&store, "docs", "spice", &qv, 5)
        .await
        .expect("hybrid_search");
    assert!(
        !hits.is_empty(),
        "hybrid search must return hits for indexed corpus"
    );
    assert!(
        hits.iter().any(|h| h.file_ref == "/tmp/melange.md"),
        "result set must include the indexed file"
    );
}

// ── Spec §8.1 #7: Empty corpus → empty hybrid_search result ──────────────

#[tokio::test]
async fn hybrid_search_on_empty_corpus_returns_empty_vec() {
    let (_dir, store) = fresh_store().await;
    let qv = [0.1_f32; hallouminate::adapters::lance::EMBEDDING_DIM];
    let hits = hybrid_search(&store, "docs", "anything", &qv, 5)
        .await
        .expect("empty corpus must yield Ok, not error");
    assert!(hits.is_empty(), "empty corpus must yield zero hits");
}

// ── Spec §8.1 #8: Top hit for single-file corpus is that file ────────────

#[tokio::test]
async fn single_file_corpus_top_hit_is_that_file() {
    let (_dir, store) = fresh_store().await;

    let pf = prepared_file_with_chunks(
        "/tmp/only.md",
        "docs",
        1,
        "h1",
        vec!["unique_token_witness_me on the fury road"],
    );
    store.apply_batch(vec![pf]).await.expect("apply");

    let mut emb = StubEmbedder;
    use hallouminate::domain::embeddings::EmbedBatch;
    let qv = emb
        .embed_batch(&["unique_token_witness_me".into()])
        .expect("embed query")[0];

    let hits = hybrid_search(&store, "docs", "unique_token_witness_me", &qv, 5)
        .await
        .expect("hybrid_search");
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(
        hits[0].file_ref, "/tmp/only.md",
        "top-1 must be the only file in the corpus"
    );
}

// ── Boundary: file_ref containing apostrophes survives SQL escaping ─────

#[tokio::test]
async fn file_ref_with_apostrophes_round_trips_through_apply_and_delete() {
    let (_dir, store) = fresh_store().await;
    let weird = "/tmp/o'brien's notes.md";
    let pf = placeholder_prepared_file(weird, 2);
    store.apply_batch(vec![pf]).await.expect("apply weird name");
    assert_eq!(store.count_rows().await.unwrap(), 2);

    let snaps = store.list_files("docs").await.unwrap();
    assert!(snaps.contains_key(&FileRef::new(PathBuf::from(weird))));

    store
        .touch_mtime("docs", weird, 4242)
        .await
        .expect("touch weird");
    let snaps2 = store.list_files("docs").await.unwrap();
    assert_eq!(snaps2[&FileRef::new(PathBuf::from(weird))].mtime_ms, 4242);

    store
        .delete_file("docs", weird)
        .await
        .expect("delete weird");
    assert_eq!(store.count_rows().await.unwrap(), 0);
}

// ── Boundary: list_files filters by corpus ──────────────────────────────

#[tokio::test]
async fn list_files_returns_only_the_requested_corpus() {
    let (_dir, store) = fresh_store().await;

    let mut a = placeholder_prepared_file("/tmp/a.md", 2);
    a.corpus = "alpha".into();
    let mut b = placeholder_prepared_file("/tmp/b.md", 2);
    b.corpus = "beta".into();
    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    let alpha = store.list_files("alpha").await.unwrap();
    let beta = store.list_files("beta").await.unwrap();

    assert_eq!(alpha.len(), 1, "alpha should see only its own file");
    assert_eq!(beta.len(), 1, "beta should see only its own file");
    assert!(alpha.contains_key(&FileRef::new(PathBuf::from("/tmp/a.md"))));
    assert!(beta.contains_key(&FileRef::new(PathBuf::from("/tmp/b.md"))));
}

// ── Multi-corpus apply_batch rejects mixed-corpus batches ───────────────

#[tokio::test]
async fn apply_batch_rejects_mixed_corpus_batches() {
    let (_dir, store) = fresh_store().await;
    let mut a = placeholder_prepared_file("/tmp/a.md", 1);
    a.corpus = "alpha".into();
    let mut b = placeholder_prepared_file("/tmp/b.md", 1);
    b.corpus = "beta".into();
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
    let (_dir, store) = fresh_store().await;
    let shared = "/tmp/shared.md";

    let mut a = prepared_file_with_chunks("docs", "alpha", 1, "h1", vec!["alpha-only token"]);
    a.file_ref = shared.into();
    let mut b = prepared_file_with_chunks("docs", "beta", 1, "h1", vec!["beta-only token"]);
    b.file_ref = shared.into();

    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    // Two corpora × one chunk each = 2 rows total. If the merge key were
    // chunk_id alone, the second apply would have overwritten the first.
    assert_eq!(store.count_rows().await.unwrap(), 2);

    // Deleting from `alpha` must not touch `beta`'s row.
    store
        .delete_file("alpha", shared)
        .await
        .expect("delete alpha row");
    assert_eq!(store.count_rows().await.unwrap(), 1);
    let beta = store.list_files("beta").await.unwrap();
    assert!(beta.contains_key(&FileRef::new(PathBuf::from(shared))));
}

// ── Multi-corpus isolation: hybrid_search stays inside its corpus ───────

#[tokio::test]
async fn hybrid_search_returns_only_hits_from_requested_corpus() {
    let (_dir, store) = fresh_store().await;

    let mut a = prepared_file_with_chunks(
        "/tmp/alpha.md",
        "alpha",
        1,
        "h1",
        vec!["unique_alpha_marker on the sand"],
    );
    a.corpus = "alpha".into();
    let mut b = prepared_file_with_chunks(
        "/tmp/beta.md",
        "beta",
        1,
        "h1",
        vec!["unique_alpha_marker on the road"],
    );
    b.corpus = "beta".into();
    store.apply_batch(vec![a]).await.expect("apply alpha");
    store.apply_batch(vec![b]).await.expect("apply beta");

    use hallouminate::domain::embeddings::EmbedBatch;
    let mut emb = StubEmbedder;
    let qv = emb
        .embed_batch(&["unique_alpha_marker".into()])
        .expect("embed")[0];

    let hits_alpha = hybrid_search(&store, "alpha", "unique_alpha_marker", &qv, 5)
        .await
        .expect("alpha search");
    let hits_beta = hybrid_search(&store, "beta", "unique_alpha_marker", &qv, 5)
        .await
        .expect("beta search");

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

// ── Bonus: chunk_id determinism end-to-end through apply_batch ──────────

#[tokio::test]
async fn apply_batch_uses_deterministic_chunk_ids_so_reapply_is_idempotent() {
    let (_dir, store) = fresh_store().await;

    let pf = placeholder_prepared_file("/tmp/idem.md", 4);
    store.apply_batch(vec![pf]).await.expect("first apply");
    assert_eq!(store.count_rows().await.unwrap(), 4);

    let pf2 = placeholder_prepared_file("/tmp/idem.md", 4);
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
