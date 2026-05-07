const SNIPPET_CAP: usize = 240;

pub fn make_snippet(text: &str) -> String {
    let collapsed = collapse_whitespace(text);
    truncate_at_word_boundary(&collapsed, SNIPPET_CAP)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn truncate_at_word_boundary(s: &str, max_chars: usize) -> String {
    let total: usize = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    match head.rfind(' ') {
        Some(byte_idx) => head[..byte_idx].to_string(),
        None => head,
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
        let word = "alpha "; // 6 chars including the trailing space
        let text = word.repeat(50); // 300 chars
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
