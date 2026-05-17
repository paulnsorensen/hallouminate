use std::path::PathBuf;

use crate::app::config::{self, Config};
use crate::app::daemon::{DaemonRequest, GroundRequest, GroundResult, client_for};
use crate::app::input_error::InputError;
use crate::domain::common::{CorpusConfig, expand_tilde};
use crate::domain::ground::{Format, GroundResponse, RenderOpts, render};

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
    /// Optional daemon socket override. Mirrors `HALLOUMINATE_SOCKET` so
    /// test fixtures can pin the socket per-test without env mutation.
    pub socket: Option<PathBuf>,
}

pub async fn cmd_ground(args: GroundArgs) -> anyhow::Result<()> {
    let format = args.format;
    let snippet_chars = args.snippet_chars;
    // Load config locally only for the cosmetic path-prefix-strip
    // resolution. The daemon owns the actual search.
    let cfg = config::load(args.config.as_deref())?;
    let path_prefix_strip = resolve_path_prefix_strip(&cfg, args.corpus.as_deref());
    let response = run_ground_with_cfg(&cfg, args).await?;
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
///
/// Resolves against `effective_corpora()` so a single configured
/// `[[repository]]` (which derives `repo:{name}:wiki` as its sole corpus)
/// gets the same single-corpus prefix-strip treatment as a single
/// `[[corpus]]` entry would. Falls back to `cfg.corpora` on a derive
/// failure rather than swallowing the error — prefix-strip is cosmetic and
/// real validation surfaces in `pick_corpus` / `config::load`.
fn resolve_path_prefix_strip(cfg: &Config, requested_corpus: Option<&str>) -> Option<String> {
    let effective = cfg.effective_corpora().ok()?;
    let candidate = match requested_corpus {
        Some(name) => effective.iter().find(|c| c.name == name),
        None if effective.len() == 1 => effective.first(),
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
    run_ground_with_cfg(&cfg, args).await
}

/// Inner core that takes a pre-loaded `Config`. Callers that already need
/// to read the config for other reasons (CLI: prefix-strip resolution)
/// reuse the same load instead of paying for a second TOML parse.
///
/// Routes through the local daemon so the LanceDB ground directory is owned
/// by exactly one process (the spec's whole point). Fails loudly when the
/// daemon is unreachable rather than silently falling back to a direct
/// store open — that fallback is what the spec exists to remove.
pub async fn run_ground_with_cfg(cfg: &Config, args: GroundArgs) -> anyhow::Result<GroundResponse> {
    // Local caller-error pre-check: unknown corpus name surfaces as
    // InputError before the daemon dial. Daemon will repeat the same check
    // and answer InvalidParams if we somehow miss; the local pre-check is
    // for a clean error path when there's no daemon to ask either.
    let _ = pick_corpus(cfg, args.corpus.as_deref())?;

    let client = client_for(args.socket.as_deref()).await?;
    let req = DaemonRequest::Ground(GroundRequest {
        query: args.query,
        corpus: args.corpus,
        top_files: args.top_files,
        chunks_per_file: args.chunks_per_file,
        limit: args.limit.or(Some(DEFAULT_LIMIT)),
        snippet_chars: args.snippet_chars,
    });
    let result: GroundResult = client.call(req).await?;
    Ok(result.response)
}

fn pick_corpus(cfg: &Config, requested: Option<&str>) -> anyhow::Result<String> {
    // All errors here are caller-supplied-argument problems: unknown corpus,
    // missing --corpus when ambiguous, no corpora at all. Constructing via
    // `InputError(...)` marks them structurally so the MCP adapter routes
    // them to JSON-RPC `-32602 invalid_params` instead of the default
    // `internal_error`. See `app::input_error`.
    //
    // Resolves against `effective_corpora()` so `repo:{name}:wiki` (derived
    // from `[[repository]]`) is reachable via `--corpus repo:tern:wiki`.
    let effective: Vec<CorpusConfig> = cfg.effective_corpora()?;
    if let Some(name) = requested {
        // Match `select_corpora` on the index path: an explicit --corpus must
        // exist in the (effective) config, even when none are configured.
        if !effective.iter().any(|c| c.name == name) {
            return Err(InputError::new(format!("corpus {name:?} not found in config")).into());
        }
        return Ok(name.to_string());
    }
    match effective.len() {
        0 => Err(InputError::new(
            "no corpora configured; pass --corpus or add [[corpus]] or [[repository]] to config",
        )
        .into()),
        1 => Ok(effective[0].name.clone()),
        _ => Err(InputError::new(
            "corpus required when multiple corpora configured; pass --corpus",
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::common::CorpusConfig;
    use crate::domain::repository::RepositoryConfig;

    #[test]
    fn pick_corpus_errors_when_explicit_name_missing_from_empty_config() {
        // Symmetry with `select_corpora` on the index path: an explicit
        // `--corpus` must name a configured corpus, even when no corpora are
        // configured at all.
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
        // Hint must point at both canonical keys so a user with a
        // repository declared somewhere obvious doesn't go adding a
        // redundant [[corpus]].
        assert!(err.to_string().contains("[[corpus]]"), "{err}");
        assert!(err.to_string().contains("[[repository]]"), "{err}");
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
    fn pick_corpus_resolves_repository_derived_wiki_corpus() {
        // The spec's headline reachability gate: a user with only a
        // `[[repository]]` declaration must be able to ground against
        // `repo:{name}:wiki` from the CLI without a [[corpus]] workaround.
        let cfg = Config {
            repositories: vec![RepositoryConfig {
                name: "tern".into(),
                path: "/repos/tern".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let got = pick_corpus(&cfg, Some("repo:tern:wiki")).unwrap();
        assert_eq!(got, "repo:tern:wiki");
    }

    #[test]
    fn pick_corpus_defaults_to_single_repository_wiki_when_unspecified() {
        // Same shape as `pick_corpus_uses_sole_configured_corpus`, but the
        // sole corpus is repository-derived. Closes the second leg of the
        // reachability fix: callers don't have to know the derived name to
        // reach the only configured corpus.
        let cfg = Config {
            repositories: vec![RepositoryConfig {
                name: "tern".into(),
                path: "/repos/tern".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let got = pick_corpus(&cfg, None).unwrap();
        assert_eq!(got, "repo:tern:wiki");
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

    #[test]
    fn path_prefix_strip_resolves_against_single_repository_wiki() {
        // A repository-only config derives exactly one corpus
        // (`repo:{name}:wiki`) with a single path under
        // `<repo>/.hallouminate/wiki`. The strip must use that derived path.
        let cfg = Config {
            repositories: vec![RepositoryConfig {
                name: "tern".into(),
                path: "/repos/tern".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            resolve_path_prefix_strip(&cfg, None),
            Some("/repos/tern/.hallouminate/wiki/".to_string())
        );
    }
}
