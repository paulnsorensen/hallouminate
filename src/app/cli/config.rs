use std::fs;
use std::path::PathBuf;

use anyhow::{Context, anyhow};

use crate::app::config::{self, Config, xdg_config_path};
use crate::domain::common::expand_tilde;
use crate::domain::corpus::load_tokenizer;
use crate::domain::embeddings::Embedder;

const DEFAULT_TEMPLATE: &str = include_str!("config-default.toml");

#[derive(Debug, Default)]
pub struct ConfigInitArgs {
    pub force: bool,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct ConfigShowArgs {
    pub config: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct ConfigDownloadArgs {
    pub config: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct ConfigValidateArgs {
    pub config: Option<PathBuf>,
}

/// Top-level keys hallouminate recognizes in its TOML config. Used by
/// `cmd_config_validate` to flag misspellings (`[[corpora]]`, `Storage`)
/// that parse cleanly but produce a silently empty/wrong config.
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "corpus",
    "repository",
    "search",
    "embeddings",
    "watch",
    "storage",
];

pub fn cmd_config_init(args: ConfigInitArgs) -> anyhow::Result<()> {
    let target = args.path.unwrap_or_else(xdg_config_path);
    if target.exists() && !args.force {
        return Err(anyhow!(
            "config already exists at {}; pass --force to overwrite",
            target.display()
        ));
    }
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&target, DEFAULT_TEMPLATE).with_context(|| format!("write {}", target.display()))?;
    println!("wrote {}", target.display());
    Ok(())
}

pub fn cmd_config_show(args: ConfigShowArgs) -> anyhow::Result<()> {
    let cfg = config::load(args.config.as_deref())?;
    print!("{}", render_config(&cfg)?);
    Ok(())
}

pub fn cmd_config_download(args: ConfigDownloadArgs) -> anyhow::Result<()> {
    let cfg = config::load(args.config.as_deref())?;
    let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
    let _embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)
        .with_context(|| format!("download embedding model {}", cfg.embeddings.model))?;
    let _tokenizer = load_tokenizer(&cfg.embeddings.model)
        .with_context(|| format!("download tokenizer for {}", cfg.embeddings.model))?;
    println!("downloaded {}", cfg.embeddings.model);
    Ok(())
}

fn render_config(cfg: &Config) -> anyhow::Result<String> {
    toml::to_string_pretty(cfg).context("render config as TOML")
}

pub fn cmd_config_validate(args: ConfigValidateArgs) -> anyhow::Result<()> {
    let resolved = args.config.clone().unwrap_or_else(xdg_config_path);
    let raw = match fs::read_to_string(&resolved) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("read {}: {e}", resolved.display())),
    };
    let cfg = config::load(args.config.as_deref())
        .with_context(|| format!("parse {}", resolved.display()))?;

    println!("config: {}", resolved.display());
    if raw.is_none() {
        println!("(file missing — showing defaults)");
    }
    println!("embeddings.model:    {}", cfg.embeddings.model);
    println!("embeddings.cache_dir: {}", cfg.embeddings.cache_dir);
    println!("storage.ground_dir:  {}", cfg.storage.ground_dir);
    println!(
        "search.top_files:    {}  chunks_per_file: {}",
        cfg.search.top_files_default, cfg.search.chunks_per_file_default
    );
    println!("corpora ({}):", cfg.corpora.len());
    for c in &cfg.corpora {
        println!(
            "  - {}  paths={:?}  globs={:?}  exclude={:?}",
            c.name, c.paths, c.globs, c.exclude
        );
    }
    println!("repositories ({}):", cfg.repositories.len());
    for r in &cfg.repositories {
        println!(
            "  - {}  path={:?}  corpus_paths={:?}",
            r.name, r.path, r.corpus_paths
        );
    }
    match cfg.effective_corpora() {
        Ok(effective) => {
            println!("effective corpora ({}):", effective.len());
            for c in &effective {
                println!("  - {}", c.name);
            }
        }
        Err(e) => {
            println!("effective corpora: error: {e}");
        }
    }

    let warnings = collect_warnings(raw.as_deref(), &cfg);
    if warnings.is_empty() {
        println!("ok");
        return Ok(());
    }
    println!();
    for w in &warnings {
        println!("warning: {w}");
    }
    Err(anyhow!("{} warning(s); see above", warnings.len()))
}

