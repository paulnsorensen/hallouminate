//! Query term extraction for search ranking.
//!
//! Delegates lowercasing, Unicode word segmentation, stopword filtering, and
//! the minimum-length gate to [`crate::corpus::normalized_words`] so the
//! query side (this module) and the index side
//! ([`crate::corpus::extract_keywords`]) agree on what counts as a term.

use crate::corpus::normalized_words;

/// Split a query into terms via [`normalized_words`], then deduplicate while
/// preserving first-occurrence order: downstream ranking counts distinct
/// terms matched, so callers need distinct terms, not a term-frequency
/// multiset.
pub fn split_terms(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for word in normalized_words(query) {
        if seen.insert(word.clone()) {
            terms.push(word);
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrees_with_keywords_normalization_on_shared_input() {
        // The two sides tokenize independently but must never drift: this
        // pins that split_terms's dedup-and-order output matches
        // extract_keywords's frequency ranking when every term occurs once
        // (rank order degenerates to first-occurrence order).
        use crate::corpus::extract_keywords;
        let text = "Worktree-Corpus, Isolation!";
        assert_eq!(split_terms(text), extract_keywords(text));
    }

    #[test]
    fn multi_word_natural_language_query() {
        // The shared list is stop-words' NLTK English (198 entries), pinned
        // explicitly in Cargo.toml — see the comment there, and note that
        // the crate's own default (ISO, 1298 entries) would drop "work"
        // here along with "call", "side" and "use". Function words go,
        // content words stay.
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
    fn unicode_word_boundaries_keep_apostrophes_inside_a_word() {
        assert_eq!(split_terms("O'Brien fusion"), vec!["o'brien", "fusion"]);
    }

    #[test]
    fn single_character_tokens_are_dropped() {
        assert_eq!(split_terms("z zz q"), vec!["zz"]);
    }

    #[test]
    fn query_with_numbers_drops_single_digit_terms() {
        // "2.0" is one Unicode word (mid-word period), so it clears MIN_LEN
        // and survives; bare "2"/"0" would not.
        assert_eq!(
            split_terms("rrf k60 weight 2.0"),
            vec!["rrf", "k60", "weight", "2.0"]
        );
    }
}
