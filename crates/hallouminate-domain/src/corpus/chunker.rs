use super::collect_heading_text;
use pulldown_cmark::{Event, HeadingLevel, OffsetIter, Parser, Tag};
use text_splitter::{ChunkConfig, MarkdownSplitter, TextSplitter};

use crate::common::{HallouminateError, Result};
use crate::embeddings::canonical_model_name;

const MAX_HEADING_LEVEL: usize = 3;

/// Re-export `text_splitter::ChunkSizer` and `tokenizers::Tokenizer` so
/// callers don't have to depend on either crate directly.
pub use text_splitter::ChunkSizer;
pub use tokenizers::Tokenizer;

/// Object-safe chunker abstraction used by the indexer.  Hides the generic
/// `ChunkSizer` parameter so `apply`/`writer` can take `&dyn CorpusChunker`
/// without propagating the type parameter into every layer.
pub trait CorpusChunker: Send + Sync {
    /// Split `text` into budget-bounded [`Chunk`]s in document order.
    ///
    /// Infallible: malformed markdown still chunks (the splitter degrades to
    /// byte windows), and empty input yields an empty `Vec`.
    fn chunk_text(&self, text: &str) -> Vec<Chunk>;
}

impl<S: ChunkSizer + Send + Sync> CorpusChunker for MarkdownChunker<S> {
    fn chunk_text(&self, text: &str) -> Vec<Chunk> {
        self.chunk(text)
    }
}

/// One budget-bounded slice of a document, annotated for citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Zero-based position of this chunk within the document's chunk sequence.
    pub ord: usize,
    /// Active heading breadcrumbs (H1→H3) above the chunk's start, outermost
    /// first. Empty when the chunk precedes any heading.
    pub heading_path: Vec<String>,
    /// First source line the chunk touches, 1-indexed.
    pub line_start: usize,
    /// Last source line the chunk touches, 1-indexed and inclusive.
    pub line_end: usize,
    /// The chunk's verbatim text slice.
    pub text: String,
}

/// Token-budgeted markdown chunker.
///
/// Owns a `text_splitter::MarkdownSplitter` configured with a tokenizer-backed
/// sizer so chunks respect the embedding model's context window.  A parallel
/// pulldown-cmark pass enriches each chunk with its active heading path and
/// line range for citation in the ground orchestrator.
pub struct MarkdownChunker<S: ChunkSizer> {
    splitter: MarkdownSplitter<S>,
}

impl<S: ChunkSizer> MarkdownChunker<S> {
    pub fn new(sizer: S, budget_tokens: usize) -> Self {
        let config: ChunkConfig<S> = ChunkConfig::new(budget_tokens).with_sizer(sizer);
        Self {
            splitter: MarkdownSplitter::new(config),
        }
    }

    /// Split `text` into budget-bounded chunks, each annotated with its
    /// heading path and line range.
    pub fn chunk(&self, text: &str) -> Vec<Chunk> {
        if text.is_empty() {
            return Vec::new();
        }
        let line_starts = build_line_starts(text);
        let breadcrumbs = build_breadcrumbs(text);
        let mut out: Vec<Chunk> = Vec::new();
        for (byte_off, slice) in self.splitter.chunk_indices(text) {
            if slice.is_empty() {
                continue;
            }
            let heading_path = heading_path_at(byte_off, &breadcrumbs);
            let line_start = byte_to_line(byte_off, &line_starts);
            let end_byte = byte_off + slice.len();
            // line_end is inclusive: the last line touched by the chunk
            let line_end = if end_byte == 0 {
                line_start
            } else {
                byte_to_line(end_byte - 1, &line_starts)
            };
            out.push(Chunk {
                ord: out.len(),
                heading_path,
                line_start,
                line_end,
                text: slice.to_string(),
            });
        }
        out
    }
}

/// Convenience: `chunk_markdown(text, sizer)` returns chunks using a fresh
/// chunker over the supplied sizer. For repeated splits, prefer constructing a
/// `MarkdownChunker` once.
pub fn chunk_markdown<S: ChunkSizer>(text: &str, sizer: S) -> Vec<Chunk> {
    // Need a generous default budget since the sizer might be Characters.
    MarkdownChunker::new(sizer, 1500).chunk(text)
}

/// Token-budgeted reStructuredText chunker.
///
/// RST is not markdown, so the markdown splitter's structure rules don't apply:
/// splitting uses `text_splitter::TextSplitter` (plain budget windows). A
/// parallel side-pass recognises RST section adornments to attach heading
/// breadcrumbs, mirroring [`MarkdownChunker`]'s citation enrichment.
pub struct RstChunker<S: ChunkSizer> {
    splitter: TextSplitter<S>,
}

