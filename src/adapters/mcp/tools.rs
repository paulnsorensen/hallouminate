//! Tool registrations for the hallouminate MCP server. Each handler reuses
//! existing domain/app functions — `run_ground`, `cmd_index` (indirectly),
//! `config::load` — and emits a `CallToolResult` carrying both a
//! human-readable text block (outline / summary) and a `structured_content`
//! field with the full typed response. Token-cheap for the LLM consumer,
//! structured for the harness consumer.

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::app::cli::{run_ground, run_index, GroundArgs, IndexArgs};
use crate::app::config;
use crate::domain::ground::{render, Format, RenderOpts};

const SERVER_INSTRUCTIONS: &str = "Hallouminate exposes three tools: `ground` (semantic search), `index` (refresh the LanceDB index), and `list_corpora` (config introspection). Each tool returns a token-cheap text view in `content` and the full structured response in `structuredContent`.";

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

#[derive(Debug, Serialize, JsonSchema)]
struct CorpusEntry {
    name: String,
    paths: Vec<String>,
}

/// Stateless server handle — every tool resolves config and storage from
/// disk per call. Keeping this empty avoids contaminating the long-lived
/// stdio process with stale Embedder / LanceStore handles when the user
/// edits config.toml between calls.
#[derive(Debug, Default, Clone)]
pub struct HallouminateTools {
    // The `tool_router` field is read by `#[tool_handler]`-generated code
    // when dispatching `tools/call`; rustc's dead-code pass doesn't see the
    // macro expansion, so silence the warning here.
    #[allow(dead_code)]
    tool_router: ToolRouter<HallouminateTools>,
}

#[tool_router]
impl HallouminateTools {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Semantic search over the configured markdown corpora. Returns an outline view in `content` and the full GroundResponse in `structuredContent`."
    )]
    pub async fn ground(
        &self,
        Parameters(params): Parameters<GroundParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = GroundArgs {
            query: params.query.clone(),
            corpus: params.corpus.clone(),
            format: Format::Outline, // unused — we render twice below
            snippet_chars: params.snippet_chars,
            top_files: params.top_files,
            chunks_per_file: params.chunks_per_file,
            limit: params.limit,
            config: None,
        };
        let response = run_ground(args)
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
        let structured = serde_json::to_value(&response)
            .map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(outline, structured))
    }

    #[tool(
        description = "Build or refresh the LanceDB index for one or all configured corpora. Returns a one-line summary in `content` and the per-corpus IndexReport in `structuredContent`."
    )]
    pub async fn index(
        &self,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let report = run_index(IndexArgs {
            corpus: params.corpus,
            paths_from: params.paths_from,
            config: None,
        })
        .await
        .map_err(|e| internal_error(e.to_string()))?;

        let summary = report
            .corpora
            .iter()
            .map(|c| {
                format!(
                    "{}: upserted={} touched={} deleted={} chunks+={}",
                    c.name,
                    c.files_upserted,
                    c.files_touched,
                    c.files_deleted,
                    c.chunks_inserted
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let structured = serde_json::to_value(&report)
            .map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(summary, structured))
    }

    #[tool(
        description = "List corpora configured in the hallouminate config file. Returns names in `content` and `{name, paths}` records in `structuredContent`."
    )]
    pub async fn list_corpora(
        &self,
        _params: Parameters<ListCorporaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = config::load(None)
            .map_err(|e| internal_error(e.to_string()))?;
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
        let structured = serde_json::to_value(&entries)
            .map_err(|e| internal_error(e.to_string()))?;
        Ok(tool_ok(names, structured))
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
