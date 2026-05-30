use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Structured payload of the `ground` MCP tool: one semantic-search query and
/// its per-file ranked results.
///
/// Serialized verbatim as `structuredContent` on the tool result; field names
/// are the wire contract clients read, so they must not change without a
/// matching client update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroundResponse {
    /// The free-text query that produced these results, echoed back.
    pub query: String,
    /// Wall-clock time the search took, in milliseconds.
    pub took_ms: u64,
    /// Aggregate counters for the search run.
    pub stats: Stats,
    /// Matched markdown files keyed by absolute path, ordered by the
    /// `BTreeMap`'s path sort.
    pub docs: BTreeMap<String, DocFile>,
    /// Matched code-repo results keyed by absolute path. Empty unless a
    /// `[[code_repo]]` is configured; values are opaque to this layer.
    pub code: BTreeMap<String, serde_json::Value>,
    /// Non-fatal advisories (e.g. no code repos configured) attached to the run.
    pub warnings: Vec<Warning>,
}

/// Aggregate counters describing a `ground` search run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Stats {
    /// Number of raw index hits before per-file rollup.
    pub hits: usize,
}

/// One matched file in a [`GroundResponse`], with its file-level metadata and
/// the chunks that matched the query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocFile {
    /// File gloss: its first H1 plus lead paragraph, or `None` when the file
    /// has no H1 to summarize.
    pub summary: Option<String>,
    /// Keywords extracted for the file.
    pub keywords: Vec<String>,
    /// File-level relevance score (the best of its chunk scores after rollup).
    pub score: f64,
    /// File modification time as an RFC 3339 timestamp.
    pub mtime: String,
    /// Name of the corpus this file belongs to.
    pub corpus: String,
    /// Matching chunks within the file, ranked by `score` descending.
    pub chunks: Vec<DocChunk>,
}

/// One matched chunk within a [`DocFile`]: a heading-delimited span of the file
/// that the query scored against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocChunk {
    /// Stable identifier for the chunk in the index.
    pub chunk_id: String,
    /// Breadcrumb of heading titles from the file's H1 down to this chunk.
    pub heading_path: Vec<String>,
    /// Inclusive `[start, end]` 1-based line range the chunk spans.
    pub line_range: [u32; 2],
    /// Chunk-level relevance score.
    pub score: f64,
    /// Chunk text, trimmed to the request's `snippet_chars` when set.
    pub snippet: String,
}

/// A non-fatal advisory attached to a [`GroundResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    /// Machine-readable warning code (e.g. `code-repos-empty`).
    pub code: String,
    /// Human-readable explanation of the warning.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_response() -> GroundResponse {
        let mut docs = BTreeMap::new();
        docs.insert(
            "/abs/path/to/file.md".into(),
            DocFile {
                summary: Some("First H1 plus first paragraph (or AI-generated later).".into()),
                keywords: vec!["fts5".into(), "rrf".into(), "vector".into()],
                score: 0.873,
                mtime: "2026-04-30T10:11:23Z".into(),
                corpus: "tern-docs".into(),
                chunks: vec![DocChunk {
                    chunk_id: "abc123".into(),
                    heading_path: vec!["Indexing pipeline".into(), "Phase B".into()],
                    line_range: [134, 198],
                    score: 0.91,
                    snippet: "first ~200 chars of chunk text…".into(),
                }],
            },
        );
        GroundResponse {
            query: "tokio task spawning".into(),
            took_ms: 47,
            stats: Stats { hits: 22 },
            docs,
            code: BTreeMap::new(),
            warnings: vec![],
        }
    }

    #[test]
    fn serialize_response_matches_documented_spec_shape() {
        let actual = serde_json::to_value(fixture_response()).expect("serialize");
        let expected = json!({
            "query": "tokio task spawning",
            "took_ms": 47,
            "stats": { "hits": 22 },
            "docs": {
                "/abs/path/to/file.md": {
                    "summary": "First H1 plus first paragraph (or AI-generated later).",
                    "keywords": ["fts5", "rrf", "vector"],
                    "score": 0.873,
                    "mtime": "2026-04-30T10:11:23Z",
                    "corpus": "tern-docs",
                    "chunks": [{
                        "chunk_id": "abc123",
                        "heading_path": ["Indexing pipeline", "Phase B"],
                        "line_range": [134, 198],
                        "score": 0.91,
                        "snippet": "first ~200 chars of chunk text…"
                    }]
                }
            },
            "code": {},
            "warnings": []
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn serialize_null_summary_when_file_lacks_h1() {
        let file = DocFile {
            summary: None,
            keywords: vec![],
            score: 0.0,
            mtime: "2026-04-30T10:11:23Z".into(),
            corpus: "docs".into(),
            chunks: vec![],
        };
        let actual = serde_json::to_value(&file).expect("serialize");
        assert_eq!(actual["summary"], json!(null));
    }

    #[test]
    fn serialize_warning_uses_code_and_message_fields() {
        let warning = Warning {
            code: "code-repos-empty".into(),
            message: "no [[code_repo]] configured".into(),
        };
        let actual = serde_json::to_value(&warning).expect("serialize");
        assert_eq!(
            actual,
            json!({ "code": "code-repos-empty", "message": "no [[code_repo]] configured" })
        );
    }

    #[test]
    fn serialize_empty_docs_and_code_render_as_objects_not_arrays() {
        let response = GroundResponse {
            query: String::new(),
            took_ms: 0,
            stats: Stats::default(),
            docs: BTreeMap::new(),
            code: BTreeMap::new(),
            warnings: vec![],
        };
        let actual = serde_json::to_value(&response).expect("serialize");
        assert!(actual["docs"].is_object());
        assert!(actual["code"].is_object());
        assert!(actual["warnings"].is_array());
    }
}
