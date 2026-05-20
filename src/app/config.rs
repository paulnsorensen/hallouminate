use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::common::{CorpusConfig, HallouminateError, Result};

use crate::domain::embeddings::{canonical_model_name, DEFAULT_MODEL};
use crate::domain::repository::{effective_corpora, RepositoryConfig};

const DEFAULT_TOP_FILES: usize = 10;
const DEFAULT_CHUNKS_PER_FILE: usize = 3;
const DEFAULT_DEBOUNCE_MS: u64 = 500;
const DEFAULT_EMBED_CACHE: &str = "~/.cache/hallouminate/fastembed";
const DEFAULT_GROUND_DIR: &str = "~/.local/share/hallouminate/ground";
const XDG_CONFIG_FALLBACK_BASE: &str = "~/.config";
const APP_CONFIG_SUBPATH: [&str; 2] = ["hallouminate", "config.toml"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_top_files")]
    pub top_files_default: usize,
    #[serde(default = "default_chunks_per_file")]
    pub chunks_per_file_default: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            top_files_default: DEFAULT_TOP_FILES,
            chunks_per_file_default: DEFAULT_CHUNKS_PER_FILE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_embed_cache")]
    pub cache_dir: String,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            cache_dir: DEFAULT_EMBED_CACHE.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_ground_dir")]
    pub ground_dir: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            ground_dir: DEFAULT_GROUND_DIR.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "corpus", default)]
    pub corpora: Vec<CorpusConfig>,
    // Accept the legacy `[[code_repo]]` plural too so configs written
    // before the rename (PR #21) keep loading instead of silently dropping
    // every repository entry. `config validate` still warns on the legacy
    // key so users have a clear nudge to migrate.
    #[serde(rename = "repository", alias = "code_repo", default)]
    pub repositories: Vec<RepositoryConfig>,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

impl Config {
    /// All corpora visible to the daemon: explicit `[[corpus]]` entries plus
    /// `repo:{name}:wiki` / `repo:{name}:corpus` derived from
    /// `[[repository]]` entries. Fails on duplicate final names so a
    /// `[[corpus]]` cannot shadow a derived repository corpus.
    pub fn effective_corpora(&self) -> Result<Vec<CorpusConfig>> {
        effective_corpora(&self.corpora, &self.repositories)
    }
}

/// Per-request diagnostic struct used by `config validate` / `config show`.
///
/// `xdg_path` is `None` when the baseline came from `--config PATH`; otherwise
/// it carries the XDG location actually consulted (even if the file was
/// absent — `load_xdg` defaults silently on `NotFound`). `repo_path` is
/// always populated because `resolve_for_cwd` errors when discovery fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLayers {
    pub xdg_path: Option<PathBuf>,
    pub repo_path: PathBuf,
}

/// Load the XDG baseline (or `--config PATH`).
///
/// A confirmed `NotFound` on the resolved path degrades to `Config::default()`
/// so a fresh install boots without a config file. Other io errors propagate.
pub fn load_xdg(path: Option<&Path>) -> Result<Config> {
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => xdg_config_path(),
    };
    // Only treat a confirmed `NotFound` as "no config file, use defaults".
    // Other io errors (permission denied, broken symlink, unreadable dir)
    // must propagate so the user isn't silently dropped to an empty
    // configuration when the actual problem is filesystem state.
    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(e) => return Err(HallouminateError::from(e)),
    };
    parse(&text, Some(&resolved))
}

/// Backwards-compatible alias for `load_xdg`. Callers outside this module
/// (CLI subcommands, the daemon entry point) still use `config::load`, so
/// the alias stays until those call sites migrate.
pub fn load(path: Option<&Path>) -> Result<Config> {
    load_xdg(path)
}

/// Walk from `cwd` up looking for `.hallouminate/config.toml`.
///
/// First-match-wins; never composes multiple repo configs. Stops at the first
/// `.git` entry (file *or* directory — git worktrees use a file) and returns
/// an error. Stops at the filesystem root and returns an error.
///
/// Relative `cwd` is normalized to an absolute path against the process'
/// `current_dir()` before walking, so `Path::parent()` walks reliably reach
/// the real filesystem root (a relative path bottoms out at the empty
/// component instead, producing a misleading "reached filesystem root"
/// error).
pub fn discover_repo_config(cwd: &Path) -> Result<PathBuf> {
    let absolute_cwd: PathBuf = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        let here = std::env::current_dir().map_err(HallouminateError::from)?;
        here.join(cwd)
    };
    let mut current: Option<&Path> = Some(&absolute_cwd);
    while let Some(level) = current {
        let candidate = level.join(".hallouminate").join("config.toml");
        // `is_file` returns false on io errors (permission denied, broken
        // symlink), which is the right call here — we want to continue
        // walking instead of erroring out partway up the tree.
        if candidate.is_file() {
            return Ok(candidate);
        }
        let git_marker = level.join(".git");
        // `exists` matches both `.git` directories (normal clone) and `.git`
        // files (git worktrees and submodules).
        if git_marker.exists() {
            return Err(HallouminateError::Config(format!(
                "no .hallouminate/config.toml found walking up from {} \
                 (stopped at repo root {})",
                cwd.display(),
                level.display(),
            )));
        }
        current = level.parent();
    }
    Err(HallouminateError::Config(format!(
        "no .hallouminate/config.toml found walking up from {} \
         (reached filesystem root without hitting a .git boundary)",
        cwd.display(),
    )))
}

