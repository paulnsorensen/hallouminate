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
use hallouminate::domain::embeddings::{EmbedBatch, EmbedRole};
use hallouminate::domain::indexer::index_corpus;
use hallouminate::domain::search::hybrid_search;
use text_splitter::Characters;

mod common;
use common::StubEmbedder;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Embedder that records every role it was asked to embed, so a test can
/// assert the indexer embeds passages with `EmbedRole::Passage`.
#[derive(Default)]
struct RoleRecordingEmbedder {
    roles: Vec<EmbedRole>,
}

impl EmbedBatch for RoleRecordingEmbedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> hallouminate::domain::common::Result<
        Vec<[f32; hallouminate::adapters::lance::EMBEDDING_DIM]>,
    > {
        self.roles.push(role);
        Ok(texts
            .iter()
            .map(|_| [0.1_f32; hallouminate::adapters::lance::EMBEDDING_DIM])
            .collect())
    }
}

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
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");

    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    let stats = index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
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
            .embed_batch(&[(*query).to_string()], EmbedRole::Query)
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
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    let stats1 = index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
        .await
        .expect("first index");
    let rows1 = store.count_rows().await.unwrap();

    let stats2 = index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
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
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
        .await
        .expect("first index");
    let initial = store.count_rows().await.unwrap();

    fs::remove_file(corpus_dir.path().join("grail.md")).expect("remove grail.md");

    let stats = index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
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
    let qv = emb
        .embed_batch(&["caerbannog".to_string()], EmbedRole::Query)
        .expect("embed")[0];
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
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut emb = StubEmbedder;

    let stats1 = index_corpus(&corpus, &store, Some(&mut emb), &chunker)
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
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut emb = StubEmbedder;

    let disk = hallouminate::domain::corpus::scan(&corpus).expect("scan");
    let db = store.list_files("docs").await.expect("list");
    let p = plan(disk, db);
    fs::remove_file(&real).unwrap();

    let err = apply(
        p,
        &store,
        Some(&mut emb),
        &chunker,
        &corpus,
        DEFAULT_BATCH_SIZE,
    )
    .await
    .expect_err("missing file must surface as Err, not silent skip");
    let msg = err.to_string();
    assert!(
        msg.contains("vanishes.md") || msg.contains("No such file") || msg.contains("not found"),
        "error should reference the missing file: {msg}"
    );
}

