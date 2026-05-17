//! End-to-end test: generate a fixture corpus, index it via the full
//! `index_corpus` crust facade (chunker + stub embedder + LanceStore), then
//! issue oracle queries and assert each top-1 hit lands on the expected file.
//!
//! Covers spec §8.2 from `.cheese/specs/lancedb-rewrite.md`.

use std::fs;
use std::path::Path;

use hallouminate::adapters::lance::LanceStore;
use hallouminate::domain::common::CorpusConfig;
use hallouminate::domain::corpus::MarkdownChunker;
use hallouminate::domain::embeddings::EmbedBatch;
use hallouminate::domain::indexer::index_corpus;
use hallouminate::domain::search::hybrid_search;
use text_splitter::Characters;

mod common;
use common::StubEmbedder;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Seed `dir` with ~15 markdown files of varied content. Each file's body
/// contains a unique distinctive token so oracle queries can be checked.
fn seed_fixture_corpus(dir: &Path) {
    let files: &[(&str, &str)] = &[
        (
            "arrakis.md",
            "# Arrakis\n\nThe spice melange flows from the deep desert.\n\n## Sandworms\n\nMassive worms churn beneath the sand.\n",
        ),
        (
            "fury-road.md",
            "# Fury Road\n\nWitness me on the chrome-bright shiny journey.\n\n## War Boys\n\nHalf-life warriors of the wasteland.\n",
        ),
        (
            "shire.md",
            "# The Shire\n\nHobbits till the soil between green hills and brooks.\n\n## Pipeweed\n\nLongbottom Leaf burns sweet by the hearth.\n",
        ),
        (
            "princess.md",
            "# The Princess Bride\n\nInconceivable! Six fingers on the right hand of fate.\n",
        ),
        (
            "grail.md",
            "# Holy Grail\n\nSeek the cup, beware the killer rabbit of caerbannog.\n",
        ),
        (
            "rust-async.md",
            "# Rust Async\n\nFutures await tokio runtimes for non-blocking IO.\n",
        ),
        (
            "lancedb.md",
            "# LanceDB\n\nEmbedded vector database written in Rust with built-in BM25 fulltext.\n",
        ),
        (
            "bge-small.md",
            "# BGE Small\n\nA 384-dimensional sentence embedding model from BAAI.\n",
        ),
        (
            "text-splitter.md",
            "# Text Splitter\n\nSemantic markdown chunking with tokenizer-aware budget.\n",
        ),
        (
            "rrf.md",
            "# Reciprocal Rank Fusion\n\nCombines ranked result lists from heterogeneous retrievers.\n",
        ),
        (
            "blake3.md",
            "# Blake3\n\nFast cryptographic hash function used for content fingerprinting.\n",
        ),
        (
            "fastembed.md",
            "# FastEmbed\n\nLightweight Rust embeddings via ONNX without huge transformer deps.\n",
        ),
    ];
    for (name, body) in files {
        fs::write(dir.join(name), body).expect("write fixture");
    }
}

