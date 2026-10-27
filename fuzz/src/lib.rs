//! Invariants that the fuzz targets check on each input.
//!
//! Each check states a documented contract of a hallouminate-domain parser.
//! A panic, an index-out-of-bounds, or a failed assertion is a finding.

use hallouminate_domain::corpus::{
    MarkdownChunker, extract_claim_marks, lint_claim_marks, lint_frontmatter, split_frontmatter,
    strip_claim_marks,
};
use hallouminate_domain::footnotes::{FootnoteMode, apply_footnote_mode, extract_footnotes};
use text_splitter::Characters;

const MAX_HEADING_DEPTH: usize = 3;

fn newline_count(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

fn physical_lines(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

/// `strip_claim_marks` keeps the line count, and extracted marks point at
/// real body lines in line order.
pub fn check_claim_marks(text: &str) {
    let stripped = strip_claim_marks(text);
    assert_eq!(
        newline_count(&stripped),
        newline_count(text),
        "strip_claim_marks changed the line count"
    );
    assert!(
        stripped.len() <= text.len(),
        "strip_claim_marks grew the text"
    );

    let lines = physical_lines(text);
    let mut previous = 0;
    for mark in extract_claim_marks(text) {
        assert!(
            (1..=lines).contains(&mark.line),
            "claim mark line {} is outside 1..={lines}",
            mark.line
        );
        assert!(mark.line >= previous, "claim marks are out of line order");
        previous = mark.line;
    }

    lint_claim_marks(text);
}

/// `split_frontmatter` returns a suffix of the body and the exact number of
/// physical lines that it removed.
pub fn check_frontmatter(text: &str) {
    let (frontmatter, rest, lines) = split_frontmatter(text);
    assert!(
        text.ends_with(rest),
        "split_frontmatter rest is not a suffix"
    );
    let removed = &text[..text.len() - rest.len()];
    match frontmatter {
        Some(_) => assert_eq!(
            lines,
            physical_lines(removed),
            "frontmatter line count does not match the removed block"
        ),
        None => {
            assert!(removed.is_empty(), "rejected frontmatter removed text");
            assert_eq!(lines, 0, "rejected frontmatter reported lines");
        }
    }

    lint_frontmatter(text);
}

/// Footnote transforms only remove source text, and `Include` is identity.
pub fn check_footnotes(text: &str) {
    extract_footnotes(text);
    assert_eq!(apply_footnote_mode(text, FootnoteMode::Include), text);
    let excluded = apply_footnote_mode(text, FootnoteMode::Exclude);
    assert!(excluded.len() <= text.len(), "Exclude grew the text");
    let only = apply_footnote_mode(text, FootnoteMode::Only);
    assert!(only.len() <= text.len(), "Only grew the text");
}

/// Chunks are verbatim, ordered slices whose line ranges stay inside the
/// document.
pub fn check_chunks(text: &str, budget: usize) {
    let chunks = MarkdownChunker::new(Characters, budget).chunk(text);
    let lines = physical_lines(text);
    let mut previous_start = 1;
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ord, index, "chunk ord is not sequential");
        assert!(!chunk.text.is_empty(), "empty chunk");
        assert!(
            text.contains(&chunk.text),
            "chunk text is not a source slice"
        );
        assert!(
            1 <= chunk.line_start && chunk.line_start <= chunk.line_end && chunk.line_end <= lines,
            "chunk lines {}..={} are outside 1..={lines}",
            chunk.line_start,
            chunk.line_end
        );
        assert!(
            chunk.line_start >= previous_start,
            "chunks are out of document order"
        );
        previous_start = chunk.line_start;
        assert!(
            chunk.heading_path.len() <= MAX_HEADING_DEPTH,
            "heading path deeper than H{MAX_HEADING_DEPTH}"
        );
    }
}