fn collect_warnings(raw: Option<&str>, cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(raw) = raw {
        match toml::from_str::<toml::Value>(raw) {
            Ok(toml::Value::Table(table)) => {
                for key in table.keys() {
                    if !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
                        let hint = match key.as_str() {
                            "corpora" => " (did you mean `[[corpus]]`?)",
                            "code_repo" | "code_repos" => " (renamed to `[[repository]]`)",
                            "repositories" => " (did you mean `[[repository]]`?)",
                            _ => "",
                        };
                        out.push(format!("unknown top-level key `{key}`{hint}"));
                    }
                }
            }
            Ok(_) => out.push("config is not a TOML table".to_string()),
            Err(e) => out.push(format!("re-parse for key check failed: {e}")),
        }
    }
    // Count effective corpora (explicit `[[corpus]]` + repository-derived
    // `repo:*:wiki` / `repo:*:corpus`) so a repository-only config doesn't
    // get falsely flagged as empty when the daemon would happily serve it.
    let effective_empty = cfg
        .effective_corpora()
        .map(|c| c.is_empty())
        .unwrap_or(true);
    if effective_empty {
        out.push(
            "no corpora configured — `ground`, `index`, and `add_markdown` will all error \
             until you add at least one `[[corpus]]` or `[[repository]]` entry"
                .to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_default_template_to_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        cmd_config_init(ConfigInitArgs {
            force: false,
            path: Some(target.clone()),
        })
        .expect("init writes file");
        let written = fs::read_to_string(&target).expect("read written config");
        assert_eq!(written, DEFAULT_TEMPLATE);
    }

    #[test]
    fn init_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c/config.toml");
        cmd_config_init(ConfigInitArgs {
            force: false,
            path: Some(nested.clone()),
        })
        .expect("init creates parents");
        assert!(nested.exists(), "nested file not created");
    }

    #[test]
    fn init_refuses_to_overwrite_existing_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        fs::write(&target, "# pre-existing").expect("seed");
        let err = cmd_config_init(ConfigInitArgs {
            force: false,
            path: Some(target.clone()),
        })
        .expect_err("must refuse overwrite");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "# pre-existing",
            "must not clobber"
        );
    }

    #[test]
    fn init_overwrites_existing_when_force_is_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        fs::write(&target, "# pre-existing").expect("seed");
        cmd_config_init(ConfigInitArgs {
            force: true,
            path: Some(target.clone()),
        })
        .expect("force overwrite");
        assert_eq!(fs::read_to_string(&target).unwrap(), DEFAULT_TEMPLATE);
    }

    #[test]
    fn default_template_parses_to_valid_config() {
        let cfg: Config = toml::from_str(DEFAULT_TEMPLATE).expect("template must be valid TOML");
        assert!(cfg.corpora.is_empty(), "corpora must start commented-out");
        assert_eq!(cfg.embeddings.model, "BAAI/bge-small-en-v1.5");
        assert_eq!(cfg.search.top_files_default, 10);
    }

    #[test]
    fn show_renders_loaded_config_as_toml_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        cmd_config_init(ConfigInitArgs {
            force: false,
            path: Some(path.clone()),
        })
        .expect("seed");
        let cfg = config::load(Some(&path)).expect("load");
        let rendered = render_config(&cfg).expect("render");
        let reparsed: Config = toml::from_str(&rendered).expect("rendered TOML reparses");
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn show_returns_defaults_when_path_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent.toml");
        let cfg = config::load(Some(&missing)).expect("missing → defaults");
        let rendered = render_config(&cfg).expect("render defaults");
        assert!(
            rendered.contains("BAAI/bge-small-en-v1.5"),
            "missing model in defaults: {rendered}"
        );
    }

    #[test]
    fn validate_flags_corpora_typo_and_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // `[[corpora]]` instead of `[[corpus]]` is the classic typo —
        // parses cleanly but produces a silently empty corpora list.
        fs::write(
            &path,
            r#"
[[corpora]]
name = "wiki"
paths = ["/tmp/wiki"]
"#,
        )
        .expect("write config");
        let err = cmd_config_validate(ConfigValidateArgs { config: Some(path) })
            .expect_err("typo must surface a warning");
        let msg = format!("{err:#}");
        assert!(msg.contains("warning"), "{msg}");
    }

    #[test]
    fn validate_accepts_config_with_one_corpus_and_no_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[[corpus]]
name = "wiki"
paths = ["/tmp/wiki"]
"#,
        )
        .expect("write config");
        cmd_config_validate(ConfigValidateArgs { config: Some(path) })
            .expect("valid config must return Ok");
    }

    #[test]
    fn collect_warnings_flags_unknown_top_level_keys_with_hints() {
        let raw = r#"
[[corpora]]
name = "wiki"

[[code_repos]]
name = "self"
"#;
        let cfg = toml::from_str::<Config>(raw).expect("parse skeleton");
        let warnings = collect_warnings(Some(raw), &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("`corpora`")
                && w.contains("[[corpus]]")),
            "missing corpora hint: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("`code_repos`")
                && w.contains("renamed to `[[repository]]`")),
            "missing code_repos hint: {warnings:?}"
        );
    }

    #[test]
    fn collect_warnings_flags_empty_corpora_even_for_valid_keys() {
        // Only known keys — but no corpora configured. Tools that need a
        // corpus would silently error at call time without this warning.
        let raw = r#"
[storage]
ground_dir = "~/.local/share/hallouminate"
"#;
        let cfg = toml::from_str::<Config>(raw).expect("parse storage-only");
        let warnings = collect_warnings(Some(raw), &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("no corpora configured")),
            "missing empty-corpora warning: {warnings:?}"
        );
    }

    #[test]
    fn collect_warnings_returns_empty_for_valid_config_with_one_corpus() {
        let raw = r#"
[[corpus]]
name = "wiki"
paths = ["/tmp/wiki"]
"#;
        let cfg = toml::from_str::<Config>(raw).expect("parse valid");
        let warnings = collect_warnings(Some(raw), &cfg);
        assert!(warnings.is_empty(), "expected no warnings, got {warnings:?}");
    }

    #[test]
    fn download_rejects_unsupported_model_during_config_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[embeddings]
