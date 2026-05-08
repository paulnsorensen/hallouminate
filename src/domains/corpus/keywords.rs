use std::collections::HashMap;

use super::fences::FenceTracker;

const TOP_K: usize = 8;
const MIN_LEN: usize = 2;

pub fn extract_keywords(text: &str) -> Vec<String> {
    let cleaned = strip_code_fences(text);
    let tokens = tokenize(&cleaned);
    rank_top(tokens)
}

fn strip_code_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence = FenceTracker::new();
    for line in text.split_inclusive('\n') {
        if !fence.skip_line(line) {
            out.push_str(line);
        }
    }
    out
}

fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in lower.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn rank_top(tokens: Vec<String>) -> Vec<String> {
    let mut counts: HashMap<String, (u32, usize)> = HashMap::new();
    for (i, tok) in tokens.into_iter().enumerate() {
        if tok.chars().count() < MIN_LEN || STOPWORDS.contains(&tok.as_str()) {
            continue;
        }
        counts.entry(tok).and_modify(|e| e.0 += 1).or_insert((1, i));
    }
    let mut entries: Vec<(String, u32, usize)> =
        counts.into_iter().map(|(t, (c, p))| (t, c, p)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    entries.into_iter().take(TOP_K).map(|(t, _, _)| t).collect()
}

const STOPWORDS: &[&str] = &[
    "an", "the", "and", "or", "but", "if", "then", "else", "for", "while", "to", "of", "in", "on",
    "at", "by", "from", "with", "into", "onto", "over", "under", "up", "down", "out", "off", "as",
    "is", "are", "was", "were", "be", "been", "being", "am", "do", "does", "did", "doing", "done",
    "have", "has", "had", "having", "this", "that", "these", "those", "you", "he", "she", "it",
    "we", "they", "them", "us", "him", "her", "my", "your", "his", "its", "our", "their", "mine",
    "yours", "ours", "theirs", "not", "no", "nor", "so", "such", "can", "could", "will", "would",
    "should", "may", "might", "must", "shall", "what", "which", "who", "whom", "whose", "where",
    "when", "why", "how", "all", "any", "both", "each", "few", "more", "most", "other", "some",
    "than", "too", "very", "just", "only", "also", "about", "above", "below", "after", "before",
    "between", "during", "through", "again", "further", "there", "here", "now", "ever", "never",
    "often", "always", "yes", "ok", "use", "used", "using", "see", "via", "let", "like", "much",
    "many", "own", "same",
];

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
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",]
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
        let text = "a b c d e ai ml ai ml ai";
        let kws = extract_keywords(text);
        assert_eq!(kws, vec!["ai", "ml"]);
    }

    #[test]
    fn extract_keywords_strips_punctuation_and_lowercases() {
        let text = "Search, search; SEARCH! Vectors? Vectors.";
        let kws = extract_keywords(text);
        assert_eq!(kws[0], "search");
        assert_eq!(kws[1], "vectors");
    }
}
