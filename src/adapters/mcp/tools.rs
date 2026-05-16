//! Tool registrations for the hallouminate MCP server. Handlers keep the
//! filesystem as the source of truth and emit a `CallToolResult` carrying both
//! a human-readable text block (outline / summary) and a `structured_content`
//! field with the full typed response. Token-cheap for the LLM consumer,
//! structured for the harness consumer.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::adapters::lance::LanceStore;
use crate::app::cli::{CorpusReport, IndexReport, select_corpora};
use crate::app::config;
use crate::domain::common::{CorpusConfig, Mtime, canonicalize_or_passthrough, expand_tilde};
use crate::domain::corpus::{MarkdownChunker, load_tokenizer, scan};
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{Format, GroundOpts, RenderOpts, ground, render};
use crate::domain::indexer::{DEFAULT_BATCH_SIZE, IndexPlan, Upsert, apply, index_corpus};

const SERVER_INSTRUCTIONS: &str = "Hallouminate exposes tools for semantic grounding, indexing, corpus/file discovery, and plain markdown writes. The filesystem is the source of truth; LanceDB indexes are derived and refreshed automatically after `add_markdown` writes.";

/// Build a `CallToolResult` with both a human-readable text content block
/// and a `structured_content` JSON payload. `CallToolResult` is
/// `#[non_exhaustive]` in `rmcp`, so we must construct via the provided
/// `success(...)` constructor then mutate public fields.
fn tool_ok(text: String, structured: serde_json::Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(structured);
    result
}

fn internal_error(msg: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(msg.into(), None)
}

