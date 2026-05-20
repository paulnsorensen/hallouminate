use std::path::PathBuf;

use anyhow::Context;

use crate::app::config::{self, Config};
use crate::app::daemon::{DaemonRequest, DaemonRequestPayload, GroundRequest, GroundResult, client_for};
use crate::domain::common::expand_tilde;
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
    // Build the *effective* (XDG-baseline ⊕ repo-layer) view locally so the
    // cosmetic path-prefix-strip can see repo-layer corpora — without this,
    // a user grounding from inside a repo whose `.hallouminate/config.toml`
    // declares the only corpus gets unstripped absolute paths in the
    // outline format. Discovery / merge errors degrade silently to `None`
    // here: prefix-strip is cosmetic, and the daemon will repeat the same
    // resolution and surface any real error on the dispatch path below.
    let path_prefix_strip = resolve_effective_prefix_strip(&args);
    let response = run_ground(args).await?;
    let opts = RenderOpts {
        snippet_chars,
        path_prefix_strip,
    };
    println!("{}", render(&response, format, &opts));
    Ok(())
}

/// Resolve the cosmetic path-prefix-strip against the *effective* (merged)
/// config the daemon will see. Mirrors `cmd_config_show`'s resolution shape:
/// load XDG baseline, capture the CLI's `current_dir`, run `resolve_for_cwd`.
/// Any error in that chain (no repo config, scalar conflict, discovery
/// failure) degrades to `None` rather than propagating — the strip is
/// purely visual and the real diagnostic surfaces in the daemon roundtrip.
fn resolve_effective_prefix_strip(args: &GroundArgs) -> Option<String> {
    let baseline = config::load_xdg(args.config.as_deref()).ok()?;
    let cwd = std::env::current_dir().ok()?;
    let (effective, _layers) = config::resolve_for_cwd(&baseline, &cwd, None).ok()?;
    resolve_path_prefix_strip(&effective, args.corpus.as_deref())
}

/// When exactly one corpus is in scope and that corpus has exactly one root
/// path, return that path (with a trailing `/`) so the outline format can
/// strip it for readability. Multi-path or multi-corpus situations return
/// `None` — the full absolute path stays so paths remain unambiguous.
///
/// Resolves against `effective_corpora()` so a single configured
/// `[[repository]]` (which derives `repo:{name}:wiki` as its sole corpus)
/// gets the same single-corpus prefix-strip treatment as a single
/// `[[corpus]]` entry would. Returns `None` on a derive failure rather than
/// falling back to `cfg.corpora`: prefix-strip is cosmetic, and real
/// validation surfaces on the daemon roundtrip.
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

/// Route a ground request through the local daemon. Corpus name
/// resolution lives daemon-side now — the layered config model means a
/// repo-only corpus is only visible after `resolve_for_cwd`, which the
/// daemon runs per-request. Any "unknown corpus" caller error surfaces
/// from the daemon's dispatcher as `InvalidParams` instead of being
/// pre-checked here against the XDG-only view.
///
/// Fails loudly when the daemon is unreachable rather than silently
/// falling back to a direct store open — that fallback is what the spec
/// exists to remove.
pub async fn run_ground(args: GroundArgs) -> anyhow::Result<GroundResponse> {
    let client = client_for(args.socket.as_deref()).await?;
    // Capture CWD at the CLI entry so the daemon can run repo-config
    // discovery against the user's working directory rather than its own
    // (`.cheese/specs/repo-config-discovery.md`, AC #3). `PathBuf::new()`
    // was a seed placeholder; replacing it here is what makes the per-
    // request layered-config path effective end-to-end.
    let cwd = std::env::current_dir().context("capture current_dir for daemon request")?;
    let req = DaemonRequest {
        cwd,
        payload: DaemonRequestPayload::Ground(GroundRequest {
            query: args.query,
            corpus: args.corpus,
            top_files: args.top_files,
            chunks_per_file: args.chunks_per_file,
            limit: args.limit.or(Some(DEFAULT_LIMIT)),
            snippet_chars: args.snippet_chars,
        }),
    };
    let result: GroundResult = client.call(req).await?;
    Ok(result.response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::common::CorpusConfig;
    use crate::domain::repository::RepositoryConfig;

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
