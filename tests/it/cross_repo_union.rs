//! End-to-end test for cross-repo union ground (#106).
//!
//! Seeds two sibling repos under a parent dir, each with its own
//! `.hallouminate/wiki/` and a distinctive token, indexes each into its own
//! `repo:{name}:wiki` corpus, then issues ONE no-corpus union ground across
//! both. Asserts:
//!   - hits come from BOTH discovered wikis,
//!   - each hit is attributed to its source corpus (file-level `corpus` AND
//!     per-chunk `provenance.corpus`),
//!   - discovery respects the depth cap and a `.gitignore`.
//!
//! Runs against the domain crust (discovery walker + `index_corpus` +
//! `ground_union`) with a deterministic stub embedder, so it needs no daemon,
//! socket, or model download.

use std::fs;
use std::path::Path;

use hallouminate::domain::common::CorpusConfig;
use hallouminate::domain::discovery::{DEFAULT_MAX_DEPTH, IgnoreRules, discover_wiki_roots};
use hallouminate::domain::ground::{GroundOpts, ground_union};
use hallouminate::domain::indexer::{HandlerRegistry, index_corpus};
use hallouminate::domain::repository::{
    RepositoryConfig, repository_for_discovered_wiki, repository_wiki_corpus,
    union_discovered_repositories, wiki_directory,
};
use text_splitter::Characters;

use crate::common::StubEmbedder;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Seed `<parent>/<repo>/.hallouminate/wiki/<repo>.md` with a body carrying a
/// distinctive token, returning the repo root.
fn seed_repo_wiki(parent: &Path, repo: &str, token: &str) -> std::path::PathBuf {
    let repo_root = parent.join(repo);
    let wiki = repo_root.join(".hallouminate").join("wiki");
    fs::create_dir_all(&wiki).expect("mkdir wiki");
    // A local-only config (not a baseline [[repository]]).
    fs::write(repo_root.join(".hallouminate").join("config.toml"), "").expect("write config");
    fs::write(
        wiki.join(format!("{repo}.md")),
        format!("# {repo}\n\nThe distinctive token {token} lives only in {repo}'s wiki.\n"),
    )
    .expect("write wiki page");
    repo_root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_returns_attributed_hits_from_multiple_discovered_wikis() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");

    // Two sibling repos, each with a local-only wiki and a unique token.
    seed_repo_wiki(parent.path(), "alpha", "zphyxnort");
    seed_repo_wiki(parent.path(), "beta", "qwobblefrotz");

    // Curd 1 + 2: discover the sub-repo wikis from above all repos and union
    // them (no baseline repos here, so the union is just the discovered set).
    let discovered: Vec<RepositoryConfig> =
        discover_wiki_roots(parent.path(), DEFAULT_MAX_DEPTH, &IgnoreRules::default())
            .into_iter()
            .filter_map(|w| repository_for_discovered_wiki(&w.repo_root))
            .collect();
    let (repos, warnings) = union_discovered_repositories(&[], discovered);
    assert_eq!(repos.len(), 2, "both sibling repo wikis must be discovered");
    assert!(warnings.is_empty(), "no name collision => no warnings");

    // Derive each repo's `repo:{name}:wiki` corpus and index it into a shared
    // store. (One store, distinct corpus per repo — mirrors the daemon.)
    let store = hallouminate::adapters::lance::LanceStore::open_or_create(
        store_dir.path(),
        MODEL,
        false,
        true,
    )
    .await
    .expect("open store");
    let registry = HandlerRegistry::new(Characters, 1500);
    let mut corpora: Vec<CorpusConfig> = Vec::new();
    for repo in &repos {
        let corpus = repository_wiki_corpus(repo).expect("derive wiki corpus");
        let mut embedder = StubEmbedder;
        index_corpus(&corpus, &store, Some(&mut embedder), &registry)
            .await
            .unwrap_or_else(|e| panic!("index {}: {e}", corpus.name));
        corpora.push(corpus);
    }

    // Curd 3: one no-corpus union ground across all effective corpora.
    let targets: Vec<(String, Vec<String>)> = corpora
        .iter()
        .map(|c| (c.name.clone(), c.paths.clone()))
        .collect();
    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("union ground");

    // Hits land in BOTH wikis.
    let corpora_seen: std::collections::HashSet<&str> =
        resp.docs.values().map(|d| d.corpus.as_str()).collect();
    assert!(
        corpora_seen.contains("repo:alpha:wiki") && corpora_seen.contains("repo:beta:wiki"),
        "union ground must return hits from both discovered wikis, saw: {corpora_seen:?}"
    );

    // Curd 4: every chunk is attributed to its file's corpus, and the per-chunk
    // provenance matches the parent DocFile's corpus.
    for (path, doc) in &resp.docs {
        for chunk in &doc.chunks {
            assert_eq!(
                chunk.provenance.corpus, doc.corpus,
                "chunk provenance must match its file's corpus for {path}"
            );
            assert!(
                !chunk.provenance.corpus.is_empty(),
                "every chunk must carry a non-empty source corpus for {path}"
            );
        }
    }

    // The summed hit count reflects both corpora's raw hits, not just one.
    assert!(
        resp.stats.hits >= 2,
        "stats.hits must sum across corpora, got {}",
        resp.stats.hits
    );
}

