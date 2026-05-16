use std::path::PathBuf;

use anyhow::{anyhow, Context};

use crate::adapters::lance::LanceStore;
use crate::app::config::{self, Config};
use crate::domain::common::expand_tilde;
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{ground, render, Format, GroundOpts, GroundResponse, RenderOpts};

const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Default, Clone)]
pub struct GroundArgs {
    pub query: String,
    pub corpus: Option<String>,
    pub format: Format,
    pub snippet_chars: Option<usize>,
    pub top_files: Option<usize>,
    pub chunks_per_file: Option<usize>,
    pub limit: Option<usize>,
    pub config: Option<PathBuf>,
}

pub async fn cmd_ground(args: GroundArgs) -> anyhow::Result<()> {
    let format = args.format;
    let snippet_chars = args.snippet_chars;
    let path_prefix_strip = path_prefix_strip(args.config.as_deref(), args.corpus.as_deref())?;
    let response = run_ground(args).await?;
    let opts = RenderOpts {
        snippet_chars,
        path_prefix_strip,
    };
    println!("{}", render(&response, format, &opts));
    Ok(())
}

/// When exactly one corpus is in scope and that corpus has exactly one root
/// path, return that path (with a trailing `/`) so the outline format can
/// strip it for readability. Multi-path or multi-corpus situations return
/// `None` — the full absolute path stays so paths remain unambiguous.
fn path_prefix_strip(
    config: Option<&std::path::Path>,
    requested_corpus: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let cfg = config::load(config)?;
    Ok(resolve_path_prefix_strip(&cfg, requested_corpus))
}

/// Pure inner: pick the corpus the prefix strip applies to, then build the
/// trailing-slash prefix. Split out from the loader so the policy can be
/// unit-tested without a temp config file.
fn resolve_path_prefix_strip(cfg: &Config, requested_corpus: Option<&str>) -> Option<String> {
    let candidate = match requested_corpus {
        Some(name) => cfg.corpora.iter().find(|c| c.name == name),
        None if cfg.corpora.len() == 1 => cfg.corpora.first(),
        _ => None,
    };
    let corpus = candidate?;
    if corpus.paths.len() != 1 {
        return None;
    }
    let expanded = expand_tilde(&corpus.paths[0]);
    let mut prefix = expanded.to_string_lossy().into_owned();
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    Some(prefix)
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
        // Match `select_corpora` on the index path: an explicit --corpus must
        // exist in the config, even when `cfg.corpora` is empty. The previous
        // "is_empty implies trust the user" carve-out diverged from the
        // index-side policy and let `ground --corpus ghost` succeed against
        // an empty config while `index --corpus ghost` errored.
        if !cfg.corpora.iter().any(|c| c.name == name) {
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
    fn pick_corpus_errors_when_explicit_name_missing_from_empty_config() {
        // Symmetry with `select_corpora` on the index path: an explicit
        // `--corpus` must name a configured corpus, even when no corpora are
        // configured at all. The previous "is_empty implies pass-through"
        // policy was the asymmetry the cure pass closed.
        let cfg = Config::default();
        let err = pick_corpus(&cfg, Some("docs")).unwrap_err();
        assert!(
            err.to_string().contains("docs") && err.to_string().contains("not found"),
            "{err}"
        );
    }

    #[test]
    fn pick_corpus_returns_named_corpus_when_present_in_config() {
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
        let got = pick_corpus(&cfg, Some("notes")).unwrap();
        assert_eq!(got, "notes");
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

    #[test]
    fn path_prefix_strip_returns_sole_corpus_path_with_trailing_slash() {
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "only".into(),
                paths: vec!["/abs/cheese".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            resolve_path_prefix_strip(&cfg, None),
            Some("/abs/cheese/".to_string())
        );
    }

    #[test]
    fn path_prefix_strip_preserves_existing_trailing_slash() {
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "only".into(),
                paths: vec!["/abs/cheese/".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            resolve_path_prefix_strip(&cfg, None),
            Some("/abs/cheese/".to_string()),
            "single trailing slash, not two"
        );
    }

    #[test]
    fn path_prefix_strip_returns_none_for_multi_path_corpus() {
        // Ambiguity guard: two roots → no strip, full path stays so the
        // user can tell which root a result came from.
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "only".into(),
                paths: vec!["/a".into(), "/b".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(resolve_path_prefix_strip(&cfg, None), None);
    }

    #[test]
    fn path_prefix_strip_returns_none_for_multi_corpus_without_filter() {
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
        assert_eq!(resolve_path_prefix_strip(&cfg, None), None);
    }

    #[test]
    fn path_prefix_strip_uses_named_corpus_in_multi_corpus_config() {
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
        assert_eq!(
            resolve_path_prefix_strip(&cfg, Some("b")),
            Some("/b/".to_string()),
        );
    }

    #[test]
    fn path_prefix_strip_returns_none_when_named_corpus_missing() {
        // Defensive: pick_corpus will error later for an unknown name, so
        // the strip just bows out instead of panicking.
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "a".into(),
                paths: vec!["/a".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(resolve_path_prefix_strip(&cfg, Some("ghost")), None);
    }

    #[test]
    fn path_prefix_strip_expands_tilde_in_corpus_path() {
        // ~/.cheese is the spec example. The strip must expand to the
        // user's actual home before matching against absolute paths from
        // the indexer.
        let cfg = Config {
            corpora: vec![CorpusConfig {
                name: "cheese".into(),
                paths: vec!["~/.cheese".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let prefix = resolve_path_prefix_strip(&cfg, None).expect("strip resolved");
        assert!(
            !prefix.starts_with('~'),
            "tilde must be expanded: got {prefix:?}"
        );
        assert!(prefix.ends_with("/.cheese/"), "trailing slash: {prefix:?}");
    }
}
