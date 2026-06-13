//! Output formatters for `GroundResponse`. One render path, multiple
//! transports (CLI today, MCP tomorrow). Format choice and snippet trim are
//! orthogonal: every format honours `RenderOpts::snippet_chars`.

use std::fmt::Write as _;

use crate::domain::ground::types::{DocFile, GroundResponse};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Outline,
    Json,
    JsonPretty,
}

#[derive(Debug, Clone, Default)]
pub struct RenderOpts {
    /// Trim each chunk's snippet to N chars (ending with `…` if truncated).
    /// `None` preserves the chunker's output (~200 chars).
    pub snippet_chars: Option<usize>,
    /// If set, strip this prefix from every doc path in the rendered output.
    /// Only applied to the outline format; JSON formats always emit the full
    /// absolute path so structured consumers don't lose information.
    pub path_prefix_strip: Option<String>,
}

pub fn render(response: &GroundResponse, fmt: Format, opts: &RenderOpts) -> String {
    // Avoid the unconditional clone — the common case (CLI + MCP defaults)
    // is `snippet_chars == None`, where the response can be serialized
    // straight from the borrow.
    match opts.snippet_chars {
        None => render_format(response, fmt, opts.path_prefix_strip.as_deref()),
        Some(limit) => {
            let trimmed = trim_snippets(response, limit);
            render_format(&trimmed, fmt, opts.path_prefix_strip.as_deref())
        }
    }
}

fn render_format(response: &GroundResponse, fmt: Format, strip_prefix: Option<&str>) -> String {
    match fmt {
        Format::Outline => render_outline(response, strip_prefix),
        Format::Json => serde_json::to_string(response).expect("serialize GroundResponse"),
        Format::JsonPretty => {
            serde_json::to_string_pretty(response).expect("serialize GroundResponse pretty")
        }
    }
}

