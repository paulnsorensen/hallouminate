//! Tool registrations for the hallouminate MCP server. Handlers keep the
//! filesystem as the source of truth and emit a `CallToolResult` carrying both
//! a human-readable text block (outline / summary) and a `structured_content`
//! field with the full typed response. Token-cheap for the LLM consumer,
//! structured for the harness consumer.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
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
use crate::domain::common::{
    CorpusConfig, FileRef, Mtime, canonicalize_or_passthrough, expand_tilde,
};
use crate::domain::corpus::{MarkdownChunker, load_tokenizer, scan};
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{Format, GroundOpts, RenderOpts, ground, render};
use crate::domain::indexer::{DEFAULT_BATCH_SIZE, FileSnapshot, apply, index_corpus, plan};

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

/// Upper bound on cached `LanceStore` handles. Long-lived daemons can see
/// many `(ground_root, model)` combinations across config changes; without
/// a cap the cache grows unbounded. Eight is generous for realistic
/// per-machine corpora rotations and keeps every open LanceDB instance
/// addressable in the small.
const MAX_STORE_CACHE: usize = 8;

/// FIFO-evicting bounded cache. Insertion order is the eviction order —
/// when capacity is reached, the oldest entry is dropped to make room for
/// the new one. Touching an existing key (`get` hit) does NOT promote it;
/// FIFO is enough for the MCP store cache (entries either stay hot under
/// normal use or churn together when the config rotates), and skipping
/// LRU bookkeeping keeps `get` lock-free read-only.
struct BoundedCache<K, V>
where
    K: Eq + Hash + Clone,
{
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> BoundedCache<K, V>
where
    K: Eq + Hash + Clone,
{
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "BoundedCache capacity must be > 0");
        Self {
            capacity,
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// Insert (or replace) `key`. Returns the evicted entry, if any.
    /// Replacing an existing key updates the value in place without
    /// changing its position in the FIFO order.
    fn insert(&mut self, key: K, value: V) -> Option<(K, V)> {
        if let Some(slot) = self.map.get_mut(&key) {
            *slot = value;
            return None;
        }
        let evicted = if self.map.len() >= self.capacity {
            let old_key = self
                .order
                .pop_front()
                .expect("order desynced from map at capacity");
            let old_val = self
                .map
                .remove(&old_key)
                .expect("order key missing from map");
            Some((old_key, old_val))
        } else {
            None
        };
        self.order.push_back(key.clone());
        self.map.insert(key, value);
        evicted
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(Default)]
struct McpRuntime {
    stores: Mutex<Option<BoundedCache<StoreKey, Arc<LanceStore>>>>,
}

impl std::fmt::Debug for McpRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let store_count = self
            .stores
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|c| c.map.len()))
            .unwrap_or(0);
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
    /// versa. Cache is bounded at `MAX_STORE_CACHE` entries; daemons that
    /// rotate through many `(ground_root, model)` combinations evict the
    /// oldest handle instead of leaking.
    async fn store_for(
        &self,
        ground_root: &Path,
        model_name: &str,
    ) -> anyhow::Result<Arc<LanceStore>> {
        let key = StoreKey {
            model: model_name.to_string(),
            root: ground_root.to_path_buf(),
        };
        if let Some(store) = {
            let mut guard = self.stores.lock().expect("store cache poisoned");
            let cache = guard.get_or_insert_with(|| BoundedCache::new(MAX_STORE_CACHE));
            cache.get(&key).cloned()
        } {
            return Ok(store);
        }

        let store = Arc::new(
            LanceStore::open_or_create(ground_root, model_name)
                .await
                .map_err(anyhow::Error::from)?,
        );
        let mut guard = self.stores.lock().expect("store cache poisoned");
        let cache = guard.get_or_insert_with(|| BoundedCache::new(MAX_STORE_CACHE));
        // Lost the race? Return whatever's there now so two concurrent
        // openers converge on a single handle.
        if let Some(existing) = cache.get(&key).cloned() {
            return Ok(existing);
        }
        cache.insert(key, store.clone());
        Ok(store)
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

    /// Re-index a single freshly-written file under `corpus`. Looks up the
    /// existing `FileSnapshot` (if any) and routes the file through the
    /// shared `plan() + apply()` pipeline rather than forcing an upsert.
    /// Unchanged content (same mtime *and* same blake3) short-circuits to a
    /// no-op; same content with a fresh mtime touches the row without
    /// re-embedding. Keeps `add_markdown` latency proportional to the file
    /// being written, not the corpus, while reusing the planner's existing
    /// hash-diff logic so we don't burn embedding cycles on a redundant
    /// write.
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
        let stats = index_single_file_with(&store, &mut embedder, &chunker, corpus, file).await?;
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

/// Inner half of `McpRuntime::index_single_file`, parameterized on the
/// embedder + chunker so tests can drive it with a counting stub instead
/// of paying the real embedding-model download.
async fn index_single_file_with(
    store: &LanceStore,
    embedder: &mut dyn crate::domain::embeddings::EmbedBatch,
    chunker: &dyn crate::domain::corpus::CorpusChunker,
    corpus: &CorpusConfig,
    file: &Path,
) -> anyhow::Result<crate::domain::indexer::ApplyStats> {
    let mtime_ms = file_mtime_ms(file).await?;
    let file_ref = canonicalize_or_passthrough(file);
    let file_ref_str = file_ref
        .as_path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", file_ref.as_path().display()))?;
    let mut db: HashMap<FileRef, FileSnapshot> = HashMap::new();
    if let Some(snap) = store.get_file_snapshot(&corpus.name, file_ref_str).await? {
        db.insert(file_ref.clone(), snap);
    }
    let p = plan(vec![(file_ref, Mtime(mtime_ms))], db);
    let stats = apply(p, store, embedder, chunker, corpus, DEFAULT_BATCH_SIZE).await?;
    Ok(stats)
}

async fn file_mtime_ms(path: &Path) -> anyhow::Result<i64> {
    let meta = tokio::fs::metadata(path).await?;
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
        let dest = safe_destination(&root, &relative)
            .await
            .map_err(|e| invalid_params(e.to_string()))?;
        // `atomic_write_no_follow` stays sync because it relies on
        // `OpenOptionsExt::custom_flags(O_NOFOLLOW)`, which tokio's async
        // `OpenOptions` does not expose. Push the blocking IO to a worker
        // thread so the executor stays free for concurrent MCP requests.
        let write_dest = dest.clone();
        let write_relative = relative.clone();
        let content_bytes = params.content.into_bytes();
        let overwrite = params.overwrite;
        tokio::task::spawn_blocking(move || {
            atomic_write_no_follow(&write_dest, &content_bytes, overwrite)
        })
        .await
        .map_err(|e| internal_error(format!("write task panicked: {e}")))?
        .map_err(|WriteError { kind, source }| match kind {
            // Refusing to clobber on `overwrite=false` and refusing a
            // symlink final component are both caller-input failures —
            // surface them as JSON-RPC `invalid_params` (-32602).
            WriteErrorKind::Exists => invalid_params(format!(
                "{} already exists; pass overwrite=true to replace it",
                write_relative.display()
            )),
            WriteErrorKind::Symlink => invalid_params(format!(
                "refusing to follow symlink at {}",
                write_relative.display()
            )),
            WriteErrorKind::Io => internal_error(source.to_string()),
        })?;
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

async fn safe_destination(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    // Walk + canonicalize the parent chain to refuse escapes through any
    // intermediate symlink. The final-component symlink race is closed
    // separately by `atomic_write_no_follow` (`O_NOFOLLOW`).
    //
    // Uses `tokio::fs` to keep the async MCP handler off blocking syscalls
    // that would otherwise stall the executor under concurrent requests.
    tokio::fs::create_dir_all(root).await?;
    let canonical_root = tokio::fs::canonicalize(root).await?;
    let dest = root.join(relative);
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path must have a parent directory"))?;
    tokio::fs::create_dir_all(parent).await?;
    let canonical_parent = tokio::fs::canonicalize(parent).await?;
    if !canonical_parent.starts_with(&canonical_root) {
        anyhow::bail!("path resolves outside the corpus root");
    }
    Ok(dest)
}

#[derive(Debug)]
enum WriteErrorKind {
    /// File exists and `overwrite=false`.
    Exists,
    /// Final-component symlink rejected by `O_NOFOLLOW` (ELOOP).
    Symlink,
    /// Any other I/O failure.
    Io,
}

#[derive(Debug)]
struct WriteError {
    kind: WriteErrorKind,
    source: std::io::Error,
}

/// Atomic write that refuses to follow a symlink at the final path
/// component (`O_NOFOLLOW`). Combined with `safe_destination`'s parent
/// canonicalization, this closes the TOCTOU window between the
/// pre-flight symlink check and the write itself: even if an attacker
/// races a symlink into place after `safe_destination` returns, the
/// `open(2)` call here fails with `ELOOP` rather than following it out
/// of the corpus root. When `overwrite=false`, `O_EXCL` makes the
/// existence check + create atomic too.
fn atomic_write_no_follow(dest: &Path, content: &[u8], overwrite: bool) -> Result<(), WriteError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true);
    if overwrite {
        opts.truncate(true);
    } else {
        opts.create_new(true); // O_CREAT | O_EXCL
    }
    // O_NOFOLLOW: refuse to follow a symlink at the final path
    // component. On macOS/Linux this fails with ELOOP.
    opts.custom_flags(libc_o_nofollow());
    let mut file = opts.open(dest).map_err(|e| {
        let kind = match e.raw_os_error() {
            // ELOOP on Linux/macOS when O_NOFOLLOW hits a symlink.
            Some(40) | Some(62) => WriteErrorKind::Symlink,
            _ if e.kind() == std::io::ErrorKind::AlreadyExists => WriteErrorKind::Exists,
            _ => WriteErrorKind::Io,
        };
        WriteError { kind, source: e }
    })?;
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|e| WriteError {
            kind: WriteErrorKind::Io,
            source: e,
        })
}

