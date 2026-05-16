use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

use crate::adapters::lance::LanceStore;
use crate::app::config::{self, Config};
use crate::app::input_error::InputError;
use crate::domain::common::{expand_tilde, CorpusConfig};
use crate::domain::corpus::{load_tokenizer, MarkdownChunker};
use crate::domain::embeddings::Embedder;
use crate::domain::indexer::index_corpus;

pub const AD_HOC_CORPUS_NAME: &str = "ad-hoc";
const CHUNK_BUDGET_TOKENS: usize = 384;

#[derive(Debug, Default, Clone)]
pub struct IndexArgs {
    pub corpus: Option<String>,
    pub paths_from: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

pub async fn cmd_index(args: IndexArgs) -> anyhow::Result<()> {
    let report = run_index(args).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Build the `IndexReport` without printing it. Split out so non-CLI
/// transports (e.g. the MCP adapter) can hand the structured report straight
/// to their caller instead of recovering it from stdout.
pub async fn run_index(args: IndexArgs) -> anyhow::Result<IndexReport> {
    let cfg = config::load(args.config.as_deref())?;
    let corpora = select_corpora(&cfg, args.corpus.as_deref(), args.paths_from.as_deref())?;
    let ground_dir = expand_tilde(&cfg.storage.ground_dir);
    ensure_parent(&ground_dir)?;
    let store = LanceStore::open_or_create(&ground_dir, &cfg.embeddings.model)
        .await
        .with_context(|| format!("open ground dir {}", ground_dir.display()))?;
    let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
    let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)
        .with_context(|| format!("init embedder ({})", cfg.embeddings.model))?;
    let tokenizer = load_tokenizer(&cfg.embeddings.model)
        .with_context(|| format!("load tokenizer for {}", cfg.embeddings.model))?;
    let chunker = MarkdownChunker::new(tokenizer, CHUNK_BUDGET_TOKENS);
    run_indexing(&corpora, &store, &mut embedder, &chunker).await
}

/// Resolve which corpora to index for a given request. Shared between the
/// CLI `index` subcommand and the MCP `index` tool so both transports use
/// the same fallback rules and `ad-hoc` naming.
pub fn select_corpora(
    cfg: &Config,
    requested: Option<&str>,
    paths_from: Option<&Path>,
) -> anyhow::Result<Vec<CorpusConfig>> {
    // Caller-input errors (unknown corpus, no corpora at all) construct
    // via `InputError(...)` so the MCP adapter routes them to JSON-RPC
    // `-32602 invalid_params`. See `app::input_error`.
    if let Some(file) = paths_from {
        return Ok(vec![ad_hoc_corpus(file)?]);
    }
    if let Some(name) = requested {
        let hit = cfg
            .corpora
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| InputError::new(format!("corpus {name:?} not found in config")))?;
        return Ok(vec![hit.clone()]);
    }
    if cfg.corpora.is_empty() {
        return Err(InputError::new("no corpora configured; add [[corpus]] to config").into());
    }
    Ok(cfg.corpora.clone())
}

fn ad_hoc_corpus(file: &Path) -> anyhow::Result<CorpusConfig> {
    let text = fs::read_to_string(file)
        .with_context(|| format!("read paths-from file {}", file.display()))?;
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect();
    if paths.is_empty() {
        return Err(InputError::new(format!(
            "paths-from file {} is empty",
            file.display()
        ))
        .into());
    }
    Ok(CorpusConfig {
        name: AD_HOC_CORPUS_NAME.into(),
        paths,
        globs: vec![],
        exclude: vec![],
    })
}

fn ensure_parent(ground_dir: &Path) -> anyhow::Result<()> {
    if let Some(parent) = ground_dir.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create ground dir parent {}", parent.display()))?;
    }
    Ok(())
}

async fn run_indexing(
    corpora: &[CorpusConfig],
    store: &LanceStore,
    embedder: &mut Embedder,
    chunker: &MarkdownChunker<tokenizers::Tokenizer>,
) -> anyhow::Result<IndexReport> {
    let mut report = IndexReport::default();
    for corpus in corpora {
        let stats = index_corpus(corpus, store, embedder, chunker)
            .await
            .with_context(|| format!("index corpus {:?}", corpus.name))?;
        report.corpora.push(CorpusReport {
            name: corpus.name.clone(),
            files_upserted: stats.files_upserted,
            files_touched: stats.files_touched,
            files_deleted: stats.files_deleted,
            files_skipped_empty: stats.files_skipped_empty,
            chunks_inserted: stats.chunks_inserted,
            embeddings_inserted: stats.embeddings_inserted,
        });
    }
    Ok(report)
}

#[derive(Debug, Default, Serialize)]
pub struct IndexReport {
    pub corpora: Vec<CorpusReport>,
}

#[derive(Debug, Serialize)]
pub struct CorpusReport {
    pub name: String,
    pub files_upserted: usize,
    pub files_touched: usize,
    pub files_deleted: usize,
    pub files_skipped_empty: usize,
    pub chunks_inserted: usize,
    pub embeddings_inserted: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_corpora_uses_paths_from_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("paths.txt");
        fs::write(&list, "/some/path\n  /another/path  \n\n").unwrap();
        let cfg = Config::default();
        let out = select_corpora(&cfg, None, Some(list.as_path())).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, AD_HOC_CORPUS_NAME);
        assert_eq!(out[0].paths, vec!["/some/path", "/another/path"]);
    }

    #[test]
    fn select_corpora_filters_by_name_when_corpus_arg_set() {
        let cfg = Config {
            corpora: vec![
                CorpusConfig {
                    name: "docs".into(),
                    paths: vec!["/d".into()],
                    ..Default::default()
                },
                CorpusConfig {
                    name: "notes".into(),
                    paths: vec!["/n".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = select_corpora(&cfg, Some("notes"), None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "notes");
    }

    #[test]
    fn select_corpora_errors_when_named_corpus_missing() {
        let cfg = Config::default();
        let err = select_corpora(&cfg, Some("ghost"), None).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn select_corpora_errors_when_no_corpora_and_no_filters() {
        let cfg = Config::default();
        let err = select_corpora(&cfg, None, None).unwrap_err();
        assert!(err.to_string().contains("no corpora"), "{err}");
    }

    #[test]
    fn select_corpora_returns_all_when_no_filters_set() {
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "alpha".into(),
                paths: vec!["/a".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = select_corpora(&cfg, None, None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "alpha");
    }

    #[test]
    fn ad_hoc_corpus_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("empty.txt");
        fs::write(&list, "\n  \n").unwrap();
        let err = ad_hoc_corpus(&list).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        assert!(
            crate::app::input_error::is_input_error(&err),
            "empty paths-from must mark as InputError so MCP routes it to -32602: {err}"
        );
    }
}
