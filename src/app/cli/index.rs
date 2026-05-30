use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::app::config::Config;
use crate::app::daemon::{DaemonRequest, DaemonRequestPayload, IndexRequest, client_for};
use crate::app::input_error::InputError;
use crate::domain::common::CorpusConfig;

pub const AD_HOC_CORPUS_NAME: &str = "ad-hoc";

#[derive(Debug, Default, Clone)]
pub struct IndexArgs {
    pub corpus: Option<String>,
    pub paths_from: Option<PathBuf>,
    pub config: Option<PathBuf>,
    /// Optional daemon socket override. Mirrors `HALLOUMINATE_SOCKET` so
    /// test fixtures can pin the socket per-test without env mutation.
    pub socket: Option<PathBuf>,
}

pub async fn cmd_index(args: IndexArgs) -> anyhow::Result<()> {
    let report = run_index(args).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Build the `IndexReport` by routing through the daemon. The CLI no longer
/// opens LanceDB directly — the spec's Approach section names the daemon as
/// the canonical owner of the ground directory; the CLI is one of its
/// clients. When the daemon is unreachable, we fail loudly with the same
/// hint shape the daemon-unavailable test pins.
///
/// `--paths-from` is handled here (instead of forwarded) because the daemon
/// doesn't yet support ad-hoc paths and we want CLI users to keep the same
/// behaviour. The list is read into an in-memory `[[corpus]]`-shaped entry
/// and shipped through `--corpus ad-hoc` after the daemon learns to register
/// transient corpora; until then, surface a clear error early.
pub async fn run_index(args: IndexArgs) -> anyhow::Result<IndexReport> {
    // `--paths-from` short-circuits to a clear "not supported via the daemon
    // yet" error so callers don't see an opaque daemon-side InvalidParams
    // later. Everything else (config layering, corpus name resolution) is
    // the daemon's job — the layered model means the CLI doesn't see repo
    // corpora locally, so any local pre-check would have to re-implement
    // discovery.
    if let Some(paths_from) = args.paths_from.as_deref() {
        return Err(ad_hoc_corpus_unsupported(paths_from));
    }

    let client = client_for(args.socket.as_deref()).await?;
    // Capture CWD at the CLI entry so the daemon can run repo-config
    // discovery against the user's working directory rather than its own
    // (`.cheese/specs/repo-config-discovery.md`, AC #3). `PathBuf::new()`
    // was a seed placeholder; replacing it here is what makes the per-
    // request layered-config path effective end-to-end on the index lane.
    let cwd = std::env::current_dir().context("capture current_dir for daemon request")?;
    let req = DaemonRequest {
        cwd,
        payload: DaemonRequestPayload::Index(IndexRequest {
            corpus: args.corpus.clone(),
            paths_from: None,
        }),
    };
    let report: IndexReport = client.call(req).await?;
    Ok(report)
}

/// Resolve which corpora to index for a given request. Shared between the
/// CLI `index` subcommand and the MCP `index` tool so both transports use
/// the same fallback rules and `ad-hoc` naming.
///
/// Looks up corpora through `cfg.effective_corpora()` so derived
/// `repo:{name}:wiki` / `repo:{name}:corpus` corpora are reachable from
/// `--corpus repo:tern:wiki` and from the no-flag "index everything" path.
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
    let effective = cfg.effective_corpora()?;
    if let Some(name) = requested {
        let hit = effective
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| InputError::new(format!("corpus {name:?} not found in config")))?;
        return Ok(vec![hit.clone()]);
    }
    if effective.is_empty() {
        return Err(InputError::new(
            "no corpora configured; add [[corpus]] or [[repository]] to config",
        )
        .into());
    }
    Ok(effective)
}

fn ad_hoc_corpus(file: &Path) -> anyhow::Result<CorpusConfig> {
    let text = fs::read_to_string(file)
        .map_err(|e| InputError::new(format!("read paths-from file {}: {e}", file.display())))?;
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            paths.push(trimmed.to_string());
        }
    }
    if paths.is_empty() {
        return Err(InputError::new(format!("paths-from file {} is empty", file.display())).into());
    }
    Ok(CorpusConfig {
        name: AD_HOC_CORPUS_NAME.into(),
        paths,
        globs: vec![],
        exclude: vec![],
        global: false,
    })
}

fn ad_hoc_corpus_unsupported(file: &Path) -> anyhow::Error {
    // Surface the same caller-error shape the daemon dispatcher uses (per
    // `tests/daemon.rs::daemon_index_with_paths_from_returns_invalid_params`)
    // so a CLI user sees a clear "not supported yet" instead of an opaque
    // daemon-side InvalidParams later.
    InputError::new(format!(
        "--paths-from is not supported via the daemon yet ({})",
        file.display()
    ))
    .into()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub corpora: Vec<CorpusReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    use crate::domain::repository::RepositoryConfig;

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

    #[test]
    fn select_corpora_resolves_repository_derived_wiki_by_name() {
        // Headline spec finding: `--corpus repo:tern:wiki` must reach the
        // derived wiki corpus instead of erroring with "not found".
        let cfg = Config {
            repositories: vec![RepositoryConfig {
                name: "tern".into(),
                path: "/repos/tern".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = select_corpora(&cfg, Some("repo:tern:wiki"), None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "repo:tern:wiki");
        assert_eq!(
            out[0].paths,
            vec!["/repos/tern/.hallouminate/wiki".to_string()]
        );
    }

    #[test]
    fn select_corpora_includes_repository_derived_corpora_when_no_filters_set() {
        // "index everything" must pick up derived corpora too, so a user
        // who declares a `[[repository]]` and runs `hallouminate index` ends
        // up with their wiki indexed.
        let cfg = Config {
            repositories: vec![RepositoryConfig {
                name: "tern".into(),
                path: "/repos/tern".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = select_corpora(&cfg, None, None).unwrap();
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["repo:tern:wiki"]);
    }

    #[tokio::test]
    async fn run_index_paths_from_returns_input_error_before_dialing_daemon() {
        // `--paths-from` is documented as not yet supported via the daemon
        // (cook flagged it; the daemon dispatcher rejects with InvalidParams).
        // The CLI surfaces the same shape early, as an InputError, so MCP
        // routes it to -32602 — and crucially, the failure must happen
        // BEFORE any socket dial, so the test does not need a daemon.
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("paths.txt");
        fs::write(&list, "/tmp/a\n").unwrap();
        let cfg_path = dir.path().join("config.toml");
        // Empty config: no corpora, no repositories. Doesn't matter because
        // paths_from short-circuits config lookup.
        fs::write(&cfg_path, "").unwrap();
        let err = run_index(IndexArgs {
            paths_from: Some(list),
            config: Some(cfg_path),
            ..Default::default()
        })
        .await
        .expect_err("paths_from must surface a clear error");
        let msg = err.to_string();
        assert!(
            msg.contains("--paths-from") && msg.contains("not supported"),
            "got: {msg}",
        );
        assert!(
            crate::app::input_error::is_input_error(&err),
            "must be an InputError so MCP routes it to -32602: {err}",
        );
    }
}
