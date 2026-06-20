//! Pure splice / replace helpers for `add_markdown` section-scoped writes.
//!
//! No I/O — callers pass the existing file text and receive composed text.
//! All three functions produce byte-identical output for the unmodified
//! regions, so a splice/replace is indistinguishable from a whole-file
//! rewrite at the indexer layer.

use pulldown_cmark::{Event, OffsetIter, Parser, Tag, TagEnd};

// ── Shared types ──────────────────────────────────────────────────────────────

/// Where to splice within a section when `under_heading` is set.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    /// Splice just before the next same-or-higher heading (or EOF).
    #[default]
    Append,
    /// Splice just after the matched heading line, before existing body.
    Prepend,
}

/// 1-based inclusive line range for `replace_lines`. `start` and `end` are
/// both 1-based; the range covers lines `start..=end`. `start == end` replaces
/// a single line.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

// ── Error types ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum SectionError {
    NotFound,
    Duplicate,
}

impl std::fmt::Display for SectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SectionError::NotFound => write!(f, "heading not found"),
            SectionError::Duplicate => write!(f, "heading ambiguous"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeError {
    OutOfRange,
    Inverted,
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeError::OutOfRange => write!(f, "line range out of range"),
            RangeError::Inverted => write!(f, "start > end"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MatchError {
    NotFound,
    Ambiguous(usize),
}

impl std::fmt::Display for MatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatchError::NotFound => write!(f, "match not found"),
            MatchError::Ambiguous(n) => write!(f, "match ambiguous \u{2014} {n} occurrences"),
        }
    }
}

// ── normalize_block ──────────────────────────────────────────────────────────

/// Ensure the fragment is surrounded by blank lines so it never fuses onto
/// an adjacent line or heading. Specifically:
/// - A leading `\n` is prepended so a splice never attaches the fragment's
///   first line to the preceding heading or paragraph.
/// - A trailing `\n\n` is appended so the next heading / body starts on
///   its own line.
fn normalize_block(fragment: &str) -> String {
    let trimmed = fragment.trim_end_matches('\n');
    format!("\n{trimmed}\n\n")
}

// ── Mode 2: section splice ───────────────────────────────────────────────────

/// Splice `fragment` into the section under `heading` (matched by rendered
/// heading text, trimmed). Returns the full composed document.
///
/// - `position == Append` → insert just before the next same-or-higher
///   heading (or EOF).
/// - `position == Prepend` → insert just after the heading line.
///
/// Errors:
/// - [`SectionError::NotFound`] when no heading matches.
/// - [`SectionError::Duplicate`] when more than one heading matches.
pub fn splice_under_heading(
    doc: &str,
    heading: &str,
    position: Position,
    fragment: &str,
) -> Result<String, SectionError> {
    // Each entry: (level_u8, heading_start_byte, byte_after_heading_line)
    let mut headings: Vec<(u8, usize, usize)> = Vec::new();
    let mut match_indices: Vec<usize> = Vec::new();

    let mut iter: OffsetIter<'_> = Parser::new(doc).into_offset_iter();
    while let Some((event, range)) = iter.next() {
        let level = match &event {
            Event::Start(Tag::Heading { level, .. }) => heading_level_u8(*level),
            _ => continue,
        };
        // Collect rendered heading text by consuming through End(Heading).
        let title = collect_heading_text(&mut iter);
        // `range.end` is the byte offset past the closing of the heading
        // node per pulldown-cmark's offset iterator contract.
        // For ATX headings this is the end of the `# …\n` line.
        // For setext headings this is the end of the underline line.
        let h_start = range.start;
        let h_line_end = range.end;
        let idx = headings.len();
        headings.push((level, h_start, h_line_end));
        if title.trim() == heading.trim() {
            match_indices.push(idx);
        }
    }

    match match_indices.len() {
        0 => return Err(SectionError::NotFound),
        1 => {}
        _ => return Err(SectionError::Duplicate),
    }

    let matched = match_indices[0];
    let (level, _h_start, h_line_end) = headings[matched];

    // Section ends at the first following heading at the same or higher level
    // (lower or equal numeric level value), otherwise at EOF.
    let section_end = headings[matched + 1..]
        .iter()
        .find(|(lvl, _, _)| *lvl <= level)
        .map(|(_, start, _)| *start)
        .unwrap_or(doc.len());

    let insert_at = match position {
        Position::Prepend => h_line_end,
        Position::Append => section_end,
    };

    let block = normalize_block(fragment);
    Ok(format!(
        "{}{}{}",
        &doc[..insert_at],
        block,
        &doc[insert_at..]
    ))
}