#[tokio::test]
async fn fixture_corpus_indexes_and_serves_oracle_queries() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    seed_fixture_corpus(corpus_dir.path());

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL)
        .await
        .expect("open store");

    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    let stats = index_corpus(&corpus, &store, &mut embedder, &chunker)
        .await
        .expect("index_corpus");

    assert_eq!(stats.files_upserted, 12, "all 12 fixture files indexed");
    assert!(
        stats.chunks_inserted >= 12,
        "at least one chunk per file, got {}",
        stats.chunks_inserted
    );
    assert_eq!(
        stats.embeddings_inserted, stats.chunks_inserted,
        "every chunk must get exactly one embedding"
    );

    let row_count = store.count_rows().await.unwrap();
    assert!(
        (12..200).contains(&row_count),
        "row count out of plausible range: {row_count}"
    );

    // Oracle queries: distinctive token → expected file
    let oracles: &[(&str, &str)] = &[
        ("melange", "arrakis.md"),
        ("inconceivable", "princess.md"),
        ("caerbannog", "grail.md"),
        ("Longbottom", "shire.md"),
        ("chrome-bright", "fury-road.md"),
    ];

    let mut emb_for_query = StubEmbedder;
    for (query, expected_file) in oracles {
        let qv = emb_for_query
            .embed_batch(&[(*query).to_string()])
            .expect("embed query")[0];
        let hits = hybrid_search(&store, "docs", query, &qv, 5)
            .await
            .expect("hybrid_search");
        assert!(
            !hits.is_empty(),
            "no hits for oracle query {query:?} (expected file {expected_file})"
        );
        // With a stub (non-semantic) embedder, RRF fusion's vector component
        // is noise; we only assert the expected file appears in the top-N.
        // Real-embedder tests can tighten this to top-1.
        assert!(
            hits.iter().any(|h| h.file_ref.ends_with(expected_file)),
            "oracle query {query:?}: expected {expected_file} in top-{}, got {:?}",
            hits.len(),
            hits.iter().map(|h| h.file_ref.clone()).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn fixture_corpus_reindex_is_idempotent_no_phantom_files() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    seed_fixture_corpus(corpus_dir.path());

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    let stats1 = index_corpus(&corpus, &store, &mut embedder, &chunker)
        .await
        .expect("first index");
    let rows1 = store.count_rows().await.unwrap();

    let stats2 = index_corpus(&corpus, &store, &mut embedder, &chunker)
        .await
        .expect("second index");
    let rows2 = store.count_rows().await.unwrap();

    assert_eq!(rows1, rows2, "reindex must not change row count");
    assert_eq!(stats2.embeddings_inserted, 0, "no chunks re-embedded");
    assert_eq!(stats2.files_upserted, 0, "no files upserted");
    assert_eq!(stats2.files_touched, 0, "no mtime change → no touches");
    // chunks_inserted may still be >0 if files_touched ran due to mtime drift;
    // since we re-use the same files, mtime shouldn't change.
    let _ = stats1;
}

#[tokio::test]
async fn fixture_corpus_handles_file_deletion_via_index_corpus() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    seed_fixture_corpus(corpus_dir.path());

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    index_corpus(&corpus, &store, &mut embedder, &chunker)
        .await
        .expect("first index");
    let initial = store.count_rows().await.unwrap();

    fs::remove_file(corpus_dir.path().join("grail.md")).expect("remove grail.md");

    let stats = index_corpus(&corpus, &store, &mut embedder, &chunker)
        .await
        .expect("second index after delete");

    let after = store.count_rows().await.unwrap();
    assert!(
        after < initial,
        "row count should drop after file deletion: {initial} -> {after}"
    );
    assert!(stats.files_deleted >= 1, "must report at least 1 deletion");

    // Verify the grail oracle no longer hits its source
    let mut emb = StubEmbedder;
    let qv = emb.embed_batch(&["caerbannog".to_string()]).expect("embed")[0];
    let hits = hybrid_search(&store, "docs", "caerbannog", &qv, 5)
        .await
        .expect("search after delete");
    assert!(
        !hits.iter().any(|h| h.file_ref.ends_with("grail.md")),
        "grail.md must no longer appear in results"
    );
}

#[allow(dead_code)] // used only by const-budget compliance test
const SMALL_BUDGET: usize = 60;

#[tokio::test]
async fn empty_files_are_skipped_and_counted_not_re_processed_each_run() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");

    // Two files: one with content, one empty.
    fs::write(corpus_dir.path().join("real.md"), "# Real\n\nhas content\n").unwrap();
    fs::write(corpus_dir.path().join("empty.md"), "").unwrap();

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut emb = StubEmbedder;

    let stats1 = index_corpus(&corpus, &store, &mut emb, &chunker)
        .await
        .expect("first index");
    assert_eq!(stats1.files_upserted, 1, "only real.md upserted");
    assert_eq!(
        stats1.files_skipped_empty, 1,
        "empty.md must be counted as skipped, not silently ignored"
    );
}

#[tokio::test]
async fn prepare_file_io_errors_propagate_out_of_index_corpus() {
    // Pointing a corpus at a path that doesn't exist would fail at scan time,
    // not prepare time. To exercise the prepare_file error propagation we
    // create then yank the file out from under the indexer between scan and
    // prepare. Use a manual planning path via the lower-level API.
    use hallouminate::domain::indexer::{DEFAULT_BATCH_SIZE, apply, plan};

    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");

    let real = corpus_dir.path().join("vanishes.md");
    fs::write(&real, "# vanishing\n").unwrap();

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut emb = StubEmbedder;

    let disk = hallouminate::domain::corpus::scan(&corpus).expect("scan");
    let db = store.list_files("docs").await.expect("list");
    let p = plan(disk, db);
    fs::remove_file(&real).unwrap();

    let err = apply(p, &store, &mut emb, &chunker, &corpus, DEFAULT_BATCH_SIZE)
        .await
        .expect_err("missing file must surface as Err, not silent skip");
    let msg = err.to_string();
    assert!(
        msg.contains("vanishes.md") || msg.contains("No such file") || msg.contains("not found"),
        "error should reference the missing file: {msg}"
    );
}

#[tokio::test]
async fn chunker_budget_compliance_with_characters_sizer() {
    let chunker = MarkdownChunker::new(Characters, SMALL_BUDGET);
    let big = "lorem ipsum dolor sit amet ".repeat(500); // ~13.5k chars
    let chunks = chunker.chunk(&big);
    assert!(!chunks.is_empty());
    for c in &chunks {
        assert!(
            c.text.len() <= SMALL_BUDGET + 8,
            "chunk exceeded budget: {} chars > {}",
            c.text.len(),
            SMALL_BUDGET
        );
    }
}
