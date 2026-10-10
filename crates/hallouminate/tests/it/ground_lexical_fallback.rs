//! Lexical fallback: ground keeps ripgrep hits for files with no indexed
//! rows in the selected root, ranked after every fused hit.

use std::fs;
use std::path::Path;

use hallouminate_adapters::LanceStore;
use hallouminate_domain::common::{CorpusConfig, Result};
use hallouminate_domain::ground::{GroundOpts, GroundResponse, ground_union};
use hallouminate_domain::indexer::{HandlerRegistry, SearchHit, index_corpus};
use hallouminate_domain::search::Crossencoder;
use text_splitter::Characters;

use crate::common::{LANCE_WRITE_LOCK, StubEmbedder};

const MODEL: &str = "BAAI/bge-small-en-v1.5";

fn corpus_for(name: &str, paths: &[&Path], exclude: &[&str]) -> CorpusConfig {
    CorpusConfig {
        name: name.into(),
        paths: paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        globs: vec!["**/*.md".into()],
        exclude: exclude.iter().map(|pattern| pattern.to_string()).collect(),
        global: false,
    }
}

async fn open_store(dir: &Path) -> LanceStore {
    LanceStore::open_or_create(dir, MODEL, false, true, Some(Box::new(StubEmbedder)))
        .await
        .expect("open store")
}

async fn index(corpus: &CorpusConfig, store: &LanceStore) {
    let registry = HandlerRegistry::new(Characters, 1500);
    index_corpus(corpus, store, &registry)
        .await
        .expect("index_corpus");
}

async fn ground(
    corpora: &[CorpusConfig],
    store: &LanceStore,
    crossencoder: Option<Box<dyn Crossencoder>>,
) -> GroundResponse {
    ground_union(
        "quokkaherd",
        corpora,
        store,
        crossencoder,
        GroundOpts::default(),
        None,
    )
    .await
    .expect("ground_union")
}

fn doc_names(response: &GroundResponse) -> Vec<String> {
    let mut ranked: Vec<(&String, f64)> = response
        .docs
        .iter()
        .map(|(path, doc)| (path, doc.score))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut names = Vec::new();
    for (path, _) in ranked {
        let name = Path::new(path).file_name().expect("file name");
        names.push(name.to_string_lossy().into_owned());
    }
    names
}

fn score_of(response: &GroundResponse, name: &str) -> f64 {
    for (path, doc) in &response.docs {
        if path.ends_with(name) {
            return doc.score;
        }
    }
    panic!("no doc named {name}");
}

fn warning_codes(response: &GroundResponse) -> Vec<&str> {
    response
        .warnings
        .iter()
        .map(|warning| warning.code.as_str())
        .collect()
}

struct ConstantScore;

