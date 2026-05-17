//! Tool registrations for the hallouminate MCP server. Handlers keep the
//! filesystem as the source of truth and emit a `CallToolResult` carrying both
//! a human-readable text block (outline / summary) and a `structured_content`
//! field with the full typed response. Token-cheap for the LLM consumer,
//! structured for the harness consumer.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::hash::Hash;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use globset::{Glob, GlobSet, GlobSetBuilder};
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
use crate::domain::corpus::{MarkdownChunker, blake3_file, load_tokenizer, scan};
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{Format, GroundOpts, RenderOpts, ground, render, trim_snippets};
use crate::domain::indexer::{DEFAULT_BATCH_SIZE, FileSnapshot, apply, index_corpus, plan};

const SERVER_INSTRUCTIONS: &str = "\
Hallouminate stores a markdown corpus on disk and exposes it for semantic search.

Tools:
- `list_corpora` — names of configured corpora.
- `list_files` — relative file paths in a corpus.
- `read_markdown` — verbatim file contents (UTF-8). Use before overwriting.
- `add_markdown` — write a file (atomic, no-symlink-follow). Auto-reindexes that file.
- `delete_markdown` — unlink file + prune index rows.
- `ground` — semantic search; returns ranked chunks with snippet, heading_path, line_range, score.
- `index` — bulk (re)index a corpus or all corpora.

Filesystem is the source of truth; LanceDB rows are derived and refreshed after `add_markdown` / `delete_markdown`. `index` is the only way to pick up edits made outside hallouminate.

Wiki conventions for LLM authors: one topic per file; H1 (`# Topic`) on the first line; file stem matches the slug. Multi-root corpora write all `add_markdown` / `delete_markdown` to the FIRST configured root only — keep one root if you can.
";

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

