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
use rmcp::{Peer, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::app::daemon::{
    AddMarkdownRequest, AddMarkdownResult, CorpusStatsResult, DaemonClient, DaemonRequest,
    DaemonRequestPayload, DaemonRpcError, DeleteMarkdownRequest, DeleteMarkdownResult, ErrorKind,
    GroundRequest, GroundResult, IndexRequest, LineRange, ListCorporaResult, ListFilesRequest,
    ListFilesResult, ListTreeRequest, ListTreeResult, Position, ReadMarkdownRequest,
    ReadMarkdownResult, client_for,
};

use crate::domain::footnotes::{FootnoteMode, apply_footnote_mode, get_footnote_target};
use crate::domain::ground::{Format, RenderOpts, render};

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
repository containing the MCP server process cwd. Pass `corpus` explicitly to \
target another wiki, the repo's source corpus (`repo:{name}:corpus`), or a \
user-declared `[[corpus]]` entry; `list_corpora` enumerates everything available.

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
- `get_footnote` — resolve a single citation: footnote target for page#footnote_number.

Filesystem is the source of truth; LanceDB rows are derived and refreshed \
after `add_markdown` / `delete_markdown`. `index` is the only way to pick \
up edits made outside hallouminate.

# Authoring conventions (REQUIRED for `add_markdown`)

ONE TOPIC PER FILE. A wiki entry is a slice of knowledge with a clear \
scope. The chunker splits on headings — two unrelated topics in one file \
make `ground` rank both sections together, which is rarely what you want.

FIRST NON-BLANK LINE IS H1. Every file's first non-blank line — or, when \
an optional frontmatter block is present, the first non-blank line after \
its closing `---` fence — must be `# Topic Name`. The chunker uses the H1 \
as the breadcrumb root; without it, search results lose navigability. The \
H1 is also what the auto-index quotes as each entry's gloss.

FILE STEM MATCHES THE SLUG. \"Corpus walker\" → `corpus-walker.md`. \
Lowercase, kebab case. No spaces, no capitals, no extensions other than \
`.md`.

LEAD WITH THE CONCLUSION. Don't bury what the file is about under \
preamble. Cite files and line ranges by path: \
`src/domain/corpus/walker.rs:42`. Prefer concrete examples to abstract \
description. ~50-150 lines per entry is the right band.

OPTIONAL FRONTMATTER. A page MAY open with a YAML frontmatter block — a \
`---` fence on line 1, key/value lines, then a closing `---`. Recognized \
keys (all optional): `status` (draft|reviewed|trusted|deprecated, \
case-insensitive), `owner`, `last_verified`, `confidence`, `sources`. \
Unknown keys are ignored. The block is stripped before indexing, so it \
never pollutes chunks, summaries, or `ground` hits, and citations still \
point at the real on-disk lines. A malformed block is left in the body \
and returns one advisory warning.

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

# Cite in footnote format

For any non-obvious claim — a file path, a line reference, an external URL, \
a version number, or a fact you derived from another document — include an \
inline `[^N]` marker immediately after the claim and a matching definition \
block at the end of the file:

```
The daemon opens one socket.[^1]

[^1]: src/app/daemon/server.rs:42
```

Authoring agents SHOULD add footnotes for any claim that a future reader \
could not easily verify without re-reading the cited source. Provenance that \
travels with the page lets grounding agents call `get_footnote` to resolve a \
citation without pulling the whole document.
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

/// Render file content with 1-based line-number gutters, `cat -n` style:
/// a right-aligned line number, a tab, then the line. Used only for the
/// human-readable text block; the structured payload stays verbatim.
fn number_lines(content: &str) -> String {
    let mut out = String::new();
    for (number, line) in content.lines().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", number + 1, line));
    }
    out
}

fn internal_error(msg: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(msg.into(), None)
}

