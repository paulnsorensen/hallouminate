use unicode_segmentation::UnicodeSegmentation;

const SNIPPET_CAP: usize = 240;

pub fn make_snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_at_word_boundary(&collapsed, SNIPPET_CAP)
}

fn truncate_at_word_boundary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut last_boundary: Option<usize> = None;
    let mut chars_so_far = 0usize;
    for (byte_idx, word) in s.split_word_bound_indices() {
        let word_chars = word.chars().count();
        if chars_so_far + word_chars > max_chars {
            break;
        }
        chars_so_far += word_chars;
        if word.chars().all(char::is_whitespace) {
            last_boundary = Some(byte_idx);
        } else {
            last_boundary = Some(byte_idx + word.len());
        }
    }
    match last_boundary {
        Some(idx) => s[..idx].trim_end().to_string(),
        None => s.chars().take(max_chars).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_snippet_returns_short_input_unchanged() {
        let text = "Just a short snippet.";
        assert_eq!(make_snippet(text), "Just a short snippet.");
    }

    #[test]
    fn make_snippet_collapses_consecutive_whitespace() {
        let text = "alpha   beta\n\nGamma\t\tdelta";
        assert_eq!(make_snippet(text), "alpha beta Gamma delta");
    }

    #[test]
    fn make_snippet_trims_leading_and_trailing_whitespace() {
        let text = "   leading and trailing   ";
        assert_eq!(make_snippet(text), "leading and trailing");
    }

    #[test]
    fn make_snippet_truncates_at_last_word_boundary_under_cap() {
        let word = "alpha ";
        let text = word.repeat(50);
        let snippet = make_snippet(&text);
        let count = snippet.chars().count();
        assert!(count <= SNIPPET_CAP, "snippet length {count} exceeds cap");
        assert!(snippet.ends_with("alpha"));
        assert!(!snippet.contains("  "));
    }

    #[test]
    fn make_snippet_at_exactly_cap_is_unchanged() {
        let text: String = "a".repeat(SNIPPET_CAP);
        assert_eq!(make_snippet(&text), text);
    }

    #[test]
    fn make_snippet_falls_back_to_hard_cut_when_no_space() {
        let text: String = "a".repeat(SNIPPET_CAP + 50);
        let snippet = make_snippet(&text);
        assert_eq!(snippet.chars().count(), SNIPPET_CAP);
    }

    #[test]
    fn make_snippet_returns_empty_for_empty_input() {
        assert_eq!(make_snippet(""), "");
    }

    #[test]
    fn make_snippet_returns_empty_for_whitespace_only_input() {
        assert_eq!(make_snippet("   \n\t\n"), "");
    }
}
