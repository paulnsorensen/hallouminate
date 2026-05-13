use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct GroundResponse {
    pub query: String,
    pub took_ms: u64,
    pub stats: Stats,
    pub docs: BTreeMap<String, DocFile>,
    pub code: BTreeMap<String, serde_json::Value>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Stats {
    pub hits: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocFile {
    pub summary: Option<String>,
    pub keywords: Vec<String>,
    pub score: f64,
    pub mtime: String,
    pub corpus: String,
    pub chunks: Vec<DocChunk>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocChunk {
    pub chunk_id: String,
    pub heading_path: Vec<String>,
    pub line_range: [u32; 2],
    pub score: f64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub code: String,
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