/// Parse a repo-layer TOML file, resolving relative paths against the
/// **repo root** (the parent of `.hallouminate/`, i.e. the directory the
/// user would `cd` into when working on the repo).
///
/// Same schema as `load_xdg`. Differences:
///   - `[[repository]].path`, `[[repository]].corpus_paths[*]`,
///     `[[corpus]].paths[*]`, `[storage].ground_dir`, and
///     `[embeddings].cache_dir` get resolved against the repo root and
///     stored as absolute strings. Resolving against the repo root (not the
///     `.hallouminate/` directory) matches user intuition — writing
///     `paths = ["docs"]` in `.hallouminate/config.toml` means
///     `<repo>/docs`, and `[[repository]] path = "."` means the repo root
///     itself (so `wiki_directory` lands at `<repo>/.hallouminate/wiki`,
///     not `<repo>/.hallouminate/.hallouminate/wiki`).
///   - Absolute paths and `~`-prefixed paths pass through untouched —
///     tilde expansion happens at consumption time via `expand_tilde`,
///     identical to the XDG layer's behavior today.
///   - The same `validate()` rules apply (post-resolution).
pub fn load_repo_layer(config_path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(config_path).map_err(HallouminateError::from)?;
    let mut cfg: Config = toml::from_str(&text).map_err(|e| {
        HallouminateError::Config(format!("parsing config at {}: {e}", config_path.display()))
    })?;
    // Resolve against the parent of `.hallouminate/`, i.e. the repo root.
    // `discover_repo_config` only returns paths ending in
    // `<repo_root>/.hallouminate/config.toml`, so two `parent()` hops are
    // always defined for paths produced by discovery. For programmatic
    // callers that hand us a flatter path we fall back to a single hop
    // rather than panic.
    let hallouminate_dir = config_path.parent().ok_or_else(|| {
        HallouminateError::Config(format!(
            "repo config path has no parent directory: {}",
            config_path.display(),
        ))
    })?;
    let repo_root = hallouminate_dir.parent().unwrap_or(hallouminate_dir);
    resolve_repo_layer_paths(&mut cfg, repo_root);
    normalize(&mut cfg)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Merge a baseline `Config` with a repo-layer `Config`.
///
/// List sections (`corpora`, `repositories`) are appended baseline-first
/// then repo-layer; cross-layer name collisions surface via
/// `effective_corpora`'s duplicate-name detection on the combined list.
///
/// Scalar sections (`search`, `embeddings`, `watch`, `storage`) merge field
/// by field. "Explicitly set" is determined by comparison against
/// `Config::default()` — the practical "sentinel" form sanctioned by the
/// spec, since `&Config` carries no per-field provenance. The single
/// consequence is that a layer that explicitly re-states the default cannot
/// trigger a conflict against an *other* layer holding the default; both
/// resolve to the default anyway, so behavior is unchanged.
pub fn merge_layers(baseline: &Config, repo: &Config) -> Result<Config> {
    merge_layers_with_sources(baseline, repo, None, None)
}

/// Variant of `merge_layers` that names source paths in conflict messages.
/// Internal helper so `resolve_for_cwd` can produce richer diagnostics
/// without inflating the public API surface.
fn merge_layers_with_sources(
    baseline: &Config,
    repo: &Config,
    baseline_path: Option<&Path>,
    repo_path: Option<&Path>,
) -> Result<Config> {
    let defaults = Config::default();
    let mut corpora = baseline.corpora.clone();
    corpora.extend(repo.corpora.iter().cloned());
    let mut repositories = baseline.repositories.clone();
    repositories.extend(repo.repositories.iter().cloned());

    let search = SearchConfig {
        top_files_default: merge_scalar(
            "search.top_files_default",
            baseline.search.top_files_default,
            repo.search.top_files_default,
            defaults.search.top_files_default,
            baseline_path,
            repo_path,
        )?,
        chunks_per_file_default: merge_scalar(
            "search.chunks_per_file_default",
            baseline.search.chunks_per_file_default,
            repo.search.chunks_per_file_default,
            defaults.search.chunks_per_file_default,
            baseline_path,
            repo_path,
        )?,
    };
    let embeddings = EmbeddingsConfig {
        model: merge_scalar(
            "embeddings.model",
            baseline.embeddings.model.clone(),
            repo.embeddings.model.clone(),
            defaults.embeddings.model.clone(),
            baseline_path,
            repo_path,
        )?,
        cache_dir: merge_scalar(
            "embeddings.cache_dir",
            baseline.embeddings.cache_dir.clone(),
            repo.embeddings.cache_dir.clone(),
            defaults.embeddings.cache_dir.clone(),
            baseline_path,
            repo_path,
        )?,
    };
    let watch = WatchConfig {
        debounce_ms: merge_scalar(
            "watch.debounce_ms",
            baseline.watch.debounce_ms,
            repo.watch.debounce_ms,
            defaults.watch.debounce_ms,
            baseline_path,
            repo_path,
        )?,
    };
    let storage = StorageConfig {
        ground_dir: merge_scalar(
            "storage.ground_dir",
            baseline.storage.ground_dir.clone(),
            repo.storage.ground_dir.clone(),
            defaults.storage.ground_dir.clone(),
            baseline_path,
            repo_path,
        )?,
    };

    let merged = Config {
        corpora,
        repositories,
        search,
        embeddings,
        watch,
        storage,
    };
    // Re-run cross-layer validation on the combined lists; the inner
    // `effective_corpora` call covers duplicate-name detection across
    // baseline and repo entries.
    validate(&merged)?;
    Ok(merged)
}

/// Per-request top-level: discover the repo config under `cwd`, load it,
/// and merge with the supplied `baseline`. `xdg_path` is the location the
/// baseline came from (`None` when the caller used `--config PATH`); it
/// only feeds the returned `ResolvedLayers` diagnostic and the conflict
/// messages in `merge_layers`.
pub fn resolve_for_cwd(
    baseline: &Config,
    cwd: &Path,
    xdg_path: Option<&Path>,
) -> Result<(Config, ResolvedLayers)> {
    let repo_path = discover_repo_config(cwd)?;
    let repo = load_repo_layer(&repo_path)?;
    let effective = merge_layers_with_sources(baseline, &repo, xdg_path, Some(&repo_path))?;
    Ok((
        effective,
        ResolvedLayers {
            xdg_path: xdg_path.map(Path::to_path_buf),
            repo_path,
        },
    ))
}

fn merge_scalar<T>(
    field: &str,
    baseline: T,
    repo: T,
    default: T,
    baseline_path: Option<&Path>,
    repo_path: Option<&Path>,
) -> Result<T>
where
    T: PartialEq + std::fmt::Debug,
{
    let baseline_set = baseline != default;
    let repo_set = repo != default;
    match (baseline_set, repo_set) {
        (false, false) => Ok(default),
        (true, false) => Ok(baseline),
        (false, true) => Ok(repo),
        (true, true) => {
            if baseline == repo {
                Ok(baseline)
            } else {
                let baseline_src = baseline_path
                    .map(|p| format!(" (baseline at {})", p.display()))
                    .unwrap_or_else(|| " (baseline)".into());
                let repo_src = repo_path
                    .map(|p| format!(" (repo at {})", p.display()))
                    .unwrap_or_else(|| " (repo layer)".into());
                Err(HallouminateError::Config(format!(
                    "scalar conflict on {field}: baseline = {baseline:?}{baseline_src}, \
                     repo = {repo:?}{repo_src}"
                )))
            }
        }
    }
}

/// Rewrite every relative non-tilde path in `cfg` as `base.join(path)`.
/// `.` / `..` segments are preserved as written — `Path::join` does not
/// normalize, and we don't post-process via `Path::components` because
/// canonicalization would require the path to exist on disk. Absolute
/// paths and `~`-prefixed paths are left alone.
fn resolve_repo_layer_paths(cfg: &mut Config, base: &Path) {
    for corpus in cfg.corpora.iter_mut() {
        for p in corpus.paths.iter_mut() {
            *p = resolve_repo_path(p, base);
        }
    }
    for repo in cfg.repositories.iter_mut() {
        repo.path = resolve_repo_path(&repo.path, base);
        for p in repo.corpus_paths.iter_mut() {
            *p = resolve_repo_path(p, base);
        }
    }
    cfg.storage.ground_dir = resolve_repo_path(&cfg.storage.ground_dir, base);
    cfg.embeddings.cache_dir = resolve_repo_path(&cfg.embeddings.cache_dir, base);
}

fn resolve_repo_path(raw: &str, base: &Path) -> String {
    if raw.is_empty() {
        return raw.to_string();
    }
    if raw.starts_with('~') {
        return raw.to_string();
    }
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return raw.to_string();
    }
    base.join(candidate).to_string_lossy().into_owned()
}

