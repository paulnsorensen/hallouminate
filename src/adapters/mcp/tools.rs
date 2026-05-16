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
use crate::app::input_error::is_input_error;
use crate::domain::ground::{render, trim_snippets, Format, RenderOpts};

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

fn invalid_params(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

/// Decide whether an error from the ground/index call path is the user's
/// fault (bad corpus name, missing config field) or the server's fault
/// (disk I/O, embedder init, LanceDB failure). Classification is
/// structural: producers in `cli::ground` / `cli::index` construct
/// `InputError(msg)` (converted into `anyhow::Error` via `.into()`) when
/// the cause is caller-supplied. Anything unmarked is treated as an
/// internal server fault. See `app::input_error` for the marker and the
/// chain-walk helper.
fn map_app_error(err: anyhow::Error) -> ErrorData {
    if is_input_error(&err) {
        invalid_params(err.to_string())
    } else {
        internal_error(err.to_string())
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
            // `format` and `snippet_chars` on GroundArgs are ignored by
            // `run_ground` — the MCP adapter renders the response itself
            // (outline + structured) below so it can hand both views back.
            format: Format::Outline,
            snippet_chars: params.snippet_chars,
            top_files: params.top_files,
            chunks_per_file: params.chunks_per_file,
            limit: params.limit,
            config: None,
        };
        let response = run_ground(args).await.map_err(map_app_error)?;
        // Trim once, reuse for both the outline and the structured payload.
        // Without this hoist the trimmed-snippet branch trimmed twice — once
        // inside `render` and once below — wasting an extra `GroundResponse`
        // clone on every snippet-capped call. `render` with `snippet_chars:
        // None` is a borrow-only path, so we skip its internal trim and
        // hand the already-trimmed response in directly.
        let trimmed = params
            .snippet_chars
            .map(|limit| trim_snippets(&response, limit));
        let view = trimmed.as_ref().unwrap_or(&response);
        let outline = render(
            view,
            Format::Outline,
            &RenderOpts {
                snippet_chars: None,
                path_prefix_strip: None,
            },
        );
        let structured =
            serde_json::to_value(view).map_err(|e| internal_error(e.to_string()))?;
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
        .map_err(map_app_error)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::input_error::InputError;
    use rmcp::model::ErrorCode;

    #[test]
    fn map_app_error_routes_input_marked_error_to_invalid_params() {
        // The structural contract: producers in `cli::ground` / `cli::index`
        // construct `InputError(msg)` when the cause is caller-supplied
        // (unknown corpus, missing --corpus, etc.). The mapper must downcast
        // and emit JSON-RPC -32602 — no substring matching on the message.
        let err: anyhow::Error =
            InputError::new("corpus \"ghost\" not found in config").into();
        let mapped = map_app_error(err);
        assert_eq!(
            mapped.code,
            ErrorCode::INVALID_PARAMS,
            "marked input error must be -32602, got {:?}",
            mapped.code
        );
    }

    #[test]
    fn map_app_error_routes_unmarked_error_to_internal_error() {
        // Inverse: an `anyhow::Error` without the marker — including one
        // whose Display happens to contain the words "corpus" and "not
        // found" — must surface as -32603. This guards against accidentally
        // un-wrapped producers in the future: forget the marker, get
        // internal_error at test time, fix it before users see it.
        let err = anyhow::anyhow!("corpus \"ghost\" not found in config");
        let mapped = map_app_error(err);
        assert_eq!(
            mapped.code,
            ErrorCode::INTERNAL_ERROR,
            "unmarked error must be -32603, got {:?}",
            mapped.code
        );
    }

    #[test]
    fn map_app_error_preserves_marker_through_with_context_chain() {
        // Producers often layer `.with_context(...)` on top of the marker
        // (e.g. the CLI wraps the inner error with file paths). The marker
        // must still be reachable via anyhow's typed-context chain walk.
        let err: anyhow::Error = anyhow::Error::new(InputError::new(
            "corpus \"ghost\" not found in config",
        ))
        .context("while running ground");
        let mapped = map_app_error(err);
        assert_eq!(
            mapped.code,
            ErrorCode::INVALID_PARAMS,
            "marker on an inner cause must still route to -32602, got {:?}",
            mapped.code
        );
    }
}
