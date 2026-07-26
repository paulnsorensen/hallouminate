//! Query term extraction for search ranking.
//!
//! Deterministic, dependency-free tokenization: lowercase, split on
//! non-alphanumeric runs, drop stopwords, dedup.

/// Closed-class English stopwords, sorted. Dropped because they carry no
/// discriminative signal for term-overlap ranking.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from",
    "how", "i", "if", "in", "into", "is", "it", "its", "of", "on", "or", "that", "the", "their",
    "then", "there", "these", "they", "this", "to", "was", "what", "when", "where", "which", "who",
    "why", "will", "with", "you", "your",
];

/// Lowercase, split on whitespace and punctuation, drop English stopwords.
/// Deterministic and dependency-free.
///
/// Deduplicates while preserving first-occurrence order: downstream ranking
/// counts distinct terms matched, so callers need distinct terms, not a
/// term-frequency multiset.
pub fn split_terms(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for word in query.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() || STOPWORDS.binary_search(&word).is_ok() {
            continue;
        }
        if seen.insert(word.to_string()) {
            terms.push(word.to_string());
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_are_sorted_for_binary_search() {
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(STOPWORDS, sorted.as_slice());
    }

    #[test]
    fn multi_word_natural_language_query() {
        assert_eq!(
            split_terms("how does the search ranking work"),
            vec!["search", "ranking", "work"]
        );
    }

    #[test]
    fn punctuation_and_hyphen_splitting() {
        assert_eq!(
            split_terms("worktree-corpus, isolation!"),
            vec!["worktree", "corpus", "isolation"]
        );
    }

    #[test]
    fn stopword_removal() {
        assert_eq!(split_terms("the a an and"), Vec::<String>::new());
    }

    #[test]
    fn dedup_preserves_first_occurrence_order() {
        assert_eq!(split_terms("rank rank score rank"), vec!["rank", "score"]);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        assert_eq!(split_terms(""), Vec::<String>::new());
    }

    #[test]
    fn stopword_only_input_returns_empty_vec() {
        assert_eq!(split_terms("the of and"), Vec::<String>::new());
    }

    #[test]
    fn case_folding() {
        assert_eq!(
            split_terms("Search RANKING Fusion"),
            vec!["search", "ranking", "fusion"]
        );
    }

    #[test]
    fn query_with_numbers() {
        assert_eq!(
            split_terms("rrf k60 weight 2.0"),
            vec!["rrf", "k60", "weight", "2", "0"]
        );
    }
}
