mod chunker;
mod claim_marks;
mod frontmatter;
mod hasher;
pub mod index_md;
mod keywords;
pub mod sandbox;
pub mod section;
mod snippet;
mod summary;
mod validate;
mod walker;

use pulldown_cmark::{Event, OffsetIter, TagEnd};

/// Collect rendered text for the current heading by consuming events through
/// the matching `End(Heading)`. Used by both `section.rs` and `chunker.rs`.
pub(crate) fn collect_heading_text(iter: &mut OffsetIter<'_>) -> String {
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

pub use chunker::{
    Chunk, ChunkSizer, CorpusChunker, MarkdownChunker, chunk_markdown, load_tokenizer,
};
pub(crate) use chunker::{build_line_starts, byte_to_line};
pub use claim_marks::{
    ClaimMark, ClaimStatus, extract_claim_marks, lint_claim_marks, marks_to_canonical_json,
    strip_claim_marks,
};
pub use frontmatter::{Frontmatter, LifecycleStatus, lint_frontmatter, split_frontmatter};
pub use hasher::{blake3_bytes, blake3_file};
pub use keywords::extract_keywords;
pub use section::{
    LineRange, MatchError, Position, RangeError, SectionError, replace_line_range,
    replace_unique_match, splice_under_heading,
};
pub use snippet::make_snippet;
pub use summary::extract_summary;
pub use validate::lint_markdown;
pub use walker::{missing_roots, scan};
