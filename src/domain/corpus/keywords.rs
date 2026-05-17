use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use stop_words::{LANGUAGE, get};
use unicode_segmentation::UnicodeSegmentation;

const TOP_K: usize = 8;
const MIN_LEN: usize = 2;

static STOPWORDS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    get(LANGUAGE::English)
        .iter()
        .map(|s| s.to_string())
        .collect()
});

pub fn extract_keywords(text: &str) -> Vec<String> {
    rank_top(tokenize_prose(text))
}

fn tokenize_prose(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut in_code_block = false;
    for event in Parser::new(text) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(t) if !in_code_block => {
                tokens.extend(t.unicode_words().map(str::to_lowercase));
            }
            Event::Code(t) => {
                tokens.extend(t.unicode_words().map(str::to_lowercase));
            }
            _ => {}
        }
    }
    tokens
}

fn rank_top(tokens: Vec<String>) -> Vec<String> {
    let mut counts: HashMap<String, (u32, usize)> = HashMap::new();
    for (i, tok) in tokens.into_iter().enumerate() {
        if tok.chars().count() < MIN_LEN || STOPWORDS.contains(&tok) {
            continue;
        }
        counts.entry(tok).and_modify(|e| e.0 += 1).or_insert((1, i));
    }
    let mut entries: Vec<(String, u32, usize)> =
        counts.into_iter().map(|(t, (c, p))| (t, c, p)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    entries.into_iter().take(TOP_K).map(|(t, _, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_keywords_ranks_by_frequency_and_filters_stopwords_and_fences() {
        let text = "\
# Search Search Search Search

Hallouminate hallouminate hallouminate.
The corpus the corpus the.
markdown markdown.

```
the the the codefence codefence codefence codefence codefence codefence
```

vector vector vector vector vector
";
        let kws = extract_keywords(text);
        assert_eq!(kws[0], "vector");
        assert_eq!(kws[1], "search");
        assert_eq!(kws[2], "hallouminate");
        assert!(kws.contains(&"corpus".to_string()));
        assert!(kws.contains(&"markdown".to_string()));
        assert!(!kws.contains(&"the".to_string()));
        assert!(!kws.contains(&"codefence".to_string()));
    }

    #[test]
    fn extract_keywords_caps_at_eight() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
        let kws = extract_keywords(text);
        assert_eq!(kws.len(), 8);
        assert_eq!(
            kws,
            vec![
                "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
            ]
        );
    }

    #[test]
    fn extract_keywords_breaks_ties_by_first_occurrence() {
        let text = "alpha beta gamma alpha beta gamma";
        let kws = extract_keywords(text);
        assert_eq!(kws, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn extract_keywords_returns_empty_for_empty_input() {
        assert!(extract_keywords("").is_empty());
    }

    #[test]
    fn extract_keywords_filters_short_tokens() {
        let text = "a b c xj qz xj qz xj";
        let kws = extract_keywords(text);
        assert_eq!(kws, vec!["xj", "qz"]);
    }

    #[test]
    fn extract_keywords_strips_punctuation_and_lowercases() {
        let text = "Search, search; SEARCH! Vectors? Vectors.";
        let kws = extract_keywords(text);
        assert_eq!(kws[0], "search");
        assert_eq!(kws[1], "vectors");
    }
}
