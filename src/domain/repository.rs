//! Repository tenant declarations and derived corpora.
//!
//! A `[[repository]]` entry in `config.toml` declares a single git repository
//! that hallouminate can own multiple corpora for: an LLM-managed wiki under
//! `<repo>/.hallouminate/wiki`, and an optional source-document corpus.
//! `repo:{name}:code` is reserved for a future code-aware indexing slice
//! and is not derivable yet.
//!
//! Derived corpora carry the canonical names `repo:{name}:wiki` and
//! `repo:{name}:corpus`. `repo_corpus_name` rejects empty repo names and
//! names containing `':'` so the namespace prefix stays unambiguous.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::common::{CorpusConfig, HallouminateError, Result};

/// Declaration of a single repository tenant.
///
/// `path` is the repository root. `corpus_paths` are document paths the
/// repository wants indexed as a separate source-document corpus; relative
/// entries resolve against `path`. `corpus_globs` / `corpus_exclude`
/// match the `[[corpus]]` semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub corpus_paths: Vec<String>,
    #[serde(default)]
    pub corpus_globs: Vec<String>,
    #[serde(default)]
    pub corpus_exclude: Vec<String>,
}

/// Kind of repository-derived corpus.
///
/// `Wiki` always exists; `Corpus` exists only when the repository declares
/// `corpus_paths`. `Code` is reserved for a future code-aware slice and is
/// not derivable today — including the variant keeps the namespace explicit
/// without committing to behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoCorpusKind {
    Wiki,
    Corpus,
    // Future: Code maps to repo:{name}:code if code-aware indexing is added.
}

impl RepoCorpusKind {
    fn suffix(self) -> &'static str {
        match self {
            RepoCorpusKind::Wiki => "wiki",
            RepoCorpusKind::Corpus => "corpus",
        }
    }
}

/// Relative path inside the repository where the LLM-managed wiki lives.
pub const WIKI_RELATIVE_PATH: &str = ".hallouminate/wiki";

/// Build the canonical `repo:{name}:{kind}` corpus name.
///
/// Rejects empty names and names containing `':'` — the colon would make the
/// derived name unparseable and let repository tenants collide with the
/// `repo:` namespace prefix.
pub fn repo_corpus_name(repo_name: &str, kind: RepoCorpusKind) -> Result<String> {
    if repo_name.is_empty() {
        return Err(HallouminateError::Config(
            "repository name must not be empty".to_string(),
        ));
    }
    if repo_name.contains(':') {
        return Err(HallouminateError::Config(format!(
            "repository name {repo_name:?} must not contain ':' \
             (reserved as the repo:{{name}}:{{kind}} separator)"
        )));
    }
    Ok(format!("repo:{repo_name}:{}", kind.suffix()))
}

/// Build the derived `repo:{name}:wiki` corpus pointing at
/// `<repo.path>/.hallouminate/wiki`.
///
/// The wiki always exists logically — the daemon creates the directory
/// before the first write or indexing pass.
pub fn repository_wiki_corpus(repo: &RepositoryConfig) -> Result<CorpusConfig> {
    let name = repo_corpus_name(&repo.name, RepoCorpusKind::Wiki)?;
    let wiki_dir = wiki_directory(repo);
    Ok(CorpusConfig {
        name,
        paths: vec![wiki_dir.to_string_lossy().into_owned()],
        globs: vec!["**/*.md".to_string()],
        exclude: Vec::new(),
    })
}

/// Build the derived `repo:{name}:corpus` for repository source documents.
///
/// Returns `None` when the repository declares no `corpus_paths`. Relative
/// paths resolve under `repository.path`; absolute paths are left alone.
pub fn repository_source_corpus(repo: &RepositoryConfig) -> Result<Option<CorpusConfig>> {
    if repo.corpus_paths.is_empty() {
        return Ok(None);
    }
    let name = repo_corpus_name(&repo.name, RepoCorpusKind::Corpus)?;
    let repo_root = PathBuf::from(&repo.path);
    let paths: Vec<String> = repo
        .corpus_paths
        .iter()
        .map(|raw| resolve_under(&repo_root, raw))
        .collect();
    Ok(Some(CorpusConfig {
        name,
        paths,
        globs: repo.corpus_globs.clone(),
        exclude: repo.corpus_exclude.clone(),
    }))
}

