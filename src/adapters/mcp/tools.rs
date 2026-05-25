//! Tool registrations for the hallouminate MCP server. Every stateful tool
//! is a proxy to the local daemon: opening a `DaemonClient` and dispatching
//! one RPC per call. Keeps the daemon as the canonical owner of the LanceDB
//! ground directory and per-corpus mutation locks per the spec's Approach.
//!
//! When the daemon is unreachable (e.g. no `hallouminate daemon` running),
//! tool calls return JSON-RPC `-32603 internal_error` with the documented
//! "daemon unavailable" hint instead of silently opening a local LanceDB
//! handle — that fallback is exactly the multi-process race the daemon is
//! built to remove.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::app::daemon::{
    AddMarkdownRequest, AddMarkdownResult, DaemonClient, DaemonRequest, DaemonRequestPayload,
    DaemonRpcError, DeleteMarkdownRequest, DeleteMarkdownResult, ErrorKind,
    GlobalizeMarkdownRequest, GlobalizeMarkdownResult, GroundRequest, GroundResult, IndexRequest,
    ListCorporaResult, ListFilesRequest, ListFilesResult, ListTreeRequest, ListTreeResult,
    ReadMarkdownRequest, ReadMarkdownResult, client_for,
};

const SERVER_INSTRUCTIONS: &str = "\
Hallouminate stores per-repository markdown wikis on disk and exposes them \
for semantic search. Each `[[repository]]` entry derives a `repo:{name}:wiki` \
corpus rooted at `<repo>/.hallouminate/wiki/`. The wiki is the canonical \
place for cross-session knowledge: architecture, conventions, gotchas, \
\"why this design not that one\" notes.

Two audiences use this server:
- CONSUMERS (grounding agents) read the wiki via `ground` / `read_markdown` \
  / `list_files` / `list_tree`.
- AUTHORS (curator agents) write entries via `add_markdown` / overwrite via \
  `read_markdown` + `add_markdown { overwrite: true }`.

Default corpus: tool calls that omit `corpus` default to the wiki for the \
repository containing the daemon's current working directory. Pass `corpus` \
explicitly to target another wiki, the repo's source corpus \
(`repo:{name}:corpus`), or a user-declared `[[corpus]]` entry. \
`list_corpora` enumerates everything available.

Tools:
- `list_corpora` — every configured corpus name.
- `list_files` — flat list of relative paths in a corpus.
- `list_tree` — same files grouped into a directory tree with subdirs; use \
  this to navigate progressively-disclosed wikis instead of reading every \
  index.md in sequence.
- `read_markdown` — verbatim file contents (UTF-8). Call before overwriting.
- `add_markdown` — write a file (atomic, no-symlink-follow). Reindexes that \
  file AND walks ancestor `index.md`s to refresh the link tree.
- `delete_markdown` — unlink file + prune index rows + refresh ancestor \
  indexes.
- `ground` — semantic search; returns ranked chunks with snippet, \
  heading_path, line_range, score.
- `index` — bulk (re)index a corpus or all corpora.

Filesystem is the source of truth; LanceDB rows are derived and refreshed \
after `add_markdown` / `delete_markdown`. `index` is the only way to pick \
up edits made outside hallouminate.

# Authoring conventions (REQUIRED for `add_markdown`)

ONE TOPIC PER FILE. A wiki entry is a slice of knowledge with a clear \
scope. The chunker splits on headings — two unrelated topics in one file \
make `ground` rank both sections together, which is rarely what you want.

FIRST NON-BLANK LINE IS H1. Every file's first non-blank line must be \
`# Topic Name`. The chunker uses the H1 as the breadcrumb root; without \
it, search results lose navigability. The H1 is also what the auto-index \
quotes as each entry's gloss.

FILE STEM MATCHES THE SLUG. \"Corpus walker\" → `corpus-walker.md`. \
Lowercase, kebab case. No spaces, no capitals, no extensions other than \
`.md`.

LEAD WITH THE CONCLUSION. Don't bury what the file is about under \
preamble. Cite files and line ranges by path: \
`src/domain/corpus/walker.rs:42`. Prefer concrete examples to abstract \
description. ~50-150 lines per entry is the right band.

# Tree layout & linking

Subdirectories work — write `architecture/dataflow.md` and the daemon \
creates `architecture/` for you. Use them to give a wiki shape:

- Top-level files for foundational topics (`architecture.md`, \
  `mcp-surface.md`).
- Subdirectories for related entries (`adapters/lance.md`, \
  `adapters/mcp.md`, `adapters/index.md`).

Each directory carries an `index.md`. The daemon scaffolds and maintains \
the LINK LIST inside it between markers, but you OWN the prose outside.