pub fn xdg_config_path() -> PathBuf {
    xdg_config_path_from(std::env::var_os("XDG_CONFIG_HOME").as_deref())
}

/// Pure resolver: honor `$XDG_CONFIG_HOME` when set and non-empty, otherwise
/// fall back to `~/.config`. Split out from `xdg_config_path` so tests can
/// exercise both branches without mutating process env (unsafe on edition
/// 2024) or relying on the developer's local shell environment.
fn xdg_config_path_from(xdg_config_home: Option<&std::ffi::OsStr>) -> PathBuf {
    let base = xdg_config_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(shellexpand::tilde(XDG_CONFIG_FALLBACK_BASE).into_owned())
        });
    APP_CONFIG_SUBPATH.iter().fold(base, |p, seg| p.join(seg))
}

fn parse(text: &str, source: Option<&Path>) -> Result<Config> {
    let mut cfg: Config = toml::from_str(text).map_err(|e| {
        let where_ = source
            .map(|p| format!(" at {}", p.display()))
            .unwrap_or_default();
        HallouminateError::Config(format!("parsing config{where_}: {e}"))
    })?;
    normalize(&mut cfg)?;
    validate(&cfg)?;
    Ok(cfg)
}

fn normalize(cfg: &mut Config) -> Result<()> {
    cfg.embeddings.model = canonical_model_name(&cfg.embeddings.model)?.to_string();
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    for (idx, c) in cfg.corpora.iter().enumerate() {
        if c.name.trim().is_empty() {
            return Err(HallouminateError::Config(format!(
                "corpus #{idx} has empty name"
            )));
        }
        if c.paths.is_empty() {
            return Err(HallouminateError::Config(format!(
                "corpus '{}' has no paths",
                c.name
            )));
        }
    }
    for (idx, r) in cfg.repositories.iter().enumerate() {
        if r.name.trim().is_empty() {
            return Err(HallouminateError::Config(format!(
                "repository #{idx} has empty name"
            )));
        }
        if r.path.trim().is_empty() {
            return Err(HallouminateError::Config(format!(
                "repository '{}' has empty path",
                r.name
            )));
        }
    }
    // Surface duplicate-name and bad-name failures at config-load time
    // instead of waiting for the daemon to enumerate corpora at request
    // time.
    cfg.effective_corpora()?;
    Ok(())
}

