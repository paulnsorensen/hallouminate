//! Ground search ranking.
//!
//! Four retrieval signals — BM25 full-text, dense vector, a ripgrep pass
//! and an FM-Index `contains()` pass — fuse as peer ranked lists through a
//! single weighted RRF stage, at chunk granularity.
//!
//! Fusion lives here rather than in the storage adapter because the
//! `lancedb` 0.31 Rust `Reranker` trait takes exactly two lists, so an
//! N-signal fusion cannot be expressed inside LanceDB. The adapter is a
//! retrieval backend returning per-signal lists unfused; ranking is ours.
//!
//! The two literal signals match per-term rather than against the whole
//! raw query — a twelve-word question never appears verbatim in a
//! document — and rank by how many DISTINCT query terms each chunk
//! matched. That count is the only rankable quantity these signals
//! produce without inventing a score, and it is what lets them join RRF
//! as ranked lists instead of as additive score bonuses. An additive
//! bonus is not commensurate with a rank-derived score, which is the
//! defect this design replaces.

pub mod crossencoder;
pub mod fuse;
pub mod ripgrep;
pub mod terms;

use std::collections::HashMap;

use crate::common::{CorpusKey, Result};
use crate::indexer::{ChunkStore, SearchHit};
use fuse::{RankedList, fuse};
use terms::split_terms;

pub use crossencoder::Noop as NoopCrossencoder;
pub use crossencoder::{
    Crossencoder, DEFAULT_CROSSENCODER_MODEL, SUPPORTED_CROSSENCODER_MODELS,
    canonical_crossencoder_model,
};
pub use ripgrep::RipgrepHit;

/// RRF dampening constant. 60 matches Cormack et al. (SIGIR 2009) and
/// LanceDB's stock default.
pub const RRF_K: f32 = 60.0;
/// BM25 full-text weight. Biased above the others because BM25 over short
/// markdown chunks beats generic embeddings on distinctive-token queries.
pub const FTS_WEIGHT: f32 = 2.0;
/// Dense-vector weight.
pub const VECTOR_WEIGHT: f32 = 1.0;
/// Ripgrep signal weight. Half the vector weight, not equal to it.
///
/// Ripgrep and the `contains()` pass frequently match the same chunk for
/// the same reason, so at equal weight the lexical family double-counts one
/// piece of evidence. Measured over the 73-query evaluation set: at 1.0
/// each they cost 0.0137 Recall@5 against the pre-fusion baseline; at 0.5
/// each Recall@5 returns to baseline and MRR gains 0.0150.
///
/// Halving them also shrinks the most a single literal hit can move a
/// chunk, from 44% of the fused score range to 28% — against 74% for the
/// additive bonus this design replaces, which is the defect being fixed.
pub const RIPGREP_WEIGHT: f32 = 0.5;
/// FM-Index `contains()` signal weight. See [`RIPGREP_WEIGHT`] — the two
/// literal signals are weighted together and for the same reason.
pub const CONTAINS_WEIGHT: f32 = 0.5;

/// Retrieve, rank and return chunk hits for `query`.
///
/// Every returned hit's `score` is the fused RRF score, not the
/// per-signal score the storage layer supplied.
pub async fn search_with_ripgrep(
    store: &dyn ChunkStore,
    corpus_key: &CorpusKey,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let terms = split_terms(query);
    let root = corpus_key.canonical_root.to_string_lossy().into_owned();
    let roots = [root];
    // Each term may contribute up to `limit` useful hits, so a flat
    // `limit` budget would truncate the stream before the later terms
    // are ever seen.
    let rg_budget = limit.saturating_mul(terms.len().max(1));

    let signals_fut = store.retrieve_signals(corpus_key, query, limit);
    let rg_fut = ripgrep::run(&roots, &terms, rg_budget);
    let (signals_res, rg_res) = tokio::join!(signals_fut, rg_fut);

    let signals = signals_res?;
    if signals.hits.is_empty() {
        return Ok(Vec::new());
    }
    let rg_hits = match rg_res {
        Ok(hits) => hits,
        Err(error) => {
            tracing::warn!(target: "hallouminate::search", err = %error, "ripgrep pass failed; ranking without the ripgrep signal");
            Vec::new()
        }
    };

    let rg_list = ranked_by_term_count(resolve_rg_hits_to_chunks(&signals.hits, &rg_hits));

    let fm_list = ranked_by_term_count(contains_term_counts(&signals.hits, &terms));

    let fused = fuse(
        &[
            RankedList {
                weight: FTS_WEIGHT,
                chunk_ids: signals.fts,
            },
            RankedList {
                weight: VECTOR_WEIGHT,
                chunk_ids: signals.vector,
            },
            RankedList {
                weight: RIPGREP_WEIGHT,
                chunk_ids: rg_list,
            },
            RankedList {
                weight: CONTAINS_WEIGHT,
                chunk_ids: fm_list,
            },
        ],
        RRF_K,
    );

    let mut hits = signals.hits;
    let mut ranked = Vec::with_capacity(fused.len());
    for (chunk_id, score) in fused {
        let Some(mut hit) = hits.remove(&chunk_id) else {
            continue;
        };
        hit.score = score;
        ranked.push(hit);
    }
    Ok(ranked)
}

