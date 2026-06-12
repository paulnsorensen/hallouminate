//! `hallouminate init-repo <name>` — seed a repository as a hallouminate
//! tenant: a repo-layer `.hallouminate/config.toml` declaring the
//! `[[repository]]` plus a `.hallouminate/wiki/` skeleton. Harness-agnostic
//! by design: every install path (Claude, Codex, opencode, manual) delegates
//! repo seeding here so the result is identical everywhere.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, anyhow};

use crate::domain::corpus::index_md::{INDEX_END_MARKER, INDEX_START_MARKER};
use crate::domain::repository::{RepoCorpusKind, WIKI_RELATIVE_PATH, repo_corpus_name};

#[derive(Debug)]
pub struct InitRepoArgs {
    /// `[[repository]]` name; the wiki becomes the `repo:<name>:wiki` corpus.
    pub name: String,
    /// Repo root to seed. `None` resolves to `std::env::current_dir()`.
    pub path: Option<PathBuf>,
    /// Overwrite an existing `.hallouminate/config.toml`.
    pub force: bool,
}

pub fn cmd_init_repo(args: InitRepoArgs) -> anyhow::Result<()> {
    // Same name rules the config loader enforces (non-empty, no colon) —
    // fail here, before writing, instead of at the first daemon request.
    repo_corpus_name(&args.name, RepoCorpusKind::Wiki)
        .map_err(|e| anyhow!("invalid repository name {:?}: {e}", args.name))?;

    let root = match args.path {
        Some(p) => p,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    let config_path = root.join(".hallouminate").join("config.toml");
    if config_path.exists() && !args.force {
        return Err(anyhow!(
            "repo config already exists at {}; pass --force to overwrite",
            config_path.display()
        ));
    }

    let wiki_dir = root.join(WIKI_RELATIVE_PATH);
    fs::create_dir_all(&wiki_dir).with_context(|| format!("create {}", wiki_dir.display()))?;

    // `path = "."` resolves against the repo root (the parent of
    // `.hallouminate/`), so the config works from any checkout or worktree.
    let config_body = format!(
        "[[repository]]\nname = {}\npath = \".\"\n",
        toml::Value::String(args.name.clone())
    );
    fs::write(&config_path, config_body)
        .with_context(|| format!("write {}", config_path.display()))?;

    // Seed the wiki index only when absent: re-running `init-repo --force`
    // must never clobber an existing wiki.
    let index_path = wiki_dir.join("index.md");
    if !index_path.exists() {
        let index_body = format!(
            "# {} wiki\n\n{INDEX_START_MARKER}\n{INDEX_END_MARKER}\n",
            args.name
        );
        fs::write(&index_path, index_body)
            .with_context(|| format!("write {}", index_path.display()))?;
    }

    println!("wrote {}", config_path.display());
    println!(
        "wiki skeleton at {} (corpus: repo:{}:wiki)",
        wiki_dir.display(),
        args.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::config::{Config, resolve_for_cwd};

    fn init(name: &str, root: &std::path::Path, force: bool) -> anyhow::Result<()> {
        cmd_init_repo(InitRepoArgs {
            name: name.to_string(),
            path: Some(root.to_path_buf()),
            force,
        })
    }

    #[test]
    fn seeds_config_and_wiki_skeleton_that_the_config_loader_accepts() {
        let dir = tempfile::tempdir().expect("tempdir");
        init("demo", dir.path(), false).expect("init-repo");

        // The seeded layout must round-trip through the real repo-layer
        // discovery + merge — the same path `config validate --cwd` takes.
        let (effective, layers) =
            resolve_for_cwd(&Config::default(), dir.path(), None).expect("resolve seeded repo");
        let repo_path = layers.repo_path.expect("repo config path");
        assert_eq!(
            repo_path.canonicalize().expect("canonicalize repo path"),
            dir.path()
                .canonicalize()
                .expect("canonicalize")
                .join(".hallouminate/config.toml")
                .canonicalize()
                .expect("canonicalize expected config path")
        );
        let names: Vec<String> = effective
            .effective_corpora()
            .expect("effective corpora")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            names.contains(&"repo:demo:wiki".to_string()),
            "seeded repo must derive the wiki corpus, got {names:?}"
        );

        let index = fs::read_to_string(dir.path().join(".hallouminate/wiki/index.md"))
            .expect("read index.md");
        assert!(index.starts_with("# demo wiki\n"), "H1-first convention");
        assert!(index.contains(INDEX_START_MARKER) && index.contains(INDEX_END_MARKER));
    }

    #[test]
    fn refuses_to_overwrite_existing_config_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        init("demo", dir.path(), false).expect("first init");
        let err = init("other", dir.path(), false).expect_err("second init must fail");
        assert!(err.to_string().contains("--force"), "err: {err}");
        let config =
            fs::read_to_string(dir.path().join(".hallouminate/config.toml")).expect("read config");
        assert!(config.contains("name = \"demo\""), "original config kept");
    }

    #[test]
    fn force_overwrites_config_but_preserves_existing_wiki_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        init("demo", dir.path(), false).expect("first init");
        let index_path = dir.path().join(".hallouminate/wiki/index.md");
        fs::write(&index_path, "# hand-written index\n").expect("write index");

        init("renamed", dir.path(), true).expect("forced init");

        let config =
            fs::read_to_string(dir.path().join(".hallouminate/config.toml")).expect("read config");
        assert!(config.contains("name = \"renamed\""), "config: {config}");
        let index = fs::read_to_string(&index_path).expect("read index");
        assert_eq!(index, "# hand-written index\n", "wiki must survive --force");
    }

    #[test]
    fn rejects_names_the_config_loader_would_reject() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in ["", "with:colon"] {
            let err = init(bad, dir.path(), false).expect_err("bad name must fail");
            assert!(
                err.to_string().contains("invalid repository name"),
                "err for {bad:?}: {err}"
            );
        }
        assert!(
            !dir.path().join(".hallouminate").exists(),
            "nothing written on validation failure"
        );
    }

    #[test]
    fn quotes_names_needing_toml_escaping() {
        let dir = tempfile::tempdir().expect("tempdir");
        init("has \"quotes\"", dir.path(), false).expect("init-repo");
        let (effective, _) =
            resolve_for_cwd(&Config::default(), dir.path(), None).expect("resolve seeded repo");
        assert_eq!(
            effective.repositories[0].name, "has \"quotes\"",
            "name must round-trip through TOML escaping"
        );
    }

    #[test]
    fn seeds_a_target_directory_that_does_not_exist_yet() {
        // `--path` may name a directory that isn't created yet; init-repo
        // creates it (git-init-style convenience) rather than erroring.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("brand/new/repo");
        init("demo", &target, false).expect("init-repo into missing dir");
        let (effective, _) =
            resolve_for_cwd(&Config::default(), &target, None).expect("resolve seeded repo");
        assert_eq!(effective.repositories[0].name, "demo");
        assert!(target.join(".hallouminate/wiki/index.md").is_file());
    }
}
