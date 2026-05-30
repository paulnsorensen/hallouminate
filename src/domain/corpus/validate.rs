//! Non-blocking lint pass run on `add_markdown` content before it is written.
//!
//! The corpus stores the LLM's bytes verbatim, so this never rewrites or
//! rejects content — it walks the same `pulldown-cmark` event stream the
//! indexer uses and returns advisory warnings the MCP response carries back
//! in the same round-trip. Catching a broken link or empty diagram at write
//! time saves the read-discover-rewrite loop later.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

/// Walk `content` and return one advisory message per detected issue. An empty
/// vec means nothing flagged. Pure and allocation-light; no I/O.
pub fn lint_markdown(content: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut last_heading: Option<usize> = None;
    let mut iter = Parser::new(content);

    while let Some(event) = iter.next() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let n = heading_num(level);
                if let Some(prev) = last_heading
                    && n > prev + 1
                {
                    warnings.push(format!(
                        "heading level jumps from h{prev} to h{n} (skips h{})",
                        prev + 1
                    ));
                }
                last_heading = Some(n);
            }
            Event::Start(Tag::Link { dest_url, .. }) if dest_url.trim().is_empty() => {
                let text = collect_link_text(&mut iter);
                warnings.push(format!("link \"{text}\" has an empty destination"));
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info
                    .split_whitespace()
                    .next()
                    .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid")) =>
            {
                if collect_code_block(&mut iter).trim().is_empty() {
                    warnings.push("empty ```mermaid code block".to_string());
                }
            }
            _ => {}
        }
    }
    warnings
}

fn heading_num(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn collect_link_text(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::Link) => break,
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf.trim().to_string()
}

fn collect_code_block(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_document_has_no_warnings() {
        let text = "# Title\n\n## Section\n\nA [real link](https://example.com) and text.\n\n\
                    ```mermaid\ngraph TD\n  A --> B\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn flags_empty_mermaid_block() {
        let text = "# Diagram\n\n```mermaid\n```\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("mermaid"));
    }

    #[test]
    fn mermaid_with_whitespace_only_body_is_empty() {
        let text = "```mermaid\n   \n\n```\n";
        assert_eq!(lint_markdown(text).len(), 1);
    }

    #[test]
    fn non_empty_mermaid_is_not_flagged() {
        let text = "```mermaid\nsequenceDiagram\n  A->>B: hi\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn empty_fence_of_other_language_is_ignored() {
        // Only mermaid blocks are checked — an empty ```rust fence is the
        // author's business, not a diagram we can flag as broken.
        let text = "```rust\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn mermaid_match_is_case_insensitive() {
        let text = "```Mermaid\n```\n";
        assert_eq!(lint_markdown(text).len(), 1);
    }

    #[test]
    fn flags_empty_destination_link() {
        let text = "See [the spec]() for details.\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("the spec"));
        assert!(warnings[0].contains("empty destination"));
    }

    #[test]
    fn link_with_fragment_destination_is_not_flagged() {
        let text = "Jump to [section](#install).\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn flags_heading_level_jump() {
        let text = "# Top\n\n### Skipped h2\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("h1 to h3"));
    }

    #[test]
    fn sequential_heading_levels_are_not_flagged() {
        let text = "# A\n\n## B\n\n### C\n\n## D\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn heading_decrease_is_not_flagged() {
        let text = "# A\n\n## B\n\n### C\n\n# E\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn first_heading_at_any_level_is_not_flagged() {
        // Starting at h3 is an MD041-style concern we deliberately do not flag;
        // only mid-document level *jumps* are.
        let text = "### Starts deep\n\n#### Deeper\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn multiple_issues_accumulate() {
        let text = "# Title\n\n### Jump\n\n[bad]() link\n\n```mermaid\n```\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 3, "warnings: {warnings:?}");
    }

    #[test]
    fn empty_content_has_no_warnings() {
        assert!(lint_markdown("").is_empty());
    }
}
