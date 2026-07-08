//! Recall@5 / MRR eval harness for `ground` — issue #150 spike.
//!
//! Indexes the frozen wiki fixture at `eval/fixtures/wiki/` (a snapshot of
//! this repo's own `.hallouminate/wiki/`, see `eval/README.md`) and runs the
//! labelled queries in `eval/queries.json` against four retrieval configs:
//! lexical-only, fusion-only (vector+lexical, no rerank), lexical+rerank,
//! and fusion+rerank. Reports Recall@5 and MRR per config, plus a z-score
//! threshold sweep on the fusion+rerank run for the #149 calibration gate.
//!
//! `#[ignore]`d like `cli_ground.rs`'s model-dependent test: first run
//! downloads the crossencoder model (~147MB) and needs network + several
//! seconds per config. Run with:
//!   cargo test --test eval_ground_recall -- --ignored --nocapture

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use hallouminate::app::cli::{GroundArgs, IndexArgs, cmd_index, run_ground};
use hallouminate::app::config::{Config, EmbeddingsConfig, SearchConfig, StorageConfig};
use hallouminate::domain::common::CorpusConfig;
use hallouminate::domain::ground::{DocFile, Format, GroundResponse};
use serde::Deserialize;

mod common;
use common::daemon::DaemonHarness;

const CORPUS_NAME: &str = "eval-wiki";
const REAL_EMBED_CACHE: &str = "~/.cache/hallouminate/fastembed";

#[derive(Debug, Deserialize)]
struct LabelledQuery {
    id: String,
    query: String,
    expected: Vec<String>,
}

fn load_queries() -> Vec<LabelledQuery> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/queries.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text).expect("parse eval/queries.json")
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/fixtures/wiki")
}

/// One retrieval configuration under test.
struct Variant {
    name: &'static str,
    embeddings_enabled: bool,
    crossencoder: Option<&'static str>,
}

const VARIANTS: &[Variant] = &[
    Variant {
        name: "lexical-only (no vector, no rerank)",
        embeddings_enabled: false,
        crossencoder: None,
    },
    Variant {
        name: "fusion-only (vector+lexical+rg, no rerank)",
        embeddings_enabled: true,
        crossencoder: None,
    },
    Variant {
        name: "lexical+rerank (no vector, crossencoder)",
        embeddings_enabled: false,
        crossencoder: Some("jina-reranker-v1-turbo-en"),
    },
    Variant {
        name: "fusion+rerank (vector+lexical+rg, crossencoder)",
        embeddings_enabled: true,
        crossencoder: Some("jina-reranker-v1-turbo-en"),
    },
];