/// JSON-RPC -32602: surface caller-supplied input failures (bad corpus name,
/// unsafe path, missing required argument) distinctly from server faults so
/// MCP clients can route them as user errors instead of retries.
fn invalid_params(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroundParams {
    /// Free-text query to embed and search against the index.
    pub query: String,
    /// Optional corpus name; required when more than one is configured.
    #[serde(default)]
    pub corpus: Option<String>,
    /// Max number of files in the rolled-up result.
    #[serde(default)]
    pub top_files: Option<usize>,
    /// Max chunks returned per file.
    #[serde(default)]
    pub chunks_per_file: Option<usize>,
    /// Internal raw-hit cap before per-file rollup.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Trim each chunk's snippet to N chars in both the outline and the
    /// structured response. Orthogonal to format selection.
    #[serde(default)]
    pub snippet_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexParams {
    /// Optional corpus name; omit to index every configured corpus.
    #[serde(default)]
    pub corpus: Option<String>,
    /// Optional path to a newline-delimited file of paths to ingest as an
    /// ad-hoc corpus. Mirrors the CLI `--paths-from` flag.
    #[serde(default)]
    pub paths_from: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCorporaParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    /// Corpus name; required when more than one corpus is configured.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path under the corpus' first configured root. The caller owns
    /// the directory structure and markdown shape.
    pub path: String,
    /// Markdown bytes to write as UTF-8 text. Hallouminate stores this verbatim
    /// and does not template or validate the markdown format.
    pub content: String,
    /// Replace an existing file. Defaults to false to avoid accidental clobber.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CorpusEntry {
    name: String,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FileEntry {
    path: String,
    absolute_path: String,
}

#[derive(Debug, Serialize)]
struct AddMarkdownResponse {
    corpus: String,
    path: String,
    absolute_path: String,
    indexed: IndexReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StoreKey {
    model: String,
    root: PathBuf,
}

#[derive(Default)]
struct McpRuntime {
    stores: Mutex<HashMap<StoreKey, Arc<LanceStore>>>,
}

impl std::fmt::Debug for McpRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let store_count = self.stores.lock().map(|stores| stores.len()).unwrap_or(0);
        f.debug_struct("McpRuntime")
            .field("store_count", &store_count)
            .finish()
    }
}

impl McpRuntime {
    /// Open (or reuse) the single LanceStore at `ground_root`. The CLI
    /// (`run_index`, `run_ground`) opens this exact directory, so sharing it
    /// here keeps both transports pointed at the same on-disk index — writes
    /// from `add_markdown` are visible to `hallouminate ground` and vice
    /// versa.
    async fn store_for(
        &self,
        ground_root: &Path,
        model_name: &str,
    ) -> anyhow::Result<Arc<LanceStore>> {
        let key = StoreKey {
            model: model_name.to_string(),
            root: ground_root.to_path_buf(),
        };
        if let Some(store) = self
            .stores
            .lock()
            .expect("store cache poisoned")
            .get(&key)
            .cloned()
        {
            return Ok(store);
        }

        let store = Arc::new(
            LanceStore::open_or_create(ground_root, model_name)
                .await
                .map_err(anyhow::Error::from)?,
        );
        let mut stores = self.stores.lock().expect("store cache poisoned");
        Ok(stores.entry(key).or_insert_with(|| store).clone())
    }

    async fn index_selected(
        &self,
        cfg: &config::Config,
        corpora: Vec<CorpusConfig>,
    ) -> anyhow::Result<IndexReport> {
        let ground_root = expand_tilde(&cfg.storage.ground_dir);
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)?;
        let tokenizer = load_tokenizer(&cfg.embeddings.model)?;
        let chunker = MarkdownChunker::new(tokenizer, 384);
        let store = self.store_for(&ground_root, &cfg.embeddings.model).await?;
        let mut report = IndexReport::default();
        for corpus in corpora {
            let stats = index_corpus(&corpus, &store, &mut embedder, &chunker).await?;
            report.corpora.push(CorpusReport {
                name: corpus.name.clone(),
                files_upserted: stats.files_upserted,
                files_touched: stats.files_touched,
                files_deleted: stats.files_deleted,
                files_skipped_empty: stats.files_skipped_empty,
                chunks_inserted: stats.chunks_inserted,
                embeddings_inserted: stats.embeddings_inserted,
            });
        }
        Ok(report)
    }

    /// Re-index a single freshly-written file under `corpus`. Skips the
    /// corpus-wide scan/plan: we know the file is the only change, so build
    /// a one-Upsert plan directly and let `apply` reuse the same write path.
    /// Keeps `add_markdown` latency O(1) in the file size rather than O(N)
    /// in the corpus size.
    async fn index_single_file(
        &self,
        cfg: &config::Config,
        corpus: &CorpusConfig,
        file: &Path,
    ) -> anyhow::Result<IndexReport> {
        let ground_root = expand_tilde(&cfg.storage.ground_dir);
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)?;
        let tokenizer = load_tokenizer(&cfg.embeddings.model)?;
        let chunker = MarkdownChunker::new(tokenizer, 384);
        let store = self.store_for(&ground_root, &cfg.embeddings.model).await?;
        let mtime_ms = file_mtime_ms(file)?;
        let file_ref = canonicalize_or_passthrough(file);
        let single_plan = IndexPlan {
            upserts: vec![Upsert {
                file: file_ref,
                mtime: Mtime(mtime_ms),
            }],
            ..IndexPlan::default()
        };
        let stats = apply(
            single_plan,
            &store,
            &mut embedder,
            &chunker,
            corpus,
            DEFAULT_BATCH_SIZE,
        )
        .await?;
        Ok(IndexReport {
            corpora: vec![CorpusReport {
                name: corpus.name.clone(),
                files_upserted: stats.files_upserted,
                files_touched: stats.files_touched,
                files_deleted: stats.files_deleted,
                files_skipped_empty: stats.files_skipped_empty,
                chunks_inserted: stats.chunks_inserted,
                embeddings_inserted: stats.embeddings_inserted,
            }],
        })
    }
}

fn file_mtime_ms(path: &Path) -> anyhow::Result<i64> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    let dur = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("pre-epoch mtime on {}", path.display()))?;
    Ok(dur.as_millis() as i64)
}

/// Long-lived MCP server handle. The LanceStore at `cfg.storage.ground_dir`
/// is cached by `(ground_root, embedding model)` so the daemon owns one
/// open LanceDB instance for the configured ground directory and reopens
/// the same on-disk store after process restarts. Sharing one store across
/// corpora matches the CLI (`run_index`, `run_ground`), which also opens
/// `ground_dir` directly — writes from MCP are immediately visible to the
/// CLI and vice versa.
#[derive(Debug, Clone)]
pub struct HallouminateTools {
    // The `tool_router` field is read by `#[tool_handler]`-generated code
    // when dispatching `tools/call`; rustc's dead-code pass doesn't see the
    // macro expansion, so silence the warning here.
    #[allow(dead_code)]
    tool_router: ToolRouter<HallouminateTools>,
    runtime: Arc<McpRuntime>,
}

