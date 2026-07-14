use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::common::{HallouminateError, Result};
use crate::indexer::{ChunkStore, SearchHit};
use crate::search::{Crossencoder, search_with_ripgrep};

use super::bucket::{build_docs, normalize_scores};
use super::types::{DocFile, GroundResponse, Stats};

/// Run `crossencoder.rerank(query, &mut hits)` on a blocking-pool thread and
/// bound it with `timeout`. `Crossencoder::rerank` is synchronous CPU-bound
/// work with no `.await`, so wrapping `tokio::time::timeout` directly around
/// it cannot preempt a stalled call (#139) — only a real OS-thread boundary
/// can. `spawn_blocking` gives us that boundary; on timeout the spawned
/// thread is abandoned (left to finish or die on its own, never joined) and
/// `hits` (cloned up front) is returned unchanged so the caller falls back
/// to fusion order. Returns `(hits, applied)`; `applied` is `false` on
/// timeout so callers gate z-score normalization on it, preserving the
/// "z-score only when the cross-encoder ran" invariant on the fallback path.
/// The abandoned thread still owns the boxed crossencoder (on the daemon
/// path, a `CrossencoderGuard` holding the shared crossencoder mutex), so
/// that mutex stays locked until the stalled call drains — concurrent rerank
/// requests serialize behind it, exactly as they did before the timeout
/// existed (#139 accepted tradeoff).
async fn rerank_with_timeout(
    mut crossencoder: Box<dyn Crossencoder>,
    query: String,
    hits: Vec<SearchHit>,
    timeout: Duration,
) -> Result<(Vec<SearchHit>, bool)> {
    let fallback = hits.clone();
    let query_len = query.len();
    let handle = tokio::task::spawn_blocking(move || {
        let mut hits = hits;
        crossencoder.rerank(&query, &mut hits)?;
        Ok::<Vec<SearchHit>, HallouminateError>(hits)
    });
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(Ok(reranked))) => Ok((reranked, true)),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(join_err)) => {
            let cause = if join_err.is_panic() {
                "panicked"
            } else {
                "was cancelled"
            };
            tracing::error!(error = %join_err, cause, "crossencoder task failed");
            Err(HallouminateError::Embed(format!(
                "crossencoder task {cause}: {join_err}"
            )))
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis() as u64,
                query_len,
                "crossencoder rerank timed out; falling back to fusion order"
            );
            Ok((fallback, false))
        }
    }
}
/// Strip the first matching corpus root prefix from `abs_path`, returning
/// a corpus-relative path string accepted by `safe_relative_path`.
/// Returns `None` when no root is a prefix (e.g. symlinked or global corpora).
fn relative_path_for(abs_path: &str, corpus_roots: &[String]) -> Option<String> {
    for root in corpus_roots {
        let root = root.trim_end_matches('/');
        if let Some(rel) = abs_path.strip_prefix(root) {
            // Only accept if the remainder starts with '/' — i.e. the prefix
            // ended at a real path-component boundary.  Without this check,
            // root "/corpus/root" would match "/corpus/rootext/f.md" and
            // return the nonsense path "ext/f.md".
            if !rel.starts_with('/') {
                continue;
            }
            let rel = rel.trim_start_matches('/');
            if !rel.is_empty() {
                return Some(rel.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundOpts {
    pub top_files: usize,
    pub chunks_per_file: usize,
    pub limit: usize,
    /// Bound on the crossencoder rerank step (#139), configurable via
    /// `[search].rerank_timeout_ms`. See `rerank_with_timeout` for why a
    /// real OS-thread boundary (not a bare `tokio::time::timeout`) is
    /// required to preempt the synchronous crossencoder.
    pub rerank_timeout: Duration,
}

impl Default for GroundOpts {
    fn default() -> Self {
        Self {
            top_files: 10,
            chunks_per_file: 3,
            limit: 50,
            rerank_timeout: Duration::from_secs(2),
        }
    }
}

pub async fn ground(
    query: &str,
    corpus: &str,
    corpus_paths: &[String],
    store: &dyn ChunkStore,
    crossencoder: Option<Box<dyn Crossencoder>>,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();
    let mut hits = search_corpus(query, corpus, corpus_paths, store, opts.limit).await?;
    if let Some(rerank) = crossencoder {
        // The crossencoder is the most expensive step; skip it on empty
        // hit lists so a no-match query doesn't pay the model latency.
        if !hits.is_empty() {
            let (reranked, applied) =
                rerank_with_timeout(rerank, query.to_string(), hits, opts.rerank_timeout).await?;
            hits = reranked;
            if applied {
                // RRF-mode guard (decision 4): z only when the cross-encoder ran.
                // Full-pool scope (decision 2): computed before build_docs truncates.
                let zs = normalize_scores(&hits);
                for (hit, z) in hits.iter_mut().zip(zs) {
                    hit.z_score = z;
                }
            }
        }
    }
    let stats = Stats { hits: hits.len() };
    let mut docs = build_docs(&hits, opts.top_files, opts.chunks_per_file)?;
    for (abs_path, doc) in docs.iter_mut() {
        doc.corpus = corpus.to_string();
        doc.path = relative_path_for(abs_path, corpus_paths);
        for chunk in &mut doc.chunks {
            chunk.provenance.corpus = corpus.to_string();
        }
    }
    Ok(GroundResponse {
        query: query.to_string(),
        took_ms: started.elapsed().as_millis() as u64,
        stats,
        docs,
        code: BTreeMap::new(),
        warnings: vec![],
    })
}

/// Search one corpus, returning its un-reranked hits.
///
/// The crossencoder rerank is the caller's concern — `ground` reranks per
/// corpus, `ground_union` hoists the single rerank past the cross-corpus
/// merge (#106).
async fn search_corpus(
    query: &str,
    corpus: &str,
    corpus_paths: &[String],
    store: &dyn ChunkStore,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    search_with_ripgrep(store, corpus, corpus_paths, query, limit).await
}

/// Fan one query across every effective corpus and merge into a single,
/// globally-ranked `GroundResponse` (#106).
///
/// Each corpus is searched independently (un-reranked), the hits are tagged
/// with their source corpus and merged, then a **single** crossencoder pass
/// runs over the merged set so the final ranking is globally coherent rather
/// than per-corpus-then-concatenated. Docs are built per corpus (so each
/// carries its `corpus` attribution and per-chunk provenance), unioned by
/// path-unique key, and truncated to the global `top_files`. `stats.hits`
/// sums the raw hit counts across corpora.
///
/// The shared single embedder means per-corpus scores are on the same scale,
/// so the merged crossencoder pass produces a coherent ordering without
/// per-corpus normalization (YAGNI until heterogeneous models exist).
pub async fn ground_union(
    query: &str,
    corpora: &[(String, Vec<String>)],
    store: &dyn ChunkStore,
    crossencoder: Option<Box<dyn Crossencoder>>,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();

    // Search each corpus independently, tagging every hit with its source so
    // the per-corpus partition survives the shared rerank's reshuffle.
    let mut tagged: Vec<(SearchHit, String)> = Vec::new();
    for (name, paths) in corpora {
        let hits = search_corpus(query, name, paths, store, opts.limit).await?;
        for hit in hits {
            tagged.push((hit, name.clone()));
        }
    }

    let stats = Stats { hits: tagged.len() };

    // One crossencoder pass over the MERGED set. The trait contract guarantees
    // rerank only reshuffles (no inserts/deletes), so the corpus tags stay
    // valid; rebuild the corpus lookup from the reordered hit list afterward.
    // Use a FIFO queue per chunk_id so cross-corpus chunk_id collisions survive
    // — two corpora indexing the same file path produce the same chunk_id, but
    // both attributions must be preserved (same content, different corpus).
    // Build the queue from borrowed chunk_id/corpus-name first, then MOVE (not
    // clone) each hit into `hits` — cloning duplicates the full text and
    // summary just to key a lookup that only needs two strings.
    let mut corpus_queues: std::collections::HashMap<String, std::collections::VecDeque<String>> =
        std::collections::HashMap::new();
    for (h, name) in &tagged {
        corpus_queues
            .entry(h.chunk_id.clone())
            .or_default()
            .push_back(name.clone());
    }
    let mut hits: Vec<SearchHit> = tagged.into_iter().map(|(h, _)| h).collect();
    if let Some(rerank) = crossencoder
        && !hits.is_empty()
    {
        let (reranked, applied) =
            rerank_with_timeout(rerank, query.to_string(), hits, opts.rerank_timeout).await?;
        hits = reranked;
        if applied {
            let zs = normalize_scores(&hits);
            for (hit, z) in hits.iter_mut().zip(zs) {
                hit.z_score = z;
            }
        }
    }

    // Build docs per corpus partition so each doc + chunk carries its source
    // corpus. Build with an unbounded per-corpus `top_files` and apply the
    // global truncation after the union, so `top_files` bounds the merged set.
    let mut by_corpus: std::collections::HashMap<String, Vec<SearchHit>> =
        std::collections::HashMap::new();
    for hit in hits {
        let corpus = corpus_queues
            .get_mut(&hit.chunk_id)
            .and_then(|q| q.pop_front())
            .unwrap_or_default();
        by_corpus.entry(corpus).or_default().push(hit);
    }

    // Build a name→roots lookup for relative-path stamping below.
    let corpus_roots_by_name: std::collections::HashMap<&str, &[String]> = corpora
        .iter()
        .map(|(n, paths)| (n.as_str(), paths.as_slice()))
        .collect();

    let mut docs: BTreeMap<String, DocFile> = BTreeMap::new();
    for (corpus, corpus_hits) in by_corpus {
        let corpus_roots = corpus_roots_by_name
            .get(corpus.as_str())
            .copied()
            .unwrap_or(&[]);
        let mut built = build_docs(&corpus_hits, usize::MAX, opts.chunks_per_file)?;
        for (abs_path, doc) in built.iter_mut() {
            doc.corpus = corpus.clone();
            doc.path = relative_path_for(abs_path, corpus_roots);
            for chunk in &mut doc.chunks {
                chunk.provenance.corpus = corpus.clone();
            }
        }
        // When two corpora index the same absolute path the file_ref keys
        // collide. Disambiguate by appending the corpus to the key so both
        // docs survive the union; the doc's `corpus` field still names the
        // true source. The common case (unique paths) is unchanged.
        for (path, doc) in built {
            let key = if docs.contains_key(&path) {
                format!("{path} [{corpus}]")
            } else {
                path
            };
            docs.insert(key, doc);
        }
    }

    // Global top-N truncation over the merged docs, by doc score descending
    // (path tiebreak for determinism).
    if docs.len() > opts.top_files {
        let mut ranked: Vec<(String, DocFile)> = docs.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.score
                .partial_cmp(&a.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.truncate(opts.top_files);
        docs = ranked.into_iter().collect();
    }

    Ok(GroundResponse {
        query: query.to_string(),
        took_ms: started.elapsed().as_millis() as u64,
        stats,
        docs,
        code: BTreeMap::new(),
        warnings: vec![],
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::indexer::{BatchWriteStats, FileSnapshot, PreparedFile};

    /// In-memory `ChunkStore` test double for orchestration/rerank tests:
    /// `hybrid_search` returns a canned, pre-seeded hit list; writes are
    /// no-ops. Keeps these tests off the real Lance/embedder adapter stack
    /// (US-002: embedding is adapter-owned, invisible to domain code).
    #[derive(Default)]
    struct FakeChunkStore {
        hits: Vec<SearchHit>,
    }

    #[async_trait]
    impl ChunkStore for FakeChunkStore {
        async fn list_files(&self, _corpus: &str) -> Result<Vec<FileSnapshot>> {
            Ok(Vec::new())
        }

        async fn hybrid_search(
            &self,
            _corpus: &str,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<SearchHit>> {
            Ok(self.hits.iter().take(limit).cloned().collect())
        }

        async fn touch_mtime(&self, _corpus: &str, _file_ref: &str, _mtime_ms: i64) -> Result<()> {
            Ok(())
        }

        async fn delete_file(&self, _corpus: &str, _file_ref: &str) -> Result<()> {
            Ok(())
        }

        async fn apply_batch(&self, _files: Vec<PreparedFile>) -> Result<BatchWriteStats> {
            Ok(BatchWriteStats::default())
        }
    }

    /// OFF mode: with no crossencoder configured, `ground` must take the
    /// lexical-only path and return a well-formed (empty, for an empty
    /// store) response. Decision-4 note: `crossencoder` is `None` here, so
    /// the `if let Some(rerank)` stamp loop in `ground` never fires — this
    /// is the orchestrator-level RRF/OFF path. Docs are empty on an empty
    /// store, so the z_score assertion is vacuous; the structural
    /// enforcement (stamp lexically inside the block) is verified by
    /// `rrf_mode_docs_have_no_z_score` in bucket.rs.
    #[tokio::test]
    async fn ground_off_mode_returns_lexical_response_without_a_crossencoder() {
        let store = FakeChunkStore::default();
        let resp = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            None,
            GroundOpts::default(),
        )
        .await
        .expect("OFF-mode ground must succeed on an empty store");
        assert_eq!(resp.query, "spice");
        assert_eq!(resp.stats.hits, 0, "empty store yields no hits");
        assert!(resp.docs.is_empty());
    }

    // --- #137: relative_path_for ---

    #[test]
    fn relative_path_for_strips_matching_root() {
        let roots = vec!["/corpus/root".to_string()];
        let rel = relative_path_for("/corpus/root/wiki/index.md", &roots);
        assert_eq!(rel.as_deref(), Some("wiki/index.md"));
    }

    #[test]
    fn relative_path_for_accepts_result_in_safe_relative_path() {
        // Regression for #137: the emitted relative path must be accepted by
        // `safe_relative_path`, which is the gate on read_markdown/add_markdown.
        // If this fails the field is useless as a handoff from ground.
        use crate::corpus::safe_relative_path;
        let roots = vec!["/var/hallouminate/wiki".to_string()];
        let rel = relative_path_for("/var/hallouminate/wiki/concepts/design.md", &roots)
            .expect("must produce a relative path");
        safe_relative_path(&rel).expect("relative path must be accepted by safe_relative_path");
    }

    #[test]
    fn relative_path_for_multi_root_uses_first_matching() {
        // #137: multi-root corpora — try each root, use first match.
        let roots = vec!["/other/root".to_string(), "/corpus/root".to_string()];
        let rel = relative_path_for("/corpus/root/sub/file.md", &roots);
        assert_eq!(rel.as_deref(), Some("sub/file.md"));
    }

    #[test]
    fn relative_path_for_returns_none_when_no_root_matches() {
        let roots = vec!["/other/root".to_string()];
        let rel = relative_path_for("/corpus/root/file.md", &roots);
        assert!(rel.is_none(), "no matching root must yield None");
    }

    #[test]
    fn relative_path_for_returns_none_for_empty_roots() {
        let rel = relative_path_for("/any/path/file.md", &[]);
        assert!(rel.is_none());
    }

    #[test]
    fn relative_path_for_returns_none_for_sibling_prefix() {
        // Regression for review finding: "/corpus/root" must NOT match
        // "/corpus/rootext/f.md" just because it's a string prefix.
        let roots = vec!["/corpus/root".to_string()];
        let rel = relative_path_for("/corpus/rootext/f.md", &roots);
        assert!(
            rel.is_none(),
            "/corpus/root must not match /corpus/rootext/f.md: got {rel:?}"
        );
    }

    // --- #139: rerank_with_timeout ---

    fn hit_for_timeout_test(file_ref: &str, score: f32) -> SearchHit {
        SearchHit {
            chunk_id: format!("{file_ref}#0"),
            file_ref: file_ref.into(),
            heading_path: vec![],
            line_start: 1,
            line_end: 2,
            text: String::new(),
            summary: String::new(),
            keywords: vec![],
            score,
            mtime_ms: 0,
            claim_marks: vec![],
            z_score: None,
        }
    }

    #[tokio::test]
    async fn rerank_with_timeout_returns_fusion_order_when_crossencoder_stalls() {
        struct SleepingCrossencoder;
        impl Crossencoder for SleepingCrossencoder {
            fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(200));
                hits.reverse();
                Ok(())
            }
        }

        let hits = vec![
            hit_for_timeout_test("/a.md", 0.1),
            hit_for_timeout_test("/b.md", 0.9),
        ];
        let fusion_order: Vec<String> = hits.iter().map(|h| h.chunk_id.clone()).collect();

        let (result, applied) = rerank_with_timeout(
            Box::new(SleepingCrossencoder),
            "q".to_string(),
            hits,
            Duration::from_millis(20),
        )
        .await
        .expect("timeout path must not error");

        assert!(
            !applied,
            "a stalled crossencoder must report applied == false"
        );
        let observed: Vec<String> = result.iter().map(|h| h.chunk_id.clone()).collect();
        assert_eq!(
            observed, fusion_order,
            "timeout fallback must preserve the original fusion order"
        );
    }

    #[tokio::test]
    async fn rerank_with_timeout_applies_the_rerank_on_the_fast_path() {
        struct ReversingCrossencoderStub;
        impl Crossencoder for ReversingCrossencoderStub {
            fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
                hits.reverse();
                Ok(())
            }
        }

        let hits = vec![
            hit_for_timeout_test("/a.md", 0.1),
            hit_for_timeout_test("/b.md", 0.9),
        ];

        let (result, applied) = rerank_with_timeout(
            Box::new(ReversingCrossencoderStub),
            "q".to_string(),
            hits,
            Duration::from_secs(2),
        )
        .await
        .expect("fast path must not error");

        assert!(applied, "a fast crossencoder must report applied == true");
        let observed: Vec<&str> = result.iter().map(|h| h.file_ref.as_str()).collect();
        assert_eq!(
            observed,
            vec!["/b.md", "/a.md"],
            "fast path must apply the crossencoder's reordering"
        );
    }

    // --- #139: GroundOpts.rerank_timeout wiring ---

    /// Five hits with distinct file_refs so `normalize_scores` (MIN_N = 5)
    /// can emit Some z-scores once the crossencoder assigns spread scores.
    /// Fewer hits would make the timeout tests' `z_score.is_none()`
    /// assertions tautological: below MIN_N, normalize_scores returns
    /// all-None unconditionally.
    fn fixture_hits() -> Vec<SearchHit> {
        (0..5)
            .map(|i| hit_for_timeout_test(&format!("/spice{i}.md"), i as f32))
            .collect()
    }

    /// Sleeps past the tiny timeouts used below, then assigns distinct
    /// scores. The distinct scores guarantee `normalize_scores` has spread
    /// (sigma > 0), so if this rerank is ever allowed to finish — i.e. the
    /// configured timeout was ignored — z_scores WILL be Some and the
    /// `is_none()` assertions fail loudly.
    struct SleepingCrossencoder;
    impl Crossencoder for SleepingCrossencoder {
        fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            for (i, hit) in hits.iter_mut().enumerate() {
                hit.score = i as f32;
            }
            Ok(())
        }
    }

    /// Assigns distinct scores immediately (no sleep). Positive control:
    /// proves the fixture + score assignment CAN produce Some z-scores when
    /// the rerank finishes inside the timeout, so the timeout tests'
    /// `is_none()` assertions pass for the right reason.
    struct ScoringCrossencoder;
    impl Crossencoder for ScoringCrossencoder {
        fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
            for (i, hit) in hits.iter_mut().enumerate() {
                hit.score = i as f32;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn ground_union_applies_z_scores_when_rerank_finishes_in_time() {
        // Positive control for the two timeout tests below: with a generous
        // timeout and a fast crossencoder, z_scores must appear. Guards the
        // fixture against silently shrinking below MIN_N (which would make
        // the is_none() assertions pass unconditionally).
        let store = FakeChunkStore {
            hits: fixture_hits(),
        };

        let opts = GroundOpts {
            rerank_timeout: Duration::from_secs(5),
            ..GroundOpts::default()
        };
        let resp = ground_union(
            "spice",
            &[("fixtures".to_string(), vec![])],
            &store,
            Some(Box::new(ScoringCrossencoder)),
            opts,
        )
        .await
        .expect("fast rerank inside a generous timeout must not error");

        assert!(
            resp.stats.hits >= 5,
            "fixture corpus must yield >= MIN_N (5) hits so normalize_scores can emit \
             Some — got {}; below that the timeout tests are tautological",
            resp.stats.hits
        );
        assert!(
            resp.docs.values().any(|d| d.z_score.is_some()),
            "a completed rerank over >=5 spread-score hits must produce Some z_score; \
             all-None means the assertion channel the timeout tests rely on is dead"
        );
    }

    #[tokio::test]
    async fn ground_union_honors_opts_rerank_timeout() {
        // Proves the knob is actually wired through GroundOpts into
        // ground_union, not just present on the struct: a tiny
        // opts.rerank_timeout must trigger the timeout fallback path on a
        // POPULATED store, so the crossencoder branch actually runs. If
        // ground_union ignored opts.rerank_timeout (e.g. a hardcoded 2s),
        // the 200ms sleep would finish well inside 2s, `applied` would be
        // true, and the spread scores assigned by SleepingCrossencoder
        // would yield Some z_scores over >=5 hits, failing the assertion
        // below (positive control:
        // ground_union_applies_z_scores_when_rerank_finishes_in_time).
        let store = FakeChunkStore {
            hits: fixture_hits(),
        };

        let opts = GroundOpts {
            rerank_timeout: Duration::from_millis(20),
            ..GroundOpts::default()
        };
        let resp = ground_union(
            "spice",
            &[("fixtures".to_string(), vec![])],
            &store,
            Some(Box::new(SleepingCrossencoder)),
            opts,
        )
        .await
        .expect("tiny rerank_timeout must not error, only fall back to fusion order");

        assert!(
            resp.stats.hits >= 5,
            "fixture corpus must yield >= MIN_N (5) hits so the z_score assertion is \
             falsifiable, got {}",
            resp.stats.hits
        );
        assert!(
            resp.docs.values().all(|d| d.z_score.is_none()),
            "a 20ms opts.rerank_timeout must time out the 200ms-sleeping crossencoder, \
             leaving z_score unset (applied == false); a Some z_score means the \
             configured timeout was ignored"
        );
    }

    #[tokio::test]
    async fn ground_honors_opts_rerank_timeout() {
        // Mirrors ground_union_honors_opts_rerank_timeout for the single-
        // corpus `ground()` entry point (#139): a regression that rewires
        // only ground_union to read opts.rerank_timeout while ground() keeps
        // (or reverts to) a hardcoded duration would pass every other test
        // in this file but must fail here.
        let store = FakeChunkStore {
            hits: fixture_hits(),
        };

        let opts = GroundOpts {
            rerank_timeout: Duration::from_millis(20),
            ..GroundOpts::default()
        };
        let resp = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            Some(Box::new(SleepingCrossencoder)),
            opts,
        )
        .await
        .expect("tiny rerank_timeout must not error, only fall back to fusion order");

        assert!(
            resp.stats.hits >= 5,
            "fixture corpus must yield >= MIN_N (5) hits so the z_score assertion is \
             falsifiable, got {}",
            resp.stats.hits
        );
        assert!(
            resp.docs.values().all(|d| d.z_score.is_none()),
            "a 20ms opts.rerank_timeout must time out the 200ms-sleeping crossencoder in ground(), \
             leaving z_score unset (applied == false); a Some z_score means the \
             configured timeout was ignored"
        );
    }
    #[tokio::test]
    async fn rerank_with_timeout_zero_duration_falls_back_without_panic() {
        // Boundary (#139): rerank_timeout_ms = 0 must degrade gracefully to
        // fusion order rather than panicking or erroring — a 0ms deadline is
        // already expired the instant the task is spawned.
        struct SleepingCrossencoder;
        impl Crossencoder for SleepingCrossencoder {
            fn rerank(&mut self, _query: &str, hits: &mut [SearchHit]) -> Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(200));
                hits.reverse();
                Ok(())
            }
        }

        let hits = vec![
            hit_for_timeout_test("/a.md", 0.1),
            hit_for_timeout_test("/b.md", 0.9),
        ];
        let fusion_order: Vec<String> = hits.iter().map(|h| h.chunk_id.clone()).collect();

        let (result, applied) = rerank_with_timeout(
            Box::new(SleepingCrossencoder),
            "q".to_string(),
            hits,
            Duration::from_millis(0),
        )
        .await
        .expect("zero timeout must not error, only fall back to fusion order");

        assert!(
            !applied,
            "a zero-duration timeout must report applied == false"
        );
        let observed: Vec<String> = result.iter().map(|h| h.chunk_id.clone()).collect();
        assert_eq!(
            observed, fusion_order,
            "zero-duration timeout fallback must preserve the original fusion order"
        );
    }
}