/// `O_NOFOLLOW` constant. `std::os::unix` doesn't re-export it and we
/// don't pull in `libc`, so hardcode the (stable, POSIX) value. Both
/// Linux and macOS define it as 0x100.
const fn libc_o_nofollow() -> i32 {
    0x0100
}

fn list_corpus_files(corpus: &CorpusConfig) -> anyhow::Result<Vec<FileEntry>> {
    // `scan` returns canonicalized absolute paths (see
    // `canonicalize_or_passthrough` in `walker.rs`), so the strip prefix has
    // to be canonicalized too. On macOS, tempdirs differ between
    // `/var/folders/...` (the configured root) and `/private/var/folders/...`
    // (what canonicalize returns), and a non-canonicalized strip silently
    // fails — leaving absolute paths in the user-facing response.
    let roots: Vec<PathBuf> = corpus
        .paths
        .iter()
        .map(|path| {
            let expanded = expand_tilde(path);
            std::fs::canonicalize(&expanded).unwrap_or(expanded)
        })
        .collect();
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

    #[tokio::test]
    async fn safe_destination_creates_inside_corpus_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let relative = safe_relative_path("wiki/concepts/attention.md").expect("relative");
        let dest = safe_destination(dir.path(), &relative)
            .await
            .expect("safe destination");
        assert!(dest.starts_with(dir.path()));
        assert!(dest.parent().expect("parent").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn safe_destination_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let relative = safe_relative_path("link/out.md").expect("relative");
        let err = safe_destination(dir.path(), &relative)
            .await
            .expect_err("must reject escape");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_no_follow_rejects_final_component_symlink() {
        // Even when `safe_destination` returned a path it considered safe,
        // the write must refuse to follow a symlink swapped in at the
        // final component (the TOCTOU window).
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("target.md");
        std::fs::write(&outside_file, "original").expect("seed target");
        let dest = dir.path().join("raced.md");
        std::os::unix::fs::symlink(&outside_file, &dest).expect("symlink");

        let err = atomic_write_no_follow(&dest, b"clobber", true)
            .expect_err("O_NOFOLLOW must reject symlink final component");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{:?}", err);
        // Target outside the corpus root must be untouched.
        assert_eq!(
            std::fs::read_to_string(&outside_file).expect("read"),
            "original"
        );
    }

    #[test]
    fn atomic_write_no_follow_refuses_existing_without_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("out.md");
        std::fs::write(&dest, "first").expect("seed");
        let err = atomic_write_no_follow(&dest, b"second", false)
            .expect_err("existing file without overwrite must fail");
        assert!(matches!(err.kind, WriteErrorKind::Exists), "{:?}", err);
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), "first");
    }

    #[test]
    fn atomic_write_no_follow_overwrites_when_requested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("out.md");
        std::fs::write(&dest, "first").expect("seed");
        atomic_write_no_follow(&dest, b"second", true).expect("overwrite ok");
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), "second");
    }

    #[test]
    fn bounded_cache_evicts_oldest_when_capacity_exceeded() {
        // Insert capacity+1 entries; the first one inserted must be evicted
        // and the most-recent `MAX_STORE_CACHE` entries must still be
        // retrievable. Pins the FIFO contract that long-lived MCP daemons
        // depend on to keep the store cache bounded.
        let cap = MAX_STORE_CACHE;
        let mut cache: BoundedCache<usize, String> = BoundedCache::new(cap);
        for i in 0..cap {
            let evicted = cache.insert(i, format!("v{i}"));
            assert!(evicted.is_none(), "no eviction expected at i={i}");
        }
        assert_eq!(cache.len(), cap);

        // The (cap+1)th insert evicts key 0.
        let evicted = cache
            .insert(cap, format!("v{cap}"))
            .expect("must evict to make room");
        assert_eq!(evicted.0, 0, "FIFO must evict the oldest key first");
        assert_eq!(evicted.1, "v0");

        assert_eq!(cache.len(), cap, "cache must stay at capacity");
        assert!(cache.get(&0).is_none(), "key 0 must be gone");
        for i in 1..=cap {
            assert_eq!(
                cache.get(&i).map(String::as_str),
                Some(format!("v{i}").as_str()),
                "key {i} must still be cached"
            );
        }
    }

    /// Counts embed_batch calls so tests can assert hash-skip short-circuits
    /// kept the embedder cold on the second write.
    struct CountingEmbedder {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::domain::embeddings::EmbedBatch for CountingEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
        ) -> crate::domain::common::Result<Vec<[f32; crate::adapters::lance::EMBEDDING_DIM]>>
        {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Deterministic non-zero vectors so the LanceStore doesn't reject
            // them; content is irrelevant — the test only checks the
            // invocation counter.
            Ok(texts
                .iter()
                .map(|_| {
                    let mut v = [0.0_f32; crate::adapters::lance::EMBEDDING_DIM];
                    v[0] = 1.0;
                    v
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn index_single_file_with_skips_reembed_when_hash_unchanged() {
        // Two writes of identical content (same mtime _or_ updated mtime,
        // same blake3) must not invoke the embedder a second time. The
        // first pass embeds; the second pass routes through the planner's
        // hash-diff branch and either skips outright (same mtime) or
        // touches mtime only (different mtime, same hash).
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use text_splitter::Characters;

        const MODEL: &str = "BAAI/bge-small-en-v1.5";
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LanceStore::open_or_create(dir.path(), MODEL)
            .await
            .expect("open store");
        let chunker = MarkdownChunker::new(Characters, 2000);
        let corpus = CorpusConfig {
            name: "wiki".into(),
            paths: vec![],
            globs: vec![],
            exclude: vec![],
        };

        let work = tempfile::tempdir().expect("work tempdir");
        let file = work.path().join("note.md");
        std::fs::write(
            &file,
            "# Spice\nThe spice must flow across the Arrakeen sands.\n",
        )
        .expect("seed file");

        let calls = Arc::new(AtomicUsize::new(0));
        let mut embedder = CountingEmbedder {
            calls: calls.clone(),
        };

        // First write: cold path, must embed.
        index_single_file_with(&store, &mut embedder, &chunker, &corpus, &file)
            .await
            .expect("first index");
        let first_calls = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            first_calls >= 1,
            "first pass must invoke embedder at least once, got {first_calls}"
        );

        // Force a future mtime so the planner cannot rely on the
        // mtime-equal short-circuit alone; only the hash branch will
        // suppress re-embedding.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .expect("reopen for mtime bump");
        f.set_modified(later).expect("set mtime");
        drop(f);

        // Second write of the same content: must NOT invoke embedder.
        index_single_file_with(&store, &mut embedder, &chunker, &corpus, &file)
            .await
            .expect("second index");
        let second_calls = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            second_calls, first_calls,
            "second pass with identical content must skip re-embed (\
             planner-routed mtime_touch + hash match): first={first_calls} \
             second={second_calls}"
        );
    }

    #[test]
    fn bounded_cache_replacing_existing_key_does_not_evict() {
        // Updating an in-cache value must not push out any other entry —
        // protects against false eviction when the same MCP request hits a
        // key twice in quick succession.
        let mut cache: BoundedCache<&'static str, i32> = BoundedCache::new(2);
        assert!(cache.insert("a", 1).is_none());
        assert!(cache.insert("b", 2).is_none());
        assert!(cache.insert("a", 11).is_none(), "replacing must not evict");
        assert_eq!(cache.get(&"a"), Some(&11));
        assert_eq!(cache.get(&"b"), Some(&2));
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