/// Translate a `WriteError` from `read_no_follow` / `unlink_no_follow` into
/// the JSON-RPC error shape MCP clients see. Path-shape failures (missing,
/// symlink, non-file) become `invalid_params` so clients route them as user
/// errors; raw I/O failures stay as `internal_error`. `verb` is the operator
/// (`"read"`, `"delete"`) used to phrase the message.
fn translate_path_error(relative: &Path, verb: &str, err: WriteError) -> ErrorData {
    let WriteError { kind, source } = err;
    match kind {
        WriteErrorKind::NotFound => {
            invalid_params(format!("{} does not exist", relative.display()))
        }
        WriteErrorKind::Symlink => invalid_params(format!(
            "refusing to {verb} symlink {}",
            relative.display()
        )),
        WriteErrorKind::InvalidPath => invalid_params(format!(
            "{} is not a regular file",
            relative.display()
        )),
        WriteErrorKind::Exists => internal_error(format!(
            "unexpected Exists from {verb} of {}: {source}",
            relative.display()
        )),
        WriteErrorKind::Io => {
            internal_error(format!("{verb} {}: {source}", relative.display()))
        }
    }
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
    /// Relative path under the corpus' FIRST configured root (multi-root
    /// corpora ignore paths[1..] for writes). The caller owns the directory
    /// structure and markdown shape — convention: `<slug>.md` or
    /// `<category>/<slug>.md`, first line `# Title`.
    pub path: String,
    /// Markdown bytes to write as UTF-8 text. Stored verbatim — hallouminate
    /// does not template or validate the markdown format.
    pub content: String,
    /// Replace an existing file. Defaults to false to avoid accidental
    /// clobber; use `read_markdown` first to inspect, then re-call with
    /// overwrite=true.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path under the corpus' first configured root, same shape as
    /// `add_markdown`. Symlinks are rejected.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path under the corpus' first configured root, same shape as
    /// `add_markdown`. Symlinks are rejected. Irreversible.
    pub path: String,
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

#[derive(Debug, Serialize)]
struct ReadMarkdownResponse {
    corpus: String,
    path: String,
    absolute_path: String,
    content: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct DeleteMarkdownResponse {
    corpus: String,
    path: String,
    absolute_path: String,
    file_ref: String,
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

    /// Remove a single file's rows from the corpus index. Does NOT touch the
    /// filesystem — caller unlinks first, then asks the store to prune. No
    /// embedder is constructed, so deletion is fast and offline-safe.
    async fn delete_indexed_file(
        &self,
        cfg: &config::Config,
        corpus_name: &str,
        file_ref: &str,
    ) -> anyhow::Result<()> {
        let ground_root = expand_tilde(&cfg.storage.ground_dir);
        let store = self.store_for(&ground_root, &cfg.embeddings.model).await?;
        store.delete_file(corpus_name, file_ref).await?;
        Ok(())
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
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", file_ref.as_path().display()))?
        .to_string();
    let existing = store.get_file_snapshot(&corpus.name, &file_ref_str).await?;
    let had_snapshot = existing.is_some();
    let mut db: HashMap<FileRef, FileSnapshot> = HashMap::new();
    if let Some(snap) = existing {
        let hash_changed_without_mtime = if snap.mtime_ms == mtime_ms {
            blake3_file(file)? != snap.content_hash.as_str()
        } else {
            false
        };
        if !hash_changed_without_mtime {
            db.insert(file_ref.clone(), snap);
        }
    }
    let p = plan(vec![(file_ref, Mtime(mtime_ms))], db);
    let mut stats = apply(p, store, embedder, chunker, corpus, DEFAULT_BATCH_SIZE).await?;
    if stats.files_skipped_empty > 0 && had_snapshot {
        store.delete_file(&corpus.name, &file_ref_str).await?;
        stats.files_deleted += 1;
    }
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
        description = "Semantic search over a markdown corpus. `content` is a ripgrep-style outline (path, summary, line_range, score, snippet). `structuredContent.docs` maps absolute_path → { corpus, score, summary, keywords, mtime, chunks: [{chunk_id, heading_path, line_range, score, snippet}] }. Defaults from config: top_files=10, chunks_per_file=3, limit=50. Snippets are full chunk text unless `snippet_chars` is set."
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
        let response = if let Some(limit) = params.snippet_chars {
            trim_snippets(&response, limit)
        } else {
            response
        };
        let outline = render(
            &response,
            Format::Outline,
            &RenderOpts {
                snippet_chars: None,
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
        description = "List files currently visible in a corpus, honoring paths/globs/exclude rules. `content` is newline-separated relative paths. `structuredContent` is an array of {path, absolute_path}. Paths are relative when the file lives under a configured corpus root, absolute otherwise."
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
        description = "Write a markdown file under the corpus' FIRST configured root, creating parent directories as needed, then refresh just that file's LanceDB rows. Atomic write, no-symlink-follow. Stores content verbatim — no markdown schema imposed. For updates, call `read_markdown` first, then re-call with `overwrite=true`."
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
        let dest = root.join(&relative);
        ensure_corpus_allows_file(&corpus, &dest).map_err(|e| invalid_params(e.to_string()))?;

        let write_root = root.clone();
        let write_relative = relative.clone();
        let error_relative = relative.clone();
        let content_bytes = params.content.into_bytes();
        let overwrite = params.overwrite;
        let dest = tokio::task::spawn_blocking(move || {
            atomic_write_no_follow(&write_root, &write_relative, &content_bytes, overwrite)
        })
        .await
        .map_err(|e| internal_error(format!("write task panicked: {e}")))?
        .map_err(|WriteError { kind, source }| match kind {
            WriteErrorKind::Exists => invalid_params(format!(
                "{} already exists; pass overwrite=true to replace it",
                error_relative.display()
            )),
            WriteErrorKind::Symlink | WriteErrorKind::InvalidPath => invalid_params(format!(
                "refusing unsafe path {}: {}",
                error_relative.display(),
                source
            )),
            // `atomic_write_no_follow` creates parents itself, so a NotFound
            // here means the kernel surfaced ENOENT on the file itself — fall
            // through to the same shape as a generic I/O failure.
            WriteErrorKind::NotFound | WriteErrorKind::Io => internal_error(source.to_string()),
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
        description = "Read verbatim UTF-8 contents of a markdown file in a corpus. `content` is the full file text; `structuredContent` is { corpus, path, absolute_path, content, bytes }. Symlinks are rejected. Returns the on-disk text, not the indexed/chunked view — call `ground` for semantic search."
    )]
    pub async fn read_markdown(
        &self,
        Parameters(params): Parameters<ReadMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpus =
            pick_corpus(&cfg, Some(&params.corpus)).map_err(|e| invalid_params(e.to_string()))?;
        let root = first_corpus_root(&corpus).map_err(|e| invalid_params(e.to_string()))?;
        let relative =
            safe_relative_path(&params.path).map_err(|e| invalid_params(e.to_string()))?;
        let dest = root.join(&relative);
        ensure_corpus_allows_file(&corpus, &dest).map_err(|e| invalid_params(e.to_string()))?;

        let read_root = root.clone();
        let read_relative = relative.clone();
        let bytes = tokio::task::spawn_blocking(move || read_no_follow(&read_root, &read_relative))
            .await
            .map_err(|e| internal_error(format!("read task panicked: {e}")))?
            .map_err(|err| translate_path_error(&relative, "read", err))?;
        let content = String::from_utf8(bytes).map_err(|e| {
            invalid_params(format!("{} is not valid UTF-8: {e}", relative.display()))
        })?;
        let byte_len = content.len() as u64;
        let response = ReadMarkdownResponse {
            corpus: corpus.name,
            path: relative.to_string_lossy().into_owned(),
            absolute_path: dest.to_string_lossy().into_owned(),
            bytes: byte_len,
            content: content.clone(),
        };
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(content, structured))
    }

    #[tool(
        description = "Unlink a markdown file from the corpus' first configured root and prune its rows from the LanceDB index. Irreversible. Symlinks are rejected. `content` is a one-line summary; `structuredContent` is { corpus, path, absolute_path, file_ref }."
    )]
    pub async fn delete_markdown(
        &self,
        Parameters(params): Parameters<DeleteMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None).map_err(|e| internal_error(e.to_string()))?;
        let corpus =
            pick_corpus(&cfg, Some(&params.corpus)).map_err(|e| invalid_params(e.to_string()))?;
        let root = first_corpus_root(&corpus).map_err(|e| invalid_params(e.to_string()))?;
        let relative =
            safe_relative_path(&params.path).map_err(|e| invalid_params(e.to_string()))?;
        let dest = root.join(&relative);
        ensure_corpus_allows_file(&corpus, &dest).map_err(|e| invalid_params(e.to_string()))?;

        // Capture the canonical file_ref BEFORE unlinking — canonicalize
        // resolves through any intermediate directory symlinks (the corpus
        // root may be a symlinked path on macOS, e.g. /var → /private/var)
        // and matches the form stored in the chunks table by the walker.
        // `unlink_no_follow` below still rejects intermediate symlinks at
        // the syscall layer, so canonicalize is only used to produce the
        // file_ref string the LanceDB walker writes.
        let file_ref = canonicalize_or_passthrough(&dest);
        let file_ref_str = file_ref
            .as_path()
            .to_str()
            .ok_or_else(|| {
                internal_error(format!("non-utf8 path: {}", file_ref.as_path().display()))
            })?
            .to_string();

        let unlink_root = root.clone();
        let unlink_relative = relative.clone();
        tokio::task::spawn_blocking(move || unlink_no_follow(&unlink_root, &unlink_relative))
            .await
            .map_err(|e| internal_error(format!("unlink task panicked: {e}")))?
            .map_err(|err| translate_path_error(&relative, "delete", err))?;

        self.runtime
            .delete_indexed_file(&cfg, &corpus.name, &file_ref_str)
            .await
            .map_err(|e| internal_error(e.to_string()))?;

        let response = DeleteMarkdownResponse {
            corpus: corpus.name.clone(),
            path: relative.to_string_lossy().into_owned(),
            absolute_path: dest.to_string_lossy().into_owned(),
            file_ref: file_ref_str,
        };
        let text = format!("deleted {} from corpus {}", response.path, response.corpus);
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "List corpora configured in the hallouminate config file. `content` is newline-separated corpus names; `structuredContent` is an array of {name, paths} records. Run `hallouminate config validate` for a richer summary."
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
    if raw.as_bytes().last() == Some(&b'/') || raw == "." || raw.ends_with("/.") {
        anyhow::bail!("path must name a file");
    }
    if raw.starts_with("./") || raw.contains("/./") {
        anyhow::bail!("path must contain only normal file components");
    }
    if !matches!(path.components().next_back(), Some(Component::Normal(_))) {
        anyhow::bail!("path must name a file");
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("path must contain only normal file components");
    }
    Ok(path.to_path_buf())
}

fn ensure_corpus_allows_file(corpus: &CorpusConfig, path: &Path) -> anyhow::Result<()> {
    let include = build_globset(&corpus.globs)?;
    if matches!(include.as_ref(), Some(inc) if !inc.is_match(path)) {
        anyhow::bail!("path is not included by corpus globs");
    }
    let exclude = build_globset(&corpus.exclude)?;
    if matches!(exclude.as_ref(), Some(ex) if ex.is_match(path)) {
        anyhow::bail!("path is excluded by corpus rules");
    }
    Ok(())
}

fn build_globset(patterns: &[String]) -> anyhow::Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)?;
        builder.add(glob);
    }
    Ok(Some(builder.build()?))
}