impl Crossencoder for ConstantScore {
    fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
        for hit in hits {
            hit.score = 10.0;
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_cold_root_returns_line_window() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::write(
        corpus_dir.path().join("notes.md"),
        "# Notes\n\nfirst line\nthe quokkaherd gathers at dawn\nlast line\n",
    )
    .expect("write notes");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;

    let response = ground(&[corpus], &store, None).await;

    assert_eq!(doc_names(&response), vec!["notes.md"]);
    let doc = response.docs.values().next().expect("one doc");
    assert_eq!(doc.chunks.len(), 1);
    assert!(doc.chunks[0].snippet.contains("quokkaherd gathers at dawn"));
    assert_eq!(doc.chunks[0].line_range[0], 1);
    assert_eq!(warning_codes(&response), vec!["lexical-fallback"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_partly_indexed_root_adds_only_unindexed_files() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::write(
        corpus_dir.path().join("indexed.md"),
        "# Indexed\n\nthe quokkaherd is indexed here\n",
    )
    .expect("write indexed");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;
    index(&corpus, &store).await;
    fs::write(
        corpus_dir.path().join("fresh.md"),
        "# Fresh\n\nthe quokkaherd arrived after the index\n",
    )
    .expect("write fresh");

    let response = ground(&[corpus], &store, None).await;

    assert_eq!(
        doc_names(&response),
        vec!["indexed.md", "fresh.md"],
        "indexed hit first, each file once, unindexed file appended"
    );
    assert_eq!(warning_codes(&response), vec!["lexical-fallback"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_fully_indexed_root_runs_no_fallback_pass() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::write(
        corpus_dir.path().join("indexed.md"),
        "# Indexed\n\nthe quokkaherd is indexed here\n",
    )
    .expect("write indexed");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;
    index(&corpus, &store).await;

    let response = ground(&[corpus], &store, None).await;

    assert_eq!(doc_names(&response), vec!["indexed.md"]);
    assert!(
        warning_codes(&response).is_empty(),
        "a covered root leaves no fallback warning: {:?}",
        warning_codes(&response)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_skips_excluded_and_nested_root_files() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    let nested_dir = corpus_dir.path().join("nested");
    fs::create_dir_all(corpus_dir.path().join("drafts")).expect("drafts dir");
    fs::create_dir_all(&nested_dir).expect("nested dir");
    fs::write(
        corpus_dir.path().join("kept.md"),
        "the quokkaherd is kept\n",
    )
    .expect("kept");
    fs::write(
        corpus_dir.path().join("drafts/excluded.md"),
        "the quokkaherd is excluded\n",
    )
    .expect("excluded");
    fs::write(nested_dir.join("inner.md"), "the quokkaherd is nested\n").expect("inner");
    let corpus = corpus_for("docs", &[corpus_dir.path(), &nested_dir], &["drafts/**"]);
    let store = open_store(store_dir.path()).await;

    let response = ground(&[corpus], &store, None).await;

    let mut names = doc_names(&response);
    names.sort();
    assert_eq!(
        names,
        vec!["inner.md", "kept.md"],
        "excluded file never appears; nested file appears once, under its own root"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_stays_out_of_the_rerank_pool() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::write(
        corpus_dir.path().join("indexed.md"),
        "# Indexed\n\nthe quokkaherd is indexed here\n",
    )
    .expect("write indexed");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;
    index(&corpus, &store).await;
    fs::write(
        corpus_dir.path().join("fresh.md"),
        "# Fresh\n\nthe quokkaherd arrived after the index\n",
    )
    .expect("write fresh");

    let response = ground(&[corpus], &store, Some(Box::new(ConstantScore))).await;

    assert_eq!(doc_names(&response), vec!["indexed.md", "fresh.md"]);
    assert!(
        score_of(&response, "indexed.md") > score_of(&response, "fresh.md"),
        "a reranked fused hit must outrank a fallback hit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_ranks_below_fused_hits_of_other_corpora() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let cold_dir = tempfile::tempdir().expect("cold dir");
    let warm_dir = tempfile::tempdir().expect("warm dir");
    fs::write(cold_dir.path().join("cold.md"), "the quokkaherd is cold\n").expect("cold");
    fs::write(warm_dir.path().join("warm.md"), "the quokkaherd is warm\n").expect("warm");
    let cold = corpus_for("cold", &[cold_dir.path()], &[]);
    let warm = corpus_for("warm", &[warm_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;
    index(&warm, &store).await;

    let response = ground(&[cold, warm], &store, None).await;

    assert_eq!(doc_names(&response), vec!["warm.md", "cold.md"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_clears_ripgrep_unresolved_when_it_covers_the_unpooled_file() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::write(
        corpus_dir.path().join("indexed.md"),
        "# Indexed\n\nnothing relevant lives here\n",
    )
    .expect("write indexed");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &[]);
    let store = open_store(store_dir.path()).await;
    index(&corpus, &store).await;
    fs::write(
        corpus_dir.path().join("fresh.md"),
        "# Fresh\n\nthe quokkaherd arrived after the index\n",
    )
    .expect("write fresh");

    let response = ground(&[corpus], &store, None).await;

    assert!(
        doc_names(&response).contains(&"fresh.md".to_string()),
        "the fallback returns the fresh file: {:?}",
        doc_names(&response)
    );
    assert_eq!(
        warning_codes(&response),
        vec!["lexical-fallback"],
        "a covered unpooled file leaves no stale ripgrep-unresolved warning"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_lexical_fallback_keeps_ripgrep_unresolved_for_files_it_cannot_cover() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let store_dir = tempfile::tempdir().expect("store dir");
    let corpus_dir = tempfile::tempdir().expect("corpus dir");
    fs::create_dir_all(corpus_dir.path().join("drafts")).expect("drafts dir");
    fs::write(
        corpus_dir.path().join("indexed.md"),
        "# Indexed\n\nnothing relevant lives here\n",
    )
    .expect("write indexed");
    let corpus = corpus_for("docs", &[corpus_dir.path()], &["drafts/**"]);
    let store = open_store(store_dir.path()).await;
    index(&corpus, &store).await;
    fs::write(
        corpus_dir.path().join("drafts/excluded.md"),
        "the quokkaherd is excluded from the index\n",
    )
    .expect("write excluded");

    let response = ground(&[corpus], &store, None).await;

    assert_eq!(
        warning_codes(&response),
        vec!["ripgrep-unresolved"],
        "the excluded file is not fallback-eligible, so the genuine failure stays"
    );
    assert!(!doc_names(&response).contains(&"excluded.md".to_string()));
}