#[test]
fn discovery_respects_depth_cap_and_gitignore_above_all_repos() {
    // AC: discovery does not scan into ignored dirs or beyond the depth cap.
    let parent = tempfile::tempdir().expect("tempdir");
    // A gitignored `vendor/` subtree with a buried wiki.
    fs::create_dir_all(parent.path().join(".git")).expect("mkdir .git");
    fs::write(parent.path().join(".gitignore"), "vendor/\n").expect("write gitignore");
    let shallow = seed_repo_wiki(parent.path(), "shallow", "tok1");
    seed_repo_wiki(parent.path(), "vendor", "tok2");
    // A repo beyond the depth cap.
    let deep_parent = parent.path().join("a").join("b").join("c").join("d");
    let deep = seed_repo_wiki(&deep_parent, "deep", "tok3");

    let found = discover_wiki_roots(parent.path(), 2, &IgnoreRules::default());
    let roots: std::collections::HashSet<std::path::PathBuf> =
        found.iter().map(|w| w.repo_root.clone()).collect();

    assert!(
        roots.contains(&shallow),
        "shallow repo within cap discovered"
    );
    assert!(
        !roots.contains(&deep),
        "repo beyond depth cap must not be discovered"
    );
    assert!(
        !roots.iter().any(|r| r.ends_with("vendor")),
        "gitignored vendor/ wiki must not be discovered: {roots:?}"
    );

    // Each discovered wiki's wiki dir is attributed.
    let shallow_hit = found
        .iter()
        .find(|w| w.repo_root == shallow)
        .expect("shallow present");
    let want_wiki = wiki_directory(&repository_for_discovered_wiki(&shallow).expect("repo"));
    assert_eq!(
        shallow_hit.wiki_dir.as_ref().expect("wiki dir present"),
        &want_wiki,
        "discovered wiki dir must point at <repo>/.hallouminate/wiki"
    );
}

// ── ground_union ranking + attribution hardening (#106 /press) ───────────
//
// The e2e above covers the happy multi-corpus path with no crossencoder. These
// tests pin the edge cases the cook agent flagged: single-corpus union sets,
// empty / all-empty corpora, and — the riskiest correctness claim — that each
// hit stays attributed to its TRUE source corpus after the single crossencoder
// pass reshuffles the merged hit list. `ground_union` rebuilds the corpus
// lookup from `chunk_id` after the rerank, relying on the trait contract that
// rerank only reshuffles (no inserts/deletes); a reversing stub exercises that
// reshuffle hard.