/// Map ripgrep hits onto the chunks whose line range contains them,
/// counting the distinct query terms each chunk matched.
///
/// A hit whose line falls outside every chunk's range is dropped rather
/// than attributed to an adjacent chunk — a neighbouring chunk did not
/// contain the match, and saying otherwise would invent evidence.
///
/// A hit resolving to a chunk outside the retrieved pool is also dropped:
/// there is no `SearchHit` to return for it, so the pool bounds the
/// result set.
fn resolve_rg_hits_to_chunks(
    pool: &HashMap<String, SearchHit>,
    rg_hits: &[RipgrepHit],
) -> HashMap<String, usize> {
    if rg_hits.is_empty() {
        return HashMap::new();
    }
    let mut by_file: HashMap<&str, Vec<&SearchHit>> = HashMap::new();
    for hit in pool.values() {
        by_file.entry(hit.file_ref.as_str()).or_default().push(hit);
    }

    let mut matched: HashMap<String, Vec<String>> = HashMap::new();
    for rg_hit in rg_hits {
        let Some(candidates) = by_file.get(rg_hit.file_ref.as_str()) else {
            continue;
        };
        for chunk in candidates {
            let (Ok(start), Ok(end)) = (
                u64::try_from(chunk.line_start),
                u64::try_from(chunk.line_end),
            ) else {
                continue;
            };
            if rg_hit.line < start || rg_hit.line > end {
                continue;
            }
            let terms = matched.entry(chunk.chunk_id.clone()).or_default();
            for term in &rg_hit.matched {
                if !terms.contains(term) {
                    terms.push(term.clone());
                }
            }
        }
    }
    matched
        .into_iter()
        .map(|(chunk_id, terms)| (chunk_id, terms.len()))
        .collect()
}

/// Count, per chunk, how many distinct query `terms` appear in the
/// chunk's `search_text`.
///
/// Case-sensitive to match the FM-Index `contains()` predicate this
/// replaces (`contains(search_text, '<term>')`, no `lower()` wrap) —
/// `terms` arrive pre-lowercased from [`split_terms`] but `search_text`
/// is not lowercased, so this is a deliberate asymmetry against the
/// ripgrep signal (which matches case-insensitively), not a bug.
///
/// Matches `search_text`, not `text` — `search_text` is the
/// footnote-stripped derived field; matching `text` would leak footnote
/// content into literal matching.
fn contains_term_counts(hits: &HashMap<String, SearchHit>, terms: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for (chunk_id, hit) in hits {
        let n = terms.iter().filter(|t| hit.search_text.contains(t.as_str())).count();
        if n > 0 {
            counts.insert(chunk_id.clone(), n);
        }
    }
    counts
}

