use pulldown_cmark::{Event, HeadingLevel, OffsetIter, Parser, Tag, TagEnd};

const MAX_HEADING_LEVEL: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ord: usize,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

pub fn chunk_markdown(text: &str) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    let line_starts = build_line_starts(text);
    let total_lines = line_starts.len() - 1;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stack: [Option<String>; MAX_HEADING_LEVEL] = Default::default();
    let mut path: Vec<String> = Vec::new();
    let mut start: usize = 1;

    let mut iter = Parser::new(text).into_offset_iter();
    while let Some((event, range)) = iter.next() {
        let Some(level_idx) = heading_level_idx(&event) else {
            continue;
        };
        let title = collect_heading_text(&mut iter);
        let heading_line = byte_to_line(range.start, &line_starts);
        push_chunk(
            &mut chunks,
            &path,
            text,
            &line_starts,
            start,
            heading_line - 1,
        );
        for slot in &mut stack[level_idx..] {
            *slot = None;
        }
        stack[level_idx] = Some(title);
        path = stack.iter().flatten().cloned().collect();
        start = heading_line;
    }
    push_chunk(&mut chunks, &path, text, &line_starts, start, total_lines);
    chunks
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

fn push_chunk(
    out: &mut Vec<Chunk>,
    path: &[String],
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) {
    if end < start {
        return;
    }
    let byte_start = line_starts[start - 1];
    let byte_end = line_starts[end];
    out.push(Chunk {
        ord: out.len(),
        heading_path: path.to_vec(),
        line_start: start,
        line_end: end,
        text: text[byte_start..byte_end].to_string(),
    });
}

fn build_line_starts(text: &str) -> Vec<usize> {
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

fn byte_to_line(byte: usize, line_starts: &[usize]) -> usize {
    match line_starts.binary_search(&byte) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_markdown_returns_empty_for_empty_input() {
        assert!(chunk_markdown("").is_empty());
    }

    #[test]
    fn chunk_markdown_returns_single_chunk_for_no_headings() {
        let text = "alpha\nbeta\ngamma\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        let c = &chunks[0];
        assert_eq!(c.ord, 0);
        assert!(c.heading_path.is_empty());
        assert_eq!((c.line_start, c.line_end), (1, 3));
        assert_eq!(c.text, text);
    }

    #[test]
    fn chunk_markdown_splits_on_nested_h1_h2_h3() {
        let text = "# Top\nintro\n## Sub\nbody\n### Deep\ndeep body\n## Sib\nsib\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].heading_path, vec!["Top".to_string()]);
        assert_eq!(
            chunks[1].heading_path,
            vec!["Top".to_string(), "Sub".to_string()]
        );
        assert_eq!(
            chunks[2].heading_path,
            vec!["Top".to_string(), "Sub".to_string(), "Deep".to_string()]
        );
        assert_eq!(
            chunks[3].heading_path,
            vec!["Top".to_string(), "Sib".to_string()]
        );
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 2));
        assert_eq!((chunks[1].line_start, chunks[1].line_end), (3, 4));
        assert_eq!((chunks[2].line_start, chunks[2].line_end), (5, 6));
        assert_eq!((chunks[3].line_start, chunks[3].line_end), (7, 8));
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ord, i);
        }
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn chunk_markdown_does_not_split_on_headings_inside_code_fence() {
        let text = "# Title\nbefore\n```rust\n# Not a heading\nfn main() {}\n```\nafter\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, vec!["Title".to_string()]);
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 7));
        assert_eq!(chunks[0].text, text);
    }

    #[test]
    fn chunk_markdown_emits_preamble_chunk_with_empty_path() {
        let text = "preamble\n\n# Title\nbody\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].heading_path.is_empty());
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 2));
        assert_eq!(chunks[1].heading_path, vec!["Title".to_string()]);
        assert_eq!((chunks[1].line_start, chunks[1].line_end), (3, 4));
    }

    #[test]
    fn chunk_markdown_does_not_split_on_h4_or_deeper() {
        let text = "# T\n#### Deep\nbody\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, vec!["T".to_string()]);
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 3));
    }

    #[test]
    fn chunk_markdown_replaces_lower_levels_when_higher_heading_appears() {
        let text = "# A\n## B\n### C\n## D\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks[3].heading_path,
            vec!["A".to_string(), "D".to_string()]
        );
    }

    #[test]
    fn chunk_markdown_handles_input_without_trailing_newline() {
        let text = "# A\nbody";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 2));
        assert_eq!(chunks[0].text, text);
    }

    #[test]
    fn chunk_markdown_splits_on_setext_h1_and_h2() {
        let text = "Top\n===\n\nintro\n\nSub\n---\n\nbody\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading_path, vec!["Top".to_string()]);
        assert_eq!(
            chunks[1].heading_path,
            vec!["Top".to_string(), "Sub".to_string()]
        );
    }

    #[test]
    fn chunk_markdown_strips_inline_formatting_from_heading_title() {
        let text = "# **Bold** title\nbody\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, vec!["Bold title".to_string()]);
    }

    #[test]
    fn chunk_markdown_strips_atx_closing_hashes_from_title() {
        let text = "## Title ##\nbody\n";
        let chunks = chunk_markdown(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, vec!["Title".to_string()]);
    }
}
