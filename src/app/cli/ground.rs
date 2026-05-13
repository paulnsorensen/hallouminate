use std::path::PathBuf;

use anyhow::{anyhow, Context};

use crate::adapters::lance::LanceStore;
use crate::app::config::{self, Config};
use crate::domain::common::expand_tilde;
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{ground, GroundOpts, GroundResponse};

const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Default, Clone)]
pub struct GroundArgs {
    pub query: String,
    pub corpus: Option<String>,
    pub pretty: bool,
    pub top_files: Option<usize>,
    pub chunks_per_file: Option<usize>,
    pub limit: Option<usize>,
    pub config: Option<PathBuf>,
}

pub async fn cmd_ground(args: GroundArgs) -> anyhow::Result<()> {
    let pretty = args.pretty;
    let response = run_ground(args).await?;
    let text = if pretty {
        serde_json::to_string_pretty(&response)?
    } else {
        serde_json::to_string(&response)?
    };
    println!("{text}");
    Ok(())
}

pub async fn run_ground(args: GroundArgs) -> anyhow::Result<GroundResponse> {
    let cfg = config::load(args.config.as_deref())?;
    let opts = ground_opts(&cfg, &args);
    let ground_dir = expand_tilde(&cfg.storage.ground_dir);
    let store = LanceStore::open_or_create(&ground_dir, &cfg.embeddings.model)
        .await
        .with_context(|| format!("open ground dir {}", ground_dir.display()))?;
    let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
    let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)
        .with_context(|| format!("init embedder ({})", cfg.embeddings.model))?;
    let corpus = pick_corpus(&cfg, args.corpus.as_deref())?;
    Ok(ground(&args.query, &corpus, &store, &mut embedder, opts).await?)
}

fn ground_opts(cfg: &Config, args: &GroundArgs) -> GroundOpts {
    GroundOpts {
        top_files: args.top_files.unwrap_or(cfg.search.top_files_default),
        chunks_per_file: args
            .chunks_per_file
            .unwrap_or(cfg.search.chunks_per_file_default),
        limit: args.limit.unwrap_or(DEFAULT_LIMIT),
    }
}

fn pick_corpus(cfg: &Config, requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(name) = requested {
        if !cfg.corpora.is_empty() && !cfg.corpora.iter().any(|c| c.name == name) {
            return Err(anyhow!("corpus {name:?} not found in config"));
        }
        return Ok(name.to_string());
    }
    match cfg.corpora.len() {
        0 => Err(anyhow!(
            "no corpora configured; pass --corpus or add [[corpus]] to config"
        )),
        1 => Ok(cfg.corpora[0].name.clone()),
        _ => Err(anyhow!(
            "corpus required when multiple corpora configured; pass --corpus"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::common::CorpusConfig;

    #[test]
    fn ground_opts_uses_config_defaults_when_args_unset() {
        let cfg = Config::default();
        let opts = ground_opts(&cfg, &GroundArgs::default());
        assert_eq!(opts.top_files, cfg.search.top_files_default);
        assert_eq!(opts.chunks_per_file, cfg.search.chunks_per_file_default);
        assert_eq!(opts.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn ground_opts_overrides_with_args() {
        let cfg = Config::default();
        let args = GroundArgs {
            top_files: Some(2),
            chunks_per_file: Some(1),
            limit: Some(7),
            ..Default::default()
        };
        let opts = ground_opts(&cfg, &args);
        assert_eq!(opts.top_files, 2);
        assert_eq!(opts.chunks_per_file, 1);
        assert_eq!(opts.limit, 7);
    }

    #[test]
    fn pick_corpus_uses_explicit_flag_when_set() {
        let cfg = Config::default();
        let got = pick_corpus(&cfg, Some("docs")).unwrap();
        assert_eq!(got, "docs");
    }

    #[test]
    fn pick_corpus_uses_sole_configured_corpus() {
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "only".into(),
                paths: vec!["/x".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let got = pick_corpus(&cfg, None).unwrap();
        assert_eq!(got, "only");
    }

    #[test]
    fn pick_corpus_errors_when_no_corpora_and_no_flag() {
        let cfg = Config::default();
        let err = pick_corpus(&cfg, None).unwrap_err();
        assert!(err.to_string().contains("no corpora"), "{err}");
    }

    #[test]
    fn pick_corpus_errors_when_named_corpus_missing_from_config() {
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "docs".into(),
                paths: vec!["/d".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = pick_corpus(&cfg, Some("dox")).unwrap_err();
        assert!(err.to_string().contains("dox"), "{err}");
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn pick_corpus_errors_when_multiple_corpora_and_no_flag() {
        let cfg = Config {
            corpora: vec![
                CorpusConfig {
                    name: "a".into(),
                    paths: vec!["/a".into()],
                    ..Default::default()
                },
                CorpusConfig {
                    name: "b".into(),
                    paths: vec!["/b".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let err = pick_corpus(&cfg, None).unwrap_err();
        assert!(err.to_string().contains("multiple"), "{err}");
    }
}
