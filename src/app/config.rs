use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::common::{CorpusConfig, HallouminateError, Result};

const DEFAULT_TOP_FILES: usize = 10;
const DEFAULT_CHUNKS_PER_FILE: usize = 3;
const DEFAULT_DEBOUNCE_MS: u64 = 500;
const DEFAULT_MODEL: &str = "bge-small-en-v1.5";
const DEFAULT_EMBED_CACHE: &str = "~/.cache/hallouminate/fastembed";
const DEFAULT_GROUND_DIR: &str = "~/.local/share/hallouminate/ground";
const XDG_CONFIG_RELATIVE: &str = "~/.config/hallouminate/config.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRepoConfig {
    pub name: String,
    pub path: String,
}

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
    #[serde(rename = "code_repo", default)]
    pub code_repos: Vec<CodeRepoConfig>,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

pub fn load(path: Option<&Path>) -> Result<Config> {
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => xdg_config_path(),
    };
    if !resolved.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&resolved)?;
    parse(&text, Some(&resolved))
}

pub fn xdg_config_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde(XDG_CONFIG_RELATIVE).into_owned())
}

fn parse(text: &str, source: Option<&Path>) -> Result<Config> {
    let cfg: Config = toml::from_str(text).map_err(|e| {
        let where_ = source
            .map(|p| format!(" at {}", p.display()))
            .unwrap_or_default();
        HallouminateError::Config(format!("parsing config{where_}: {e}"))
    })?;
    validate(&cfg)?;
    Ok(cfg)
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

[[code_repo]]
name = "tern"
path = "~/Dev/tern"

[search]
top_files_default       = 10
chunks_per_file_default = 3

[embeddings]
model     = "bge-small-en-v1.5"
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

        assert_eq!(cfg.code_repos.len(), 1);
        assert_eq!(cfg.code_repos[0].name, "tern");
        assert_eq!(cfg.code_repos[0].path, "~/Dev/tern");

        assert_eq!(cfg.search.top_files_default, 10);
        assert_eq!(cfg.search.chunks_per_file_default, 3);

        assert_eq!(cfg.embeddings.model, "bge-small-en-v1.5");
        assert_eq!(cfg.embeddings.cache_dir, "~/.cache/hallouminate/fastembed");

        assert_eq!(cfg.watch.debounce_ms, 500);
        assert_eq!(cfg.storage.ground_dir, "~/.local/share/hallouminate/ground");
    }

    #[test]
    fn parse_empty_string_yields_full_defaults() {
        let cfg = parse("", None).expect("empty toml parses");
        assert!(cfg.corpora.is_empty());
        assert!(cfg.code_repos.is_empty());
        assert_eq!(cfg.search, SearchConfig::default());
        assert_eq!(cfg.embeddings, EmbeddingsConfig::default());
        assert_eq!(cfg.watch, WatchConfig::default());
        assert_eq!(cfg.storage, StorageConfig::default());
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
    fn xdg_config_path_resolves_under_home() {
        let path = xdg_config_path();
        assert!(
            path.ends_with(".config/hallouminate/config.toml"),
            "got {}",
            path.display()
        );
        assert!(path.is_absolute(), "tilde must expand: {}", path.display());
    }
}