After `add_markdown` / `delete_markdown`, the daemon walks from the corpus \
root down to the changed file's parent and refreshes the link list inside \
`<!-- HALLOUMINATE:INDEX-START -->` / `<!-- HALLOUMINATE:INDEX-END -->` \
in each ancestor `index.md`. A missing `index.md` is scaffolded; prose \
outside the markers is preserved verbatim. To opt OUT, remove the markers \
— the daemon will then leave the file alone.

Link convention: `[stem](./stem.md)` for files, \
`[subdir/](./subdir/index.md)` for directories. Use relative paths so the \
links survive moves of the whole wiki.

# Authoring loop

```
1. list_tree                          (see what's already there)
2. ground \"<topic-adjacent search>\"   (find related entries to cross-link)
3. read_markdown index.md             (confirm naming + style)
4. draft (H1 first line, kebab slug, link adjacent entries)
5. add_markdown { corpus, path, content, overwrite: false }
6. (the daemon updates ancestor index.md link lists for you)
```

# Examples

Add a top-level entry:
```
add_markdown {
  corpus: \"repo:myrepo:wiki\",
  path: \"corpus-walker.md\",
  content: \"# Corpus walker\\n\\nGitignore-aware...\\n\",
  overwrite: false,
}
```

Add a nested entry — daemon creates `adapters/index.md` if missing:
```
add_markdown {
  corpus: \"repo:myrepo:wiki\",
  path: \"adapters/lance.md\",
  content: \"# LanceDB adapter\\n\\n...\\n\",
  overwrite: false,
}
```

Update with rollback safety:
```
read_markdown { corpus: \"repo:myrepo:wiki\", path: \"corpus-walker.md\" }
// edit content
add_markdown { ..., overwrite: true }
```

Multi-root corpora write all `add_markdown` / `delete_markdown` to the \
FIRST configured root only — keep one root if you can.
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