use hallouminate::adapters::lance::{LanceStore, SearchHit};
use hallouminate::domain::ground::{GroundResponse, ground};
use hallouminate::domain::search::Crossencoder;

/// A crossencoder that reverses the hit list in place. It changes the order of
/// every hit (maximally stressing the post-rerank corpus-tag rebuild) while
/// honoring the trait contract: contents are preserved, only reshuffled.
struct ReversingCrossencoder;

impl Crossencoder for ReversingCrossencoder {
    fn rerank(
        &mut self,
        _query: &str,
        hits: &mut [SearchHit],
    ) -> hallouminate::domain::common::Result<()> {
        hits.reverse();
        Ok(())
    }
}

/// Boot a store and index each `(corpus_name, repo_root)` pair's wiki into its
/// own corpus, returning the `(name, paths)` targets `ground_union` consumes.
async fn index_wikis(
    store: &LanceStore,
    repos: &[std::path::PathBuf],
) -> Vec<(String, Vec<String>)> {
    let discovered: Vec<RepositoryConfig> = repos
        .iter()
        .filter_map(|r| repository_for_discovered_wiki(r))
        .collect();
    let (repos, _warnings) = union_discovered_repositories(&[], discovered);
    let registry = HandlerRegistry::new(Characters, 1500);
    let mut targets = Vec::new();
    for repo in &repos {
        let corpus = repository_wiki_corpus(repo).expect("derive wiki corpus");
        let mut embedder = StubEmbedder;
        index_corpus(&corpus, store, Some(&mut embedder), &registry)
            .await
            .unwrap_or_else(|e| panic!("index {}: {e}", corpus.name));
        targets.push((corpus.name.clone(), corpus.paths.clone()));
    }
    targets
}

/// AC: each hit must be attributed to the RIGHT source corpus — not merely a
/// non-empty one — even after the crossencoder reshuffles the merged hit list.
/// A regression that rebuilt the corpus tags positionally (instead of keyed by
/// chunk_id) would survive the e2e (which runs no crossencoder) but cross-wire
/// attributions here, where the reversing rerank guarantees the merged order
/// differs from the per-corpus search order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_attributes_each_hit_to_its_true_corpus_after_rerank_reshuffle() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");

    // Three sibling wikis, each carrying a token UNIQUE to that repo, so the
    // token a chunk contains is ground truth for which corpus it belongs to.
    let alpha = seed_repo_wiki(parent.path(), "alpha", "zphyxnort");
    let beta = seed_repo_wiki(parent.path(), "beta", "qwobblefrotz");
    let gamma = seed_repo_wiki(parent.path(), "gamma", "vummelthwap");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let targets = index_wikis(&store, &[alpha, beta, gamma]).await;

    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        Some(Box::new(ReversingCrossencoder)),
        GroundOpts::default(),
    )
    .await
    .expect("union ground with rerank");

    // Ground truth: token -> owning corpus. Each chunk's snippet contains
    // exactly one repo's unique token; its provenance MUST name that repo's
    // corpus.
    let token_owner = |snippet: &str| -> Option<&'static str> {
        if snippet.contains("zphyxnort") {
            Some("repo:alpha:wiki")
        } else if snippet.contains("qwobblefrotz") {
            Some("repo:beta:wiki")
        } else if snippet.contains("vummelthwap") {
            Some("repo:gamma:wiki")
        } else {
            None
        }
    };

    let mut checked = 0usize;
    for (path, doc) in &resp.docs {
        for chunk in &doc.chunks {
            let Some(true_corpus) = token_owner(&chunk.snippet) else {
                continue;
            };
            assert_eq!(
                chunk.provenance.corpus, true_corpus,
                "chunk in {path} carries {true_corpus}'s token but was attributed to \
                 {:?} after the rerank reshuffle",
                chunk.provenance.corpus,
            );
            assert_eq!(
                doc.corpus, true_corpus,
                "file {path} carries {true_corpus}'s token but DocFile.corpus is {:?}",
                doc.corpus,
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 3,
        "expected at least one token-bearing chunk per corpus, checked {checked}"
    );
}

/// Edge case: a union set of exactly ONE corpus must behave like a single
/// search — every hit attributed to that one corpus, no panic, hits present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_with_single_corpus_attributes_all_hits_to_it() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let solo = seed_repo_wiki(parent.path(), "solo", "zphyxnort");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let targets = index_wikis(&store, &[solo]).await;
    assert_eq!(targets.len(), 1, "exactly one corpus in the union set");

    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("single-corpus union ground");

    assert!(
        !resp.docs.is_empty(),
        "single corpus must still return hits"
    );
    for (path, doc) in &resp.docs {
        assert_eq!(
            doc.corpus, "repo:solo:wiki",
            "single-corpus union must attribute {path} to repo:solo:wiki"
        );
        for chunk in &doc.chunks {
            assert_eq!(
                chunk.provenance.corpus, "repo:solo:wiki",
                "every chunk in a single-corpus union is attributed to that corpus"
            );
        }
    }
}