impl<S: ChunkSizer> RstChunker<S> {
    pub fn new(sizer: S, budget_tokens: usize) -> Self {
        let config: ChunkConfig<S> = ChunkConfig::new(budget_tokens).with_sizer(sizer);
        Self {
            splitter: TextSplitter::new(config),
        }
    }

    /// Split `text` into budget-bounded chunks, each annotated with its RST
    /// section breadcrumb and line range.
    pub fn chunk(&self, text: &str) -> Vec<Chunk> {
        if text.is_empty() {
            return Vec::new();
        }
        let line_starts = build_line_starts(text);
        let breadcrumbs = build_rst_breadcrumbs(text);
        let mut out: Vec<Chunk> = Vec::new();
        for (byte_off, slice) in self.splitter.chunk_indices(text) {
            if slice.is_empty() {
                continue;
            }
            let heading_path = heading_path_at(byte_off, &breadcrumbs);
            let line_start = byte_to_line(byte_off, &line_starts);
            let end_byte = byte_off + slice.len();
            let line_end = if end_byte == 0 {
                line_start
            } else {
                byte_to_line(end_byte - 1, &line_starts)
            };
            out.push(Chunk {
                ord: out.len(),
                heading_path,
                line_start,
                line_end,
                text: slice.to_string(),
            });
        }
        out
    }
}

impl<S: ChunkSizer + Send + Sync> CorpusChunker for RstChunker<S> {
    fn chunk_text(&self, text: &str) -> Vec<Chunk> {
        self.chunk(text)
    }
}

// ── RST section breadcrumbs ─────────────────────────────────────

/// If `line` is an RST adornment — a run of a single ASCII-punctuation
/// character such as `=====` or `-----` — return that character. Trailing
/// whitespace is tolerated; leading whitespace disqualifies it.
fn rst_adornment_char(line: &str) -> Option<char> {
    if line.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let trimmed = line.trim_end();
    let first = trimmed.chars().next()?;
    if !first.is_ascii_punctuation() {
        return None;
    }
    if trimmed.chars().all(|c| c == first) {
        Some(first)
    } else {
        None
    }
}

/// Build section breadcrumbs for an RST document.
///
/// A section is a title line immediately followed by an underline adornment at
/// least as long as the title, optionally preceded by a matching overline. RST
/// assigns heading levels by the order in which distinct adornment *styles*
/// (character + whether overlined) first appear: the first style seen is H1,
/// the next new style H2, and so on. Levels at or beyond [`MAX_HEADING_LEVEL`]
/// are ignored, matching the markdown H4+ cutoff.
fn build_rst_breadcrumbs(text: &str) -> Vec<Breadcrumb> {
    let mut out: Vec<Breadcrumb> = vec![Breadcrumb {
        byte_offset: 0,
        path: Vec::new(),
    }];
    let lines: Vec<&str> = text.split('\n').collect();
    let mut line_offsets: Vec<usize> = Vec::with_capacity(lines.len());
    let mut off = 0usize;
    for line in &lines {
        line_offsets.push(off);
        off += line.len() + 1;
    }
    // Distinct adornment styles in first-seen order; index == heading level.
    let mut styles: Vec<(char, bool)> = Vec::new();
    let mut stack: [Option<String>; MAX_HEADING_LEVEL] = Default::default();
    for k in 1..lines.len() {
        let Some(under_char) = rst_adornment_char(lines[k]) else {
            continue;
        };
        let title = lines[k - 1];
        let title_text = title.trim();
        // The line above must be real title text, not blank and not itself an
        // adornment (an overline is handled when its underline is reached).
        if title_text.is_empty() || rst_adornment_char(title).is_some() {
            continue;
        }
        // The underline must be at least as wide as the title text.
        if lines[k].trim_end().chars().count() < title_text.chars().count() {
            continue;
        }
        let has_overline = k >= 2 && rst_adornment_char(lines[k - 2]) == Some(under_char);
        let style = (under_char, has_overline);
        let level = match styles.iter().position(|s| *s == style) {
            Some(idx) => idx,
            None => {
                styles.push(style);
                styles.len() - 1
            }
        };
        if level >= MAX_HEADING_LEVEL {
            continue;
        }
        for slot in &mut stack[level..] {
            *slot = None;
        }
        stack[level] = Some(title_text.to_string());
        let path: Vec<String> = stack.iter().flatten().cloned().collect();
        out.push(Breadcrumb {
            byte_offset: line_offsets[k - 1],
            path,
        });
    }
    out
}