#[derive(Debug)]
enum WriteErrorKind {
    /// File exists and `overwrite=false`.
    Exists,
    /// File (or an intermediate directory) does not exist. Only meaningful
    /// for read/unlink — `atomic_write_no_follow` creates missing parents.
    NotFound,
    /// A path component was a symlink while walking with `O_NOFOLLOW`.
    Symlink,
    /// The relative path names a non-directory parent or non-file target.
    InvalidPath,
    /// Any other I/O failure.
    Io,
}

#[derive(Debug)]
struct WriteError {
    kind: WriteErrorKind,
    source: std::io::Error,
}

impl WriteError {
    fn new(kind: WriteErrorKind, source: std::io::Error) -> Self {
        Self { kind, source }
    }
}

fn atomic_write_no_follow(
    root: &Path,
    relative: &Path,
    content: &[u8],
    overwrite: bool,
) -> Result<PathBuf, WriteError> {
    std::fs::create_dir_all(root).map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;
    if overwrite {
        atomic_replace(&parent, file_name.as_os_str(), content)?;
    } else {
        write_new_file(&parent, file_name.as_os_str(), content)?;
    }
    Ok(root.join(relative))
}

fn normal_components(path: &Path) -> Result<Vec<OsString>, WriteError> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            _ => {
                return Err(invalid_path_error(
                    "path must contain only normal file components",
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(invalid_path_error("path must name a file"));
    }
    Ok(out)
}

fn open_parent_dir(root: &Path, dirs: &[OsString]) -> Result<OwnedFd, WriteError> {
    let mut current = open_root_dir(root)?;
    for dir in dirs {
        current = open_or_create_child_dir(current.as_raw_fd(), dir.as_os_str())?;
    }
    Ok(current)
}

/// Walk `dirs` from `root` without creating anything. Used by read/unlink
/// paths where missing components are an error, not an implicit mkdir.
/// Re-classifies ENOENT as `NotFound` so callers can distinguish a missing
/// intermediate directory from generic I/O failure.
fn open_parent_dir_no_create(root: &Path, dirs: &[OsString]) -> Result<OwnedFd, WriteError> {
    let mut current = open_root_dir(root)?;
    for dir in dirs {
        let c_name = cstring(dir.as_os_str())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let fd = unsafe { libc::openat(current.as_raw_fd(), c_name.as_ptr(), flags) };
        if fd == -1 {
            return Err(classify_open_error(std::io::Error::last_os_error()));
        }
        current = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(current)
}

/// Read `relative` under `root` rejecting any symlink encountered along the
/// path. Mirrors `atomic_write_no_follow`'s safety contract: every component
/// is opened with `O_NOFOLLOW`, including the final file.
fn read_no_follow(root: &Path, relative: &Path) -> Result<Vec<u8>, WriteError> {
    use std::io::Read;
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir_no_create(root, &names[..names.len() - 1])?;
    let c_name = cstring(file_name.as_os_str())?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags) };
    if fd == -1 {
        return Err(classify_open_error(std::io::Error::last_os_error()));
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = file
        .metadata()
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    if !meta.is_file() {
        return Err(invalid_path_error("target is not a regular file"));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut buf)
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    Ok(buf)
}

/// Unlink `relative` under `root` rejecting any symlink. Uses
/// `fstatat(..., AT_SYMLINK_NOFOLLOW)` on the final component before
/// `unlinkat` so a symlinked final file cannot be removed and intermediate
/// symlinked directories are caught during the `open_parent_dir_no_create`
/// walk.
fn unlink_no_follow(root: &Path, relative: &Path) -> Result<(), WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir_no_create(root, &names[..names.len() - 1])?;
    let c_name = cstring(file_name.as_os_str())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        return Err(classify_open_error(std::io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        return Err(WriteError::new(
            WriteErrorKind::Symlink,
            std::io::Error::from_raw_os_error(libc::ELOOP),
        ));
    }
    if file_type != libc::S_IFREG {
        return Err(invalid_path_error("target is not a regular file"));
    }
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc == -1 {
        return Err(WriteError::new(
            WriteErrorKind::Io,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn open_root_dir(root: &Path) -> Result<OwnedFd, WriteError> {
    let c_root = cstring(root.as_os_str())?;
    let fd = unsafe {
        libc::open(
            c_root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    fd_to_owned(fd, WriteErrorKind::Io)
}

fn open_or_create_child_dir(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, WriteError> {
    match open_child_dir_no_follow(parent_fd, name) {
        Ok(fd) => Ok(fd),
        Err(err) if err.source.raw_os_error() == Some(libc::ENOENT) => {
            let c_name = cstring(name)?;
            let made = unsafe { libc::mkdirat(parent_fd, c_name.as_ptr(), 0o755) };
            if made == -1 {
                let source = std::io::Error::last_os_error();
                if source.raw_os_error() != Some(libc::EEXIST) {
                    return Err(classify_path_error(source));
                }
            }
            open_child_dir_no_follow(parent_fd, name)
        }
        Err(err) => Err(err),
    }
}

fn open_child_dir_no_follow(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, WriteError> {
    let c_name = cstring(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    fd_to_owned(fd, WriteErrorKind::Io).map_err(|err| classify_path_error(err.source))
}

fn write_new_file(parent: &OwnedFd, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    let c_name = cstring(name)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags, 0o644) };
    if fd == -1 {
        return Err(classify_create_error(std::io::Error::last_os_error()));
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    write_and_sync(&mut file, content)?;
    drop(file);
    fsync_dir(parent)
}

fn atomic_replace(parent: &OwnedFd, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    validate_replace_target(parent.as_raw_fd(), name)?;
    let (temp_name, mut file) = create_temp_file(parent.as_raw_fd(), name)?;
    if let Err(err) = write_and_sync(&mut file, content) {
        cleanup_temp(parent.as_raw_fd(), &temp_name);
        return Err(err);
    }
    drop(file);

    let c_name = cstring(name)?;
    let renamed = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            temp_name.as_ptr(),
            parent.as_raw_fd(),
            c_name.as_ptr(),
        )
    };
    if renamed == -1 {
        let source = std::io::Error::last_os_error();
        cleanup_temp(parent.as_raw_fd(), &temp_name);
        return Err(classify_create_error(source));
    }
    fsync_dir(parent)
}

fn validate_replace_target(parent_fd: i32, name: &OsStr) -> Result<(), WriteError> {
    let c_name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(classify_create_error(source));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        return Err(WriteError::new(
            WriteErrorKind::Symlink,
            std::io::Error::from_raw_os_error(libc::ELOOP),
        ));
    }
    if file_type != libc::S_IFREG {
        return Err(invalid_path_error("target is not a regular file"));
    }
    Ok(())
}

fn create_temp_file(parent_fd: i32, name: &OsStr) -> Result<(CString, std::fs::File), WriteError> {
    for attempt in 0..100 {
        let mut temp = OsString::from(".");
        temp.push(name);
        temp.push(format!(
            ".hallouminate-{}-{attempt}.tmp",
            std::process::id()
        ));
        let c_temp = cstring(temp.as_os_str())?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let fd = unsafe { libc::openat(parent_fd, c_temp.as_ptr(), flags, 0o644) };
        if fd != -1 {
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            return Ok((c_temp, file));
        }
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::EEXIST) {
            return Err(classify_create_error(source));
        }
    }
    Err(WriteError::new(
        WriteErrorKind::Io,
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary filename collision",
        ),
    ))
}

fn write_and_sync(file: &mut std::fs::File, content: &[u8]) -> Result<(), WriteError> {
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))
}

fn fsync_dir(dir: &OwnedFd) -> Result<(), WriteError> {
    let rc = unsafe { libc::fsync(dir.as_raw_fd()) };
    if rc == -1 {
        Err(WriteError::new(
            WriteErrorKind::Io,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

fn cleanup_temp(parent_fd: i32, name: &CString) {
    unsafe {
        libc::unlinkat(parent_fd, name.as_ptr(), 0);
    }
}

fn fd_to_owned(fd: i32, default: WriteErrorKind) -> Result<OwnedFd, WriteError> {
    if fd == -1 {
        Err(WriteError::new(default, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn cstring(name: &OsStr) -> Result<CString, WriteError> {
    CString::new(name.as_bytes()).map_err(|_| invalid_path_error("path contains a NUL byte"))
}

fn classify_path_error(source: std::io::Error) -> WriteError {
    match source.raw_os_error() {
        Some(errno) if errno == libc::ELOOP => WriteError::new(WriteErrorKind::Symlink, source),
        Some(errno) if errno == libc::ENOTDIR => {
            WriteError::new(WriteErrorKind::InvalidPath, source)
        }
        _ => WriteError::new(WriteErrorKind::Io, source),
    }
}

/// Classifier for read/unlink open paths: ENOENT becomes NotFound so the
/// caller can render "does not exist" without leaking errno, while ELOOP /
/// ENOTDIR keep their write-path meanings.
fn classify_open_error(source: std::io::Error) -> WriteError {
    match source.raw_os_error() {
        Some(errno) if errno == libc::ENOENT => WriteError::new(WriteErrorKind::NotFound, source),
        Some(errno) if errno == libc::ELOOP => WriteError::new(WriteErrorKind::Symlink, source),
        Some(errno) if errno == libc::ENOTDIR => {
            WriteError::new(WriteErrorKind::InvalidPath, source)
        }
        _ => WriteError::new(WriteErrorKind::Io, source),
    }
}

fn classify_create_error(source: std::io::Error) -> WriteError {
    match source.raw_os_error() {
        Some(errno) if errno == libc::ELOOP => WriteError::new(WriteErrorKind::Symlink, source),
        Some(errno) if errno == libc::EEXIST => WriteError::new(WriteErrorKind::Exists, source),
        Some(errno) if errno == libc::ENOTDIR || errno == libc::EISDIR => {
            WriteError::new(WriteErrorKind::InvalidPath, source)
        }
        _ => WriteError::new(WriteErrorKind::Io, source),
    }
}

fn invalid_path_error(msg: &'static str) -> WriteError {
    WriteError::new(
        WriteErrorKind::InvalidPath,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, msg),
    )
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
    fn safe_relative_path_rejects_absolute_parent_and_non_file_components() {
        for (raw, expected) in [
            ("/tmp/out.md", "path must be a non-empty relative path"),
            ("", "path must be a non-empty relative path"),
            (".", "path must name a file"),
            ("dir/.", "path must name a file"),
            ("dir/", "path must name a file"),
        ] {
            let err = safe_relative_path(raw).expect_err("path must be rejected");
            assert_eq!(err.to_string(), expected);
        }
        for raw in ["../out.md", "wiki/../out.md", "./out.md", "wiki/./out.md"] {
            let err = safe_relative_path(raw).expect_err("non-normal path must be rejected");
            assert_eq!(
                err.to_string(),
                "path must contain only normal file components"
            );
        }
    }

    #[test]
    fn safe_relative_path_accepts_nested_agent_owned_structure() {
        let path = safe_relative_path("wiki/concepts/attention.md").expect("valid relative path");
        assert_eq!(path, PathBuf::from("wiki/concepts/attention.md"));
    }

    #[test]
    fn corpus_rules_reject_non_matching_and_excluded_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = corpus_for(dir.path());
        ensure_corpus_allows_file(&corpus, &dir.path().join("wiki/overview.md"))
            .expect("matching markdown path must be allowed");
        let err = ensure_corpus_allows_file(&corpus, &dir.path().join("wiki/ignore.txt"))
            .expect_err("non-matching extension must be rejected");
        assert_eq!(err.to_string(), "path is not included by corpus globs");
        let err = ensure_corpus_allows_file(&corpus, &dir.path().join("wiki/drafts/private.md"))
            .expect_err("excluded path must be rejected");
        assert_eq!(err.to_string(), "path is excluded by corpus rules");
    }

    #[test]
    fn atomic_write_no_follow_creates_parent_dirs_inside_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let relative = safe_relative_path("wiki/concepts/attention.md").expect("relative");
        let dest = atomic_write_no_follow(dir.path(), &relative, b"# Attention\n", false)
            .expect("safe write");
        assert!(dest.starts_with(dir.path()));
        assert!(dest.parent().expect("parent").exists());
        assert_eq!(
            std::fs::read_to_string(dest).expect("read"),
            "# Attention\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_no_follow_rejects_intermediate_symlink_without_creating_outside_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let relative = safe_relative_path("link/new/out.md").expect("relative");

        let err = atomic_write_no_follow(dir.path(), &relative, b"# Escape\n", false)
            .expect_err("must reject intermediate symlink");

        assert!(
            matches!(
                err.kind,
                WriteErrorKind::Symlink | WriteErrorKind::InvalidPath
            ),
            "{:?}",
            err
        );
        assert!(
            !outside.path().join("new").exists(),
            "must not create directories through a symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_no_follow_rejects_final_component_symlink() {
        // Even when path validation returned a relative path it considered
        // safe, the write must refuse to follow a symlink swapped in at the
        // final component.
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("target.md");
        std::fs::write(&outside_file, "original").expect("seed target");
        std::os::unix::fs::symlink(&outside_file, dir.path().join("raced.md")).expect("symlink");

        let err = atomic_write_no_follow(dir.path(), Path::new("raced.md"), b"clobber", true)
            .expect_err("O_NOFOLLOW must reject symlink final component");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{:?}", err);
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
        let err = atomic_write_no_follow(dir.path(), Path::new("out.md"), b"second", false)
            .expect_err("existing file without overwrite must fail");
        assert!(matches!(err.kind, WriteErrorKind::Exists), "{:?}", err);
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), "first");
    }

    #[test]
    fn atomic_write_no_follow_overwrites_when_requested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("out.md");
        std::fs::write(&dest, "first").expect("seed");
        atomic_write_no_follow(dir.path(), Path::new("out.md"), b"second", true)
            .expect("overwrite ok");
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

    #[tokio::test]
    async fn index_single_file_with_deletes_old_rows_when_new_content_has_no_chunks() {
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
        std::fs::write(&file, "# Spice\nThe spice must flow.\n").expect("seed file");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut embedder = CountingEmbedder { calls };

        index_single_file_with(&store, &mut embedder, &chunker, &corpus, &file)
            .await
            .expect("first index");
        let file_ref = canonicalize_or_passthrough(&file);
        let file_ref = file_ref.as_path().to_str().expect("utf8 file ref");
        let snap = store
            .get_file_snapshot(&corpus.name, file_ref)
            .await
            .expect("snapshot lookup")
            .expect("seed index must create rows");
        assert_eq!(snap.corpus, corpus.name);
        assert_eq!(snap.file_ref, file_ref);

        std::fs::write(&file, "\n").expect("write empty markdown");
        let stats = index_single_file_with(&store, &mut embedder, &chunker, &corpus, &file)
            .await
            .expect("empty reindex");

        assert_eq!(stats.files_skipped_empty, 1);
        assert_eq!(stats.files_deleted, 1);
        let snap = store
            .get_file_snapshot(&corpus.name, file_ref)
            .await
            .expect("snapshot lookup");
        assert_eq!(snap, None, "empty rewrite must delete stale rows");
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