/// Render a `TreeNode` as an indented ASCII outline — subdirs first, then
/// files, with one entry per line and two-space indents per depth level.
/// Mirrors the structured tree for clients that only want the text block.
fn render_tree_outline(
    node: &crate::domain::corpus::sandbox::TreeNode,
    depth: usize,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let label = if node.path.is_empty() {
        ".".to_string()
    } else {
        Path::new(&node.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&node.path)
            .to_string()
    };
    out.push_str(&format!("{indent}{label}/\n"));
    for sub in &node.subdirs {
        render_tree_outline(sub, depth + 1, out);
    }
    let file_indent = "  ".repeat(depth + 1);
    for f in &node.files {
        let name = Path::new(&f.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&f.path);
        out.push_str(&format!("{file_indent}{name}\n"));
    }
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

/// Open a `DaemonClient` for one tool call, surfacing a clear
/// "daemon unavailable" error if the socket is missing. The MCP server is
/// long-lived but the daemon may not be, so we dial per-call instead of
/// caching a client across requests. Production callers go through
/// `daemon_socket_path()` (which respects `HALLOUMINATE_SOCKET`); the env
/// is the only override hook the MCP transport needs.
async fn daemon_for_tool() -> Result<DaemonClient, ErrorData> {
    // Per the spec's Non-goals: do NOT auto-start the daemon from MCP.
    client_for(None)
        .await
        .map_err(|e| internal_error(format!("{e:#}")))
}

/// Translate a daemon RPC error into the MCP transport's `ErrorData` shape.
/// Daemon `InvalidParams` becomes `-32602`, `Internal` becomes `-32603`, and
/// transport / decode failures (already `anyhow::Error` by the time we get
/// here) collapse to `-32603` so MCP clients don't misinterpret a network
/// flake as a user error.
fn map_daemon_err(err: anyhow::Error) -> ErrorData {
    if let Some(rpc) = err.downcast_ref::<DaemonRpcError>() {
        return match rpc.kind {
            ErrorKind::InvalidParams => invalid_params(rpc.message.clone()),
            ErrorKind::Internal => internal_error(rpc.message.clone()),
        };
    }
    internal_error(format!("{err:#}"))
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
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCorporaParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    /// Corpus name; defaults to the wiki for the repo containing the
    /// daemon's cwd. Required only when no default applies and multiple
    /// corpora are configured.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTreeParams {
    /// Corpus name; defaults to the wiki for the repo containing the
    /// daemon's cwd. Required only when no default applies and multiple
    /// corpora are configured.
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobalizeMarkdownParams {
    /// Corpus the entry currently lives in.
    pub source_corpus: String,
    /// Relative path of the entry within `source_corpus`.
    pub path: String,
    /// Destination path within the global corpus. Defaults to `path`.
    #[serde(default)]
    pub dest_path: Option<String>,
    /// Replace an existing destination entry. Defaults to false.
    #[serde(default)]
    pub overwrite: bool,
}

/// Long-lived MCP server handle. Every tool method dials the daemon over a
/// fresh `UnixStream`, so the server is stateless beyond `tool_router`
/// and the captured client cwd.
#[derive(Debug, Clone)]
pub struct HallouminateTools {
    // The `tool_router` field is read by `#[tool_handler]`-generated code
    // when dispatching `tools/call`; rustc's dead-code pass doesn't see the
    // macro expansion, so silence the warning here.
    #[allow(dead_code)]
    tool_router: ToolRouter<HallouminateTools>,
    /// CWD captured once at MCP server startup, forwarded on every daemon
    /// hop so repo discovery resolves against the client's workspace.
    cwd: PathBuf,
}

#[tool_router]
impl HallouminateTools {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            cwd,
        }
    }

    #[tool(
        description = "Semantic search over a markdown corpus. `content` is a ripgrep-style outline (path, summary, line_range, score, snippet). `structuredContent.docs` maps absolute_path → { corpus, score, summary, keywords, mtime, chunks: [{chunk_id, heading_path, line_range, score, snippet}] }. Defaults from config: top_files=10, chunks_per_file=3, limit=50. Snippets are full chunk text unless `snippet_chars` is set."
    )]
    pub async fn ground(
        &self,
        Parameters(params): Parameters<GroundParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::Ground(GroundRequest {
                query: params.query,
                corpus: params.corpus,
                top_files: params.top_files,
                chunks_per_file: params.chunks_per_file,
                limit: params.limit,
                snippet_chars: params.snippet_chars,
            }),
        };
        let result: GroundResult = client.call(req).await.map_err(map_daemon_err)?;
        let structured =
            serde_json::to_value(&result.response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(result.outline, structured))
    }

    #[tool(
        description = "Build or refresh the LanceDB index for one or all configured corpora. Returns a one-line summary in `content` and the per-corpus IndexReport in `structuredContent`."
    )]
    pub async fn index(
        &self,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::Index(IndexRequest {
                corpus: params.corpus,
                paths_from: None,
            }),
        };
        let report: crate::app::cli::IndexReport =
            client.call(req).await.map_err(map_daemon_err)?;
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
        description = "List the corpus' files as a directory tree. `content` is an indented ASCII outline (subdirs first). `structuredContent` is { corpus, root: {path, absolute_path, files: [...], subdirs: [...]} } — recursive so an LLM can navigate progressively-disclosed wikis without reading every index.md. Defaults to the wiki for the repo containing the daemon's cwd when `corpus` is omitted."
    )]
    pub async fn list_tree(
        &self,
        Parameters(params): Parameters<ListTreeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::ListTree(ListTreeRequest {
                corpus: params.corpus,
            }),
        };
        let result: ListTreeResult = client.call(req).await.map_err(map_daemon_err)?;
        let mut outline = String::new();
        render_tree_outline(&result.root, 0, &mut outline);
        let structured =
            serde_json::to_value(&result).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(outline, structured))
    }

    #[tool(
        description = "List files currently visible in a corpus, honoring paths/globs/exclude rules. `content` is newline-separated relative paths. `structuredContent` is { files: [{path, absolute_path}, …] }. Paths are relative when the file lives under a configured corpus root, absolute otherwise. Defaults to the wiki for the repo containing the daemon's cwd when `corpus` is omitted."
    )]
    pub async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::ListFiles(ListFilesRequest {
                corpus: params.corpus,
            }),
        };
        let entries: ListFilesResult = client.call(req).await.map_err(map_daemon_err)?;
        let text = entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let structured = serde_json::json!({ "files": &entries });
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Write a markdown file under the corpus' FIRST configured root, creating parent directories as needed, then refresh just that file's LanceDB rows. Atomic write, no-symlink-follow. Stores content verbatim — no markdown schema imposed. For updates, call `read_markdown` first, then re-call with `overwrite=true`."
    )]
    pub async fn add_markdown(
        &self,
        Parameters(params): Parameters<AddMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: params.corpus,
                path: params.path,
                content: params.content,
                overwrite: params.overwrite,
            }),
        };
        let response: AddMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
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
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: params.corpus,
                path: params.path,
            }),
        };
        let response: ReadMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(response.content, structured))
    }

    #[tool(
        description = "Unlink a markdown file from the corpus' first configured root and prune its rows from the LanceDB index. Irreversible. Symlinks are rejected. `content` is a one-line summary; `structuredContent` is { corpus, path, absolute_path, file_ref }."
    )]
    pub async fn delete_markdown(
        &self,
        Parameters(params): Parameters<DeleteMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::DeleteMarkdown(DeleteMarkdownRequest {
                corpus: params.corpus,
                path: params.path,
            }),
        };
        let response: DeleteMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let text = format!("deleted {} from corpus {}", response.path, response.corpus);
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Copy a single markdown entry from `source_corpus` into the corpus marked `global = true` in config, making it searchable everywhere. The source stays in place (copy, not move). `dest_path` defaults to the source path; `overwrite=false` (default) errors if the destination already exists. The global corpus row is reindexed synchronously. Errors if no global corpus is configured or if `source_corpus` is itself the global corpus. `structuredContent` is { source_corpus, source_path, global_corpus, dest_path, absolute_path, indexed }."
    )]
    pub async fn globalize_markdown(
        &self,
        Parameters(params): Parameters<GlobalizeMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let req = DaemonRequest {
            cwd: self.cwd.clone(),
            payload: DaemonRequestPayload::GlobalizeMarkdown(GlobalizeMarkdownRequest {
                source_corpus: params.source_corpus,
                path: params.path,
                dest_path: params.dest_path,
                overwrite: params.overwrite,
            }),
        };
        let response: GlobalizeMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let text = format!(
            "globalized {} from {} into {} as {}",
            response.source_path,
            response.source_corpus,
            response.global_corpus,
            response.dest_path,
        );
        let structured =
            serde_json::to_value(&response).map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "List corpora configured in the hallouminate config file, including derived `repo:{name}:wiki` / `repo:{name}:corpus` entries from `[[repository]]` declarations. `content` is newline-separated corpus names; `structuredContent` is { corpora: [{name, paths}, …] }. Run `hallouminate config validate` for a richer summary."
    )]
    pub async fn list_corpora(
        &self,
        _params: Parameters<ListCorporaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = daemon_for_tool().await?;
        let entries: ListCorporaResult = client
            .call(DaemonRequest {
                cwd: self.cwd.clone(),
                payload: DaemonRequestPayload::ListCorpora,
            })
            .await
            .map_err(map_daemon_err)?;
        let names = entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let structured = serde_json::json!({ "corpora": &entries });
        Ok(tool_ok(names, structured))
    }
}

