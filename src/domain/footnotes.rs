//! Footnote-aware transforms for markdown content.
//!
//! Operates on raw markdown text: no re-rendering. Used by the MCP adapter
//! to filter footnote noise from `ground` snippets and `read_markdown` pages
//! without touching the stored bytes or re-indexing.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use schemars::JsonSchema;
use serde::Deserialize;

/// Controls how footnote definitions and inline markers appear in a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FootnoteMode {
    /// Verbatim — current behavior. Footnotes pass through unchanged.
    #[default]
    Include,
    /// Strip footnote definition blocks and inline `[^label]` markers.
    Exclude,
    /// Return only the footnote definition lines; all other content is dropped.
    Only,
}

/// Return ordered `(label, target_text)` pairs for every footnote definition
/// in `content`. `target_text` is the plain-text content of the definition
/// block (heading and inline markup stripped).
pub fn extract_footnotes(content: &str) -> Vec<(String, String)> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(content, opts);

    let mut results = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_text = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::FootnoteDefinition(label)) => {
                current_label = Some(label.into_string());
                current_text.clear();
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some(label) = current_label.take() {
                    results.push((label, current_text.trim().to_string()));
                }
            }
            Event::Text(t) | Event::Code(t) if current_label.is_some() => {
                current_text.push_str(&t);
            }
            Event::SoftBreak | Event::HardBreak if current_label.is_some() => {
                current_text.push(' ');
            }
            _ => {}
        }
    }

    results
}

/// Apply a footnote mode to a markdown fragment (whole file or one chunk
/// snippet). `Include` is identity; `Exclude` strips both footnote definition
/// blocks and inline `[^label]` markers; `Only` returns just the footnote
/// definition lines.
pub fn apply_footnote_mode(content: &str, mode: FootnoteMode) -> String {
    match mode {
        FootnoteMode::Include => content.to_string(),
        FootnoteMode::Exclude => exclude_footnotes(content),
        FootnoteMode::Only => only_footnotes(content),
    }
}

/// Resolve a single footnote target by label. Returns `None` when the label
/// is absent.
pub fn get_footnote_target(content: &str, label: &str) -> Option<String> {
    extract_footnotes(content)
        .into_iter()
        .find(|(l, _)| l == label)
        .map(|(_, text)| text)
}

// ── private helpers ────────────────────────────────────────────────────────

/// Strip footnote definition blocks and inline `[^label]` markers.
///
/// A footnote definition block starts at a line beginning with `[^...]:`
/// and continues until the next non-indented, non-blank line (or EOF).
/// Inline references `[^label]` are stripped from ordinary lines.
fn exclude_footnotes(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut skip_block = false;

    for line in content.lines() {
        if is_footnote_def_start(line) {
            skip_block = true;
            continue;
        }
        if skip_block {
            // Continuation lines are indented (at least one space/tab) or blank.
            if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }
            skip_block = false;
        }
        // Strip inline [^label] references from ordinary lines.
        let stripped = strip_inline_refs(line);
        out.push_str(&stripped);
        out.push('\n');
    }

    // Preserve trailing newline only if the original ended with one.
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Return only the footnote definition lines (label + content), one per
/// definition. Each definition is emitted as its source lines, blank
/// continuation lines between definitions are dropped.
fn only_footnotes(content: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;

    for line in content.lines() {
        if is_footnote_def_start(line) {
            in_block = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_block {
            if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
                if !line.is_empty() {
                    out.push_str(line);
                    out.push('\n');
                }
            } else {
                in_block = false;
            }
        }
    }
    out
}

fn is_footnote_def_start(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("[^") else {
        return false;
    };
    if let Some(bracket_end) = rest.find(']') {
        let after = &rest[bracket_end + 1..];
        return after.starts_with(':');
    }
    false
}