/// Order chunks by how many distinct query terms they matched, best
/// first, with `chunk_id` settling ties so the list is deterministic.
fn ranked_by_term_count(counts: HashMap<String, usize>) -> Vec<String> {
    let mut ranked: Vec<(String, usize)> =
        counts.into_iter().filter(|(_, count)| *count > 0).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(chunk_id, _)| chunk_id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn corpus_key() -> CorpusKey {
        CorpusKey {
            name: "wiki".into(),
            canonical_root: PathBuf::from("/repo/wiki"),
        }
    }

    fn hit(chunk_id: &str, file_ref: &str, line_start: usize, line_end: usize) -> SearchHit {
        SearchHit {
            chunk_id: chunk_id.into(),
            corpus_key: corpus_key(),
            file_ref: file_ref.into(),
            heading_path: vec!["H".into()],
            line_start,
            line_end,
            text: String::new(),
            search_text: String::new(),
            summary: String::new(),
            keywords: Vec::new(),
            score: 0.0,
            mtime_ms: 0,
            claim_marks: Vec::new(),
            z_score: None,
        }
    }

    fn hit_with_search_text(chunk_id: &str, search_text: &str) -> SearchHit {
        SearchHit {
            search_text: search_text.into(),
            ..hit(chunk_id, "/repo/wiki/x.md", 1, 10)
        }
    }

    fn rg_hit(file_ref: &str, line: u64, matched: &[&str]) -> RipgrepHit {
        RipgrepHit {
            file_ref: file_ref.into(),
            line,
            snippet: String::new(),
            matched: matched.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    fn pool(hits: Vec<SearchHit>) -> HashMap<String, SearchHit> {
        hits.into_iter().map(|h| (h.chunk_id.clone(), h)).collect()
    }

    #[test]
    fn rg_hit_resolves_to_the_chunk_whose_line_range_contains_it() {
        let p = pool(vec![
            hit("a", "/repo/wiki/x.md", 1, 10),
            hit("b", "/repo/wiki/x.md", 11, 20),
        ]);
        let counts = resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 15, &["term"])]);
        assert_eq!(counts.get("b"), Some(&1));
        assert_eq!(
            counts.get("a"),
            None,
            "the non-containing chunk must not match"
        );
    }

    #[test]
    fn rg_hit_outside_every_chunk_range_is_dropped_not_attributed_to_a_neighbour() {
        // Line 42 falls in the gap between chunks — a footnote region or
        // stripped content. It must not be credited to either neighbour.
        let p = pool(vec![
            hit("a", "/repo/wiki/x.md", 1, 10),
            hit("b", "/repo/wiki/x.md", 60, 70),
        ]);
        let counts = resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 42, &["term"])]);
        assert!(counts.is_empty(), "hit outside every range must be dropped");
    }

    #[test]
    fn rg_hit_in_a_file_outside_the_pool_is_dropped() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 1, 10)]);
        let counts = resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/other.md", 5, &["term"])]);
        assert!(counts.is_empty());
    }

    #[test]
    fn distinct_terms_are_counted_once_per_chunk_across_several_lines() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 1, 20)]);
        let counts = resolve_rg_hits_to_chunks(
            &p,
            &[
                rg_hit("/repo/wiki/x.md", 3, &["alpha"]),
                rg_hit("/repo/wiki/x.md", 9, &["alpha"]),
                rg_hit("/repo/wiki/x.md", 12, &["beta"]),
            ],
        );
        // "alpha" twice is one distinct term, not two.
        assert_eq!(counts.get("a"), Some(&2));
    }

    #[test]
    fn chunk_matching_more_distinct_terms_ranks_first() {
        let mut counts = HashMap::new();
        counts.insert("few".to_string(), 1);
        counts.insert("many".to_string(), 3);
        counts.insert("some".to_string(), 2);
        assert_eq!(ranked_by_term_count(counts), vec!["many", "some", "few"]);
    }

    #[test]
    fn zero_count_chunks_are_excluded_and_ties_settle_on_chunk_id() {
        let mut counts = HashMap::new();
        counts.insert("zzz".to_string(), 2);
        counts.insert("aaa".to_string(), 2);
        counts.insert("none".to_string(), 0);
        assert_eq!(ranked_by_term_count(counts), vec!["aaa", "zzz"]);
    }

    #[test]
    fn contains_term_counts_counts_distinct_terms_not_occurrences() {
        let hits = pool(vec![hit_with_search_text("a", "alpha alpha alpha alpha alpha")]);
        let terms = vec!["alpha".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert_eq!(counts.get("a"), Some(&1), "five occurrences of one term is one distinct match");
    }

    #[test]
    fn contains_term_counts_counts_distinct_terms_matched_not_total_query_terms() {
        let hits = pool(vec![hit_with_search_text("a", "alpha beta")]);
        let terms = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert_eq!(counts.get("a"), Some(&2), "two of three query terms matched");
    }

    #[test]
    fn contains_term_counts_omits_chunks_matching_no_term() {
        let hits = pool(vec![hit_with_search_text("a", "unrelated text")]);
        let terms = vec!["alpha".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert!(
            !counts.contains_key("a"),
            "a chunk matching zero terms must be absent, not present with 0"
        );
    }

    #[test]
    fn contains_term_counts_empty_terms_yields_empty_map() {
        let hits = pool(vec![hit_with_search_text("a", "alpha beta")]);
        let counts = contains_term_counts(&hits, &[]);
        assert!(counts.is_empty());
    }

    #[test]
    fn contains_term_counts_reads_search_text_not_text() {
        let mut h = hit_with_search_text("a", "the term appears here");
        h.text = "footnote-laden text without the query word".to_string();
        let hits = pool(vec![h]);
        let terms = vec!["term".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert_eq!(
            counts.get("a"),
            Some(&1),
            "the FM pass must read search_text (footnote-stripped), not text"
        );
    }
}