/// Serialize a daemon response into the `structuredContent` JSON payload,
/// mapping a serializer failure to a `-32603 internal_error`. A failure here
/// means a daemon result type produced non-JSON-representable data, which is a
/// server fault, not caller input.
fn to_structured<T: Serialize>(v: &T) -> Result<serde_json::Value, ErrorData> {
    serde_json::to_value(v).map_err(|e| internal_error(e.to_string()))
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

/// Resolve the effective cwd for a daemon request.
///
/// When the client advertised `roots` capability, sends `roots/list` and uses
/// the first `file://` root path as cwd. Falls back to the process-startup cwd
/// when the client has no roots capability or returns an empty list.
async fn cwd_from_peer(peer: &Peer<RoleServer>, fallback: &Path) -> PathBuf {
    let has_roots = peer
        .peer_info()
        .and_then(|info| info.capabilities.roots.as_ref())
        .is_some();

    if !has_roots {
        return fallback.to_path_buf();
    }

    if let Ok(result) = peer.list_roots().await {
        if let Some(root) = result.roots.first() {
            if let Some(path_str) = root.uri.strip_prefix("file://") {
                if !path_str.is_empty() {
                    return PathBuf::from(path_str);
                }
            }
        }
    }

    fallback.to_path_buf()
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
    /// Controls footnote visibility in snippets. `include` (default) passes
    /// footnotes through verbatim; `exclude` strips definition blocks and
    /// inline `[^label]` markers; `only` returns just the definition lines.
    #[serde(default)]
    pub footnotes: FootnoteMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexParams {
    /// Optional corpus name; omit to index every configured corpus.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CorpusStatsParams {
    /// Corpus name; defaults to the wiki for the repo containing the MCP
    /// workspace root. Required only when no default applies and multiple
    /// corpora are configured.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCorporaParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    /// Corpus name; defaults to the wiki for the repo containing the
    /// MCP workspace root. Required only when no default applies and multiple
    /// corpora are configured.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTreeParams {
    /// Corpus name; defaults to the wiki for the repo containing the
    /// MCP workspace root. Required only when no default applies and multiple
    /// corpora are configured.
    #[serde(default)]
    pub corpus: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path under the corpus' single configured root. Writes require
    /// a single-root corpus — multi-root corpora are read- and search-only and
    /// reject `add_markdown`. The caller owns the directory structure and
    /// markdown shape — convention: `<slug>.md` or `<category>/<slug>.md`,
    /// first line `# Title`.
    pub path: String,
    /// Markdown bytes. Interpretation depends on the edit mode:
    /// - default (no edit-mode field): the WHOLE file.
    /// - `under_heading`: the FRAGMENT spliced into the section.
    /// - `replace_lines`: the replacement body for the line range.
    /// - `replace_match`: the replacement text for the matched substring.
    pub content: String,
    /// Replace an existing file. Applies to the WHOLE-FILE mode only; the
    /// three edit modes are inherently modify-in-place and ignore it.
    #[serde(default)]
    pub overwrite: bool,

    /// Section-splice: splice `content` under this heading's section instead of
    /// writing the whole file. Matched by rendered heading text (trimmed), at
    /// any level. Requires the file to already exist. Mutually exclusive with
    /// `replace_lines` and `replace_match`.
    #[serde(default)]
    pub under_heading: Option<String>,
    /// Where to splice within the section when `under_heading` is set. Default
    /// `append`. Ignored by the other modes.
    #[serde(default)]
    pub position: Position,
    /// Line-range replace: replace lines `{start, end}` (1-based, inclusive)
    /// with `content`. Requires the file to already exist. Mutually exclusive
    /// with `under_heading` and `replace_match`.
    #[serde(default)]
    pub replace_lines: Option<LineRange>,
    /// Text-match replace: replace the UNIQUE literal occurrence of this
    /// substring with `content`. 0 matches → not found; >1 → ambiguous; both
    /// rejected with InvalidParams. Requires the file to already exist.
    /// Mutually exclusive with `under_heading` and `replace_lines`.
    #[serde(default)]
    pub replace_match: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path within the corpus, same shape as `add_markdown`. For a
    /// multi-root corpus it resolves against every configured root (first
    /// match wins), so a file searchable under `paths[1..]` is also readable.
    /// Symlinks are rejected.
    pub path: String,
    /// When true, render the human-readable text block with 1-based
    /// line-number gutters (`cat -n` style) so callers can cite and verify
    /// `path:line` ranges. The structured `content` stays verbatim. Defaults
    /// to false.
    #[serde(default)]
    pub line_numbers: bool,
    /// Controls footnote visibility in the text block. `include` (default)
    /// passes footnotes through verbatim; `exclude` strips definition blocks
    /// and inline `[^label]` markers; `only` returns just the definition lines.
    /// The structured `content` stays verbatim regardless of this setting.
    #[serde(default)]
    pub footnotes: FootnoteMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteMarkdownParams {
    /// Corpus that owns the markdown file.
    pub corpus: String,
    /// Relative path under the corpus' single configured root, same shape as
    /// `add_markdown`. Requires a single-root corpus — multi-root corpora are
    /// read- and search-only. Symlinks are rejected. Irreversible.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFootnoteParams {
    /// Corpus that owns the page. Defaults to the wiki for the repo
    /// containing the client's MCP workspace root, same as `ground`.
    #[serde(default)]
    pub corpus: Option<String>,
    /// Relative path of the wiki page within the corpus.
    pub page: String,
    /// The footnote label (the text after `^`). For `[^1]` use `"1"`;
    /// for `[^note]` use `"note"`.
    pub footnote_number: String,
}

/// Long-lived MCP server handle. Every tool method dials the daemon over a
/// fresh `UnixStream`, so the server is stateless beyond `tool_router`
/// and the fallback cwd captured at startup.
#[derive(Debug, Clone)]
pub struct HallouminateTools {
    // The `tool_router` field is read by `#[tool_handler]`-generated code
    // when dispatching `tools/call`; rustc's dead-code pass doesn't see the
    // macro expansion, so silence the warning here.
    #[allow(dead_code)]
    tool_router: ToolRouter<HallouminateTools>,
    /// CWD captured once at MCP server startup, used as the cwd for every
    /// daemon request.
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

    /// Shared per-call preamble: dial the daemon and resolve the effective cwd.
    /// Every tool method opens with this before building its `DaemonRequest`.
    async fn tool_setup(
        &self,
        peer: &Peer<RoleServer>,
    ) -> Result<(DaemonClient, PathBuf), ErrorData> {
        let client = daemon_for_tool().await?;
        let cwd = cwd_from_peer(peer, &self.cwd).await;
        Ok((client, cwd))
    }

    #[tool(
        description = "Semantic search over a markdown corpus. `content` is a ripgrep-style outline (path, summary, line_range, score, snippet). `structuredContent.docs` maps absolute_path → { corpus, score, summary, keywords, mtime, path, stale, chunks: [{chunk_id, heading_path, line_range, score, snippet, provenance: {corpus}}] }, where `path` is the corpus-relative path accepted directly by `read_markdown`/`add_markdown` (null when no corpus root matches), `stale: true` means the file was modified on disk since it was last indexed (index may be stale), and each chunk's `provenance.corpus` names its source wiki. Score note: the default `score` is rank-fusion RRF (rank-derived, not a similarity value; top hits cluster ~0.02–0.07; not comparable across queries — do not threshold on it for dedup or routing). To get a calibrated semantic score, enable the opt-in cross-encoder reranker via `search.crossencoder` in config. With no `corpus` from a directory above all repos, the search unions every effective corpus: discovered sub-repo wikis, baseline-registered `[[repository]]` wikis, user-declared `[[corpus]]` entries, and each repository's `repo:<name>:corpus` source corpus when configured — each hit carries its source corpus. Passing an explicit `corpus` pins the search to that one corpus. Defaults from config: top_files=10, chunks_per_file=3, limit=50. Snippets are full chunk text unless `snippet_chars` is set."
    )]
    pub async fn ground(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<GroundParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::Ground(GroundRequest {
                query: params.query,
                corpus: params.corpus,
                top_files: params.top_files,
                chunks_per_file: params.chunks_per_file,
                limit: params.limit,
                snippet_chars: params.snippet_chars,
            }),
        };
        let mut result: GroundResult = client.call(req).await.map_err(map_daemon_err)?;
        if params.footnotes != FootnoteMode::Include {
            for doc in result.response.docs.values_mut() {
                for chunk in &mut doc.chunks {
                    chunk.snippet = apply_footnote_mode(&chunk.snippet, params.footnotes);
                }
            }
            result.outline = render(&result.response, Format::Outline, &RenderOpts::default());
        }
        let structured = to_structured(&result.response)?;
        Ok(tool_ok(result.outline, structured))
    }

    #[tool(
        description = "Build or refresh the LanceDB index for one or all configured corpora. Returns a one-line summary in `content` and the per-corpus IndexReport in `structuredContent`."
    )]
    pub async fn index(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::Index(IndexRequest {
                corpus: params.corpus,
                paths_from: None,
                strict: false,
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
        let structured = to_structured(&report)?;
        Ok(tool_ok(summary, structured))
    }

    #[tool(
        description = "List the corpus' files as a directory tree. `content` is an indented ASCII outline (subdirs first). `structuredContent` is { corpus, root: {path, absolute_path, files: [...], subdirs: [...]} } — recursive so an LLM can navigate progressively-disclosed wikis without reading every index.md. Defaults to the wiki for the repo containing the MCP workspace root when `corpus` is omitted."
    )]
    pub async fn list_tree(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<ListTreeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ListTree(ListTreeRequest {
                corpus: params.corpus,
            }),
        };
        let result: ListTreeResult = client.call(req).await.map_err(map_daemon_err)?;
        let mut outline = String::new();
        render_tree_outline(&result.root, 0, &mut outline);
        let structured = to_structured(&result)?;
        Ok(tool_ok(outline, structured))
    }

    #[tool(
        description = "List files currently visible in a corpus, honoring paths/globs/exclude rules. `content` is newline-separated relative paths. `structuredContent` is { files: [{path, absolute_path}, …] }. Paths are relative when the file lives under a configured corpus root, absolute otherwise. Defaults to the wiki for the repo containing the MCP workspace root when `corpus` is omitted."
    )]
    pub async fn list_files(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ListFiles(ListFilesRequest {
                corpus: params.corpus,
            }),
        };
        let entries: ListFilesResult = client.call(req).await.map_err(map_daemon_err)?;
        let mut text = String::new();
        for e in entries.iter() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(e.path.as_str());
        }
        let structured = serde_json::json!({ "files": &entries });
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Write a markdown file under the corpus' single configured root, creating parent directories as needed, then refresh just that file's LanceDB rows. Requires a single-root corpus — multi-root corpora are read- and search-only and reject writes. Atomic write, no-symlink-follow. Stores content verbatim — no markdown schema imposed. Returns advisory lint `warnings` (empty-destination links, empty mermaid blocks, heading-level jumps) without blocking or altering the write. For updates, call `read_markdown` first, then re-call with `overwrite=true`. For a targeted edit instead of a whole-file write, set exactly ONE of: `under_heading` (splice `content` into an existing heading's section; `position` = `append` (default, before the next same-or-higher heading) or `prepend` (right after the heading line)); `replace_lines` ({start, end}, 1-based inclusive, replace that line range with `content`); or `replace_match` (replace the unique literal occurrence of the given substring with `content`). All three require the file to already exist and ignore `overwrite`. Setting more than one is rejected. A missing/duplicate heading, an out-of-range line range, or a substring with zero or multiple matches is rejected with InvalidParams."
    )]
    pub async fn add_markdown(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<AddMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: params.corpus,
                path: params.path,
                content: params.content,
                overwrite: params.overwrite,
                under_heading: params.under_heading,
                position: params.position,
                replace_lines: params.replace_lines,
                replace_match: params.replace_match,
            }),
        };
        let response: AddMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let mut text = format!(
            "wrote {} and refreshed corpus {}",
            response.path, response.corpus
        );
        if !response.warnings.is_empty() {
            text.push_str("\n\nlint warnings (advisory, file was written as-is):");
            for warning in &response.warnings {
                text.push_str(&format!("\n- {warning}"));
            }
        }
        let structured = to_structured(&response)?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Read verbatim UTF-8 contents of a markdown file in a corpus. `content` is the full file text; `structuredContent` is { corpus, path, absolute_path, content, bytes }. Symlinks are rejected. Returns the on-disk text, not the indexed/chunked view — call `ground` for semantic search. Set `line_numbers: true` to render the text block with `cat -n`-style line-number gutters for citing `path:line`; the structured `content` stays verbatim."
    )]
    pub async fn read_markdown(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<ReadMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: Some(params.corpus),
                path: params.path,
            }),
        };
        let response: ReadMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let structured = to_structured(&response)?;
        let body = apply_footnote_mode(&response.content, params.footnotes);
        let text = if params.line_numbers {
            number_lines(&body)
        } else {
            body
        };
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Unlink a markdown file from the corpus' single configured root and prune its rows from the LanceDB index. Requires a single-root corpus — multi-root corpora are read- and search-only. Irreversible. Symlinks are rejected. `content` is a one-line summary; `structuredContent` is { corpus, path, absolute_path, file_ref }."
    )]
    pub async fn delete_markdown(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<DeleteMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::DeleteMarkdown(DeleteMarkdownRequest {
                corpus: params.corpus,
                path: params.path,
            }),
        };
        let response: DeleteMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let text = format!("deleted {} from corpus {}", response.path, response.corpus);
        let structured = to_structured(&response)?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "Return index health statistics for one corpus: how many files are indexed, \
                      total chunk row count, the newest index timestamp (ms since epoch, null when \
                      the corpus has never been indexed), and how many on-disk files matching the \
                      corpus globs have not yet been indexed. Corpus selection follows the same \
                      default resolution as `list_files`. `structuredContent` is \
                      { corpus, indexed_files, total_chunks, last_indexed_ms, unindexed_files }."
    )]
    pub async fn corpus_stats(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<CorpusStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let result: CorpusStatsResult = client
            .call(DaemonRequest {
                cwd,
                payload: DaemonRequestPayload::CorpusStats {
                    corpus: params.corpus,
                },
            })
            .await
            .map_err(map_daemon_err)?;
        let text = format!(
            "corpus: {}\nindexed_files: {}\ntotal_chunks: {}\nlast_indexed_ms: {}\nunindexed_files: {}",
            result.corpus,
            result.indexed_files,
            result.total_chunks,
            result
                .last_indexed_ms
                .map_or("null".to_string(), |ms| ms.to_string()),
            result.unindexed_files,
        );
        let structured = to_structured(&result)?;
        Ok(tool_ok(text, structured))
    }

    #[tool(
        description = "List corpora configured in the hallouminate config file, including derived `repo:{name}:wiki` / `repo:{name}:corpus` entries from `[[repository]]` declarations. `content` is newline-separated corpus names; `structuredContent` is { corpora: [{name, paths}, …] }. Run `hallouminate config validate` for a richer summary."
    )]
    pub async fn list_corpora(
        &self,
        peer: Peer<RoleServer>,
        _params: Parameters<ListCorporaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let entries: ListCorporaResult = client
            .call(DaemonRequest {
                cwd,
                payload: DaemonRequestPayload::ListCorpora,
            })
            .await
            .map_err(map_daemon_err)?;
        let names = entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let structured = serde_json::json!({ "corpora": &entries });
        Ok(tool_ok(names, structured))
    }

    #[tool(
        description = "Resolve a single citation: return the footnote target (source text / link) \
                      for page#footnote_number without pulling the whole document."
    )]
    pub async fn get_footnote(
        &self,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<GetFootnoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (client, cwd) = self.tool_setup(&peer).await?;
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: params.corpus,
                path: params.page.clone(),
            }),
        };
        let response: ReadMarkdownResult = client.call(req).await.map_err(map_daemon_err)?;
        let target =
            get_footnote_target(&response.content, &params.footnote_number).ok_or_else(|| {
                invalid_params(format!(
                    "footnote [^{}] not found in {}",
                    params.footnote_number, params.page
                ))
            })?;
        let structured = serde_json::json!({
            "page": params.page,
            "footnote_number": params.footnote_number,
            "target": target,
        });
        Ok(tool_ok(target, structured))
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
    fn to_structured_maps_serialize_failure_to_internal_error() {
        // A type whose `Serialize` impl always errors must surface as a
        // -32603 internal_error, not panic or silently drop the structured
        // payload: a non-JSON-representable daemon result is a server fault.
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }
        let err = to_structured(&Unserializable).expect_err("serialize must fail");
        assert_eq!(err.code.0, -32603);
        assert!(err.message.contains("nope"));
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

    #[test]
    fn number_lines_renders_cat_n_gutters() {
        // The gutter must be 6 chars wide, right-aligned, followed by a tab
        // and the original line. A regression in alignment would break an
        // agent's ability to parse `path:line` references from the output.
        let result = number_lines("foo\nbar");
        assert_eq!(result, "     1\tfoo\n     2\tbar\n");
    }

    #[test]
    fn number_lines_empty_input_produces_empty_output() {
        assert_eq!(number_lines(""), "");
    }
}
