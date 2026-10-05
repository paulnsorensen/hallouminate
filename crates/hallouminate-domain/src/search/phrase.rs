//! Exact-phrase retrieval for `ground` phrase mode.
//!
//! Phrase mode bypasses BM25, vector, ripgrep, RRF, and the crossencoder.
//! The store returns every chunk whose search text holds the phrase as a
//! case-insensitive substring, and this module ranks those chunks by how
//! many times the phrase occurs. The occurrence count is the only rankable
//! quantity a literal match produces without an invented scale.
//!
//! Both sides collapse each run of Unicode whitespace to one ASCII space
//! before they compare, so a phrase that wraps across a hard line break
//! still matches. All other characters match literally.

use crate::common::{CorpusKey, HallouminateError, Result};
use crate::ground::Warning;

use super::{ChunkRetrieval, FusedSearch, hit_tie_break_key};

/// Maximum length of a phrase-mode query, in Unicode scalar values.
pub const MAX_PHRASE_CHARS: usize = 512;

/// Maximum number of matching chunks one phrase scan reads from the store.
///
/// The domain ranks every scanned chunk before it applies the caller's
/// `limit`, so this cap bounds memory, not ranking order.
pub const MAX_PHRASE_SCAN_ROWS: usize = 10_000;

/// Reason a phrase-mode query is rejected before retrieval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhraseError {
    /// The phrase is empty or holds only whitespace.
    #[error("phrase match requires a non-blank query")]
    Blank,
    /// The phrase is longer than [`MAX_PHRASE_CHARS`].
    #[error("phrase match query is {chars} characters; the maximum is {MAX_PHRASE_CHARS}")]
    TooLong {
        /// Length of the rejected phrase, in Unicode scalar values.
        chars: usize,
    },
}

/// Checks that `phrase` is a usable phrase-mode query.
///
/// # Examples
///
/// ```
/// use hallouminate_domain::search::{validate_phrase, PhraseError};
/// assert_eq!(validate_phrase("exact title"), Ok(()));
/// assert_eq!(validate_phrase("  "), Err(PhraseError::Blank));
/// ```
///
/// # Errors
///
/// Returns [`PhraseError::Blank`] for an empty or whitespace-only phrase,
/// and [`PhraseError::TooLong`] for a phrase over [`MAX_PHRASE_CHARS`].
pub fn validate_phrase(phrase: &str) -> std::result::Result<(), PhraseError> {
    if phrase.trim().is_empty() {
        return Err(PhraseError::Blank);
    }
    let chars = phrase.chars().count();
    if chars > MAX_PHRASE_CHARS {
        return Err(PhraseError::TooLong { chars });
    }
    Ok(())
}

/// Retrieves the chunks of one corpus root that contain `phrase`, ranked.
///
/// The store scans up to [`MAX_PHRASE_SCAN_ROWS`] matching chunks. This
/// function ranks all of them, then keeps the first `limit`. The order is
/// occurrence count of the lowercased phrase in the lowercased
/// `search_text`, descending, then root-relative path, `line_start`, and
/// `chunk_id`. Each returned hit's `score` is its occurrence count. Both
/// texts pass through [`collapse_whitespace`] before the comparison.
///
/// A `phrase-truncated` warning is pushed when more than `limit`
/// chunks matched or the scan reached [`MAX_PHRASE_SCAN_ROWS`]. Callers
/// must not conclude absence or uniqueness from a truncated set.
///
/// # Examples
///
/// ```no_run
/// # async fn example(store: &dyn hallouminate_domain::search::ChunkRetrieval,
/// #     key: &hallouminate_domain::common::CorpusKey) -> hallouminate_domain::common::Result<()> {
/// use hallouminate_domain::search::search_phrase;
/// let found = search_phrase(store, key, "exact title", 50).await?;
/// assert!(found.hits.len() <= 50);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns an error if `phrase` is blank, exceeds [`MAX_PHRASE_CHARS`],
/// or the store scan fails.
pub async fn search_phrase(
    store: &dyn ChunkRetrieval,
    corpus_key: &CorpusKey,
    phrase: &str,
    limit: usize,
) -> Result<FusedSearch> {
    validate_phrase(phrase).map_err(|error| HallouminateError::Search(error.to_string()))?;
    let needle = collapse_whitespace(&phrase.to_lowercase());
    let hits = store
        .retrieve_phrase(corpus_key, &needle, MAX_PHRASE_SCAN_ROWS)
        .await?;
    let matched = hits.len();
    let mut decorated = Vec::with_capacity(matched);
    for mut hit in hits {
        let count = occurrence_count(&hit.search_text, &needle);
        hit.score = count as f32;
        let key = hit_tie_break_key(&hit, &corpus_key.canonical_root);
        decorated.push((count, key, hit));
    }
    decorated.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut ranked = Vec::with_capacity(matched.min(limit));
    for (_, _, hit) in decorated.into_iter().take(limit) {
        ranked.push(hit);
    }
    let mut warnings = Vec::new();
    if let Some(warning) = truncation_warning(corpus_key, matched, limit) {
        warnings.push(warning);
    }
    Ok(FusedSearch {
        hits: ranked,
        warnings,
    })
}