/// Edge case: when a corpus in the union set yields NO hits, it must contribute
/// nothing while the other corpora's hits survive — the empty partition must
/// not strand or mis-tag the non-empty ones. Models a discovered-but-unindexed
/// wiki: a real `repo:empty:wiki` target whose corpus has zero rows in the
/// store, so `search_corpus` returns an empty `Vec` for it (the store is
/// populated, but the corpus filter matches nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_with_one_empty_corpus_keeps_the_non_empty_hits() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let full = seed_repo_wiki(parent.path(), "full", "zphyxnort");

    // The empty wiki: a real, existing `.hallouminate/wiki` dir with no
    // markdown pages, so the on-disk ripgrep leg has a valid path to scan and
    // finds nothing.
    let empty_wiki = parent
        .path()
        .join("empty")
        .join(".hallouminate")
        .join("wiki");
    fs::create_dir_all(&empty_wiki).expect("mkdir empty wiki");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    // Index only the full wiki. The empty corpus is a valid target with no
    // indexed rows — exactly what a discovered-but-never-indexed sub-repo wiki
    // looks like in the live union: the corpus filter matches zero LanceDB rows.
    let mut targets = index_wikis(&store, &[full]).await;
    targets.push((
        "repo:empty:wiki".to_string(),
        vec![empty_wiki.to_string_lossy().into_owned()],
    ));
    assert_eq!(targets.len(), 2, "both corpora present in the union set");

    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("union ground with an empty corpus");

    assert!(
        !resp.docs.is_empty(),
        "the non-empty corpus's hits must survive alongside the empty one"
    );
    let corpora_seen: std::collections::HashSet<&str> =
        resp.docs.values().map(|d| d.corpus.as_str()).collect();
    assert!(
        corpora_seen.contains("repo:full:wiki"),
        "the full corpus must be represented: {corpora_seen:?}"
    );
    assert!(
        !corpora_seen.contains("repo:empty:wiki"),
        "an empty corpus must contribute no docs, saw: {corpora_seen:?}"
    );
}

/// Edge case: all corpora empty (or no corpora at all) must yield a well-formed
/// empty response, not a panic — this also exercises the crossencoder-skip
/// branch (`!hits.is_empty()`), which a reversing rerank on an empty slice would
/// otherwise be asked to run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_over_all_empty_corpora_returns_empty_response_without_panic() {
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");

    // No corpora at all is the degenerate floor; a crossencoder is supplied so
    // the empty-hit skip is exercised rather than handed an empty slice.
    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &[],
        &store,
        Some(&mut embedder),
        Some(Box::new(ReversingCrossencoder)),
        GroundOpts::default(),
    )
    .await
    .expect("empty union ground must not error");

    assert!(resp.docs.is_empty(), "no corpora => no docs");
    assert_eq!(resp.stats.hits, 0, "no corpora => zero summed hits");
    assert_eq!(resp.query, "distinctive token wiki", "query echoed back");
}