/// Load a Hugging Face tokenizer for the given model and wrap it as a sizer
/// that text-splitter can use. Networked on first call; cached in the standard
/// HF cache directory.
pub fn load_tokenizer(model_id: &str) -> Result<Tokenizer> {
    let canonical_model_id = canonical_model_name(model_id)?;
    tokenizers::Tokenizer::from_pretrained(canonical_model_id, None)
        .map_err(|e| HallouminateError::Embed(format!("load tokenizer {canonical_model_id}: {e}")))
}

// ── Heading path side-pass ──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Breadcrumb {
    byte_offset: usize,
    path: Vec<String>,
}

fn build_breadcrumbs(text: &str) -> Vec<Breadcrumb> {
    let mut out: Vec<Breadcrumb> = Vec::new();
    out.push(Breadcrumb {
        byte_offset: 0,
        path: Vec::new(),
    });
    let mut stack: [Option<String>; MAX_HEADING_LEVEL] = Default::default();
    let mut iter: OffsetIter<'_> = Parser::new(text).into_offset_iter();
    while let Some((event, range)) = iter.next() {
        let Some(level_idx) = heading_level_idx(&event) else {
            continue;
        };
        let title = collect_heading_text(&mut iter);
        for slot in &mut stack[level_idx..] {
            *slot = None;
        }
        stack[level_idx] = Some(title);
        let path: Vec<String> = stack.iter().flatten().cloned().collect();
        out.push(Breadcrumb {
            byte_offset: range.start,
            path,
        });
    }
    out
}

fn heading_level_idx(event: &Event<'_>) -> Option<usize> {
    let Event::Start(Tag::Heading { level, .. }) = event else {
        return None;
    };
    match level {
        HeadingLevel::H1 => Some(0),
        HeadingLevel::H2 => Some(1),
        HeadingLevel::H3 => Some(2),
        _ => None,
    }
}

fn heading_path_at(byte_offset: usize, breadcrumbs: &[Breadcrumb]) -> Vec<String> {
    // Find the last breadcrumb whose byte_offset <= the chunk's start.
    let mut active = &breadcrumbs[0];
    for crumb in breadcrumbs.iter() {
        if crumb.byte_offset <= byte_offset {
            active = crumb;
        } else {
            break;
        }
    }
    active.path.clone()
}

// ── Line tracking ────────────────────────────────────────────────────────

pub(crate) fn build_line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(64);
    starts.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    if !text.ends_with('\n') {
        starts.push(text.len());
    }
    starts
}