/// Collect rendered text for the current heading by consuming events through
/// the matching `End(Heading)`. Mirrors `chunker.rs:161`.
fn collect_heading_text(iter: &mut OffsetIter<'_>) -> String {
    let mut buf = String::new();
    for (event, _) in iter.by_ref() {
        match event {
            Event::End(TagEnd::Heading(_)) => break,
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf.trim().to_string()
}

/// Map pulldown-cmark's `HeadingLevel` to a `u8` where lower == higher level
/// in the document hierarchy (H1 == 1).
fn heading_level_u8(level: pulldown_cmark::HeadingLevel) -> u8 {
    use pulldown_cmark::HeadingLevel;
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ── Mode 3: line-range replace ───────────────────────────────────────────────

/// Replace lines `[range.start, range.end]` (1-based, inclusive) with `body`.
/// Line terminators are preserved via `split_inclusive`.
///
/// Errors:
/// - [`RangeError::OutOfRange`] when `start == 0`, `end == 0`, or
///   `end > line_count`.
/// - [`RangeError::Inverted`] when `start > end`.
pub fn replace_line_range(doc: &str, range: LineRange, body: &str) -> Result<String, RangeError> {
    let LineRange { start, end } = range;
    if start == 0 || end == 0 {
        return Err(RangeError::OutOfRange);
    }
    if start > end {
        return Err(RangeError::Inverted);
    }
    // split_inclusive keeps line terminators attached so suffix reattaches
    // cleanly without fusing the body onto the next line.
    let lines: Vec<&str> = doc.split_inclusive('\n').collect();
    if end > lines.len() {
        return Err(RangeError::OutOfRange);
    }
    // 1-based inclusive [start, end] → 0-based [start-1, end)
    let prefix: String = lines[..start - 1].concat();
    let suffix: String = lines[end..].concat();
    Ok(format!("{prefix}{}{suffix}", normalize_block(body)))
}

// ── Mode 4: unique-literal-match replace ─────────────────────────────────────

/// Replace the UNIQUE non-overlapping occurrence of `needle` in `doc` with
/// `body`. Empty needle is treated as not-found.
///
/// Errors:
/// - [`MatchError::NotFound`] when `needle` is empty or has zero matches.
/// - [`MatchError::Ambiguous`] when `needle` appears more than once.
pub fn replace_unique_match(doc: &str, needle: &str, body: &str) -> Result<String, MatchError> {
    if needle.is_empty() {
        return Err(MatchError::NotFound);
    }
    let count = doc.matches(needle).count();
    match count {
        0 => Err(MatchError::NotFound),
        1 => Ok(doc.replacen(needle, body, 1)),
        _ => Err(MatchError::Ambiguous(count)),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── T1: append under existing H2 ──────────────────────────────────────────
    #[test]
    fn t1_append_under_h2_lands_before_next_heading() {
        // WHY: the basic append path must place the fragment before the next
        // same-or-higher heading so the section boundary is respected.
        let doc = "# Title\n\n## Section A\n\nExisting body.\n\n## Section B\n\nOther.\n";
        let result = splice_under_heading(doc, "Section A", Position::Append, "New line.").unwrap();
        // Fragment lands before "## Section B"
        assert!(
            result.contains("Existing body.\n\n\nNew line.\n\n## Section B"),
            "fragment must be before Section B; got: {result:?}"
        );
        // Rest is byte-identical
        assert!(result.contains("# Title"), "title must be preserved");
        assert!(
            result.contains("## Section B\n\nOther."),
            "section B must be preserved"
        );
    }

    // ── T2: prepend under existing H2 ─────────────────────────────────────────
    #[test]
    fn t2_prepend_under_h2_inserts_after_heading_line() {
        // WHY: prepend must insert right after the heading line, before
        // existing body — not at the end of the section.
        let doc = "## Section A\n\nExisting body.\n";
        let result = splice_under_heading(doc, "Section A", Position::Prepend, "First!").unwrap();
        // Byte-exact: heading + blank + fragment + blank + original body (normalize_block
        // prepends \n and appends \n\n; existing blank after heading survives).
        assert_eq!(
            result, "## Section A\n\nFirst!\n\n\nExisting body.\n",
            "prepend must produce exact heading\\n\\nfragment\\n\\n shape: {result:?}"
        );
    }

    // ── T3: append under last section (no following heading) ──────────────────
    #[test]
    fn t3_append_under_last_section_lands_at_eof() {
        // WHY: when there is no following heading the section ends at EOF;
        // the fragment must still be inserted correctly.
        let doc = "## Only Section\n\nSome text.\n";
        let result =
            splice_under_heading(doc, "Only Section", Position::Append, "New line.").unwrap();
        // doc ends with \n, normalize_block prepends \n → two \n total (one blank line)
        assert!(
            result.ends_with("Some text.\n\nNew line.\n\n"),
            "fragment must be at EOF; got: {result:?}"
        );
    }

    // ── T4: section boundary at next same-or-higher heading, not deeper H4 ────
    #[test]
    fn t4_section_end_stops_at_same_or_higher_not_child_heading() {
        // WHY: an H4 inside an H3 section must not terminate that section;
        // only a heading at H3 or above closes it.
        let doc =
            "## H2\n\n### H3 Target\n\nBody.\n\n#### H4 Child\n\nDeep.\n\n## H2 Next\n\nAfter.\n";
        let result = splice_under_heading(doc, "H3 Target", Position::Append, "New!").unwrap();
        // Fragment must appear before "## H2 Next", not before "#### H4 Child"
        let frag_pos = result.find("New!").expect("fragment present");
        let h4_pos = result.find("#### H4 Child").expect("H4 present");
        let h2_next_pos = result.find("## H2 Next").expect("H2 Next present");
        assert!(
            h4_pos < frag_pos,
            "H4 child must be before fragment (section continues past H4): {result:?}"
        );
        assert!(
            frag_pos < h2_next_pos,
            "fragment must be before H2 Next: {result:?}"
        );
    }

    // ── T5: heading not found ──────────────────────────────────────────────────
    #[test]
    fn t5_heading_not_found_returns_not_found_error() {
        // WHY: a typo'd heading must fail loudly, never silently append at EOF.
        let doc = "## Real Heading\n\nContent.\n";
        let err = splice_under_heading(doc, "Typo Heading", Position::Append, "x").unwrap_err();
        assert_eq!(err, SectionError::NotFound);
    }

    // ── T6: duplicate heading text ─────────────────────────────────────────────
    #[test]
    fn t6_duplicate_heading_returns_duplicate_error() {
        // WHY: ambiguous heading must fail; the correct target is unknowable.
        let doc = "## Same\n\nFirst.\n\n## Same\n\nSecond.\n";
        let err = splice_under_heading(doc, "Same", Position::Append, "x").unwrap_err();
        assert_eq!(err, SectionError::Duplicate);
    }

    // ── T7: # in fenced code block is not matched ──────────────────────────────
    #[test]
    fn t7_hash_in_fenced_code_not_matched_as_heading() {
        // WHY: pulldown-cmark treats fenced content as opaque, so `# x`
        // inside a code block must never match as a heading.
        let doc = "## Real\n\n```\n# not a heading\n```\n\nBody.\n";
        // If the code-block `#` were mistakenly treated as a heading, there
        // would be a duplicate and we'd get Duplicate; instead only "Real" matches.
        let result = splice_under_heading(doc, "Real", Position::Append, "Added.").unwrap();
        assert!(
            result.contains("Added."),
            "splice must succeed with only the real heading: {result:?}"
        );
    }

    // ── T8: setext heading (H2 underline) ───────────────────────────────────────
    #[test]
    fn t8_setext_heading_matched_and_splice_after_underline() {
        // WHY: setext (`Text\n---`) headings must be matchable. The prepend
        // insert point is after the underline line, not after the text line.
        let doc = "Section A\n---------\n\nExisting body.\n";
        let result =
            splice_under_heading(doc, "Section A", Position::Prepend, "Before body!").unwrap();
        // Byte-exact: underline + blank + fragment + blank + original body (insert
        // point is after underline line; normalize_block prepends \n, appends \n\n).
        assert_eq!(
            result, "Section A\n---------\n\nBefore body!\n\n\nExisting body.\n",
            "prepend after setext underline must produce exact shape: {result:?}"
        );
    }

    // ── T9: replace a middle range [3, 5] ─────────────────────────────────────
    #[test]
    fn t9_replace_middle_range() {
        // WHY: core replace_lines path; lines outside the range must be byte-identical.
        let doc = "line1\nline2\nline3\nline4\nline5\nline6\n";
        let result = replace_line_range(doc, LineRange { start: 3, end: 5 }, "replaced").unwrap();
        assert!(
            result.starts_with("line1\nline2\n"),
            "prefix byte-identical: {result:?}"
        );
        assert!(
            result.ends_with("line6\n"),
            "suffix byte-identical: {result:?}"
        );
        assert!(
            result.contains("replaced"),
            "replacement present: {result:?}"
        );
        // Original lines 3-5 must be gone
        assert!(!result.contains("line3"), "line3 must be gone: {result:?}");
        assert!(!result.contains("line4"), "line4 must be gone: {result:?}");
        assert!(!result.contains("line5"), "line5 must be gone: {result:?}");
    }

    // ── T10: single-line replace (start == end) ────────────────────────────────
    #[test]
    fn t10_single_line_replace() {
        // WHY: start == end must replace exactly one line.
        let doc = "alpha\nbeta\ngamma\n";
        let result = replace_line_range(doc, LineRange { start: 2, end: 2 }, "replaced").unwrap();
        assert!(result.contains("alpha\n"), "line1 intact: {result:?}");
        assert!(result.contains("gamma\n"), "line3 intact: {result:?}");
        assert!(!result.contains("beta"), "original line2 gone: {result:?}");
        assert!(
            result.contains("replaced"),
            "replacement present: {result:?}"
        );
    }

    // ── T11: start == 0 or end == 0 → OutOfRange ──────────────────────────────
    #[test]
    fn t11_zero_start_or_end_returns_out_of_range() {
        // WHY: 1-based indexing; 0 is invalid and must be rejected.
        let doc = "a\nb\n";
        assert_eq!(
            replace_line_range(doc, LineRange { start: 0, end: 1 }, "x").unwrap_err(),
            RangeError::OutOfRange
        );
        assert_eq!(
            replace_line_range(doc, LineRange { start: 1, end: 0 }, "x").unwrap_err(),
            RangeError::OutOfRange
        );
    }

    // ── T12: end > line_count → OutOfRange ────────────────────────────────────
    #[test]
    fn t12_end_beyond_line_count_returns_out_of_range() {
        // WHY: accessing a non-existent line must fail loudly.
        let doc = "a\nb\n";
        assert_eq!(
            replace_line_range(doc, LineRange { start: 1, end: 99 }, "x").unwrap_err(),
            RangeError::OutOfRange
        );
    }

    // ── T13: start > end → Inverted ───────────────────────────────────────────
    #[test]
    fn t13_start_greater_than_end_returns_inverted() {
        // WHY: an inverted range is always a caller error; fail loudly.
        let doc = "a\nb\nc\n";
        assert_eq!(
            replace_line_range(doc, LineRange { start: 3, end: 1 }, "x").unwrap_err(),
            RangeError::Inverted
        );
    }

    // ── T14: unique literal substring replaced ─────────────────────────────────
    #[test]
    fn t14_unique_literal_match_replaced() {
        // WHY: core replace_unique_match path; rest must be byte-identical.
        let doc = "The quick brown fox jumps over the lazy dog.\n";
        let result = replace_unique_match(doc, "brown fox", "red cat").unwrap();
        assert_eq!(result, "The quick red cat jumps over the lazy dog.\n");
    }

    // ── T15: HALLOUMINATE:INDEX marker block preserved ────────────────────────
    #[test]
    fn t15_index_marker_block_intact_when_editing_sibling_region() {
        // WHY: the INDEX marker block is daemon-owned; a section edit
        // targeting another region must not disturb it.
        let doc = concat!(
            "# Wiki Index\n",
            "\n",
            "<!-- HALLOUMINATE:INDEX-START -->\n",
            "- [A](a.md)\n",
            "<!-- HALLOUMINATE:INDEX-END -->\n",
            "\n",
            "## Prose Section\n",
            "\n",
            "Existing content.\n",
        );
        let result =
            splice_under_heading(doc, "Prose Section", Position::Append, "New entry.").unwrap();
        // Marker block must be intact
        assert!(
            result.contains("<!-- HALLOUMINATE:INDEX-START -->"),
            "INDEX-START marker must be preserved: {result:?}"
        );
        assert!(
            result.contains("<!-- HALLOUMINATE:INDEX-END -->"),
            "INDEX-END marker must be preserved: {result:?}"
        );
        // Edit must have applied
        assert!(
            result.contains("New entry."),
            "splice must be present: {result:?}"
        );
    }

    // ── T16: newline hygiene — fragment lands with surrounding blank lines ──────
    #[test]
    fn t16_newline_hygiene_no_line_fusion() {
        // WHY: normalize_block must ensure the spliced fragment never fuses
        // onto an adjacent line — surrounding blank lines are required.
        let doc = "## Section\n\nExisting.\n";
        let result = splice_under_heading(doc, "Section", Position::Append, "Fragment").unwrap();
        // The result must not have "Existing.Fragment"
        assert!(
            !result.contains("Existing.Fragment"),
            "fragment must not fuse with preceding text: {result:?}"
        );
        // Fragment followed eventually by EOF, not immediately by another word
        let frag_pos = result.find("Fragment").expect("fragment present");
        let after_frag = &result[frag_pos + "Fragment".len()..];
        assert!(
            after_frag.starts_with('\n'),
            "fragment must end with newline: after={after_frag:?}"
        );
    }

    // ── T17: substring not found ───────────────────────────────────────────────
    #[test]
    fn t17_match_not_found_returns_not_found() {
        // WHY: zero matches must fail loudly.
        let doc = "Hello world.\n";
        assert_eq!(
            replace_unique_match(doc, "xyz", "abc").unwrap_err(),
            MatchError::NotFound
        );
    }

    // ── T18: ambiguous match (>1 occurrences) ─────────────────────────────────
    #[test]
    fn t18_ambiguous_match_returns_ambiguous() {
        // WHY: >1 non-overlapping matches must fail — first-match guess
        // is a corruption hazard the caller can't see.
        let doc = "foo bar foo baz\n";
        assert_eq!(
            replace_unique_match(doc, "foo", "qux").unwrap_err(),
            MatchError::Ambiguous(2)
        );
    }

    // ── T19: empty replace_match string → NotFound ────────────────────────────
    #[test]
    fn t19_empty_needle_treated_as_not_found() {
        // WHY: an empty string matches at every position; that is ambiguous
        // in an unbounded way. Treated as not-found per decision D5.
        let doc = "Some text.\n";
        assert_eq!(
            replace_unique_match(doc, "", "x").unwrap_err(),
            MatchError::NotFound
        );
    }
}