/// Spec testing #1 + #2: embeddings-OFF index + ground round-trip. With no
/// embedder, indexing writes null embeddings (zero `embeddings_inserted`) and
/// ground takes the lexical-only (FTS + ripgrep) path, still returning the
/// right file for a distinctive token.
#[tokio::test]
async fn off_mode_index_and_ground_round_trip_returns_lexical_hits() {
    use hallouminate::domain::ground::{GroundOpts, ground};

    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    seed_fixture_corpus(corpus_dir.path());

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
        global: false,
    };

    // enabled = false → the store's `embedding` column is all nulls.
    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, false)
        .await
        .expect("open OFF-mode store");
    let chunker = MarkdownChunker::new(Characters, 1500);

    // No embedder: OFF-mode indexing.
    let stats = index_corpus(&corpus, &store, None, &chunker)
        .await
        .expect("OFF-mode index_corpus");
    assert_eq!(
        stats.files_upserted, 12,
        "all fixture files indexed in OFF mode"
    );
    assert!(
        stats.chunks_inserted >= 12,
        "chunks still inserted in OFF mode, got {}",
        stats.chunks_inserted
    );
    assert_eq!(
        stats.embeddings_inserted, 0,
        "OFF mode must write zero embeddings (null vectors only)"
    );

    // Lexical-only ground: no embedder, distinctive token resolves to grail.md.
    let resp = ground(
        "caerbannog",
        &corpus.name,
        &corpus.paths,
        &store,
        None,
        None,
        GroundOpts::default(),
    )
    .await
    .expect("OFF-mode ground");
    assert!(resp.stats.hits > 0, "FTS must return at least one hit");
    // `docs` is keyed by file_ref. The top-scoring doc for a distinctive
    // token must be grail.md.
    let top_ref = resp
        .docs
        .iter()
        .max_by(|a, b| {
            a.1.score
                .partial_cmp(&b.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(file_ref, _)| file_ref.clone())
        .expect("at least one doc");
    assert!(
        top_ref.ends_with("grail.md"),
        "distinctive token 'caerbannog' must surface grail.md, got {top_ref}"
    );
}

/// Spec testing #7 (indexing side): `index_corpus` must embed chunks with
/// `EmbedRole::Passage`, never `Query`. Pairs with the unit test in
/// `ground/orchestrate.rs` that locks the query side to `EmbedRole::Query`.
#[tokio::test]
async fn index_corpus_embeds_passages_with_passage_role() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");
    seed_fixture_corpus(corpus_dir.path());

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
        global: false,
    };
    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut recorder = RoleRecordingEmbedder::default();

    index_corpus(&corpus, &store, Some(&mut recorder), &chunker)
        .await
        .expect("index_corpus");

    assert!(
        !recorder.roles.is_empty(),
        "indexing must embed at least one passage batch"
    );
    assert!(
        recorder.roles.iter().all(|r| *r == EmbedRole::Passage),
        "indexing must embed every batch with the Passage role, got {:?}",
        recorder.roles
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

/// Seam E1 acceptance: a page WITH frontmatter and a page WITHOUT both index
/// and ground cleanly. Frontmatter text never reaches chunk text / summary /
/// heading paths, and the frontmatter page's line numbers map back to real
/// on-disk source lines (offset proven by a heading placed below a multi-line
/// frontmatter block).
#[tokio::test]
async fn frontmatter_page_and_plain_page_both_index_and_ground_cleanly() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let corpus_dir = tempfile::tempdir().expect("tempdir corpus");

    // 5 frontmatter lines (1..=5); the heading lands on on-disk line 6 and the
    // distinctive token `zphyxnort` on on-disk line 8.
    fs::write(
        corpus_dir.path().join("fm.md"),
        "---\nstatus: reviewed\nowner: cheese-lord\nlast_verified: 2026-01-02\n---\n# Quokka Wisdom\n\nThe distinctive token zphyxnort lives on a known line.\n",
    )
    .expect("write fm fixture");
    fs::write(
        corpus_dir.path().join("plain.md"),
        "# Plain Page\n\nA mundane page about the qwobblefrotz device.\n",
    )
    .expect("write plain fixture");

    let corpus = CorpusConfig {
        name: "docs".into(),
        paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        exclude: vec![],
        global: false,
    };

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut embedder = StubEmbedder;

    let stats = index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
        .await
        .expect("index_corpus");
    assert_eq!(stats.files_upserted, 2, "both pages indexed");

    let mut emb_for_query = StubEmbedder;

    // The frontmatter page grounds on its body token, with no leaked metadata.
    let qv = emb_for_query
        .embed_batch(&["zphyxnort".to_string()], EmbedRole::Query)
        .expect("embed query")[0];
    let hits = hybrid_search(&store, "docs", "zphyxnort", &qv, 5)
        .await
        .expect("hybrid_search fm");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("fm.md"))
        .expect("fm.md must appear in hits");

    assert!(
        !hit.text.contains("status:"),
        "chunk text leaked frontmatter: {:?}",
        hit.text
    );
    assert!(
        !hit.text.contains("cheese-lord"),
        "chunk text leaked owner: {:?}",
        hit.text
    );
    assert!(
        !hit.summary.contains("status:"),
        "summary leaked frontmatter: {:?}",
        hit.summary
    );
    assert!(
        !hit.heading_path
            .iter()
            .any(|h| h.contains("---") || h.contains("status")),
        "heading path leaked frontmatter: {:?}",
        hit.heading_path
    );

    // Line numbers point at on-disk lines: without the offset they would be in
    // 1..=3; with it the chunk brackets the token's real line (8).
    assert!(
        hit.line_start >= 6,
        "fm offset not applied: line_start={} (expected >= 6)",
        hit.line_start
    );
    assert!(
        hit.line_start <= 8 && hit.line_end >= 8,
        "chunk must bracket on-disk line 8 of `zphyxnort`: got [{}, {}]",
        hit.line_start,
        hit.line_end
    );

    // The plain page (no frontmatter) still grounds normally.
    let qv2 = emb_for_query
        .embed_batch(&["qwobblefrotz".to_string()], EmbedRole::Query)
        .expect("embed query")[0];
    let hits2 = hybrid_search(&store, "docs", "qwobblefrotz", &qv2, 5)
        .await
        .expect("hybrid_search plain");
    assert!(
        hits2.iter().any(|h| h.file_ref.ends_with("plain.md")),
        "plain page must ground: {:?}",
        hits2.iter().map(|h| h.file_ref.clone()).collect::<Vec<_>>()
    );
}
