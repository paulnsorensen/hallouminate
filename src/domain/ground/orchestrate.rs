use std::collections::BTreeMap;
use std::time::Instant;

use crate::adapters::lance::{LanceStore, SearchHit};
use crate::domain::common::{HallouminateError, Result};
use crate::domain::embeddings::{EmbedBatch, EmbedRole};
use crate::domain::search::{Crossencoder, fts_with_ripgrep, hybrid_with_ripgrep};

use super::bucket::build_docs;
use super::types::{DocFile, GroundResponse, Stats};

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
}

impl Default for GroundOpts {
    fn default() -> Self {
        Self {
            top_files: 10,
            chunks_per_file: 3,
            limit: 50,
        }
    }
}

pub async fn ground(
    query: &str,
    corpus: &str,
    corpus_paths: &[String],
    store: &LanceStore,
    embedder: Option<&mut dyn EmbedBatch>,
    crossencoder: Option<&mut dyn Crossencoder>,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();
    let mut hits = search_corpus(query, corpus, corpus_paths, store, embedder, opts.limit).await?;
    if let Some(rerank) = crossencoder {
        // The crossencoder is the most expensive step; skip it on empty
        // hit lists so a no-match query doesn't pay the model latency.
        if !hits.is_empty() {
            rerank.rerank(query, &mut hits)?;
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
/// ON mode (embedder present): embed the query and fuse FTS + vector + rg.
/// OFF mode (None): lexical-only FTS + rg. The crossencoder rerank is the
/// caller's concern — `ground` reranks per corpus, `ground_union` hoists the
/// single rerank past the cross-corpus merge (#106).
async fn search_corpus(
    query: &str,
    corpus: &str,
    corpus_paths: &[String],
    store: &LanceStore,
    embedder: Option<&mut dyn EmbedBatch>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    match embedder {
        Some(embedder) => {
            let embeddings = embedder.embed_batch(&[query.to_string()], EmbedRole::Query)?;
            let query_vec = embeddings.into_iter().next().ok_or_else(|| {
                HallouminateError::Embed("embed_batch returned no vector for query".into())
            })?;
            hybrid_with_ripgrep(store, corpus, corpus_paths, query, &query_vec, limit).await
        }
        None => fts_with_ripgrep(store, corpus, corpus_paths, query, limit).await,
    }
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
    store: &LanceStore,
    embedder: Option<&mut dyn EmbedBatch>,
    crossencoder: Option<&mut dyn Crossencoder>,
    opts: GroundOpts,
) -> Result<GroundResponse> {
    let started = Instant::now();

    // Search each corpus independently, tagging every hit with its source so
    // the per-corpus partition survives the shared rerank's reshuffle. The
    // `&mut dyn` is reborrowed per iteration so the shared embedder is used
    // across all corpora without the borrow outliving the loop.
    let mut embedder = embedder;
    let mut tagged: Vec<(SearchHit, String)> = Vec::new();
    for (name, paths) in corpora {
        let reborrow: Option<&mut dyn EmbedBatch> = match &mut embedder {
            Some(e) => Some(&mut **e),
            None => None,
        };
        let hits = search_corpus(query, name, paths, store, reborrow, opts.limit).await?;
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
    let mut hits: Vec<SearchHit> = tagged.iter().map(|(h, _)| h.clone()).collect();
    let mut corpus_queues: std::collections::HashMap<String, std::collections::VecDeque<String>> =
        std::collections::HashMap::new();
    for (h, name) in tagged.iter() {
        corpus_queues
            .entry(h.chunk_id.clone())
            .or_default()
            .push_back(name.clone());
    }
    if let Some(rerank) = crossencoder
        && !hits.is_empty()
    {
        rerank.rerank(query, &mut hits)?;
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
    use super::*;
    use crate::domain::embeddings::EMBEDDING_DIM;

    /// Fake embedder whose `embed_batch` always returns an empty Vec, exercising
    /// the defensive `ok_or_else` branch in `ground` that protects against an
    /// embedder impl violating the "one input → one output" invariant.
    struct EmptyVecEmbedder;

    impl EmbedBatch for EmptyVecEmbedder {
        fn embed_batch(
            &mut self,
            _texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(Vec::new())
        }
    }

    /// Records the role each `embed_batch` call received so tests can assert
    /// the query side of the asymmetric-prefix wiring is `EmbedRole::Query`.
    #[derive(Default)]
    struct RoleRecordingEmbedder {
        roles: Vec<EmbedRole>,
    }

    impl EmbedBatch for RoleRecordingEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.roles.push(role);
            Ok(texts.iter().map(|_| [0.1_f32; EMBEDDING_DIM]).collect())
        }
    }

    async fn open_test_store(dir: &std::path::Path) -> LanceStore {
        crate::adapters::lance::LanceStore::open_or_create(
            dir,
            "BAAI/bge-small-en-v1.5",
            false,
            true,
        )
        .await
        .expect("open store")
    }

    #[tokio::test]
    async fn ground_errors_when_embedder_returns_no_vector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let mut embedder = EmptyVecEmbedder;
        let err = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            Some(&mut embedder),
            None,
            GroundOpts::default(),
        )
        .await
        .expect_err("empty embed vec must error");
        match err {
            HallouminateError::Embed(msg) => {
                assert!(
                    msg.contains("no vector"),
                    "embed error must mention missing vector: {msg}"
                );
            }
            other => panic!("expected Embed error, got: {other:?}"),
        }
    }

    /// OFF mode: with no embedder, `ground` must take the lexical-only path
    /// and return a well-formed (empty, for an empty store) response instead
    /// of erroring on a missing query vector.
    #[tokio::test]
    async fn ground_off_mode_returns_lexical_response_without_an_embedder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let resp = ground(
            "spice",
            "fixtures",
            &[],
            &store,
            None,
            None,
            GroundOpts::default(),
        )
        .await
        .expect("OFF-mode ground must succeed on an empty store");
        assert_eq!(resp.query, "spice");
        assert_eq!(resp.stats.hits, 0, "empty store yields no hits");
        assert!(resp.docs.is_empty());
    }

    /// ON mode: `ground` must embed the query with `EmbedRole::Query` so the
    /// per-model instruction prefix matches the query side. The embed call
    /// runs before search, so an empty store still exercises it.
    #[tokio::test]
    async fn ground_embeds_query_with_query_role() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_test_store(dir.path()).await;
        let mut embedder = RoleRecordingEmbedder::default();
        ground(
            "spice",
            "fixtures",
            &[],
            &store,
            Some(&mut embedder),
            None,
            GroundOpts::default(),
        )
        .await
        .expect("ON-mode ground");
        assert_eq!(
            embedder.roles,
            vec![EmbedRole::Query],
            "ground must embed the query exactly once, with the Query role"
        );
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
        use crate::domain::corpus::sandbox::safe_relative_path;
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
}