fn default_top_files() -> usize {
    DEFAULT_TOP_FILES
}
fn default_chunks_per_file() -> usize {
    DEFAULT_CHUNKS_PER_FILE
}
fn default_debounce_ms() -> u64 {
    DEFAULT_DEBOUNCE_MS
}
fn default_model() -> String {
    DEFAULT_MODEL.into()
}
fn default_embed_cache() -> String {
    DEFAULT_EMBED_CACHE.into()
}
fn default_ground_dir() -> String {
    DEFAULT_GROUND_DIR.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = r#"
[[corpus]]
name = "claude-config"
paths = ["~/.claude/skills", "~/.claude/agents", "~/.claude/CLAUDE.md"]
globs = ["**/*.md"]
exclude = ["**/.git/**", "**/node_modules/**"]

[[repository]]
name = "tern"
path = "~/Dev/tern"

[search]
top_files_default       = 10
chunks_per_file_default = 3

[embeddings]
model     = "BAAI/bge-small-en-v1.5"
cache_dir = "~/.cache/hallouminate/fastembed"

[watch]
debounce_ms = 500

[storage]
ground_dir = "~/.local/share/hallouminate/ground"
"#;

    #[test]
    fn parse_spec_example_decodes_every_field() {
        let cfg = parse(SPEC_EXAMPLE, None).expect("spec example parses");

        assert_eq!(cfg.corpora.len(), 1);
        let corpus = &cfg.corpora[0];
        assert_eq!(corpus.name, "claude-config");
        assert_eq!(
            corpus.paths,
            vec![
                "~/.claude/skills".to_string(),
                "~/.claude/agents".into(),
                "~/.claude/CLAUDE.md".into(),
            ]
        );
        assert_eq!(corpus.globs, vec!["**/*.md".to_string()]);
        assert_eq!(
            corpus.exclude,
            vec!["**/.git/**".to_string(), "**/node_modules/**".into()]
        );

        assert_eq!(cfg.repositories.len(), 1);
        assert_eq!(cfg.repositories[0].name, "tern");
        assert_eq!(cfg.repositories[0].path, "~/Dev/tern");

        assert_eq!(cfg.search.top_files_default, 10);
        assert_eq!(cfg.search.chunks_per_file_default, 3);

        assert_eq!(cfg.embeddings.model, "BAAI/bge-small-en-v1.5");
        assert_eq!(cfg.embeddings.cache_dir, "~/.cache/hallouminate/fastembed");

        assert_eq!(cfg.watch.debounce_ms, 500);
        assert_eq!(cfg.storage.ground_dir, "~/.local/share/hallouminate/ground");
    }

    #[test]
    fn parse_empty_string_yields_full_defaults() {
        let cfg = parse("", None).expect("empty toml parses");
        assert!(cfg.corpora.is_empty());
        assert!(cfg.repositories.is_empty());
        assert_eq!(cfg.search, SearchConfig::default());
        assert_eq!(cfg.embeddings, EmbeddingsConfig::default());
        assert_eq!(cfg.watch, WatchConfig::default());
        assert_eq!(cfg.storage, StorageConfig::default());
    }

    #[test]
    fn parse_legacy_embedding_alias_normalizes_to_canonical_model() {
        let cfg =
            parse("[embeddings]\nmodel = \"bge-small-en-v1.5\"\n", None).expect("legacy alias");
        assert_eq!(cfg.embeddings.model, "BAAI/bge-small-en-v1.5");
    }

    #[test]
    fn parse_rejects_unknown_embedding_model_before_runtime_downloads() {
        let err = parse("[embeddings]\nmodel = \"clip-vit-b32\"\n", None)
            .expect_err("unsupported model must fail during config parse");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("unsupported embedding model"), "got: {msg}");
                assert!(msg.contains("BAAI/bge-small-en-v1.5"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_partial_search_section_uses_defaults_for_missing_fields() {
        let cfg = parse("[search]\ntop_files_default = 5\n", None).expect("partial search parses");
        assert_eq!(cfg.search.top_files_default, 5);
        assert_eq!(cfg.search.chunks_per_file_default, DEFAULT_CHUNKS_PER_FILE);
    }

    #[test]
    fn parse_rejects_corpus_with_empty_name() {
        let err = parse("[[corpus]]\nname = \"\"\npaths = [\"/x\"]\n", None)
            .expect_err("empty corpus name");
        match err {
            HallouminateError::Config(msg) => assert!(msg.contains("empty name"), "got: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_corpus_with_no_paths() {
        let err = parse("[[corpus]]\nname = \"docs\"\n", None).expect_err("no paths");
        match err {
            HallouminateError::Config(msg) => assert!(msg.contains("no paths"), "got: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_repository_with_empty_name() {
        let err = parse("[[repository]]\nname = \"\"\npath = \"/r\"\n", None)
            .expect_err("empty repository name");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("empty name"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_repository_with_empty_path() {
        let err = parse("[[repository]]\nname = \"tern\"\npath = \"\"\n", None)
            .expect_err("empty repository path");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("empty path"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_repository_name_containing_colon() {
        let err = parse("[[repository]]\nname = \"bad:name\"\npath = \"/r\"\n", None)
            .expect_err("colon in repo name must surface during validate");
        match err {
            HallouminateError::Config(msg) => assert!(msg.contains("bad:name"), "got: {msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn effective_corpora_includes_repository_wiki_after_user_corpora() {
        let cfg = parse(
            r#"
[[corpus]]
name = "docs"
paths = ["/docs"]

[[repository]]
name = "tern"
path = "/repos/tern"
"#,
            None,
        )
        .expect("parses");
        let all = cfg.effective_corpora().expect("derive");
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "repo:tern:wiki"]);
    }

    #[test]
    fn effective_corpora_includes_repository_source_corpus_when_paths_set() {
        let cfg = parse(
            r#"
[[repository]]
name = "tern"
path = "/repos/tern"
corpus_paths = ["docs"]
"#,
            None,
        )
        .expect("parses");
        let all = cfg.effective_corpora().expect("derive");
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["repo:tern:wiki", "repo:tern:corpus"]);
        let source = &all[1];
        assert_eq!(source.paths, vec!["/repos/tern/docs".to_string()]);
    }

    #[test]
    fn parse_rejects_duplicate_user_corpus_shadowing_repository_wiki() {
        let err = parse(
            r#"
[[corpus]]
name = "repo:tern:wiki"
paths = ["/x"]

[[repository]]
name = "tern"
path = "/r"
"#,
            None,
        )
        .expect_err("duplicate must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("duplicate"), "got: {msg}");
                assert!(msg.contains("repo:tern:wiki"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_two_repositories_with_same_name_via_derived_corpus_collision() {
        // Two `[[repository]]` entries with the same name both derive
        // `repo:{name}:wiki`, so the second entry must surface as a
        // duplicate-name failure at config-load time — not at the first
        // daemon request that happens to enumerate corpora.
        let err = parse(
            r#"
[[repository]]
name = "tern"
path = "/r1"

[[repository]]
name = "tern"
path = "/r2"
"#,
            None,
        )
        .expect_err("two repos with the same name must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("duplicate"), "got: {msg}");
                assert!(msg.contains("repo:tern:wiki"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn load_xdg_with_explicit_missing_path_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.toml");
        let cfg = load_xdg(Some(&missing)).expect("missing file → defaults");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_xdg_reads_file_from_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, SPEC_EXAMPLE).expect("write");
        let cfg = load_xdg(Some(&cfg_path)).expect("load");
        assert_eq!(cfg.corpora[0].name, "claude-config");
    }

    #[test]
    fn load_is_alias_for_load_xdg() {
        // The legacy `load` name stays as a thin alias for outside callers;
        // pin the equivalence so future renames notice the contract.
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, SPEC_EXAMPLE).expect("write");
        assert_eq!(
            load(Some(&cfg_path)).expect("load"),
            load_xdg(Some(&cfg_path)).expect("load_xdg"),
        );
    }

    #[test]
    fn parse_silently_ignores_legacy_fusion_fields() {
        // A user upgrading from the SQLite era may still have a config with
        // `[search].fusion`, `convex_alpha`, `rrf_k`. The restack removed the
        // knob; serde defaults to ignoring unknown fields, so the load must
        // succeed and the SearchConfig must come back as defaults rather than
        // failing with an "unknown field" error.
        let legacy = r#"
[search]
top_files_default       = 7
chunks_per_file_default = 2
fusion                  = "convex"
convex_alpha            = 0.65
rrf_k                   = 60
"#;
        let cfg = parse(legacy, None).expect("legacy config must still parse");
        assert_eq!(cfg.search.top_files_default, 7);
        assert_eq!(cfg.search.chunks_per_file_default, 2);
        assert_eq!(
            cfg.search,
            SearchConfig {
                top_files_default: 7,
                chunks_per_file_default: 2,
            },
            "SearchConfig must hold only the two surviving fields"
        );
    }

    #[test]
    fn xdg_config_path_falls_back_to_dot_config_when_xdg_env_absent() {
        let path = xdg_config_path_from(None);
        assert!(
            path.ends_with(".config/hallouminate/config.toml"),
            "got {}",
            path.display()
        );
        assert!(path.is_absolute(), "tilde must expand: {}", path.display());
    }

    #[test]
    fn xdg_config_path_falls_back_when_xdg_env_is_empty_string() {
        // POSIX/XDG: an empty XDG_CONFIG_HOME is treated as unset.
        let path = xdg_config_path_from(Some(std::ffi::OsStr::new("")));
        assert!(
            path.ends_with(".config/hallouminate/config.toml"),
            "empty XDG_CONFIG_HOME must fall back; got {}",
            path.display()
        );
    }

    #[test]
    fn xdg_config_path_honors_custom_xdg_config_home() {
        // Regression for PR #7 Copilot review: the loader must honor a
        // custom XDG_CONFIG_HOME instead of always resolving to ~/.config.
        let custom = std::path::PathBuf::from("/var/tmp/custom-xdg");
        let path = xdg_config_path_from(Some(custom.as_os_str()));
        assert_eq!(path, custom.join("hallouminate").join("config.toml"));
    }

    #[test]
    fn load_xdg_missing_path_returns_defaults_without_error() {
        // A confirmed NotFound on an explicit path must still degrade to
        // defaults — the NotFound-only filter shouldn't regress this case.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let cfg = load_xdg(Some(&missing)).expect("missing -> defaults");
        assert_eq!(cfg, Config::default());
    }

    #[cfg(unix)]
    #[test]
    fn load_xdg_propagates_non_notfound_io_error() {
        // Regression for PR #7 Copilot review: a non-NotFound io error
        // (here: unreadable directory → EACCES on read_to_string) must
        // propagate as HallouminateError::Io, not silently default.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let unreadable = dir.path().join("locked");
        std::fs::create_dir(&unreadable).expect("mkdir");
        let cfg_path = unreadable.join("config.toml");
        std::fs::write(&cfg_path, "").expect("write");
        // 0o000 on parent dir → read of the file inside fails with EACCES.
        // root can bypass this; skip the assertion when running as root.
        let is_root = nix_getuid_is_zero();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
            .expect("chmod");
        let result = load_xdg(Some(&cfg_path));
        // Restore perms before any potential test failure unwind, so the
        // tempdir can be cleaned up.
        let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755));
        if is_root {
            return; // root reads through 0o000; the negative test is meaningless.
        }
        let err = result.expect_err("unreadable parent must surface an io error");
        match err {
            HallouminateError::Io(io) => {
                assert_ne!(
                    io.kind(),
                    std::io::ErrorKind::NotFound,
                    "must NOT classify as NotFound: {io}"
                );
            }
            other => panic!("expected HallouminateError::Io, got {other:?}"),
        }
    }

    #[cfg(unix)]
    fn nix_getuid_is_zero() -> bool {
        // Avoid a libc dep just for this; read /proc/self/status on Linux,
        // shell out to `id -u` everywhere else (macOS, BSDs). The test
        // tolerates either path failing — worst case we run the assertion
        // when we shouldn't, which only false-positives in CI containers
        // running as root, where the assertion is a no-op anyway.
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            if let Some(line) = s.lines().find(|l| l.starts_with("Uid:")) {
                return line.split_whitespace().nth(1) == Some("0");
            }
        }
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false)
    }

    // ── discover_repo_config ────────────────────────────────────────────

    /// Canonicalize a tempdir so comparisons survive macOS's `/var → /private/var`
    /// symlink. Without this, paths returned by the walker may not equal-string
    /// the path we built locally even though they point at the same inode.
    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }

    fn write_repo_config(dir: &Path, body: &str) -> PathBuf {
        let cfg_dir = dir.join(".hallouminate");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir .hallouminate");
        let cfg_path = cfg_dir.join("config.toml");
        std::fs::write(&cfg_path, body).expect("write repo config");
        cfg_path
    }

    #[test]
    fn discover_repo_config_finds_at_cwd_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canon(dir.path());
        let expected = write_repo_config(&root, "");
        let found = discover_repo_config(&root).expect("found at cwd");
        assert_eq!(canon(&found), canon(&expected));
    }

    #[test]
    fn discover_repo_config_finds_at_ancestor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canon(dir.path());
        let expected = write_repo_config(&root, "");
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let found = discover_repo_config(&nested).expect("walked up to ancestor");
        assert_eq!(canon(&found), canon(&expected));
    }

    #[test]
    fn discover_repo_config_stops_at_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canon(dir.path());
        std::fs::create_dir(root.join(".git")).expect("mkdir .git");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let err = discover_repo_config(&nested).expect_err("stop at repo root");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("stopped at repo root"), "got: {msg}");
                // The CWD we walked from must appear in the error so a user can
                // tell at a glance which directory failed to resolve.
                assert!(msg.contains(&nested.display().to_string()), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn discover_repo_config_treats_git_file_as_repo_boundary() {
        // git worktrees and submodules use a `.git` *file* (containing
        // `gitdir: ...`) rather than a directory; the walk must still stop.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canon(dir.path());
        std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").expect("write .git file");
        let err = discover_repo_config(&root).expect_err("git file stops walk");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("stopped at repo root"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn discover_repo_config_relative_cwd_resolves_against_current_dir() {
        // The relative-cwd normalization fix: a relative `cwd` must be
        // joined against `std::env::current_dir()` before walking, so
        // `Path::parent()` walks reach the real filesystem root rather
        // than bottoming out at the empty path.
        //
        // The error message preserves the user's original `cwd` input
        // verbatim ("walking up from <cwd>"), so the relative and
        // absolute inputs differ there. But the *walked* path — the
        // "stopped at repo root <level>" / "reached filesystem root"
        // tail — must be identical after normalization, because both
        // inputs resolve to the same absolute starting point and
        // ascend the same parent chain.
        //
        // Before the fix, the relative input bottomed out at the empty
        // path and produced either an empty `<level>` in the error or a
        // misleading "reached filesystem root" branch.
        let rel = Path::new("nonexistent-relative-input-zzz");
        let here = std::env::current_dir().expect("current_dir");
        let abs = here.join(rel);
        let rel_msg = match discover_repo_config(rel) {
            Err(HallouminateError::Config(m)) => m,
            other => panic!("relative input must error: {other:?}"),
        };
        let abs_msg = match discover_repo_config(&abs) {
            Err(HallouminateError::Config(m)) => m,
            other => panic!("absolute input must error: {other:?}"),
        };

        // Extract the walk-tail (the parenthesized "(... )" suffix) and
        // assert relative and absolute inputs ended the walk at the
        // same point.
        let tail = |m: &str| {
            m.rfind('(')
                .map(|i| m[i..].to_string())
                .unwrap_or_else(|| m.to_string())
        };
        assert_eq!(
            tail(&rel_msg),
            tail(&abs_msg),
            "relative and absolute cwd must reach the same walk endpoint after normalization;\n  rel: {rel_msg}\n  abs: {abs_msg}",
        );
    }

    #[test]
    fn discover_repo_config_errors_walking_past_no_git_no_config() {
        // A subtree with no `.git` and no `.hallouminate/config.toml`
        // anywhere up to the filesystem root must error rather than walk
        // forever or silently succeed. We can't realistically test "all the
        // way to /" so simulate by walking from a tempdir whose ancestors
        // are guaranteed not to host `.hallouminate/config.toml` (the system
        // tmp tree). The error message must mention the filesystem-root
        // exhaust path because we never hit a `.git`.
        let dir = tempfile::tempdir().expect("tempdir");
        // The system tmp dir on macOS *might* have a `.git` ancestor in
        // weird CI sandboxes; skip the assertion if it does. The point of
        // this test is the message-shape contract.
        let cwd = canon(dir.path());
        match discover_repo_config(&cwd) {
            Err(HallouminateError::Config(msg)) => {
                // Either we hit FS root (unusual CI sandboxes) or a `.git`
                // somewhere up the chain. Both are valid "no config here"
                // outcomes; the message just has to be non-empty.
                assert!(
                    msg.contains("filesystem root") || msg.contains("stopped at repo root"),
                    "got: {msg}"
                );
            }
            Ok(p) => panic!("did not expect to find a config; got {}", p.display()),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    // ── load_repo_layer ─────────────────────────────────────────────────

    #[test]
    fn load_repo_layer_resolves_relative_paths_against_repo_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        let cfg = r#"
[[corpus]]
name = "docs"
paths = ["docs", "specs/cur"]

[[repository]]
name = "self"
path = "."
corpus_paths = ["sub/docs"]

[storage]
ground_dir = "var/ground"

[embeddings]
cache_dir = "var/fastembed"
"#;
        let cfg_path = write_repo_config(&repo_root, cfg);

        let parsed = load_repo_layer(&cfg_path).expect("load_repo_layer");
        // Repo-layer relative paths are resolved against the repo root
        // (the parent of `.hallouminate/`), not against `.hallouminate/`
        // itself. This matches user intuition: `paths = ["docs"]` written
        // in `.hallouminate/config.toml` means `<repo>/docs`.
        let base = &repo_root;

        assert_eq!(
            parsed.corpora[0].paths,
            vec![
                base.join("docs").to_string_lossy().into_owned(),
                base.join("specs/cur").to_string_lossy().into_owned(),
            ]
        );
        // Repository `path = "."` resolves to the repo root itself, so
        // `wiki_directory` lands at `<repo>/.hallouminate/wiki` (no double
        // `.hallouminate/`).
        assert_eq!(
            parsed.repositories[0].path,
            base.join(".").to_string_lossy().into_owned(),
        );
        assert_eq!(
            parsed.repositories[0].corpus_paths,
            vec![base.join("sub/docs").to_string_lossy().into_owned()],
        );
        assert_eq!(
            parsed.storage.ground_dir,
            base.join("var/ground").to_string_lossy().into_owned(),
        );
        assert_eq!(
            parsed.embeddings.cache_dir,
            base.join("var/fastembed").to_string_lossy().into_owned(),
        );
    }

    #[test]
    fn load_repo_layer_preserves_absolute_paths_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        let cfg = r#"
[[corpus]]
name = "abs"
paths = ["/abs/docs"]

[[repository]]
name = "absrepo"
path = "/abs/repo"
corpus_paths = ["/abs/repo/docs"]

[storage]
ground_dir = "/abs/ground"

[embeddings]
cache_dir = "/abs/cache"
"#;
        let cfg_path = write_repo_config(&repo_root, cfg);
        let parsed = load_repo_layer(&cfg_path).expect("load_repo_layer");

        assert_eq!(parsed.corpora[0].paths, vec!["/abs/docs".to_string()]);
        assert_eq!(parsed.repositories[0].path, "/abs/repo");
        assert_eq!(
            parsed.repositories[0].corpus_paths,
            vec!["/abs/repo/docs".to_string()],
        );
        assert_eq!(parsed.storage.ground_dir, "/abs/ground");
        assert_eq!(parsed.embeddings.cache_dir, "/abs/cache");
    }

    #[test]
    fn load_repo_layer_preserves_tilde_paths_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        let cfg = r#"
[[corpus]]
name = "home"
paths = ["~/docs"]

[[repository]]
name = "homerepo"
path = "~/repo"
corpus_paths = ["~/repo/docs"]

[storage]
ground_dir = "~/ground"

[embeddings]
cache_dir = "~/cache"
"#;
        let cfg_path = write_repo_config(&repo_root, cfg);
        let parsed = load_repo_layer(&cfg_path).expect("load_repo_layer");

        // Tilde expansion happens at consumption time via `expand_tilde`;
        // the loader must NOT rewrite tilde-prefixed strings.
        assert_eq!(parsed.corpora[0].paths, vec!["~/docs".to_string()]);
        assert_eq!(parsed.repositories[0].path, "~/repo");
        assert_eq!(
            parsed.repositories[0].corpus_paths,
            vec!["~/repo/docs".to_string()],
        );
        assert_eq!(parsed.storage.ground_dir, "~/ground");
        assert_eq!(parsed.embeddings.cache_dir, "~/cache");
    }

    // ── merge_layers ────────────────────────────────────────────────────

    #[test]
    fn merge_layers_appends_repo_corpora_after_baseline() {
        let baseline = parse(
            r#"
[[corpus]]
name = "global"
paths = ["/global"]
"#,
            None,
        )
        .expect("baseline parses");
        let repo = parse(
            r#"
[[corpus]]
name = "local"
paths = ["/local"]
"#,
            None,
        )
        .expect("repo parses");
        let merged = merge_layers(&baseline, &repo).expect("merge");
        let names: Vec<&str> = merged.corpora.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["global", "local"]);
    }

    #[test]
    fn merge_layers_appends_repo_repositories_after_baseline() {
        let baseline = parse(
            r#"
[[repository]]
name = "a"
path = "/a"
"#,
            None,
        )
        .expect("baseline parses");
        let repo = parse(
            r#"
[[repository]]
name = "b"
path = "/b"
"#,
            None,
        )
        .expect("repo parses");
        let merged = merge_layers(&baseline, &repo).expect("merge");
        let names: Vec<&str> = merged
            .repositories
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn merge_layers_uses_baseline_scalar_when_repo_left_default() {
        let baseline = parse("[search]\ntop_files_default = 20\n", None).expect("baseline");
        let repo = parse("", None).expect("repo default");
        let merged = merge_layers(&baseline, &repo).expect("merge");
        assert_eq!(merged.search.top_files_default, 20);
    }

    #[test]
    fn merge_layers_uses_repo_scalar_when_baseline_left_default() {
        let baseline = parse("", None).expect("baseline default");
        let repo = parse("[search]\ntop_files_default = 30\n", None).expect("repo");
        let merged = merge_layers(&baseline, &repo).expect("merge");
        assert_eq!(merged.search.top_files_default, 30);
    }

    #[test]
    fn merge_layers_accepts_both_sides_explicit_equal() {
        let cfg = "[embeddings]\nmodel = \"BAAI/bge-small-en-v1.5\"\ncache_dir = \"/shared\"\n";
        let baseline = parse(cfg, None).expect("baseline");
        let repo = parse(cfg, None).expect("repo");
        let merged = merge_layers(&baseline, &repo).expect("merge");
        assert_eq!(merged.embeddings.cache_dir, "/shared");
        assert_eq!(merged.embeddings.model, "BAAI/bge-small-en-v1.5");
    }

    #[test]
    fn merge_layers_fails_on_scalar_conflict_with_field_name_in_message() {
        // AC #7: scalar conflict produces HallouminateError::Config naming
        // the field. We assert on `embeddings.cache_dir` because both layers
        // can set it to genuinely different non-default values without
        // running into the `canonical_model_name` normalization that would
        // collapse two "different" model strings.
        let baseline = parse("[embeddings]\ncache_dir = \"/a\"\n", None).expect("baseline");
        let repo = parse("[embeddings]\ncache_dir = \"/b\"\n", None).expect("repo");
        let err = merge_layers(&baseline, &repo).expect_err("conflict must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("embeddings.cache_dir"), "got: {msg}");
                assert!(msg.contains("\"/a\""), "got: {msg}");
                assert!(msg.contains("\"/b\""), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn merge_layers_conflict_names_both_source_paths_when_supplied() {
        // The internal `merge_layers_with_sources` carries source paths so
        // `resolve_for_cwd` can produce a richer error. Pin both paths in
        // the message — AC #7 wants this for the user-facing flow.
        let baseline = parse("[embeddings]\ncache_dir = \"/a\"\n", None).expect("baseline");
        let repo = parse("[embeddings]\ncache_dir = \"/b\"\n", None).expect("repo");
        let xdg = Path::new("/etc/hallouminate/config.toml");
        let repo_p = Path::new("/work/.hallouminate/config.toml");
        let err = merge_layers_with_sources(&baseline, &repo, Some(xdg), Some(repo_p))
            .expect_err("conflict must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("/etc/hallouminate/config.toml"), "got: {msg}");
                assert!(
                    msg.contains("/work/.hallouminate/config.toml"),
                    "got: {msg}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ── resolve_for_cwd ─────────────────────────────────────────────────

    #[test]
    fn resolve_for_cwd_walks_finds_and_merges_repo_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        write_repo_config(
            &repo_root,
            r#"
[[corpus]]
name = "repo-docs"
paths = ["docs"]
"#,
        );
        let baseline = parse(
            r#"
[[corpus]]
name = "global"
paths = ["/g"]
"#,
            None,
        )
        .expect("baseline");

        let nested = repo_root.join("src").join("inner");
        std::fs::create_dir_all(&nested).expect("mkdir nested");

        let xdg = PathBuf::from("/etc/hallouminate/config.toml");
        let (effective, layers) = resolve_for_cwd(&baseline, &nested, Some(&xdg)).expect("resolve");

        let names: Vec<&str> = effective.corpora.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["global", "repo-docs"]);
        assert_eq!(layers.xdg_path, Some(xdg));
        assert_eq!(
            canon(&layers.repo_path),
            canon(&repo_root.join(".hallouminate").join("config.toml")),
        );
    }

    #[test]
    fn resolve_for_cwd_with_repository_dot_path_derives_corpora_against_repo_root() {
        // AC #8: `[[repository]] name="X" path="."` must derive
        // `repo:X:wiki` and `repo:X:corpus` with paths resolved against the
        // repo root (the parent of `.hallouminate/`, i.e. the directory the
        // user `cd`s into).
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        write_repo_config(
            &repo_root,
            r#"
[[repository]]
name = "X"
path = "."
corpus_paths = ["docs"]
"#,
        );

        let baseline = Config::default();
        let (effective, _layers) = resolve_for_cwd(&baseline, &repo_root, None).expect("resolve");

        let all = effective.effective_corpora().expect("derive corpora");
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["repo:X:wiki", "repo:X:corpus"]);

        // The wiki corpus resolves under the repo root — no double
        // `.hallouminate/.hallouminate/` segment.
        let wiki_expected = repo_root
            .join(".")
            .join(".hallouminate")
            .join("wiki")
            .to_string_lossy()
            .into_owned();
        assert_eq!(all[0].paths, vec![wiki_expected]);

        // The repo source corpus resolves "docs" against the repo root at
        // `load_repo_layer` time, then `resolve_under` sees it as already
        // absolute and passes it through verbatim — so the final path is
        // `<repo_root>/docs` with no extra `.` segment.
        let docs_expected = repo_root.join("docs").to_string_lossy().into_owned();
        assert_eq!(all[1].paths, vec![docs_expected]);
    }

    #[test]
    fn resolve_for_cwd_returns_hard_error_when_no_repo_config_found() {
        // A `.git` boundary with no config in between must surface as a
        // hard error — the daemon refuses to fall back to baseline-only.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        std::fs::create_dir(repo_root.join(".git")).expect("mkdir .git");
        let nested = repo_root.join("src");
        std::fs::create_dir_all(&nested).expect("mkdir nested");

        let baseline = Config::default();
        let err = resolve_for_cwd(&baseline, &nested, None).expect_err("must error");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("stopped at repo root"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn resolve_for_cwd_passes_xdg_path_into_conflict_messages() {
        // Verifies the source-path threading: a scalar conflict between
        // baseline (XDG) and the repo layer should name *both* paths.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = canon(dir.path());
        let cfg_path = write_repo_config(&repo_root, "[embeddings]\ncache_dir = \"/repo-cache\"\n");
        let baseline =
            parse("[embeddings]\ncache_dir = \"/xdg-cache\"\n", None).expect("baseline parse");
        let xdg = PathBuf::from("/etc/hallouminate/config.toml");

        let err =
            resolve_for_cwd(&baseline, &repo_root, Some(&xdg)).expect_err("conflict must fail");
        match err {
            HallouminateError::Config(msg) => {
                assert!(msg.contains("embeddings.cache_dir"), "got: {msg}");
                assert!(msg.contains("/etc/hallouminate/config.toml"), "got: {msg}");
                assert!(msg.contains(&cfg_path.display().to_string()), "got: {msg}");
                assert!(msg.contains("/xdg-cache"), "got: {msg}");
                assert!(msg.contains("/repo-cache"), "got: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
