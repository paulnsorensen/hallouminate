use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

const SUMMARY_CAP: usize = 280;

pub fn extract_summary(text: &str, fallback_filename: &str) -> String {
    let (title, paragraph) = walk(text);
    let title = title.unwrap_or_else(|| fallback_filename.to_string());
    let combined = match paragraph {
        Some(p) => format!("{title} — {p}"),
        None => title,
    };
    combined.chars().take(SUMMARY_CAP).collect()
}

fn walk(text: &str) -> (Option<String>, Option<String>) {
    let mut title: Option<String> = None;
    let mut paragraph_before_title: Option<String> = None;
    let mut paragraph_after_title: Option<String> = None;
    let mut iter = Parser::new(text);
    while let Some(event) = iter.next() {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) if title.is_none() => {
                title = Some(collect_until_end_heading(&mut iter));
            }
            Event::Start(Tag::Paragraph) => {
                let p = collect_until_end_paragraph(&mut iter);
                if title.is_some() {
                    paragraph_after_title = Some(p);
                    break;
                }
                if paragraph_before_title.is_none() {
                    paragraph_before_title = Some(p);
                }
            }
            _ => {}
        }
    }
    let paragraph = if title.is_some() {
        paragraph_after_title
    } else {
        paragraph_before_title
    };
    (title, paragraph)
}

fn collect_until_end_heading(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::Heading(_)) => break,
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf.trim().to_string()
}

fn collect_until_end_paragraph(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::Paragraph) => break,
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            Event::SoftBreak | Event::HardBreak => buf.push(' '),
            _ => {}
        }
    }
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_summary_uses_h1_and_first_paragraph() {
        let text = "# Hallouminate\nA hybrid doc-search CLI.\nMore text.\n\n## Section\nbody\n";
        let summary = extract_summary(text, "ignored.md");
        assert_eq!(
            summary,
            "Hallouminate — A hybrid doc-search CLI. More text."
        );
    }

    #[test]
    fn extract_summary_falls_back_to_filename_when_no_h1() {
        let text = "Just some content.\nMore stuff.\n";
        let summary = extract_summary(text, "notes.md");
        assert_eq!(summary, "notes.md — Just some content. More stuff.");
    }

    #[test]
    fn extract_summary_caps_at_280_chars() {
        let long_para = "alpha ".repeat(200);
        let text = format!("# Title\n{long_para}");
        let summary = extract_summary(&text, "f.md");
        assert_eq!(summary.chars().count(), SUMMARY_CAP);
        assert!(summary.starts_with("Title — alpha"));
    }

    #[test]
    fn extract_summary_returns_just_title_when_no_paragraph() {
        let text = "# Only Heading\n";
        let summary = extract_summary(text, "f.md");
        assert_eq!(summary, "Only Heading");
    }

    #[test]
    fn extract_summary_skips_h2_and_uses_first_h1() {
        let text = "## H2 first\nignored?\n\n# Real Title\nbody.\n";
        let summary = extract_summary(text, "f.md");
        assert_eq!(summary, "Real Title — body.");
    }

    #[test]
    fn extract_summary_ignores_h1_inside_code_fence() {
        let text = "```\n# Not Title\n```\n# Real\nbody.\n";
        let summary = extract_summary(text, "f.md");
        assert_eq!(summary, "Real — body.");
    }

    #[test]
    fn extract_summary_skips_code_fence_in_paragraph_search() {
        let text = "# Title\n```\nfn fenced() {}\n```\nreal paragraph.\n";
        let summary = extract_summary(text, "f.md");
        assert_eq!(summary, "Title — real paragraph.");
    }

    #[test]
    fn extract_summary_terminates_paragraph_at_code_fence() {
        let text = "# Title\nintro line\n```\nfn fenced() {}\n```\nafter line.\n";
        let summary = extract_summary(text, "f.md");
        assert_eq!(summary, "Title — intro line");
    }
}