pub(crate) fn byte_to_line(byte: usize, line_starts: &[usize]) -> usize {
    match line_starts.binary_search(&byte) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text_splitter::Characters;

    fn small_chunker() -> MarkdownChunker<Characters> {
        // Characters sizer; budget large enough that simple test docs become 1 chunk
        // unless we deliberately force multi-chunk behaviour with a small budget.
        MarkdownChunker::new(Characters, 2000)
    }

    fn tiny_chunker() -> MarkdownChunker<Characters> {
        // small budget to force splitting
        MarkdownChunker::new(Characters, 40)
    }

    #[test]
    fn chunk_markdown_returns_empty_for_empty_input() {
        let chunks = small_chunker().chunk("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_markdown_assigns_ords_in_order() {
        let text = "# A\n\nshort body\n\n# B\n\nanother body\n";
        let chunks = tiny_chunker().chunk(text);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ord, i, "ord mismatch at index {i}");
        }
    }

    #[test]
    fn chunk_markdown_attaches_heading_path() {
        let text = "# Top\nintro\n## Sub\nbody\n";
        let chunks = small_chunker().chunk(text);
        // every chunk should have at least the H1 in its breadcrumbs
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(!c.heading_path.is_empty(), "missing heading_path: {c:?}");
        }
    }

    #[test]
    fn chunk_markdown_records_line_range() {
        let text = "line1\nline2\nline3\n";
        let chunks = small_chunker().chunk(text);
        assert!(!chunks.is_empty());
        let first = &chunks[0];
        assert_eq!(first.line_start, 1, "{first:?}");
        assert!(first.line_end >= first.line_start);
    }

    #[test]
    fn chunk_markdown_handles_input_without_trailing_newline() {
        let text = "# A\nbody";
        let chunks = small_chunker().chunk(text);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn breadcrumbs_track_h1_h2_h3_levels() {
        let text = "# A\nx\n## B\ny\n### C\nz\n";
        let crumbs = build_breadcrumbs(text);
        // initial empty + 3 headings
        assert_eq!(crumbs.len(), 4);
        assert!(crumbs[0].path.is_empty());
        assert_eq!(crumbs[1].path, vec!["A".to_string()]);
        assert_eq!(crumbs[2].path, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(
            crumbs[3].path,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn breadcrumbs_replace_lower_levels_when_higher_appears() {
        let text = "# A\n## B\n### C\n## D\n";
        let crumbs = build_breadcrumbs(text);
        // last breadcrumb (for "## D") drops "C" and replaces "B"
        let last = crumbs.last().unwrap();
        assert_eq!(last.path, vec!["A".to_string(), "D".to_string()]);
    }

    #[test]
    fn chunk_with_tiny_budget_produces_multiple_chunks() {
        // Force splitting: budget tighter than any single paragraph
        let chunker = MarkdownChunker::new(Characters, 8);
        let text = "alpha beta gamma\ndelta epsilon zeta\neta theta iota\n";
        let chunks = chunker.chunk(text);
        assert!(
            chunks.len() >= 2,
            "tiny budget should force splitting, got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn chunks_preserve_text_reconstructable_under_concat() {
        // Use a budget that produces multiple chunks but still keeps content
        // recoverable via concatenation (text-splitter chunks are
        // non-overlapping byte windows).
        let chunker = MarkdownChunker::new(Characters, 32);
        let text = "Lorem ipsum dolor sit amet consectetur adipiscing elit\n";
        let chunks = chunker.chunk(text);
        let concat: String = chunks.iter().map(|c| c.text.as_str()).collect();
        // Reconstruction may differ in interior whitespace if text-splitter
        // trims, so we look for substring containment of the original tokens
        for token in ["Lorem", "ipsum", "consectetur", "elit"] {
            assert!(concat.contains(token), "lost token {token:?}: {concat:?}");
        }
    }

    #[test]
    fn breadcrumbs_ignore_h4_and_deeper() {
        let text = "# Top\n#### Buried\nbody\n";
        let crumbs = build_breadcrumbs(text);
        for crumb in &crumbs {
            assert!(
                !crumb.path.iter().any(|s| s == "Buried"),
                "H4 leaked into breadcrumbs: {crumb:?}"
            );
        }
    }

    #[test]
    fn line_starts_handle_input_without_trailing_newline() {
        let text = "no\nnewline\nat end";
        let starts = build_line_starts(text);
        // 1st char (pos 0), after "no\n" (pos 3), after "newline\n" (pos 11), end (pos 17)
        assert_eq!(starts.first(), Some(&0));
        assert_eq!(starts.last(), Some(&text.len()));
    }

    #[test]
    fn byte_to_line_returns_one_indexed_lines() {
        let text = "a\nb\nc\n";
        let starts = build_line_starts(text);
        assert_eq!(byte_to_line(0, &starts), 1, "first byte is line 1");
        assert_eq!(byte_to_line(2, &starts), 2, "after first \\n is line 2");
    }

    #[test]
    fn load_tokenizer_rejects_unsupported_model_before_network_call() {
        let err = load_tokenizer("clip-vit-b32").expect_err("unsupported must error locally");
        let msg = err.to_string();
        assert!(msg.contains("unsupported embedding model"), "{msg}");
    }

    fn small_rst_chunker() -> RstChunker<Characters> {
        RstChunker::new(Characters, 2000)
    }

    #[test]
    fn rst_breadcrumbs_track_underline_sections_in_order() {
        let text = "Top\n===\n\nintro\n\nSub\n---\n\nbody\n";
        let crumbs = build_rst_breadcrumbs(text);
        // initial empty + two sections
        assert_eq!(crumbs.len(), 3);
        assert!(crumbs[0].path.is_empty());
        assert_eq!(crumbs[1].path, vec!["Top".to_string()]);
        assert_eq!(crumbs[2].path, vec!["Top".to_string(), "Sub".to_string()]);
    }

    #[test]
    fn rst_breadcrumbs_treat_overline_style_as_distinct_level() {
        // Overlined `=` is a different style from underline-only `=`, so it
        // opens its own level rather than colliding: Top (overlined) is H1 and
        // Sub (underline-only) is H2.
        let text = "===\nTop\n===\n\nSub\n===\n\nbody\n";
        let crumbs = build_rst_breadcrumbs(text);
        let last = crumbs.last().unwrap();
        assert_eq!(last.path, vec!["Top".to_string(), "Sub".to_string()]);
    }

    #[test]
    fn rst_breadcrumbs_ignore_underline_shorter_than_title() {
        let text = "Title\n==\n\nbody\n";
        let crumbs = build_rst_breadcrumbs(text);
        assert_eq!(crumbs.len(), 1, "no section detected: {crumbs:?}");
    }

    #[test]
    fn rst_chunker_attaches_section_breadcrumb() {
        let text = "Overview\n========\n\nThe melange flows.\n";
        let chunks = small_rst_chunker().chunk(text);
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .all(|c| c.heading_path == vec!["Overview".to_string()]),
            "every chunk under the section: {chunks:?}"
        );
    }
}