/// Regression guard for the spec's byte-for-byte-unchanged guarantee: the
/// `corpus=Some(name)` single-corpus path (`ground`) must attribute every chunk
/// to exactly the requested corpus, and the per-chunk provenance must mirror the
/// parent DocFile.corpus. #106 added the additive `provenance.corpus` stamp on
/// this path; this locks that the stamp equals the single requested corpus and
/// nothing leaked from the union machinery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_corpus_ground_stamps_provenance_with_exactly_the_requested_corpus() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let only = seed_repo_wiki(parent.path(), "only", "zphyxnort");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let targets = index_wikis(&store, &[only]).await;
    let (corpus_name, corpus_paths) = &targets[0];

    let mut embedder = StubEmbedder;
    let resp: GroundResponse = ground(
        "distinctive token wiki",
        corpus_name,
        corpus_paths,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("single-corpus ground");

    assert!(!resp.docs.is_empty(), "requested corpus must return hits");
    for (path, doc) in &resp.docs {
        assert_eq!(
            &doc.corpus, corpus_name,
            "single-corpus ground must attribute {path} to the requested corpus"
        );
        for chunk in &doc.chunks {
            assert_eq!(
                &chunk.provenance.corpus, corpus_name,
                "provenance.corpus must equal the requested corpus, not empty or another"
            );
        }
    }
}

/// Fix 1 regression guard: when two corpora index the same absolute file path,
/// their chunk_ids collide (chunk_id = blake3(file_ref#ord), file_ref = absolute
/// path). A HashMap<chunk_id, corpus> collapses both attributions to one. The
/// VecDeque queue must preserve both so BOTH corpora appear in the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_preserves_attribution_when_chunk_ids_collide_across_corpora() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");

    // One wiki file that BOTH corpora will index. Same absolute path => same
    // chunk_id for every chunk in both corpora.
    let wiki_dir = parent.path().join("shared_wiki");
    fs::create_dir_all(&wiki_dir).expect("mkdir shared_wiki");
    let wiki_file = wiki_dir.join("shared.md");
    fs::write(
        &wiki_file,
        "# Shared\n\nThe distinctive token zphyxnort lives here.\n",
    )
    .expect("write shared wiki");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let registry =
        hallouminate::domain::indexer::HandlerRegistry::new(text_splitter::Characters, 1500);

    // Index the same directory under two distinct corpus names.
    let corpus_a = hallouminate::domain::common::CorpusConfig {
        name: "repo:alpha:wiki".to_string(),
        paths: vec![wiki_dir.to_string_lossy().into_owned()],
        globs: vec!["**/*.md".to_string()],
        exclude: Vec::new(),
        global: false,
    };
    let corpus_b = hallouminate::domain::common::CorpusConfig {
        name: "repo:beta:wiki".to_string(),
        paths: vec![wiki_dir.to_string_lossy().into_owned()],
        globs: vec!["**/*.md".to_string()],
        exclude: Vec::new(),
        global: false,
    };
    let mut embedder = StubEmbedder;
    index_corpus(&corpus_a, &store, Some(&mut embedder), &registry)
        .await
        .expect("index alpha");
    let mut embedder = StubEmbedder;
    index_corpus(&corpus_b, &store, Some(&mut embedder), &registry)
        .await
        .expect("index beta");

    let targets = vec![
        (corpus_a.name.clone(), corpus_a.paths.clone()),
        (corpus_b.name.clone(), corpus_b.paths.clone()),
    ];
    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("union ground with colliding chunk_ids");

    // Both corpora must appear in the attribution — the VecDeque queue must not
    // have collapsed them to one.
    let corpora_seen: std::collections::HashSet<&str> =
        resp.docs.values().map(|d| d.corpus.as_str()).collect();
    assert!(
        corpora_seen.contains("repo:alpha:wiki") && corpora_seen.contains("repo:beta:wiki"),
        "both corpora must appear in attribution even when chunk_ids collide; saw: {corpora_seen:?}"
    );
}