impl Default for HallouminateTools {
    /// `#[derive(Default)]` would construct `ToolRouter::default()` (an empty
    /// router) and skip the `#[tool_router]`-generated registration. Manual
    /// impl routes through `new()` so `HallouminateTools::default()` exposes
    /// the same tool set as `new()`. The default cwd is empty — production
    /// callers go through `serve_stdio` which captures the real cwd.
    fn default() -> Self {
        Self::new(PathBuf::new())
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
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[cfg(test)]
mod tests {
    //! MCP-specific tests. The corpus-boundary helpers
    //! (`safe_relative_path`, `pick_corpus`, `ensure_corpus_allows_file`,
    //! `first_corpus_root`, `atomic_write_no_follow`, `list_corpus_files`)
    //! live in `crate::domain::corpus::sandbox` and are tested there once.
    //! Daemon RPC error mapping has its own contract — pin the JSON-RPC
    //! code each error variant maps to so a future refactor of the
    //! daemon's `ErrorKind` would have to deliberately update the test.

    use super::*;

    #[test]
    fn map_daemon_err_routes_invalid_params_to_minus_32602() {
        let err: anyhow::Error = DaemonRpcError::invalid_params("bad corpus name").into();
        let mapped = map_daemon_err(err);
        assert_eq!(mapped.code.0, -32602);
        assert!(mapped.message.contains("bad corpus name"));
    }

    #[test]
    fn map_daemon_err_routes_internal_to_minus_32603() {
        let err: anyhow::Error = DaemonRpcError::internal("disk on fire").into();
        let mapped = map_daemon_err(err);
        assert_eq!(mapped.code.0, -32603);
        assert!(mapped.message.contains("disk on fire"));
    }

    #[test]
    fn new_stores_cwd_for_daemon_hops() {
        // Pin the field plumbing: the cwd handed to `HallouminateTools::new`
        // at MCP startup must be the same value every tool handler clones
        // into its `DaemonRequest`. Testing that cwd actually flows over
        // the socket needs a daemon fixture and lives in the integration
        // suite — this guards the boring-but-easy-to-break wiring.
        let cwd = PathBuf::from("/test/cwd");
        let tools = HallouminateTools::new(cwd.clone());
        assert_eq!(tools.cwd, cwd);
    }

    #[test]
    fn map_daemon_err_routes_transport_failure_to_minus_32603() {
        // Anything that is not a DaemonRpcError (transport / decode failure
        // before we got a typed error envelope from the daemon) must NOT be
        // misinterpreted as a caller-input failure. JSON-RPC -32603 is
        // explicitly "internal error" for cases like this.
        let err: anyhow::Error = anyhow::anyhow!("daemon unavailable: socket missing");
        let mapped = map_daemon_err(err);
        assert_eq!(mapped.code.0, -32603);
        assert!(mapped.message.contains("daemon unavailable"));
    }
}
