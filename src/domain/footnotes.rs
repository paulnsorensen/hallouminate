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
/// block; link and image destinations are preserved as `text (url)`, so a
/// citation written as `[docs](https://…)` keeps its URL.
pub fn extract_footnotes(content: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_text = String::new();
    // Stack of (destination_url, text_len_at_open) for open links/images, so
    // nested image-in-link markup resolves both URLs in source order.
    let mut open_dests: Vec<(String, usize)> = Vec::new();

    for (event, _range) in make_parser(content) {
        match event {
            Event::Start(Tag::FootnoteDefinition(label)) => {
                current_label = Some(label.into_string());
                current_text.clear();
                open_dests.clear();
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some(label) = current_label.take() {
                    results.push((label, current_text.trim().to_string()));
                }
            }
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. })
                if current_label.is_some() =>
            {
                open_dests.push((dest_url.into_string(), current_text.len()));
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) if current_label.is_some() => {
                if let Some((dest, mark)) = open_dests.pop() {
                    // Autolinks (`<https://…>`) already render the URL as their
                    // visible text; appending it again would duplicate it.
                    if !current_text[mark..].contains(dest.as_str()) {
                        current_text.push_str(" (");
                        current_text.push_str(&dest);
                        current_text.push(')');
                    }
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

fn make_parser(content: &str) -> impl Iterator<Item = (Event<'_>, std::ops::Range<usize>)> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_FOOTNOTES);
    Parser::new_ext(content, opts).into_offset_iter()
}

/// Strip footnote definition blocks and inline `[^label]` markers.
///
/// Parser-based: `[^...]` inside fenced code blocks or inline code spans is
/// NOT treated as a footnote marker and survives untouched.
fn exclude_footnotes(content: &str) -> String {
    // Collect byte ranges to delete: inline refs + definition blocks.
    let mut remove: Vec<std::ops::Range<usize>> = Vec::new();
    let mut def_start: Option<usize> = None;

    for (event, range) in make_parser(content) {
        match event {
            Event::FootnoteReference(_) => {
                remove.push(range);
            }
            Event::Start(Tag::FootnoteDefinition(_)) => {
                def_start = Some(range.start);
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some(start) = def_start.take() {
                    remove.push(start..range.end);
                }
            }
            _ => {}
        }
    }

    if remove.is_empty() {
        return content.to_string();
    }

    remove.sort_by_key(|r| r.start);
    apply_deletions(content, &remove)
}

/// Return only the footnote definition content (raw source bytes of each
/// definition block).
///
/// Parser-based: definition ranges come from `pulldown-cmark`'s offset
/// iterator, so `[^...]` inside code is never misidentified as a definition.
fn only_footnotes(content: &str) -> String {
    let mut out = String::new();
    let mut def_start: Option<usize> = None;

    for (event, range) in make_parser(content) {
        match event {
            Event::Start(Tag::FootnoteDefinition(_)) => {
                def_start = Some(range.start);
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some(start) = def_start.take() {
                    out.push_str(&content[start..range.end]);
                }
            }
            _ => {}
        }
    }

    out
}

/// Copy `content` bytes, skipping every byte range in `ranges`.
///
/// `ranges` must be sorted by start and non-overlapping.
fn apply_deletions(content: &str, ranges: &[std::ops::Range<usize>]) -> String {
    let mut out = String::with_capacity(content.len());
    let mut pos = 0usize;
    for r in ranges {
        if pos < r.start {
            out.push_str(&content[pos..r.start]);
        }
        pos = pos.max(r.end);
    }
    if pos < content.len() {
        out.push_str(&content[pos..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_footnotes ──────────────────────────────────────────────────

    #[test]
    fn extract_footnotes_returns_ordered_label_text_pairs() {
        let md = "Some text[^1] and[^note].\n\n[^1]: First source.\n[^note]: Second source.\n";
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

    #[test]
    fn extract_footnotes_preserves_link_destination() {
        // Regression: a citation written as a markdown link must keep its URL,
        // not collapse to the link text alone.
        let md = "[^1]: See [docs](https://example.com/spec)\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].1.contains("https://example.com/spec"),
            "link URL dropped: {:?}",
            got[0].1
        );
        assert!(
            got[0].1.contains("docs"),
            "link text dropped: {:?}",
            got[0].1
        );
    }

    #[test]
    fn extract_footnotes_preserves_image_destination() {
        // Image citations must keep the destination URL too.
        let md = "[^img]: ![diagram](https://example.com/d.png)\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].1.contains("https://example.com/d.png"),
            "image URL dropped: {:?}",
            got[0].1
        );
    }

    #[test]
    fn extract_footnotes_autolink_not_duplicated() {
        // An autolink renders its URL as the visible text already — the URL
        // must appear exactly once, not twice.
        let md = "[^1]: <https://example.com/spec>\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].1.matches("https://example.com/spec").count(),
            1,
            "autolink URL duplicated: {:?}",
            got[0].1
        );
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

    // ── ADVERSARIAL: extract_footnotes edge cases ──────────────────────────

    #[test]
    fn extract_footnotes_with_unmatched_inline_marker() {
        // Inline [^missing] with no corresponding definition — should be ignored by extract
        let md = "Text[^missing] here.\n\n[^1]: Present.\n";
        let got = extract_footnotes(md);
        assert_eq!(
            got.len(),
            1,
            "only definition [^1] should be extracted: {got:?}"
        );
        assert_eq!(got[0].0, "1");
    }

    #[test]
    fn extract_footnotes_with_orphaned_definition() {
        // Definition [^orphan] with no inline marker — should still be extracted
        let md = "Some text[^used].\n\n[^used]: Cited.\n[^orphan]: Not cited.\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2, "both definitions should extract: {got:?}");
        assert_eq!(got[0].0, "used");
        assert_eq!(got[1].0, "orphan");
    }

    #[test]
    fn extract_footnotes_multiline_definition_captures_all_lines() {
        // Footnote body spans multiple indented lines
        let md = "Text[^multi].\n\n[^multi]: First line\n  second line\n  third line\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 1);
        let target = &got[0].1;
        assert!(
            target.contains("First line"),
            "first line missing: {target:?}"
        );
        assert!(
            target.contains("second line"),
            "second line missing: {target:?}"
        );
        assert!(
            target.contains("third line"),
            "third line missing: {target:?}"
        );
    }

    #[test]
    fn extract_footnotes_duplicate_labels_both_extracted() {
        // pulldown-cmark extracts BOTH definitions when labels duplicate
        let md = "[^1]: First.\n[^1]: Second.\n";
        let got = extract_footnotes(md);
        assert_eq!(
            got.len(),
            2,
            "pulldown-cmark extracts both duplicates: {got:?}"
        );
        let labels: Vec<&str> = got.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec!["1", "1"],
            "both entries present with same label"
        );
    }

    #[test]
    fn extract_footnotes_special_characters_in_label() {
        // Labels with hyphens, underscores, etc.
        let md = "[^my-note]: With hyphen.\n[^ref_2]: With underscore.\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "my-note");
        assert_eq!(got[1].0, "ref_2");
    }

    #[test]
    fn extract_footnotes_substring_labels_distinct() {
        // [^1] vs [^11] should be treated as different labels
        let md = "[^1]: First.\n[^11]: Second.\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2, "labels '1' and '11' are distinct: {got:?}");
        assert_eq!(got[0].0, "1");
        assert_eq!(got[1].0, "11");
    }

    #[test]
    fn extract_footnotes_consecutive_definitions_no_blank_line() {
        // Definitions back-to-back without blank separator
        let md = "[^1]: First.\n[^2]: Second.\n";
        let got = extract_footnotes(md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "1");
        assert_eq!(got[1].0, "2");
    }

    #[test]
    fn extract_footnotes_preserves_definition_order() {
        // Footnotes should be returned in the order they appear in the document
        let md = "[^z]: Last.\n[^a]: First.\n[^m]: Middle.\n";
        let got = extract_footnotes(md);
        assert_eq!(got[0].0, "z", "first definition order");
        assert_eq!(got[1].0, "a", "second definition order");
        assert_eq!(got[2].0, "m", "third definition order");
    }

    // ── ADVERSARIAL: apply_footnote_mode edge cases ────────────────────────

    #[test]
    fn apply_exclude_inline_marker_between_words_no_space_loss() {
        // Ensure we don't accidentally create double-spaces or lose critical spacing
        let md = "Word1[^1]Word2.\n\n[^1]: Source.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert_eq!(out, "Word1Word2.\n\n", "spacing preserved exactly: {out:?}");
    }

    #[test]
    fn apply_exclude_marker_at_line_boundary() {
        // Marker at end of line
        let md = "End of line[^1]\nStart of next line.\n\n[^1]: Citation.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(
            out.contains("End of line\nStart of next"),
            "line boundary handled: {out:?}"
        );
    }

    #[test]
    fn apply_exclude_multiple_markers_same_line() {
        // Multiple references on a single line
        let md = "A[^1] B[^2] C[^3].\n\n[^1]: One.\n[^2]: Two.\n[^3]: Three.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "all markers stripped: {out:?}");
        assert_eq!(out, "A B C.\n\n", "text collapsed correctly: {out:?}");
    }

    #[test]
    fn apply_exclude_def_with_blank_continuation_lines() {
        // Definition has blank lines between continuation lines.
        // pulldown-cmark includes the trailing whitespace-only line in the definition
        // range but treats subsequent content-bearing lines as separate paragraphs.
        let md = "Text[^1].\n\n[^1]: Start.\n  \n  End.\n\nMore.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "definition and markers removed");
        assert!(out.contains("Text"), "body preserved");
        assert!(out.contains("More."), "content after def preserved");
    }

    #[test]
    fn apply_exclude_adjacent_definitions_no_blank_line() {
        // Definitions immediately following each other
        let md = "Text[^1][^2].\n\n[^1]: First.\n[^2]: Second.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert_eq!(
            out, "Text.\n\n",
            "adjacent markers and defs removed: {out:?}"
        );
    }

    #[test]
    fn apply_exclude_definition_with_inline_formatting() {
        // Definition contains **bold** or *italic* — should still be identified as a def
        let md = "Text[^1].\n\n[^1]: **Bold source** and *italic*.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "definition removed despite formatting");
        assert_eq!(out, "Text.\n\n", "body preserved, def gone");
    }

    #[test]
    fn apply_exclude_looks_like_ref_in_code_fence_bug() {
        // [^...] inside a fenced code block is NOT a footnote reference under the
        // pulldown-cmark parser. The parser-based implementation preserves it.
        let md = "Normal[^1].\n\n```\nCode with [^fake] inside\n```\n\n[^1]: Real.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(
            out.contains("[^fake]"),
            "[^fake] in code fence must survive exclude"
        );
        assert!(!out.contains("[^1]"), "real inline ref must be stripped");
        assert!(
            !out.contains("[^1]: Real"),
            "real definition must be stripped"
        );
    }

    #[test]
    fn apply_only_preserves_multiline_definition_structure() {
        // Only mode should preserve continuation-line indentation
        let md = "Body.\n\n[^note]: Start\n  continuation\n  more.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Only);
        assert!(out.contains("[^note]: Start"), "def start preserved");
        assert!(out.contains("continuation"), "continuation lines preserved");
    }

    #[test]
    fn apply_only_with_multiple_definitions_and_blanks() {
        // Multiple defs separated by blank lines
        let md = "[^1]: First.\n\n[^2]: Second.\n\n[^3]: Third.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Only);
        assert!(out.contains("[^1]:"));
        assert!(out.contains("[^2]:"));
        assert!(out.contains("[^3]:"));
    }

    // ── ADVERSARIAL: get_footnote_target edge cases ────────────────────────

    #[test]
    fn get_footnote_target_exact_match_not_prefix() {
        // [^note] should NOT match when searching for "not"
        let md = "[^note]: Content.\n";
        assert!(
            get_footnote_target(md, "not").is_none(),
            "prefix should not match"
        );
        assert!(
            get_footnote_target(md, "note").is_some(),
            "exact label should match"
        );
    }

    #[test]
    fn get_footnote_target_case_sensitive() {
        // [^Note] vs [^note] should be different
        let md = "[^Note]: First.\n[^note]: Second.\n";
        let upper = get_footnote_target(md, "Note");
        let lower = get_footnote_target(md, "note");
        assert_eq!(upper.as_deref(), Some("First."));
        assert_eq!(lower.as_deref(), Some("Second."));
    }

    #[test]
    fn get_footnote_target_with_empty_label_search() {
        // Searching for empty label should not match or panic
        let md = "[^1]: Content.\n";
        let result = get_footnote_target(md, "");
        assert!(result.is_none(), "empty label should not match");
    }

    #[test]
    fn get_footnote_target_numeric_and_word_labels_same_doc() {
        let md = "[^123]: Numeric.\n[^note]: Word.\n[^9]: Another num.\n";
        assert_eq!(get_footnote_target(md, "123").as_deref(), Some("Numeric."));
        assert_eq!(get_footnote_target(md, "note").as_deref(), Some("Word."));
        assert_eq!(
            get_footnote_target(md, "9").as_deref(),
            Some("Another num.")
        );
    }

    #[test]
    fn get_footnote_target_with_special_chars_in_target() {
        // Target text contains special characters, URLs, etc.
        let md = "[^src]: https://example.com/path?query=1&other=2\n";
        let target = get_footnote_target(md, "src").unwrap();
        assert!(target.contains("https://"));
        assert!(target.contains("?query=1"));
    }

    // ── ADVERSARIAL: boundary and recovery ─────────────────────────────────

    #[test]
    fn apply_exclude_with_line_exactly_indented_one_space() {
        // Continuation line with exactly 1 space (minimum indentation)
        let md = "Text[^1].\n\n[^1]: Start\n Second line.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "def and marker removed");
        // "Second line." should also be gone (part of definition)
        assert!(
            !out.contains("Second line"),
            "continuation removed as part of def"
        );
    }

    #[test]
    fn apply_only_with_definition_containing_link_or_code() {
        // Definition target may contain markdown inline elements
        let md = "[^cite]: See [my docs](https://example.com) or `code`.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Only);
        assert!(out.contains("[^cite]:"), "definition preserved");
        // The function should preserve the raw markdown as-is
        assert!(
            out.contains("https://example.com") || out.contains("docs"),
            "link text preserved"
        );
    }

    #[test]
    fn extract_footnotes_empty_label_boundary() {
        // pulldown-cmark does not recognize [^]: as a footnote definition
        // (empty label is rejected by the parser).
        let md = "[^]: Empty label.\n";
        let got = extract_footnotes(md);
        assert!(
            got.is_empty(),
            "empty label is not extracted by pulldown-cmark"
        );
    }

    #[test]
    fn apply_exclude_with_unterminated_bracket_in_body() {
        // Body text has an unmatched [, but not a footnote pattern
        let md = "Text [with bracket.\n\n[^1]: Citation.\n";
        let out = apply_footnote_mode(md, FootnoteMode::Exclude);
        assert!(!out.contains("[^"), "definition removed");
        assert!(out.contains("[with bracket"), "regular [ preserved");
    }
}