#[tool_router]
impl HallouminateTools {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            runtime: Arc::new(McpRuntime::default()),
        }
    }

    #[tool(
        description = "Semantic search over the configured markdown corpora. Returns an outline view in `content` and the full GroundResponse in `structuredContent`."
    )]
    pub async fn ground(
        &self,
        Parameters(params): Parameters<GroundParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpus = pick_corpus(&cfg, params.corpus.as_deref())
            .map_err(|e| invalid_params(e.to_string()))?;
        let ground_root = expand_tilde(&cfg.storage.ground_dir);
        let store = self
            .runtime
            .store_for(&ground_root, &cfg.embeddings.model)
            .await
            .map_err(|e| internal_error(e.to_string()))?;
        let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
        let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir)
            .map_err(|e| internal_error(e.to_string()))?;
        let opts = GroundOpts {
            top_files: params.top_files.unwrap_or(cfg.search.top_files_default),
            chunks_per_file: params
                .chunks_per_file
                .unwrap_or(cfg.search.chunks_per_file_default),
            limit: params.limit.unwrap_or(50),
        };
        let response = ground(&params.query, &corpus.name, &store, &mut embedder, opts)
            .await
            .map_err(|e| internal_error(e.to_string()))?;
        let outline = render(
            &response,
            Format::Outline,
            &RenderOpts {
                snippet_chars: params.snippet_chars,
                path_prefix_strip: None,
            },
        );
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(outline, structured))
    }

    #[tool(
        description = "Build or refresh the LanceDB index for one or all configured corpora. Returns a one-line summary in `content` and the per-corpus IndexReport in `structuredContent`."
    )]
    pub async fn index(
        &self,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpora = select_corpora(&cfg, params.corpus.as_deref(), params.paths_from.as_deref())
            .map_err(|e| invalid_params(e.to_string()))?;
        let report = self
            .runtime
            .index_selected(&cfg, corpora)
            .await
            .map_err(|e| internal_error(e.to_string()))?;
        let summary = report
            .corpora
            .iter()
            .map(|c| {
                format!(
                    "{}: upserted={} touched={} deleted={} chunks+={}",
                    c.name, c.files_upserted, c.files_touched, c.files_deleted, c.chunks_inserted
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let structured =
            serde_json::to_value(&report).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(summary, structured))
    }

    #[tool(
        description = "List files currently visible in a configured corpus, honoring its paths/globs/exclude rules. Returns relative paths when a file is under a configured corpus root."
    )]
    pub async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpus = pick_corpus(&cfg, params.corpus.as_deref())
            .map_err(|e| invalid_params(e.to_string()))?;
        let entries = list_corpus_files(&corpus).map_err(|e| internal_error(e.to_string()))?;
        let text = entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let structured =
            serde_json::to_value(&entries).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Write a markdown file under a corpus root, creating parent directories as needed, then refresh that corpus' LanceDB index. Hallouminate stores content verbatim and does not impose a markdown schema."
    )]
    pub async fn add_markdown(
        &self,
        Parameters(params): Parameters<AddMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpus =
            pick_corpus(&cfg, Some(&params.corpus)).map_err(|e| invalid_params(e.to_string()))?;
        let root = first_corpus_root(&corpus).map_err(|e| invalid_params(e.to_string()))?;
        let relative =
            safe_relative_path(&params.path).map_err(|e| invalid_params(e.to_string()))?;
        let dest = safe_destination(&root, &relative).map_err(|e| invalid_params(e.to_string()))?;
        if dest.exists() && !params.overwrite {
            return Err(invalid_params(format!(
                "{} already exists; pass overwrite=true to replace it",
                relative.display()
            )));
        }
        std::fs::write(&dest, params.content).map_err(|e| internal_error(e.to_string()))?;
        let report = self
            .runtime
            .index_single_file(&cfg, &corpus, &dest)
            .await
            .map_err(|e| internal_error(e.to_string()))?;
        let response = AddMarkdownResponse {
            corpus: corpus.name,
            path: relative.to_string_lossy().into_owned(),
            absolute_path: dest.to_string_lossy().into_owned(),
            indexed: report,
        };
        let text = format!(
            "wrote {} and refreshed corpus {}",
            response.path, response.corpus
        );
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "List corpora configured in the hallouminate config file. Returns names in `content` and `{name, paths}` records in `structuredContent`."
    )]
    pub async fn list_corpora(
        &self,
        _params: Parameters<ListCorporaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let entries: Vec<CorpusEntry> = cfg
            .corpora
            .iter()
            .map(|c| CorpusEntry {
                name: c.name.clone(),
                paths: c.paths.clone(),
            })
            .collect();
        let names = entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let structured =
            serde_json::to_value(&entries).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(names, structured))
    }
}

impl Default for HallouminateTools {
    fn default() -> Self {
        Self::new()
    }
}