fn build_config(variant: &Variant, ground_dir: &Path) -> Config {
    Config {
        corpora: vec![CorpusConfig {
            name: CORPUS_NAME.into(),
            paths: vec![fixture_root().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            ..Default::default()
        }],
        search: SearchConfig {
            crossencoder: variant.crossencoder.map(|s| s.to_string()),
            ..Default::default()
        },
        embeddings: EmbeddingsConfig {
            enabled: variant.embeddings_enabled,
            model: "BAAI/bge-small-en-v1.5".into(),
            quantized: true,
            cache_dir: REAL_EMBED_CACHE.into(),
            ..Default::default()
        },
        storage: StorageConfig {
            ground_dir: ground_dir.to_string_lossy().into_owned(),
        },
        ..Default::default()
    }
}

/// Docs are keyed by absolute path in score-unordered `BTreeMap` order —
/// re-rank by score descending to get the actual result order the caller
/// would see rendered.
fn ranked_paths(docs: &BTreeMap<String, DocFile>) -> Vec<(String, DocFile)> {
    let mut ranked: Vec<(String, DocFile)> = docs.clone().into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// 1-indexed rank of the first doc whose absolute path ends with one of
/// `expected`'s relative paths, or `None` if absent from the ranked list.
fn rank_of_expected(ranked: &[(String, DocFile)], expected: &[String]) -> Option<usize> {
    ranked
        .iter()
        .position(|(path, _)| expected.iter().any(|rel| path.ends_with(rel.as_str())))
        .map(|idx| idx + 1)
}

struct QueryResult {
    id: String,
    rank: Option<usize>,
    top1_z: Option<f64>,
}

async fn run_variant(variant: &Variant, queries: &[LabelledQuery]) -> Vec<QueryResult> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground_dir = tmp.path().join("ground");
    let cfg = build_config(variant, &ground_dir);

    let harness = DaemonHarness::spawn(cfg.clone()).await;
    let socket = harness.socket().to_path_buf();

    cmd_index(IndexArgs {
        socket: Some(socket.clone()),
        ..Default::default()
    })
    .await
    .unwrap_or_else(|e| panic!("index eval corpus for {}: {e}", variant.name));

    let mut results = Vec::with_capacity(queries.len());
    for q in queries {
        let response: GroundResponse = run_ground(GroundArgs {
            query: q.query.clone(),
            corpus: Some(CORPUS_NAME.into()),
            format: Format::Json,
            top_files: Some(10),
            chunks_per_file: Some(1),
            limit: Some(50),
            socket: Some(socket.clone()),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("ground query {:?} on {}: {e}", q.query, variant.name));

        let ranked = ranked_paths(&response.docs);
        let rank = rank_of_expected(&ranked, &q.expected);
        let top1_z = ranked.first().and_then(|(_, doc)| doc.z_score);
        results.push(QueryResult {
            id: q.id.clone(),
            rank,
            top1_z,
        });
    }
    results
}

fn recall_at_5(results: &[QueryResult]) -> f64 {
    let hits = results
        .iter()
        .filter(|r| r.rank.is_some_and(|r| r <= 5))
        .count();
    hits as f64 / results.len() as f64
}

fn mrr(results: &[QueryResult]) -> f64 {
    let sum: f64 = results
        .iter()
        .map(|r| r.rank.map(|r| 1.0 / r as f64).unwrap_or(0.0))
        .sum();
    sum / results.len() as f64
}

/// z-score threshold sweep over the fusion+rerank run: for each threshold,
/// what fraction of queries would still recall their expected doc at rank 1
/// if results with `top1_z < threshold` were gated out (treated as "no
/// confident match"). Only meaningful when the crossencoder ran — z-score is
/// `None` otherwise (see `bucket::normalize_scores`).
fn z_threshold_sweep(results: &[QueryResult]) {
    let thresholds = [-2.0, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0];
    println!("\n  z-score threshold sweep (fusion+rerank top-1 gate):");
    println!(
        "  {:>9} | {:>14} | {:>16}",
        "threshold", "gate would keep", "top1-correct kept"
    );
    for t in thresholds {
        let kept = results
            .iter()
            .filter(|r| r.top1_z.is_none_or(|z| z >= t))
            .count();
        let correct_kept = results
            .iter()
            .filter(|r| r.rank == Some(1) && r.top1_z.is_none_or(|z| z >= t))
            .count();
        println!("  {t:>9.1} | {kept:>14} | {correct_kept:>16}",);
    }
}

#[tokio::test]
#[ignore = "downloads bge-small (cached) + jina-reranker (~147MB) on first run; needs network"]
async fn ground_recall_and_mrr_across_variants() {
    let queries = load_queries();
    assert!(
        queries.len() >= 15,
        "eval/queries.json must have at least 15 labelled queries, got {}",
        queries.len()
    );

    println!(
        "\n=== ground retrieval eval — {} queries ===",
        queries.len()
    );
    println!("{:<45} | {:>10} | {:>10}", "variant", "recall@5", "mrr");

    let mut rerank_results: Option<Vec<QueryResult>> = None;
    for variant in VARIANTS {
        let results = run_variant(variant, &queries).await;
        let r5 = recall_at_5(&results);
        let m = mrr(&results);
        println!("{:<45} | {r5:>10.3} | {m:>10.3}", variant.name);

        let misses: Vec<&str> = results
            .iter()
            .filter(|r| r.rank.is_none_or(|r| r > 5))
            .map(|r| r.id.as_str())
            .collect();
        if !misses.is_empty() {
            println!("  misses (rank > 5 or absent): {misses:?}");
        }

        if variant.embeddings_enabled && variant.crossencoder.is_some() {
            rerank_results = Some(results);
        }
    }

    if let Some(results) = rerank_results {
        z_threshold_sweep(&results);
    }
}
