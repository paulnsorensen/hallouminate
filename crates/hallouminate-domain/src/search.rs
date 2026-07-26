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
//!
//! The two literal signals are computed over the FTS/vector candidate
//! pool ([`search_fused`]), not the full corpus: they reorder
//! candidates BM25 or the vector search already retrieved and cannot
//! contribute a candidate of their own.

pub mod crossencoder;
pub mod fuse;
pub mod ripgrep;
pub mod terms;

use std::collections::HashMap;

use crate::common::{CorpusKey, Result};
use crate::ground::Warning;
use crate::indexer::{SearchHit, SignalLists};
use async_trait::async_trait;
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

/// Storage-agnostic port for chunk retrieval (ranking-time reads).
///
/// Split from `ChunkStore` so the ranking path can depend on retrieval
/// alone and never reach the write path.
#[async_trait]
pub trait ChunkRetrieval: Send + Sync {
    /// Retrieve the FTS and vector ranked lists separately, unfused.
    async fn retrieve_signals(
        &self,
        corpus_key: &CorpusKey,
        query: &str,
        limit: usize,
    ) -> Result<SignalLists>;
}

/// Outcome of [`search_fused`]: the fused hits plus any warnings raised
/// while assembling the four signals (e.g. a degraded ripgrep pass).
pub struct FusedSearch {
    pub hits: Vec<SearchHit>,
    pub warnings: Vec<Warning>,
}

/// Retrieve, rank and return chunk hits for `query`.
///
/// Every returned hit's `score` is the fused RRF score, not the
/// per-signal score the storage layer supplied.
///
/// The ripgrep and FM-Index `contains()` passes below only filter and
/// rank `signals.hits` — the pool `store.retrieve_signals` returned —
/// so a chunk outside that FTS/vector pool is invisible to both.
pub async fn search_fused(
    store: &dyn ChunkRetrieval,
    corpus_key: &CorpusKey,
    query: &str,
    limit: usize,
) -> Result<FusedSearch> {
    let terms = cap_terms(split_terms(query));
    let mut warnings = Vec::new();
    // Bound on the rg subprocess. The `max_hits` truncation only fires when
    // matches are plentiful; a sparse-or-empty match still forces rg to walk
    // the whole corpus root under `--sort path`'s single traversal thread.
    // This deadline caps that worst case instead of letting a rare query
    // stall Ground on however long the corpus takes to walk exhaustively.
    const RIPGREP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
    let root = corpus_key.canonical_root.to_string_lossy().into_owned();
    let roots = [root];
    // Each term may contribute up to `limit` useful hits, so a flat
    // `limit` budget would truncate the stream before the later terms
    // are ever seen.
    let rg_budget = limit.saturating_mul(terms.len().max(1));

    let signals_fut = store.retrieve_signals(corpus_key, query, limit);
    let rg_fut = tokio::time::timeout(RIPGREP_TIMEOUT, ripgrep::run(&roots, &terms, rg_budget));
    let (signals_res, rg_res) = tokio::join!(signals_fut, rg_fut);

    let signals = signals_res?;
    if signals.hits.is_empty() {
        return Ok(FusedSearch {
            hits: Vec::new(),
            warnings,
        });
    }
    let (rg_hits, rg_truncated, rg_elapsed_ms, rg_unparseable) = match rg_res {
        Ok(Ok(run)) => (run.hits, run.truncated, run.elapsed_ms, run.unparseable),
        Ok(Err(error)) => {
            tracing::warn!(target: "hallouminate::search", err = %error, "ripgrep pass failed; ranking without the ripgrep signal");
            warnings.push(Warning {
                code: "ripgrep-failed".to_string(),
                message: format!(
                    "ripgrep pass failed ({error}); ranking without the ripgrep signal"
                ),
            });
            (Vec::new(), false, 0, 0)
        }
        Err(_elapsed) => {
            tracing::warn!(
                target: "hallouminate::search",
                timeout_ms = RIPGREP_TIMEOUT.as_millis() as u64,
                "ripgrep pass timed out; ranking without the ripgrep signal"
            );
            warnings.push(Warning {
                code: "ripgrep-timeout".to_string(),
                message: format!(
                    "ripgrep pass timed out after {} ms; ranking without the ripgrep signal",
                    RIPGREP_TIMEOUT.as_millis()
                ),
            });
            (Vec::new(), false, RIPGREP_TIMEOUT.as_millis() as u64, 0)
        }
    };

    let (rg_chunk_counts, rg_stats) = resolve_rg_hits_to_chunks(&signals.hits, &rg_hits);
    let resolved_chunks = rg_chunk_counts.len();
    let rg_list = ranked_by_term_count(rg_chunk_counts);

    tracing::debug!(
        target: "hallouminate::search",
        rg_hits = rg_hits.len(),
        resolved_chunks,
        dropped_file_not_in_pool = rg_stats.dropped_file_not_in_pool,
        dropped_line_out_of_range = rg_stats.dropped_line_out_of_range,
        truncated = rg_truncated,
        unparseable = rg_unparseable,
        elapsed_ms = rg_elapsed_ms,
        "ripgrep signal resolved"
    );
    if !rg_hits.is_empty() && resolved_chunks == 0 {
        tracing::warn!(
            target: "hallouminate::search",
            rg_hits = rg_hits.len(),
            dropped_file_not_in_pool = rg_stats.dropped_file_not_in_pool,
            dropped_line_out_of_range = rg_stats.dropped_line_out_of_range,
            "ripgrep signal produced hits but resolved to zero chunks"
        );
        warnings.push(Warning {
            code: "ripgrep-unresolved".to_string(),
            message: format!(
                "ripgrep signal produced {} hits but resolved to zero chunks in corpus root {}",
                rg_hits.len(),
                corpus_key.canonical_root.display()
            ),
        });
    }

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
    Ok(FusedSearch {
        hits: ranked,
        warnings,
    })
}

