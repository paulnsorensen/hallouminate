//! Invariants that the fuzz targets check on each input.
//!
//! Each check states a documented contract of a hallouminate-domain parser.
//! A panic, an index-out-of-bounds, or a failed assertion is a finding.

use std::collections::HashSet;
use std::path::{Component, Path};

use hallouminate_domain::corpus::index_md::{INDEX_END_MARKER, INDEX_START_MARKER, inject_block};
use hallouminate_domain::corpus::{
    LineRange, MarkdownChunker, MatchError, Position, RangeError, RstChunker, SlugResolution,
    extract_claim_marks, extract_keywords, extract_summary, find_wikilinks, lint_claim_marks,
    lint_frontmatter, lint_markdown, lint_wikilinks, make_snippet, normalize_slug,
    replace_line_range, replace_unique_match, resolve_slug, safe_relative_path,
    splice_under_heading, split_frontmatter, strip_claim_marks,
};
use hallouminate_domain::footnotes::{FootnoteMode, apply_footnote_mode, extract_footnotes};
use hallouminate_domain::search::phrase::{
    MAX_PHRASE_CHARS, PhraseError, collapse_whitespace, validate_phrase,
};
use hallouminate_domain::search::terms::split_terms;
use text_splitter::Characters;

const MAX_HEADING_DEPTH: usize = 3;
const SNIPPET_CAP: usize = 240;
const SUMMARY_CAP: usize = 280;
const DEFAULT_BODY: &str = "inserted line\n";
const INDEX_BODY: &str = "- [Page](page.md)\n";
const KNOWN_PATHS: [&str; 5] = [
    "index.md",
    "architecture.md",
    "docs/architecture.md",
    "a/b/c.md",
    "Claim-Provenance-Marks.md",
];
const MAX_DERIVED_PATHS: usize = 8;

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

/// The chunker a `chunker` or `rst_chunker` input exercises.
#[derive(Clone, Copy)]
pub enum ChunkerKind {
    Markdown,
    Rst,
}

/// Chunks are verbatim, ordered slices whose line ranges stay inside the
/// document.
pub fn check_chunks(text: &str, budget: usize, kind: ChunkerKind) {
    let chunks = match kind {
        ChunkerKind::Markdown => MarkdownChunker::new(Characters, budget).chunk(text),
        ChunkerKind::Rst => RstChunker::new(Characters, budget).chunk(text),
    };
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
        match kind {
            ChunkerKind::Markdown => assert!(
                chunk.heading_path.len() <= MAX_HEADING_DEPTH,
                "heading path deeper than H{MAX_HEADING_DEPTH}"
            ),
            ChunkerKind::Rst => {}
        }
    }
}

/// The section edit a `section_edit` input exercises.
enum EditOp {
    Splice,
    LineRange,
    UniqueMatch,
}

impl EditOp {
    fn from_byte(byte: u8) -> Self {
        match byte % 3 {
            0 => Self::Splice,
            1 => Self::LineRange,
            2.. => Self::UniqueMatch,
        }
    }
}

/// Section edits keep every byte outside the edit and fail only for the
/// documented reasons. `controls` picks the edit and its arguments; a NUL
/// in `input` separates the document from the inserted body.
pub fn check_section_edit(controls: [u8; 3], input: &str) {
    let [op, a, b] = controls;
    let (doc, body) = input.split_once('\0').unwrap_or((input, DEFAULT_BODY));
    match EditOp::from_byte(op) {
        EditOp::Splice => check_splice(doc, body, a, b),
        EditOp::LineRange => check_line_range(doc, body, a, b),
        EditOp::UniqueMatch => check_unique_match(doc, body, a, b),
    }
}