fn pick_corpus(cfg: &config::Config, requested: Option<&str>) -> anyhow::Result<CorpusConfig> {
    if let Some(name) = requested {
        return cfg
            .corpora
            .iter()
            .find(|corpus| corpus.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("corpus {name:?} not found in config"));
    }
    match cfg.corpora.as_slice() {
        [] => anyhow::bail!("no corpora configured; add [[corpus]] to config"),
        [only] => Ok(only.clone()),
        _ => anyhow::bail!("corpus required when multiple corpora configured; pass corpus"),
    }
}

fn first_corpus_root(corpus: &CorpusConfig) -> anyhow::Result<PathBuf> {
    let raw = corpus
        .paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("corpus {:?} has no paths", corpus.name))?;
    Ok(expand_tilde(raw))
}

fn safe_relative_path(raw: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() || path.is_absolute() {
        anyhow::bail!("path must be a non-empty relative path");
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        anyhow::bail!("path must not contain parent-directory components");
    }
    Ok(path.to_path_buf())
}

fn safe_destination(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let canonical_root = std::fs::canonicalize(root)?;
    let dest = root.join(relative);
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path must have a parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let canonical_parent = std::fs::canonicalize(parent)?;
    if !canonical_parent.starts_with(&canonical_root) {
        anyhow::bail!("path resolves outside the corpus root");
    }
    if std::fs::symlink_metadata(&dest)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        anyhow::bail!("refusing to overwrite symlink {}", relative.display());
    }
    Ok(dest)
}

fn list_corpus_files(corpus: &CorpusConfig) -> anyhow::Result<Vec<FileEntry>> {
    let roots: Vec<PathBuf> = corpus.paths.iter().map(|path| expand_tilde(path)).collect();
    let mut entries: Vec<FileEntry> = scan(corpus)
        .map_err(anyhow::Error::from)?
        .into_iter()
        .map(|(file, _)| {
            let absolute = file.into_path_buf();
            let relative = roots
                .iter()
                .find_map(|root| absolute.strip_prefix(root).ok())
                .unwrap_or(absolute.as_path());
            FileEntry {
                path: relative.to_string_lossy().into_owned(),
                absolute_path: absolute.to_string_lossy().into_owned(),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_for(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "wiki".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec!["**/drafts/**".into()],
        }
    }

    #[test]
    fn safe_relative_path_rejects_absolute_and_parent_components() {
        assert!(safe_relative_path("/tmp/out.md").is_err());
        assert!(safe_relative_path("../out.md").is_err());
        assert!(safe_relative_path("wiki/../out.md").is_err());
        assert!(safe_relative_path("").is_err());
    }

    #[test]
    fn safe_relative_path_accepts_nested_agent_owned_structure() {
        let path = safe_relative_path("wiki/concepts/attention.md").expect("valid relative path");
        assert_eq!(path, PathBuf::from("wiki/concepts/attention.md"));
    }

    #[test]
    fn safe_destination_creates_inside_corpus_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let relative = safe_relative_path("wiki/concepts/attention.md").expect("relative");
        let dest = safe_destination(dir.path(), &relative).expect("safe destination");
        assert!(dest.starts_with(dir.path()));
        assert!(dest.parent().expect("parent").exists());
    }

    #[cfg(unix)]
    #[test]
    fn safe_destination_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let relative = safe_relative_path("link/out.md").expect("relative");
        let err = safe_destination(dir.path(), &relative).expect_err("must reject escape");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[test]
    fn list_corpus_files_returns_sorted_relative_markdown_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("wiki/concepts")).expect("mkdir concepts");
        std::fs::create_dir_all(dir.path().join("wiki/drafts")).expect("mkdir drafts");
        std::fs::write(dir.path().join("wiki/overview.md"), "# Overview\n").expect("write");
        std::fs::write(
            dir.path().join("wiki/concepts/attention.md"),
            "# Attention\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("wiki/drafts/private.md"), "# Draft\n").expect("write");
        std::fs::write(dir.path().join("wiki/ignore.txt"), "ignore").expect("write");

        let files = list_corpus_files(&corpus_for(dir.path())).expect("list files");
        let paths: Vec<String> = files.into_iter().map(|entry| entry.path).collect();
        assert_eq!(
            paths,
            vec!["wiki/concepts/attention.md", "wiki/overview.md"]
        );
    }
}

#[tool_handler]
impl ServerHandler for HallouminateTools {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` and `Implementation` are both `#[non_exhaustive]` in
        // `rmcp`; construct via `Default::default()` and mutate the fields
        // we care about so we don't fight the crate's evolution constraints.
        let mut info = ServerInfo::default();
        info.server_info.name = env!("CARGO_PKG_NAME").into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(SERVER_INSTRUCTIONS.to_string());
        info
    }
}