fn truncation_warning(corpus_key: &CorpusKey, matched: usize, limit: usize) -> Option<Warning> {
    let root = corpus_key.canonical_root.display();
    let message = if matched >= MAX_PHRASE_SCAN_ROWS {
        format!(
            "phrase scan in corpus root {root} stopped at the cap of {MAX_PHRASE_SCAN_ROWS} matched chunks; more chunks can match, and at most {limit} are returned"
        )
    } else if matched > limit {
        format!(
            "phrase matched {matched} chunks in corpus root {root}; only the first {limit} are returned"
        )
    } else {
        return None;
    };
    Some(Warning {
        code: "phrase-truncated".to_string(),
        message,
    })
}

/// Replaces each run of Unicode whitespace in `text` with one ASCII space.
///
/// The match uses [`char::is_whitespace`], which is the Unicode
/// `White_Space` property. That property includes CR, LF, tab, and
/// U+00A0. Leading and trailing runs collapse but stay, so a phrase
/// that ends in a space still requires a word boundary.
///
/// # Examples
///
/// ```
/// use hallouminate_domain::search::collapse_whitespace;
/// assert_eq!(collapse_whitespace("a\r\n\tb\u{a0} c "), "a b c ");
/// ```
pub fn collapse_whitespace(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_run {
                collapsed.push(' ');
            }
            in_run = true;
        } else {
            collapsed.push(ch);
            in_run = false;
        }
    }
    collapsed
}

fn occurrence_count(search_text: &str, needle: &str) -> usize {
    let mut count = 0;
    for _ in collapse_whitespace(&search_text.to_lowercase()).matches(needle) {
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_phrase_rejects_blank_and_overlong_input() {
        assert_eq!(validate_phrase(""), Err(PhraseError::Blank));
        assert_eq!(validate_phrase(" \t\n"), Err(PhraseError::Blank));
        let at_limit = "é".repeat(MAX_PHRASE_CHARS);
        assert_eq!(validate_phrase(&at_limit), Ok(()));
        let over = "é".repeat(MAX_PHRASE_CHARS + 1);
        assert_eq!(
            validate_phrase(&over),
            Err(PhraseError::TooLong {
                chars: MAX_PHRASE_CHARS + 1
            })
        );
    }

    #[test]
    fn occurrence_count_is_case_insensitive_and_non_overlapping() {
        assert_eq!(
            occurrence_count("The Art of X, the art of x", "the art of x"),
            2
        );
        assert_eq!(occurrence_count("aaaa", "aa"), 2);
        assert_eq!(occurrence_count("art and x", "the art of x"), 0);
    }

    #[test]
    fn collapse_whitespace_folds_every_unicode_whitespace_run() {
        assert_eq!(collapse_whitespace("a\nb"), "a b");
        assert_eq!(collapse_whitespace("a\r\nb"), "a b");
        assert_eq!(collapse_whitespace("a\tb"), "a b");
        assert_eq!(collapse_whitespace("a  b"), "a b");
        assert_eq!(collapse_whitespace("a\u{a0}b"), "a b");
        assert_eq!(collapse_whitespace("a \n\u{3000} b"), "a b");
        assert_eq!(collapse_whitespace("\n a \n"), " a ");
        assert_eq!(collapse_whitespace("a-b"), "a-b");
    }

    #[test]
    fn occurrence_count_matches_across_wrapped_whitespace_once() {
        let text = "the company is the majority shareholder\nof both plants";
        assert_eq!(
            occurrence_count(text, "majority shareholder of both plants"),
            1
        );
        assert_eq!(
            occurrence_count("minority partner in a third\r\nfab", "third fab"),
            1
        );
        assert_eq!(
            occurrence_count("majority-shareholder of", "majority shareholder of"),
            0
        );
    }

    #[test]
    fn truncation_warning_fires_above_limit_and_at_the_scan_cap() {
        let key = CorpusKey::from_configured_root("c", "/tmp");
        assert!(truncation_warning(&key, 0, 50).is_none());
        assert!(truncation_warning(&key, 50, 50).is_none());

        let Some(over_limit) = truncation_warning(&key, 51, 50) else {
            panic!("51 matches over a limit of 50 must warn");
        };
        assert_eq!(over_limit.code, "phrase-truncated");
        assert!(
            over_limit.message.contains("matched 51 chunks"),
            "{}",
            over_limit.message
        );

        let Some(at_cap) = truncation_warning(&key, MAX_PHRASE_SCAN_ROWS, MAX_PHRASE_SCAN_ROWS)
        else {
            panic!("a scan that reaches the cap must warn even when it equals the limit");
        };
        assert_eq!(at_cap.code, "phrase-truncated");
        assert!(
            at_cap.message.contains("cap of 10000"),
            "{}",
            at_cap.message
        );
    }
}