model = "clip-vit-b32"
"#,
        )
        .expect("write config");
        let err = cmd_config_download(ConfigDownloadArgs { config: Some(path) })
            .expect_err("unsupported model must fail before download");
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported embedding model"), "{msg}");
    }

    // ── collect_warnings ──────────────────────────────────────────────
    //
    // Spec gate: `config validate` must surface a `code_repo → repository`
    // rename hint and flag obvious typos so a user who upgrades doesn't end
    // up with a silently-empty config because their old key parsed-and-was-
    // ignored.

    #[test]
    fn collect_warnings_flags_code_repo_with_rename_hint() {
        let raw = "[[code_repo]]\nname = \"tern\"\npath = \"/r\"\n";
        let cfg = Config::default();
        let warnings = collect_warnings(Some(raw), &cfg);
        let hint = warnings
            .iter()
            .find(|w| w.contains("code_repo"))
            .unwrap_or_else(|| panic!("missing code_repo hint: {warnings:?}"));
        assert!(
            hint.contains("renamed to `[[repository]]`"),
            "hint must spell out the rename target: {hint}"
        );
    }

    #[test]
    fn collect_warnings_flags_code_repos_plural_with_same_rename_hint() {
        let raw = "[[code_repos]]\nname = \"tern\"\npath = \"/r\"\n";
        let cfg = Config::default();
        let warnings = collect_warnings(Some(raw), &cfg);
        let hint = warnings
            .iter()
            .find(|w| w.contains("code_repos"))
            .unwrap_or_else(|| panic!("missing code_repos hint: {warnings:?}"));
        assert!(
            hint.contains("renamed to `[[repository]]`"),
            "plural alias must point at the same target: {hint}"
        );
    }

    #[test]
    fn collect_warnings_flags_corpora_typo_with_corpus_hint() {
        let raw = "[[corpora]]\nname = \"docs\"\npaths = [\"/x\"]\n";
        let cfg = Config::default();
        let warnings = collect_warnings(Some(raw), &cfg);
        let hint = warnings
            .iter()
            .find(|w| w.contains("corpora"))
            .unwrap_or_else(|| panic!("missing corpora hint: {warnings:?}"));
        assert!(
            hint.contains("[[corpus]]"),
            "corpora hint must point at the singular: {hint}"
        );
    }

    #[test]
    fn collect_warnings_flags_repositories_plural_typo_with_repository_hint() {
        let raw = "[[repositories]]\nname = \"tern\"\npath = \"/r\"\n";
        let cfg = Config::default();
        let warnings = collect_warnings(Some(raw), &cfg);
        let hint = warnings
            .iter()
            .find(|w| w.contains("repositories"))
            .unwrap_or_else(|| panic!("missing repositories hint: {warnings:?}"));
        assert!(
            hint.contains("[[repository]]"),
            "plural typo hint must point at singular: {hint}"
        );
    }

    #[test]
    fn collect_warnings_flags_unknown_key_without_specific_hint() {
        // An unrecognized key that isn't in the typo table still surfaces as
        // a warning so a user spots it; the hint suffix is empty.
        let raw = "[zzz_unknown]\nfoo = 1\n";
        let cfg = Config::default();
        let warnings = collect_warnings(Some(raw), &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("zzz_unknown")
                && !w.contains("did you mean")
                && !w.contains("renamed to")),
            "unknown key must surface as a plain warning: {warnings:?}"
        );
    }

    #[test]
    fn collect_warnings_is_silent_when_every_top_level_key_is_known() {
        let raw = "[[corpus]]\nname = \"docs\"\npaths = [\"/x\"]\n\n[[repository]]\nname = \"tern\"\npath = \"/r\"\n\n[search]\n[embeddings]\n[watch]\n[storage]\n";
        let mut cfg = Config::default();
        cfg.corpora.push(crate::domain::common::CorpusConfig {
            name: "docs".into(),
            paths: vec!["/x".into()],
            globs: Vec::new(),
            exclude: Vec::new(),
        });
        let warnings = collect_warnings(Some(raw), &cfg);
        assert!(
            warnings.is_empty(),
            "known keys + non-empty corpora must produce zero warnings: {warnings:?}"
        );
    }

    #[test]
    fn collect_warnings_surfaces_empty_corpora_advisory_when_no_corpora_present() {
        // Empty cfg.corpora produces the "no corpora configured" advisory so
        // `config validate` exits non-zero rather than silently shipping an
        // unusable setup.
        let cfg = Config::default();
        let warnings = collect_warnings(None, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("no corpora configured")),
            "empty corpora advisory missing: {warnings:?}"
        );
    }
}
