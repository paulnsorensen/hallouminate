use super::fences::FenceTracker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ord: usize,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

pub fn chunk_markdown(text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut stack: [Option<String>; 3] = [None, None, None];
    let mut start: usize = 1;
    let mut fence = FenceTracker::new();
    for (i, raw) in lines.iter().enumerate() {
        let lineno = i + 1;
        let trimmed = trim_line(raw);
        if fence.skip_line(trimmed) {
            continue;
        }
        let Some((level, title)) = parse_heading(trimmed) else {
            continue;
        };
        push_chunk(&mut chunks, &path, &lines, (start, lineno - 1));
        for slot in &mut stack[level - 1..] {
            *slot = None;
        }
        stack[level - 1] = Some(title);
        path = stack.iter().flatten().cloned().collect();
        start = lineno;
    }
    push_chunk(&mut chunks, &path, &lines, (start, lines.len()));
    chunks
}

fn push_chunk(out: &mut Vec<Chunk>, path: &[String], lines: &[&str], range: (usize, usize)) {
    let (start, end) = range;
    if end < start {
        return;
    }
    out.push(Chunk {
        ord: out.len(),
        heading_path: path.to_vec(),
        line_start: start,
        line_end: end,
        text: lines[start - 1..end].concat(),
    });
}

fn trim_line(raw: &str) -> &str {
    raw.trim_end_matches('\n')
        .trim_end_matches('\r')
        .trim_start()
}

fn parse_heading(line: &str) -> Option<(usize, String)> {
    let bytes = line.as_bytes();
    let mut level = 0usize;
    while level < bytes.len() && bytes[level] == b'#' {
        level += 1;
    }
    if level == 0 || level > 3 {
        return None;
    }
    if level >= bytes.len() || bytes[level] != b' ' {
        return None;
    }
    Some((level, line[level + 1..].trim().to_string()))
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
}