/// Trim every chunk's snippet down to `limit` chars. Public so that
/// callers (e.g. the MCP adapter) can apply the same trim to the
/// structured payload they hand back, keeping the `snippet_chars`
/// contract consistent across both views.
pub fn trim_snippets(response: &GroundResponse, limit: usize) -> GroundResponse {
    let mut out = response.clone();
    for doc in out.docs.values_mut() {
        for chunk in &mut doc.chunks {
            chunk.snippet = truncate(&chunk.snippet, limit);
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

fn render_outline(response: &GroundResponse, strip_prefix: Option<&str>) -> String {
    let mut buf = String::with_capacity(2048);
    writeln!(
        buf,
        "{query}  {took}ms  {hits} hits",
        query = response.query,
        took = response.took_ms,
        hits = response.stats.hits,
    )
    .expect("write header");

    // The header is always followed by a blank separator when there is any
    // body content — docs OR warnings. Warnings on a zero-doc response
    // (e.g. `code-repos-empty`) must still surface; the previous
    // early-return on empty docs dropped them silently.
    let has_body = !response.docs.is_empty() || !response.warnings.is_empty();
    if !has_body {
        return buf;
    }

    writeln!(buf).expect("blank");
    for (path, doc) in &response.docs {
        write_doc_block(&mut buf, path, doc, strip_prefix);
    }

    for warning in &response.warnings {
        writeln!(buf, "warning [{}]: {}", warning.code, warning.message).expect("warning");
    }

    buf
}

fn write_doc_block(buf: &mut String, path: &str, doc: &DocFile, strip_prefix: Option<&str>) {
    let display_path = match strip_prefix {
        Some(p) if path.starts_with(p) => path.trim_start_matches(p).to_string(),
        _ => path.to_string(),
    };
    writeln!(buf, "{display_path}  ({score:.3})", score = doc.score,).expect("doc header");
    if let Some(summary) = &doc.summary {
        writeln!(buf, "  {summary}").expect("summary");
    }
    for chunk in &doc.chunks {
        let heading = chunk.heading_path.join(" > ");
        writeln!(
            buf,
            "  L{start}-{end}  {heading}  ({score:.3})",
            start = chunk.line_range[0],
            end = chunk.line_range[1],
            score = chunk.score,
        )
        .expect("chunk header");
        writeln!(buf, "    {snippet}", snippet = chunk.snippet).expect("snippet");
    }
    writeln!(buf).expect("doc trailer");
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::ground::types::{ChunkProvenance, DocChunk, Stats, Warning};

    fn fixture() -> GroundResponse {
        let mut docs = BTreeMap::new();
        docs.insert(
            "/home/u/.cheese/research/cheese-flow/INDEX.md".into(),
            DocFile {
                summary: Some("Planner Cognition Research".into()),
                keywords: vec!["planner".into(), "plan".into()],
                score: 0.873,
                mtime: "2026-04-30T10:11:23Z".into(),
                corpus: "cheese".into(),
                chunks: vec![
                    DocChunk {
                        chunk_id: "abc123".into(),
                        heading_path: vec!["Planner Cognition Research".into()],
                        line_range: [26, 28],
                        score: 0.91,
                        snippet: "Three research rounds on planning LLMs handling code.".into(),
                        provenance: ChunkProvenance {
                            corpus: "cheese".into(),
                        },
                    },
                    DocChunk {
                        chunk_id: "def456".into(),
                        heading_path: vec!["Planner Cognition Research".into(), "Open Gaps".into()],
                        line_range: [44, 52],
                        score: 0.84,
                        snippet: "Signature-graph planning is unexplored in detail.".into(),
                        provenance: ChunkProvenance {
                            corpus: "cheese".into(),
                        },
                    },
                ],
            },
        );
        GroundResponse {
            query: "planner cognition".into(),
            took_ms: 48,
            stats: Stats { hits: 50 },
            docs,
            code: BTreeMap::new(),
            warnings: vec![],
        }
    }

    #[test]
    fn outline_format_renders_header_path_and_chunks() {
        let response = fixture();
        let out = render(&response, Format::Outline, &RenderOpts::default());

        assert!(out.starts_with("planner cognition  48ms  50 hits\n"));
        assert!(
            out.contains("/home/u/.cheese/research/cheese-flow/INDEX.md  (0.873)"),
            "full path with score: {out}"
        );
        assert!(
            out.contains("  Planner Cognition Research\n"),
            "summary indented two spaces: {out}"
        );
        assert!(
            out.contains("  L26-28  Planner Cognition Research  (0.910)"),
            "chunk heading line: {out}"
        );
        assert!(
            out.contains("    Three research rounds on planning LLMs handling code."),
            "snippet indented four spaces: {out}"
        );
        assert!(
            out.contains("  L44-52  Planner Cognition Research > Open Gaps  (0.840)"),
            "joined heading path: {out}"
        );
    }

    #[test]
    fn outline_format_strips_path_prefix_when_supplied() {
        let response = fixture();
        let opts = RenderOpts {
            path_prefix_strip: Some("/home/u/.cheese/research/".into()),
            ..Default::default()
        };
        let out = render(&response, Format::Outline, &opts);
        assert!(
            out.contains("cheese-flow/INDEX.md  (0.873)"),
            "prefix stripped: {out}"
        );
        assert!(
            !out.contains("/home/u/.cheese/research/cheese-flow/"),
            "no residual prefix: {out}"
        );
    }

    #[test]
    fn json_format_round_trips_through_serde() {
        let response = fixture();
        let out = render(&response, Format::Json, &RenderOpts::default());
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("parse json");
        assert_eq!(parsed["query"], "planner cognition");
        assert_eq!(parsed["stats"]["hits"], 50);
        assert!(parsed["docs"].is_object());
    }

    #[test]
    fn json_pretty_format_includes_indentation_and_newlines() {
        let response = fixture();
        let out = render(&response, Format::JsonPretty, &RenderOpts::default());
        assert!(out.contains("\n  \"query\""), "indentation present: {out}");
        assert!(out.contains("\"chunks\": ["), "pretty key spacing: {out}");
    }

    #[test]
    fn snippet_chars_trims_snippets_in_outline_format() {
        let response = fixture();
        let opts = RenderOpts {
            snippet_chars: Some(20),
            ..Default::default()
        };
        let out = render(&response, Format::Outline, &opts);
        // Original snippet is longer than 20 chars; truncation yields head + '…'.
        assert!(
            out.contains("    Three research roun…\n"),
            "snippet truncated with ellipsis: {out}"
        );
        // Verify no full snippet leaks through.
        assert!(
            !out.contains("rounds on planning"),
            "untrimmed text must not appear: {out}"
        );
    }

    #[test]
    fn snippet_chars_trims_snippets_in_json_format_too() {
        let response = fixture();
        let opts = RenderOpts {
            snippet_chars: Some(15),
            ..Default::default()
        };
        let out = render(&response, Format::Json, &opts);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("parse json");
        let snippet = parsed["docs"]["/home/u/.cheese/research/cheese-flow/INDEX.md"]["chunks"][0]
            ["snippet"]
            .as_str()
            .expect("snippet present");
        assert!(
            snippet.chars().count() <= 15,
            "snippet must be ≤ 15 chars: got {} chars in {snippet:?}",
            snippet.chars().count()
        );
        assert!(snippet.ends_with('…'), "ellipsis on truncation: {snippet}");
    }

    #[test]
    fn outline_empty_docs_renders_header_line_only() {
        let response = GroundResponse {
            query: "no hits".into(),
            took_ms: 5,
            stats: Stats { hits: 0 },
            docs: BTreeMap::new(),
            code: BTreeMap::new(),
            warnings: vec![],
        };
        let out = render(&response, Format::Outline, &RenderOpts::default());
        assert_eq!(
            out, "no hits  5ms  0 hits\n",
            "header only, no body: {out:?}"
        );
    }

    #[test]
    fn outline_renders_warnings_after_doc_blocks() {
        let mut response = fixture();
        response.warnings.push(Warning {
            code: "code-repos-empty".into(),
            message: "no [[code_repo]] configured".into(),
        });
        let out = render(&response, Format::Outline, &RenderOpts::default());
        assert!(
            out.contains("warning [code-repos-empty]: no [[code_repo]] configured"),
            "warning line: {out}"
        );
    }

    #[test]
    fn snippet_chars_zero_yields_empty_snippet() {
        let response = fixture();
        let opts = RenderOpts {
            snippet_chars: Some(0),
            ..Default::default()
        };
        let out = render(&response, Format::Outline, &opts);
        // An empty snippet still emits the leading 4-space prefix + newline.
        assert!(out.contains("\n    \n"), "zero-char snippet line: {out}");
    }

    #[test]
    fn outline_renders_warnings_even_when_docs_is_empty() {
        // Regression: the empty-docs early return previously dropped the
        // warnings loop entirely, hiding `code-repos-empty` and similar
        // diagnostics from anyone running a query that returned zero docs.
        let response = GroundResponse {
            query: "no hits".into(),
            took_ms: 5,
            stats: Stats { hits: 0 },
            docs: BTreeMap::new(),
            code: BTreeMap::new(),
            warnings: vec![Warning {
                code: "code-repos-empty".into(),
                message: "no [[code_repo]] configured".into(),
            }],
        };
        let out = render(&response, Format::Outline, &RenderOpts::default());
        assert!(
            out.contains("warning [code-repos-empty]: no [[code_repo]] configured"),
            "warnings must render even when docs is empty: {out:?}"
        );
    }

    #[test]
    fn outline_omits_summary_line_when_doc_lacks_summary() {
        // Without a summary, the doc header line is followed directly by
        // the first chunk header — no orphan blank line, no leftover text
        // from a stale render.
        let mut response = fixture();
        for doc in response.docs.values_mut() {
            doc.summary = None;
        }
        let out = render(&response, Format::Outline, &RenderOpts::default());
        // Doc header is still present.
        assert!(
            out.contains("/home/u/.cheese/research/cheese-flow/INDEX.md  (0.873)"),
            "doc header present: {out}"
        );
        // The next non-blank line is the first chunk header, not a summary line.
        assert!(
            out.contains("(0.873)\n  L26-28"),
            "first chunk follows doc header directly (no summary line): {out}"
        );
    }

    #[test]
    fn truncate_handles_multibyte_chars_at_boundary() {
        // Defensive: snippet trim must count chars, not bytes, or it can
        // slice a multi-byte UTF-8 codepoint and produce invalid output.
        let s = "Plänner cögnitiön";
        let t = truncate(s, 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
    }
}