fn check_splice(doc: &str, body: &str, a: u8, b: u8) {
    let mut headings = Vec::new();
    for line in doc.lines() {
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        headings.push(rest.trim_start_matches('#').trim());
    }
    let heading = headings
        .get(usize::from(a) % headings.len().max(1))
        .copied()
        .unwrap_or("missing heading");
    let position = match b % 2 {
        0 => Position::Append,
        1.. => Position::Prepend,
    };
    let Ok(spliced) = splice_under_heading(doc, heading, position, body) else {
        return;
    };
    let block = format!("\n{}\n\n", body.trim_end_matches('\n'));
    assert!(
        is_single_insertion(doc, &block, &spliced),
        "splice did not insert exactly one block into the unchanged document"
    );
}

/// `edited` is `doc` with `block` inserted at one byte offset.
fn is_single_insertion(doc: &str, block: &str, edited: &str) -> bool {
    let (doc, block, edited) = (doc.as_bytes(), block.as_bytes(), edited.as_bytes());
    if edited.len() != doc.len() + block.len() {
        return false;
    }
    let mut prefix = 0;
    for (original, changed) in doc.iter().zip(edited) {
        if original != changed {
            break;
        }
        prefix += 1;
    }
    let mut suffix = 0;
    for (original, changed) in doc.iter().rev().zip(edited.iter().rev()) {
        if original != changed {
            break;
        }
        suffix += 1;
    }
    for offset in doc.len() - suffix..=prefix {
        if edited[offset..offset + block.len()] == *block {
            return true;
        }
    }
    false
}

fn check_line_range(doc: &str, body: &str, a: u8, b: u8) {
    let lines: Vec<&str> = doc.split_inclusive('\n').collect();
    let span = lines.len() + 2;
    let start = usize::from(a) % span;
    let end = usize::from(b) % span;
    match replace_line_range(doc, LineRange { start, end }, body) {
        Ok(replaced) => {
            assert!(
                1 <= start && start <= end && end <= lines.len(),
                "accepted invalid range {start}..={end} of {}",
                lines.len()
            );
            let expected = format!(
                "{}{}\n{}",
                lines[..start - 1].concat(),
                body.trim_matches('\n'),
                lines[end..].concat()
            );
            assert_eq!(
                replaced, expected,
                "line replace is not prefix, trimmed body, newline, suffix"
            );
        }
        Err(RangeError::OutOfRange) => assert!(
            start == 0 || end == 0 || end > lines.len(),
            "rejected in-range {start}..={end} of {}",
            lines.len()
        ),
        Err(RangeError::Inverted) => assert!(
            start > end && end >= 1,
            "rejected ordered {start}..={end} as inverted"
        ),
    }
}

fn check_unique_match(doc: &str, body: &str, a: u8, b: u8) {
    let mut boundaries: Vec<usize> = doc.char_indices().map(|(index, _)| index).collect();
    boundaries.push(doc.len());
    let first = usize::from(a) % boundaries.len();
    let last = (first + usize::from(b % 16)).min(boundaries.len() - 1);
    let needle = &doc[boundaries[first]..boundaries[last]];
    let count = match needle.is_empty() {
        true => 0,
        false => doc.matches(needle).count(),
    };
    match replace_unique_match(doc, needle, body) {
        Ok(replaced) => {
            assert_eq!(count, 1, "replaced a needle that is not unique");
            let Some(offset) = doc.find(needle) else {
                panic!("replaced an absent needle");
            };
            let expected = format!("{}{body}{}", &doc[..offset], &doc[offset + needle.len()..]);
            assert_eq!(
                replaced, expected,
                "unique replace did not put the body at the needle"
            );
        }
        Err(MatchError::NotFound) => assert_eq!(count, 0, "missed a present needle"),
        Err(MatchError::Ambiguous(found)) => {
            assert_eq!(found, count, "wrong ambiguity count");
            assert!(found >= 2, "ambiguous with fewer than two matches");
        }
    }
}

/// An accepted relative path has only normal components, so it can never
/// escape the corpus root.
pub fn check_relative_path(raw: &str) {
    let Ok(path) = safe_relative_path(raw) else {
        return;
    };
    assert_eq!(path, Path::new(raw), "accepted path was rewritten");
    assert!(!raw.contains('\0'), "accepted a NUL byte");
    assert!(path.is_relative(), "accepted an absolute path");
    let mut normal = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal += 1,
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                panic!("accepted {raw:?} with component {component:?}")
            }
        }
    }
    assert!(normal >= 1, "accepted a path with no file name");
}

