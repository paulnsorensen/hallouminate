use super::fences::FenceTracker;

const SUMMARY_CAP: usize = 280;

pub fn extract_summary(text: &str, fallback_filename: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let (title, paragraph_start) = find_title(&lines, fallback_filename);
    let paragraph = first_paragraph(&lines, paragraph_start);
    let combined = match paragraph {
        Some(p) => format!("{title} — {p}"),
        None => title,
    };
    combined.chars().take(SUMMARY_CAP).collect()
}

fn find_title(lines: &[&str], fallback: &str) -> (String, usize) {
    let mut fence = FenceTracker::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if fence.skip_line(trimmed) {
            continue;
        }
        let Some(title) = trimmed
            .strip_prefix("# ")
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        return (title.to_string(), idx + 1);
    }
    (fallback.to_string(), 0)
}

fn first_paragraph(lines: &[&str], start: usize) -> Option<String> {
    let mut fence = FenceTracker::new();
    let mut paragraph_lines: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start) {
        if fence.skip_line(line) {
            continue;
        }
        let trimmed = line.trim();
        let is_break = trimmed.is_empty() || trimmed.starts_with('#');
        if is_break && paragraph_lines.is_empty() {
            continue;
        }
        if is_break {
            break;
        }
        paragraph_lines.push(trimmed);
    }
    if paragraph_lines.is_empty() {
        None
    } else {
        Some(paragraph_lines.join(" "))
    }
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
}