// ── Package C: ground_union efficiency (single embed, no clones) ─────────

use hallouminate::domain::embeddings::{EmbedBatch, EmbedRole};

/// Wraps `StubEmbedder`, counting `embed_batch` invocations. Regression guard
/// for the union query-embed hoist: a multi-corpus union must embed the query
/// exactly ONCE and share the vector, not re-embed per corpus (each embed is
/// a forward pass serialized on the embedder mutex).
struct CountingEmbedder {
    calls: usize,
}

impl EmbedBatch for CountingEmbedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        role: EmbedRole,
    ) -> hallouminate::domain::common::Result<
        Vec<[f32; hallouminate::adapters::lance::EMBEDDING_DIM]>,
    > {
        self.calls += 1;
        StubEmbedder.embed_batch(texts, role)
    }
}

/// AC: `ground_union` must embed the query exactly once and reuse the vector
/// across every corpus in the union set, regardless of corpus count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_union_embeds_query_exactly_once_across_three_corpora() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let alpha = seed_repo_wiki(parent.path(), "alpha", "zphyxnort");
    let beta = seed_repo_wiki(parent.path(), "beta", "qwobblefrotz");
    let gamma = seed_repo_wiki(parent.path(), "gamma", "vummelthwap");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let targets = index_wikis(&store, &[alpha, beta, gamma]).await;
    assert_eq!(targets.len(), 3, "three corpora in the union set");

    let mut embedder = CountingEmbedder { calls: 0 };
    ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("union ground");

    assert_eq!(
        embedder.calls, 1,
        "query must be embedded exactly once and shared across all corpora, not re-embedded per corpus"
    );
}

/// Regression guard for the corpus-queue refactor (finding 2): the queue keyed
/// by chunk_id must be built from borrowed `chunk_id`/corpus-name, then the
/// hits themselves MOVED (not cloned) into the final docs. Every chunk's exact
/// snippet text must still equal its true source file's body — a broken move
/// (e.g. building docs from a stale or duplicated hit) would corrupt or lose
/// this text even though attribution-only checks elsewhere might still pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn union_ground_preserves_exact_hit_text_after_queue_refactor() {
    let parent = tempfile::tempdir().expect("tempdir parent");
    let store_dir = tempfile::tempdir().expect("tempdir store");
    let alpha = seed_repo_wiki(parent.path(), "alpha", "zphyxnort");
    let beta = seed_repo_wiki(parent.path(), "beta", "qwobblefrotz");

    let store = LanceStore::open_or_create(store_dir.path(), MODEL, false, true)
        .await
        .expect("open store");
    let targets = index_wikis(&store, &[alpha, beta]).await;

    let mut embedder = StubEmbedder;
    let resp = ground_union(
        "distinctive token wiki",
        &targets,
        &store,
        Some(&mut embedder),
        None,
        GroundOpts::default(),
    )
    .await
    .expect("union ground");

    let expected_snippet = |repo: &str, token: &str| {
        format!("The distinctive token {token} lives only in {repo}'s wiki.")
    };

    let mut checked = 0usize;
    for doc in resp.docs.values() {
        for chunk in &doc.chunks {
            let (repo, token) = match doc.corpus.as_str() {
                "repo:alpha:wiki" => ("alpha", "zphyxnort"),
                "repo:beta:wiki" => ("beta", "qwobblefrotz"),
                other => panic!("unexpected corpus: {other}"),
            };
            assert!(
                chunk.snippet.contains(&expected_snippet(repo, token)),
                "chunk snippet must carry {repo}'s exact source text unmodified, got: {:?}",
                chunk.snippet
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 2,
        "expected chunks from both corpora, checked {checked}"
    );
}