/// Ceiling on how many query terms reach the two literal signals.
///
/// Each term becomes an `-e` pattern on the rg command line and a pass
/// over every pooled chunk's `search_text`, and the rg budget is sized as
/// `limit * terms.len()`, so an arbitrarily long query steers both the
/// argument list and the amount of scanning. 32 content terms is well past
/// any real question — the evaluation set's longest query yields 12.
const MAX_QUERY_TERMS: usize = 32;

/// Truncate to [`MAX_QUERY_TERMS`], keeping the **first** terms.
///
/// `split_terms` returns first-occurrence order, so the head of the list
/// is the opening of the query — the part a reader would call the subject.
fn cap_terms(mut terms: Vec<String>) -> Vec<String> {
    terms.truncate(MAX_QUERY_TERMS);
    terms
}

/// Result counters from [`resolve_rg_hits_to_chunks`], split by drop cause.
#[derive(Debug, Clone, Default)]
struct RgResolveStats {
    /// The hit's file was never retrieved by the FTS/vector pool — a known
    /// pool-gating limitation, tracked separately from the cause below.
    dropped_file_not_in_pool: usize,
    /// The hit's line fell outside every retrieved chunk's range — the
    /// silent-match-loss risk ADR-003 flags (footnote or stripped regions
    /// swallowing most of a signal's matches).
    dropped_line_out_of_range: usize,
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
///
/// Returns the per-chunk distinct-term counts alongside [`RgResolveStats`],
/// which keeps the two drop causes as separate counters instead of one
/// conflated total.
fn resolve_rg_hits_to_chunks(
    pool: &HashMap<String, SearchHit>,
    rg_hits: &[RipgrepHit],
) -> (HashMap<String, usize>, RgResolveStats) {
    if rg_hits.is_empty() {
        return (HashMap::new(), RgResolveStats::default());
    }
    let mut by_file: HashMap<&str, Vec<&SearchHit>> = HashMap::new();
    for hit in pool.values() {
        by_file.entry(hit.file_ref.as_str()).or_default().push(hit);
    }

    let mut stats = RgResolveStats::default();
    let mut matched: HashMap<String, Vec<String>> = HashMap::new();
    for rg_hit in rg_hits {
        let Some(candidates) = by_file.get(rg_hit.file_ref.as_str()) else {
            stats.dropped_file_not_in_pool += 1;
            continue;
        };
        let mut attributed = false;
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
            attributed = true;
            let terms = matched.entry(chunk.chunk_id.clone()).or_default();
            for term in &rg_hit.matched {
                if !terms.contains(term) {
                    terms.push(term.clone());
                }
            }
        }
        if !attributed {
            stats.dropped_line_out_of_range += 1;
        }
    }
    let counts = matched
        .into_iter()
        .map(|(chunk_id, terms)| (chunk_id, terms.len()))
        .collect();
    (counts, stats)
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
fn contains_term_counts(
    hits: &HashMap<String, SearchHit>,
    terms: &[String],
) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for (chunk_id, hit) in hits {
        let n = terms
            .iter()
            .filter(|t| hit.search_text.contains(t.as_str()))
            .count();
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
    use crate::indexer::SignalLists;
    use async_trait::async_trait;
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
        let (counts, _stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 15, &["term"])]);
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
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 42, &["term"])]);
        assert!(counts.is_empty(), "hit outside every range must be dropped");
        assert_eq!(
            stats.dropped_line_out_of_range, 1,
            "a gap-between-chunks miss must bump dropped_line_out_of_range, not dropped_file_not_in_pool"
        );
        assert_eq!(stats.dropped_file_not_in_pool, 0);
    }

    #[test]
    fn rg_hit_in_a_file_outside_the_pool_is_dropped() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 1, 10)]);
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/other.md", 5, &["term"])]);
        assert!(counts.is_empty());
        assert_eq!(
            stats.dropped_file_not_in_pool, 1,
            "a file the pool never retrieved must bump dropped_file_not_in_pool, not dropped_line_out_of_range"
        );
        assert_eq!(stats.dropped_line_out_of_range, 0);
    }

    /// Term count sizes both the rg argument list and the rg budget, so an
    /// arbitrarily long query must not steer either.
    #[test]
    fn cap_terms_truncates_an_over_long_query_keeping_the_first_terms() {
        let terms: Vec<String> = (0..MAX_QUERY_TERMS + 20).map(|i| format!("t{i}")).collect();
        let capped = cap_terms(terms);
        assert_eq!(capped.len(), MAX_QUERY_TERMS);
        assert_eq!(capped.first().map(String::as_str), Some("t0"));
        assert_eq!(
            capped.last().map(String::as_str),
            Some(format!("t{}", MAX_QUERY_TERMS - 1).as_str()),
            "truncation must keep the head of the query, not an arbitrary slice"
        );
    }

    #[test]
    fn cap_terms_leaves_an_ordinary_query_untouched() {
        let terms = vec!["ranking".to_string(), "fusion".to_string()];
        assert_eq!(cap_terms(terms.clone()), terms);
    }

    #[test]
    fn distinct_terms_are_counted_once_per_chunk_across_several_lines() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 1, 20)]);
        let (counts, _stats) = resolve_rg_hits_to_chunks(
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

    // --- Finding 2: boundary cases against chunk [10,20]. Line 15 vs
    // [11,20]/[1,10] and line 42 vs [1,10]/[60,70] (the pre-existing cases
    // above) never touch the containment comparison's edges, so mutating
    // `line < start || line > end` to `<=`/`>=` left them green. These four
    // pin the boundary itself.

    #[test]
    fn rg_hit_on_the_chunk_start_line_boundary_matches() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 10, 20)]);
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 10, &["term"])]);
        assert_eq!(counts.get("a"), Some(&1), "line == start must match");
        assert_eq!(stats.dropped_line_out_of_range, 0);
    }

    #[test]
    fn rg_hit_on_the_chunk_end_line_boundary_matches() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 10, 20)]);
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 20, &["term"])]);
        assert_eq!(counts.get("a"), Some(&1), "line == end must match");
        assert_eq!(stats.dropped_line_out_of_range, 0);
    }

    #[test]
    fn rg_hit_one_line_before_the_chunk_start_is_dropped() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 10, 20)]);
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 9, &["term"])]);
        assert!(counts.is_empty(), "line == start - 1 must not match");
        assert_eq!(stats.dropped_line_out_of_range, 1);
    }

    #[test]
    fn rg_hit_one_line_after_the_chunk_end_is_dropped() {
        let p = pool(vec![hit("a", "/repo/wiki/x.md", 10, 20)]);
        let (counts, stats) =
            resolve_rg_hits_to_chunks(&p, &[rg_hit("/repo/wiki/x.md", 21, &["term"])]);
        assert!(counts.is_empty(), "line == end + 1 must not match");
        assert_eq!(stats.dropped_line_out_of_range, 1);
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
        let hits = pool(vec![hit_with_search_text(
            "a",
            "alpha alpha alpha alpha alpha",
        )]);
        let terms = vec!["alpha".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert_eq!(
            counts.get("a"),
            Some(&1),
            "five occurrences of one term is one distinct match"
        );
    }

    #[test]
    fn contains_term_counts_counts_distinct_terms_matched_not_total_query_terms() {
        let hits = pool(vec![hit_with_search_text("a", "alpha beta")]);
        let terms = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let counts = contains_term_counts(&hits, &terms);
        assert_eq!(
            counts.get("a"),
            Some(&2),
            "two of three query terms matched"
        );
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

    // --- Finding 1: fusion assembly. Pins the weight-to-list pairing in
    // `search_fused`'s `fuse(...)` call. The domain-level test
    // double in ground/orchestrate.rs always returns an empty `vector` and
    // an empty term-count map (three of four lists provably empty), so no
    // existing test can tell four-signal fusion from one-signal fusion.
    // This fake returns disjoint, non-empty `fts`/`vector` lists and seeds
    // `search_text` on one hit so the FM contains() pass (computed
    // in-domain from `signals.hits`) produces a non-empty list too.
    //
    // Limitation: the ripgrep list is not driven non-empty here (no real rg
    // subprocess control from this fake) — the root is an empty tempdir, so
    // `rg_list` stays empty regardless of RIPGREP_WEIGHT's pairing. That
    // means a RIPGREP_WEIGHT<->CONTAINS_WEIGHT swap specifically (both are
    // 0.5) is invisible to this test; every other weight-to-list swap
    // (FTS<->VECTOR, FTS<->RIPGREP, FTS<->CONTAINS, VECTOR<->RIPGREP,
    // VECTOR<->CONTAINS) changes the asserted order below.
    struct FakeFusionStore {
        fts: Vec<String>,
        vector: Vec<String>,
        hits: HashMap<String, SearchHit>,
    }

    #[async_trait]
    impl ChunkRetrieval for FakeFusionStore {
        async fn retrieve_signals(
            &self,
            _corpus_key: &CorpusKey,
            _query: &str,
            _limit: usize,
        ) -> Result<SignalLists> {
            Ok(SignalLists {
                fts: self.fts.clone(),
                vector: self.vector.clone(),
                hits: self.hits.clone(),
            })
        }
    }

    #[tokio::test]
    async fn search_fused_weights_four_signals_against_their_own_lists() {
        let root = tempfile::tempdir().expect("empty ripgrep root");
        let corpus_key = CorpusKey {
            name: "fixtures".into(),
            canonical_root: root.path().to_path_buf(),
        };

        let a = hit_with_search_text("a", "unrelated");
        let b = hit_with_search_text("b", "unrelated");
        let c = hit_with_search_text("c", "unrelated");
        // Only "d" contains the query term, so it is the sole contributor
        // to the FM contains() signal.
        let d = hit_with_search_text("d", "distinctiveterm present");

        let store = FakeFusionStore {
            fts: vec!["a".into(), "b".into()],
            vector: vec!["c".into(), "d".into()],
            hits: pool(vec![a, b, c, d]),
        };

        let ranked = search_fused(&store, &corpus_key, "distinctiveterm", 10)
            .await
            .expect("fusion over a fake store must succeed");
        let order: Vec<&str> = ranked.hits.iter().map(|h| h.chunk_id.as_str()).collect();

        // a: FTS rank 0 only            -> 2.0/60            = 0.033333
        // b: FTS rank 1 only             -> 2.0/61            = 0.032787
        // d: vector rank 1 + contains rank 0 -> 1.0/61 + 0.5/60 = 0.024726
        // c: vector rank 0 only          -> 1.0/60            = 0.016667
        assert_eq!(
            order,
            vec!["a", "b", "d", "c"],
            "fused order must reflect FTS_WEIGHT/VECTOR_WEIGHT/CONTAINS_WEIGHT applied to their own lists, not swapped"
        );
    }

    #[tokio::test]
    async fn a_failed_ripgrep_pass_warns_the_caller_instead_of_ranking_silently() {
        // A Ground response that claims four-signal fusion while actually
        // ranked on fewer is the failure this warning exists to prevent: the
        // degradation is already logged, but logs do not reach the caller.
        let corpus_key = CorpusKey {
            name: "fixtures".into(),
            canonical_root: std::path::PathBuf::from(
                "/nonexistent/hallouminate-rg-failure-fixture",
            ),
        };
        let store = FakeFusionStore {
            fts: vec!["a".into()],
            vector: Vec::new(),
            hits: pool(vec![hit_with_search_text("a", "unrelated")]),
        };

        let result = search_fused(&store, &corpus_key, "distinctiveterm", 10)
            .await
            .expect("a failed ripgrep pass must degrade the ranking, not fail the query");

        assert_eq!(
            result
                .warnings
                .iter()
                .map(|w| w.code.as_str())
                .collect::<Vec<_>>(),
            vec!["ripgrep-failed"],
            "the caller must be told which signal dropped out"
        );
        assert_eq!(
            result.hits.len(),
            1,
            "ranking continues on the signals that did survive"
        );
    }
}
