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
use hallouminate::domain::corpus::MarkdownChunker;
use hallouminate::domain::discovery::{DEFAULT_MAX_DEPTH, IgnoreRules, discover_wiki_roots};
use hallouminate::domain::ground::{GroundOpts, ground_union};
use hallouminate::domain::indexer::index_corpus;
use hallouminate::domain::repository::{
    RepositoryConfig, repository_for_discovered_wiki, repository_wiki_corpus,
    union_discovered_repositories, wiki_directory,
};
use text_splitter::Characters;

mod common;
use common::StubEmbedder;

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

#[tokio::test]
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
    let chunker = MarkdownChunker::new(Characters, 1500);
    let mut corpora: Vec<CorpusConfig> = Vec::new();
    for repo in &repos {
        let corpus = repository_wiki_corpus(repo).expect("derive wiki corpus");
        let mut embedder = StubEmbedder;
        index_corpus(&corpus, &store, Some(&mut embedder), &chunker)
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
