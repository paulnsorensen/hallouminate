use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::common::{CorpusConfig, HallouminateError, Result};

use crate::domain::embeddings::{DEFAULT_MODEL, canonical_model_name};
use crate::domain::repository::{RepositoryConfig, effective_corpora};

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

pub fn load(path: Option<&Path>) -> Result<Config> {
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
    fn load_with_explicit_missing_path_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.toml");
        let cfg = load(Some(&missing)).expect("missing file → defaults");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_reads_file_from_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, SPEC_EXAMPLE).expect("write");
        let cfg = load(Some(&cfg_path)).expect("load");
        assert_eq!(cfg.corpora[0].name, "claude-config");
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
    fn load_missing_path_returns_defaults_without_error() {
        // A confirmed NotFound on an explicit path must still degrade to
        // defaults — the NotFound-only filter shouldn't regress this case.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let cfg = load(Some(&missing)).expect("missing -> defaults");
        assert_eq!(cfg, Config::default());
    }

    #[cfg(unix)]
    #[test]
    fn load_propagates_non_notfound_io_error() {
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
        let result = load(Some(&cfg_path));
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
}