/// Remove `[^label]` inline references from a line (not definition starters).
fn strip_inline_refs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        if ch == '[' {
            // Peek ahead to see if this is [^...]  (inline ref, not a def)
            let rest = &line[i..];
            if rest.starts_with("[^") && let Some(close) = rest.find(']') {
                // Check it's not a definition start (followed by ':')
                let after_bracket = &rest[close + 1..];
                if !after_bracket.starts_with(':') {
                    // Skip over the whole [^label]
                    let end_byte = i + close + 1;
                    // advance chars iterator past the ref
                    while let Some(&(next_i, _)) = chars.peek() {
                        if next_i >= end_byte {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_footnotes ──────────────────────────────────────────────────

    #[test]
    fn extract_footnotes_returns_ordered_label_text_pairs() {
        let md = "Some text[^1] and[^note].

[^1]: First source.
[^note]: Second source.
";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2, "expected 2 footnotes: {got:?}");
        assert_eq!(got[0].0, "1");
        assert!(got[0].1.contains("First source"), "label 1: {:?}", got[0].1);
        assert_eq!(got[1].0, "note");
        assert!(got[1].1.contains("Second source"), "note: {:?}", got[1].1);
    }

    #[test]
    fn extract_footnotes_handles_numeric_and_word_labels() {
        let md = "[^42]: Answer.\n[^todo]: To do.\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "42");
        assert_eq!(got[1].0, "todo");
    }

    #[test]
    fn extract_footnotes_empty_doc_returns_empty() {
        assert!(extract_footnotes("").is_empty());
        assert!(extract_footnotes("No footnotes here.").is_empty());
    }

    // ── apply_footnote_mode: Include ───────────────────────────────────────

    #[test]
    fn apply_include_is_identity() {
        let md = "Text[^1].\n\n[^1]: Source.\n";
        assert_eq!(apply_footnote_mode(md, FootnoteMode::Include), md);
    }

    // ── apply_footnote_mode: Exclude ───────────────────────────────────────

    #[test]
    fn apply_exclude_removes_definition_blocks() {
        let md = "Main body.\n\n[^1]: Citation here.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "def block present: {out:?}");
        assert!(out.contains("Main body."), "body missing: {out:?}");
    }

    #[test]
    fn apply_exclude_strips_inline_markers() {
        let md = "Claim[^1] is true.\n\n[^1]: Source.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "marker present: {out:?}");
        assert!(out.contains("Claim is true."), "text garbled: {out:?}");
    }

    #[test]
    fn apply_exclude_no_footnotes_is_identity() {
        let md = "Clean content.\n";
        assert_eq!(apply_footnote_mode(md, FootnoteMode::Exclude), md);
    }

    #[test]
    fn apply_exclude_multi_footnote() {
        let md = "A[^a] B[^b].\n\n[^a]: Alpha.\n[^b]: Beta.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "refs/defs present: {out:?}");
        assert!(out.contains("A B."), "body garbled: {out:?}");
    }

    // ── apply_footnote_mode: Only ──────────────────────────────────────────

    #[test]
    fn apply_only_returns_definition_lines() {
        let md = "Main body.\n\n[^1]: Citation.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Only);
        assert!(out.contains("[^1]: Citation."), "def missing: {out:?}");
        assert!(!out.contains("Main body"), "body leaked: {out:?}");
    }

    #[test]
    fn apply_only_empty_doc_yields_empty() {
        assert_eq!(apply_footnote_mode("No refs.", FootnoteMode::Only), "");
    }

    #[test]
    fn apply_only_multiple_definitions() {
        let md = "Body.[^1][^2]\n\n[^1]: First.\n[^2]: Second.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Only);
        assert!(out.contains("[^1]: First."));
        assert!(out.contains("[^2]: Second."));
        assert!(!out.contains("Body."));
    }

    // ── get_footnote_target ────────────────────────────────────────────────

    #[test]
    fn get_footnote_target_resolves_by_label() {
        let md = "[^1]: The source.\n[^note]: Another.\n";
        let got = get_footnote_target(md, "1");
        assert_eq!(got.as_deref(), Some("The source."));
    }

    #[test]
    fn get_footnote_target_returns_none_for_absent_label() {
        let md = "[^1]: Present.\n";
        assert!(get_footnote_target(md, "2").is_none());
    }

    #[test]
    fn get_footnote_target_word_label() {
        let md = "[^cite]: Author 2024.\n";
        assert_eq!(
            get_footnote_target(md, "cite").as_deref(),
            Some("Author 2024.")
        );
    }
}