/// All corpora visible to the daemon: explicit `[[corpus]]` entries plus
/// derived repository wiki/source corpora. Rejects duplicate final names so
/// a user-defined corpus cannot shadow a `repo:` derived name.
pub fn effective_corpora(
    corpora: &[CorpusConfig],
    repositories: &[RepositoryConfig],
) -> Result<Vec<CorpusConfig>> {
    let mut out: Vec<CorpusConfig> = corpora.to_vec();
    for repo in repositories {
        out.push(repository_wiki_corpus(repo)?);
        if let Some(src) = repository_source_corpus(repo)? {
            out.push(src);
        }
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for corpus in &out {
        if !seen.insert(corpus.name.as_str()) {
            return Err(HallouminateError::Config(format!(
                "duplicate corpus name {:?} after deriving repository corpora",
                corpus.name
            )));
        }
    }
    Ok(out)
}

/// Wiki directory for a repository: `<repo.path>/.hallouminate/wiki`.
pub fn wiki_directory(repo: &RepositoryConfig) -> PathBuf {
    PathBuf::from(&repo.path).join(WIKI_RELATIVE_PATH)
}

fn resolve_under(base: &Path, raw: &str) -> String {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        raw.to_string()
    } else {
        base.join(candidate).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, path: &str) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            path: path.into(),
            corpus_paths: Vec::new(),
            corpus_globs: Vec::new(),
            corpus_exclude: Vec::new(),
        }
    }

    #[test]
    fn repo_corpus_name_emits_canonical_wiki_and_corpus_suffixes() {
        assert_eq!(
            repo_corpus_name("tern", RepoCorpusKind::Wiki).unwrap(),
            "repo:tern:wiki",
        );
        assert_eq!(
            repo_corpus_name("tern", RepoCorpusKind::Corpus).unwrap(),
            "repo:tern:corpus",
        );
    }

    #[test]
    fn repo_corpus_name_rejects_empty_name() {
        let err = repo_corpus_name("", RepoCorpusKind::Wiki).expect_err("empty must fail");
        match err {
            HallouminateError::Config(msg) => assert!(msg.contains("empty"), "got: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn repo_corpus_name_rejects_names_containing_colon() {
        // `:` is the namespace separator; allowing it would let a repo
        // declare itself as `tern:wiki` and clash with the derived suffix.
        let err = repo_corpus_name("a:b", RepoCorpusKind::Wiki).expect_err("colon must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("':'") || msg.contains("colon"), "got: {msg}");
                assert!(msg.contains("a:b"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn repository_wiki_corpus_anchors_under_dot_hallouminate_wiki() {
        let cfg = repository_wiki_corpus(&repo("tern", "/repos/tern")).unwrap();
        assert_eq!(cfg.name, "repo:tern:wiki");
        assert_eq!(
            cfg.paths,
            vec!["/repos/tern/.hallouminate/wiki".to_string()],
        );
        assert_eq!(cfg.globs, vec!["**/*.md".to_string()]);
        assert!(cfg.exclude.is_empty());
    }

    #[test]
    fn repository_source_corpus_returns_none_when_corpus_paths_empty() {
        let cfg = repository_source_corpus(&repo("tern", "/r")).unwrap();
        assert!(cfg.is_none(), "no corpus_paths => no source corpus");
    }

    #[test]
    fn repository_source_corpus_resolves_relative_paths_against_repo_path() {
        let mut r = repo("tern", "/repos/tern");
        r.corpus_paths = vec!["docs".into(), "/abs/elsewhere".into()];
        r.corpus_globs = vec!["**/*.md".into()];
        r.corpus_exclude = vec!["**/drafts/**".into()];
        let cfg = repository_source_corpus(&r).unwrap().expect("present");
        assert_eq!(cfg.name, "repo:tern:corpus");
        assert_eq!(
            cfg.paths,
            vec!["/repos/tern/docs".to_string(), "/abs/elsewhere".to_string(),],
        );
        assert_eq!(cfg.globs, vec!["**/*.md".to_string()]);
        assert_eq!(cfg.exclude, vec!["**/drafts/**".to_string()]);
    }

    #[test]
    fn effective_corpora_appends_derived_repository_corpora() {
        let user = CorpusConfig {
            name: "docs".into(),
            paths: vec!["/docs".into()],
            globs: vec!["**/*.md".into()],
            exclude: Vec::new(),
        };
        let mut r = repo("tern", "/r");
        r.corpus_paths = vec!["src/docs".into()];
        let all = effective_corpora(std::slice::from_ref(&user), &[r]).unwrap();
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "repo:tern:wiki", "repo:tern:corpus"]);
    }

    #[test]
    fn effective_corpora_rejects_user_corpus_colliding_with_derived_name() {
        let shadow = CorpusConfig {
            name: "repo:tern:wiki".into(),
            paths: vec!["/x".into()],
            ..Default::default()
        };
        let r = repo("tern", "/r");
        let err = effective_corpora(&[shadow], &[r]).expect_err("duplicate must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("duplicate"), "got: {msg}");
                assert!(msg.contains("repo:tern:wiki"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn effective_corpora_omits_source_corpus_when_repo_declares_no_paths() {
        let r = repo("tern", "/r");
        let all = effective_corpora(&[], &[r]).unwrap();
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["repo:tern:wiki"]);
    }

    #[test]
    fn wiki_directory_is_repo_path_joined_with_dot_hallouminate_wiki() {
        let r = repo("tern", "/repos/tern");
        assert_eq!(
            wiki_directory(&r),
            PathBuf::from("/repos/tern/.hallouminate/wiki"),
        );
    }
}