/// Wikilink targets are trimmed and closed, and `lint_wikilinks` flags
/// exactly the targets that `resolve_slug` cannot resolve uniquely.
pub fn check_wikilinks(content: &str) {
    lint_markdown(content);
    let targets = find_wikilinks(content);
    let mut paths: Vec<String> = KNOWN_PATHS.iter().map(|path| path.to_string()).collect();
    for target in targets.iter().take(MAX_DERIVED_PATHS) {
        let path = format!("{target}.md");
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let mut expected = 0;
    for target in &targets {
        assert!(!target.is_empty(), "empty wikilink target");
        assert_eq!(target.trim(), target, "untrimmed wikilink target");
        assert!(!target.contains("]]"), "wikilink target spans a close");
        assert!(!target.contains('|'), "wikilink target keeps its alias");
        assert!(
            !normalize_slug(target).contains('\\'),
            "slug keeps a backslash"
        );
        match resolve_slug(target, &paths) {
            SlugResolution::Unique(_) => {}
            SlugResolution::Missing | SlugResolution::Ambiguous(_) => expected += 1,
        }
    }
    assert_eq!(
        lint_wikilinks(content, &paths).len(),
        expected,
        "lint_wikilinks and resolve_slug disagree"
    );
}

/// Snippet, summary, query, and index helpers keep their documented caps
/// and shapes.
pub fn check_text_helpers(text: &str) {
    assert!(
        make_snippet(text).chars().count() <= SNIPPET_CAP,
        "snippet over cap"
    );
    assert!(
        extract_summary(text, "fallback.md").chars().count() <= SUMMARY_CAP,
        "summary over cap"
    );
    extract_keywords(text);

    let terms = split_terms(text);
    let distinct: HashSet<&String> = terms.iter().collect();
    assert_eq!(distinct.len(), terms.len(), "split_terms kept a duplicate");

    let collapsed = collapse_whitespace(text);
    assert_eq!(
        collapse_whitespace(&collapsed),
        collapsed,
        "collapse_whitespace is not idempotent"
    );
    let mut previous_whitespace = false;
    for ch in collapsed.chars() {
        let whitespace = ch.is_whitespace();
        assert!(
            !(whitespace && previous_whitespace),
            "collapse_whitespace left a run"
        );
        previous_whitespace = whitespace;
    }
    assert!(
        collapsed
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .eq(text.chars().filter(|ch| !ch.is_whitespace())),
        "collapse_whitespace changed non-whitespace text"
    );

    match validate_phrase(text) {
        Ok(()) => assert!(
            !text.trim().is_empty() && text.chars().count() <= MAX_PHRASE_CHARS,
            "accepted an invalid phrase"
        ),
        Err(PhraseError::Blank) => assert!(text.trim().is_empty(), "rejected a non-blank phrase"),
        Err(PhraseError::TooLong { chars }) => {
            assert_eq!(chars, text.chars().count(), "wrong phrase length");
            assert!(chars > MAX_PHRASE_CHARS, "rejected a short phrase");
        }
    }

    let markers = text.find(INDEX_START_MARKER).and_then(|start| {
        text[start..]
            .find(INDEX_END_MARKER)
            .map(|offset| (start, start + offset))
    });
    match (inject_block(text, INDEX_BODY), markers) {
        (Some(injected), Some((start, end))) => {
            assert!(
                injected.starts_with(&text[..start]),
                "index inject changed prose before the block"
            );
            assert!(
                injected.ends_with(&text[end..]),
                "index inject changed prose after the block"
            );
        }
        (None, None) => {}
        (Some(_), None) => panic!("index inject without markers"),
        (None, Some(_)) => panic!("index inject skipped present markers"),
    }
}
