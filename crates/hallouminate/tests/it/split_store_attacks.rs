//! Adversarial checks for the schema 7 split store: the `CorpusKey`
//! invariant, content-key collisions, and lexical fallback isolation across
//! two roots of one corpus name, plus the SessionStart hook under failure.

use std::fs;
use std::path::{Path, PathBuf};

use hallouminate_adapters::LanceStore;
use hallouminate_domain::common::{CorpusConfig, CorpusKey, retired_roots};
use hallouminate_domain::ground::{GroundOpts, ground};
use hallouminate_domain::indexer::{ChunkStore, HandlerRegistry, index_corpus};
use text_splitter::Characters;

use crate::common::{LANCE_WRITE_LOCK, StubEmbedder};

const MODEL: &str = "BAAI/bge-small-en-v1.5";
const TOKEN: &str = "zorblaxquux";

fn corpus_for_root(root: &Path) -> CorpusConfig {
    CorpusConfig {
        name: "docs".to_string(),
        paths: vec![root.to_string_lossy().into_owned()],
        globs: vec!["**/*.md".to_string()],
        exclude: Vec::new(),
        global: false,
    }
}

fn key_of(corpus: &CorpusConfig) -> CorpusKey {
    corpus.primary_corpus_key().expect("corpus key")
}

fn write(root: &Path, name: &str, body: &str) {
    fs::create_dir_all(root).expect("mkdir root");
    fs::write(root.join(name), body).expect("write file");
}

async fn open_store(dir: &Path) -> LanceStore {
    LanceStore::open_or_create(dir, MODEL, false, true, Some(Box::new(StubEmbedder)))
        .await
        .expect("open store")
}

async fn index_with(corpus: &CorpusConfig, store: &LanceStore, max_chars: usize) -> u64 {
    let registry = HandlerRegistry::new(Characters, max_chars);
    let stats = index_corpus(corpus, store, &registry)
        .await
        .expect("index_corpus");
    stats.embeddings_inserted as u64
}

async fn ground_paths(corpus: &CorpusConfig, store: &LanceStore) -> Vec<String> {
    let response = ground(TOKEN, corpus, store, None, GroundOpts::default())
        .await
        .expect("ground");
    let mut paths: Vec<String> = response.docs.keys().cloned().collect();
    paths.sort();
    paths
}

async fn file_refs(store: &LanceStore, corpus: &CorpusConfig) -> Vec<String> {
    let snaps = store.list_files(&key_of(corpus)).await.expect("list_files");
    let mut refs: Vec<String> = snaps.into_iter().map(|snap| snap.file_ref).collect();
    refs.sort();
    refs
}

fn any_ends_with(paths: &[String], suffix: &str) -> bool {
    paths.iter().any(|path| path.ends_with(suffix))
}

struct TwoRoots {
    _parent: tempfile::TempDir,
    store_dir: tempfile::TempDir,
    root_a: PathBuf,
    root_b: PathBuf,
}

fn two_roots() -> TwoRoots {
    let parent = tempfile::tempdir().expect("parent");
    let root_a = parent.path().join("root-a");
    let root_b = parent.path().join("root-b");
    TwoRoots {
        store_dir: tempfile::tempdir().expect("store dir"),
        _parent: parent,
        root_a,
        root_b,
    }
}

fn seed_overlapping(roots: &TwoRoots) {
    let shared = format!("# Shared\n\nthe {TOKEN} shared body\n");
    write(&roots.root_a, "shared.md", &shared);
    write(
        &roots.root_a,
        "a_only.md",
        &format!("# A\n\nthe {TOKEN} alpha body\n"),
    );
    write(&roots.root_b, "shared.md", &shared);
    write(
        &roots.root_b,
        "b_only.md",
        &format!("# B\n\nthe {TOKEN} bravo body\n"),
    );
}

#[tokio::test]
async fn split_store_root_a_never_surfaces_b_only_files() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    seed_overlapping(&roots);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;
    index_with(&corpus_b, &store, 1500).await;

    let paths_a = ground_paths(&corpus_a, &store).await;
    let paths_b = ground_paths(&corpus_b, &store).await;

    assert!(any_ends_with(&paths_a, "a_only.md"), "{paths_a:?}");
    assert!(any_ends_with(&paths_a, "shared.md"), "{paths_a:?}");
    assert!(!any_ends_with(&paths_a, "b_only.md"), "{paths_a:?}");
    assert!(!any_ends_with(&paths_b, "a_only.md"), "{paths_b:?}");
    for path in paths_a.iter().chain(paths_b.iter()) {
        let in_a = path.contains("/root-a/");
        let in_b = path.contains("/root-b/");
        assert!(in_a != in_b, "each hit names exactly one root: {path}");
    }
    for path in &paths_a {
        assert!(path.contains("/root-a/"), "root A hit leaked to B: {path}");
    }
    let refs_a = file_refs(&store, &corpus_a).await;
    assert_eq!(refs_a.len(), 2, "{refs_a:?}");
    assert!(!any_ends_with(&refs_a, "b_only.md"), "{refs_a:?}");
    let stats_a = store
        .corpus_chunk_stats(&key_of(&corpus_a))
        .await
        .expect("stats a");
    assert_eq!(stats_a.indexed_files, 2);
}

#[tokio::test]
async fn split_store_delete_file_on_a_leaves_b_results_intact() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    seed_overlapping(&roots);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;
    index_with(&corpus_b, &store, 1500).await;
    let refs_a = file_refs(&store, &corpus_a).await;
    let shared_ref = refs_a
        .iter()
        .find(|file_ref| file_ref.ends_with("shared.md"))
        .expect("shared.md in A")
        .clone();

    store
        .delete_file(&key_of(&corpus_a), &shared_ref)
        .await
        .expect("delete shared in A");
    store.delete_orphan_content().await.expect("gc");

    let refs_b = file_refs(&store, &corpus_b).await;
    assert_eq!(refs_b.len(), 2, "B keeps both files: {refs_b:?}");
    let paths_b = ground_paths(&corpus_b, &store).await;
    assert!(any_ends_with(&paths_b, "shared.md"), "{paths_b:?}");
    assert!(any_ends_with(&paths_b, "b_only.md"), "{paths_b:?}");
    let refs_a = file_refs(&store, &corpus_a).await;
    assert!(!any_ends_with(&refs_a, "shared.md"), "{refs_a:?}");
}

#[tokio::test]
async fn split_store_gc_after_retiring_root_a_keeps_b_shared_content() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    seed_overlapping(&roots);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;
    index_with(&corpus_b, &store, 1500).await;
    fs::remove_dir_all(&roots.root_a).expect("retire root a");

    let known = store.distinct_roots().await.expect("distinct roots");
    let retired = retired_roots(&known);
    assert_eq!(retired.len(), 1, "only root A is gone: {known:?}");
    for root in &retired {
        store.delete_root(root).await.expect("delete retired root");
    }
    store.delete_orphan_content().await.expect("gc");

    let paths_b = ground_paths(&corpus_b, &store).await;
    assert!(any_ends_with(&paths_b, "shared.md"), "{paths_b:?}");
    assert!(any_ends_with(&paths_b, "b_only.md"), "{paths_b:?}");
    let refs_a = file_refs(&store, &corpus_a).await;
    assert!(
        refs_a.is_empty(),
        "retired root keeps no map rows: {refs_a:?}"
    );
}

#[tokio::test]
async fn fresh_worktree_first_ground_answers_from_own_paths_and_embeds_nothing() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    let body = format!("# Same\n\nthe {TOKEN} byte identical body\n");
    write(&roots.root_a, "same.md", &body);
    write(&roots.root_b, "same.md", &body);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    let embedded_a = index_with(&corpus_a, &store, 1500).await;
    assert!(embedded_a > 0, "the first root must embed its own chunks");

    let first_ground = ground_paths(&corpus_b, &store).await;

    assert_eq!(first_ground.len(), 1, "{first_ground:?}");
    assert!(
        first_ground[0].ends_with("/root-b/same.md"),
        "the first ground answers from the worktree's own path: {first_ground:?}"
    );
    let embedded_b = index_with(&corpus_b, &store, 1500).await;
    assert_eq!(
        embedded_b, 0,
        "byte-identical files at the same relative path reuse the sibling's embeddings"
    );
    let after_catch_up = ground_paths(&corpus_b, &store).await;
    assert_eq!(after_catch_up, first_ground);
}

#[tokio::test]
async fn split_store_same_name_different_bytes_never_share_rows() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    write(&roots.root_a, "a.md", &format!("the {TOKEN} alphaonly\n"));
    write(&roots.root_b, "a.md", &format!("the {TOKEN} betaonly\n"));
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;
    let embedded_b = index_with(&corpus_b, &store, 1500).await;
    assert!(embedded_b > 0, "different bytes must embed again");

    for (corpus, own, other) in [
        (&corpus_a, "alphaonly", "betaonly"),
        (&corpus_b, "betaonly", "alphaonly"),
    ] {
        let response = ground(TOKEN, corpus, &store, None, GroundOpts::default())
            .await
            .expect("ground");
        let mut snippets = String::new();
        for doc in response.docs.values() {
            for chunk in &doc.chunks {
                snippets.push_str(&chunk.snippet);
            }
        }
        assert!(snippets.contains(own), "own text missing: {snippets}");
        assert!(!snippets.contains(other), "foreign text leaked: {snippets}");
    }
}

#[tokio::test]
async fn split_store_identical_bytes_with_other_file_name_report_the_new_path() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    let body = format!("# Same\n\nthe {TOKEN} identical body\n");
    write(&roots.root_a, "one.md", &body);
    write(&roots.root_b, "two.md", &body);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    let embedded_a = index_with(&corpus_a, &store, 1500).await;
    index_with(&corpus_b, &store, 1500).await;
    assert!(embedded_a > 0, "the first root must embed its own chunks");

    let paths_b = ground_paths(&corpus_b, &store).await;

    assert_eq!(paths_b.len(), 1, "{paths_b:?}");
    assert!(paths_b[0].ends_with("/root-b/two.md"), "{paths_b:?}");
    let refs_b = file_refs(&store, &corpus_b).await;
    assert!(!any_ends_with(&refs_b, "one.md"), "{refs_b:?}");
}

#[tokio::test]
async fn split_store_rename_within_a_root_reports_only_the_new_path() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    write(
        &roots.root_b,
        "x.md",
        &format!("the {TOKEN} renamed body\n"),
    );
    let store = open_store(roots.store_dir.path()).await;
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_b, &store, 1500).await;
    fs::rename(roots.root_b.join("x.md"), roots.root_b.join("y.md")).expect("rename");

    index_with(&corpus_b, &store, 1500).await;

    let refs = file_refs(&store, &corpus_b).await;
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert!(refs[0].ends_with("y.md"), "{refs:?}");
    let paths = ground_paths(&corpus_b, &store).await;
    assert!(any_ends_with(&paths, "y.md"), "{paths:?}");
    assert!(!any_ends_with(&paths, "x.md"), "{paths:?}");
}

#[tokio::test]
async fn split_store_other_chunker_fingerprint_never_reuses_content() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    let paragraph =
        format!("the {TOKEN} fingerprint paragraph with enough words to fill a chunk\n\n");
    let body = format!("# Fp\n\n{}", paragraph.repeat(20));
    write(&roots.root_a, "a.md", &body);
    write(&roots.root_b, "a.md", &body);
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;

    let embedded_b = index_with(&corpus_b, &store, 200).await;

    assert!(
        embedded_b > 0,
        "a different chunker setting changes the fingerprint, so B must embed itself"
    );
    let paths_a = ground_paths(&corpus_a, &store).await;
    assert!(any_ends_with(&paths_a, "a.md"), "{paths_a:?}");
}

#[tokio::test]
async fn lexical_fallback_cold_root_b_never_surfaces_warm_root_a_files() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    write(
        &roots.root_a,
        "a_only.md",
        &format!("the {TOKEN} alpha body\n"),
    );
    write(
        &roots.root_b,
        "b_only.md",
        &format!("the {TOKEN} bravo body\n"),
    );
    let store = open_store(roots.store_dir.path()).await;
    let corpus_a = corpus_for_root(&roots.root_a);
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_a, &store, 1500).await;

    let paths_b = ground_paths(&corpus_b, &store).await;

    assert_eq!(paths_b.len(), 1, "{paths_b:?}");
    assert!(paths_b[0].ends_with("/root-b/b_only.md"), "{paths_b:?}");
    let refs_b = file_refs(&store, &corpus_b).await;
    assert!(
        refs_b.is_empty(),
        "a read-only ground writes no map rows: {refs_b:?}"
    );
}

#[tokio::test]
async fn lexical_fallback_term_in_indexed_and_unindexed_file_lists_each_once() {
    let _guard = LANCE_WRITE_LOCK.lock().await;
    let roots = two_roots();
    write(&roots.root_b, "old.md", &format!("the {TOKEN} old body\n"));
    let store = open_store(roots.store_dir.path()).await;
    let corpus_b = corpus_for_root(&roots.root_b);
    index_with(&corpus_b, &store, 1500).await;
    write(&roots.root_b, "new.md", &format!("the {TOKEN} new body\n"));
    // Edit the indexed file after indexing: its map row is stale, the file is still indexed.
    write(
        &roots.root_b,
        "old.md",
        &format!("the {TOKEN} old body edited\n"),
    );

    let response = ground(TOKEN, &corpus_b, &store, None, GroundOpts::default())
        .await
        .expect("ground");

    assert_eq!(response.docs.len(), 2, "{:?}", response.docs.keys());
    for (path, doc) in &response.docs {
        let fallback = path.ends_with("new.md");
        for chunk in &doc.chunks {
            assert_eq!(
                chunk.chunk_id.starts_with("lexical-fallback:"),
                fallback,
                "only the unindexed file is a line window: {path} {}",
                chunk.chunk_id
            );
        }
    }
}

fn hook_command() -> String {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = Path::new(manifest).join("../../plugins/hallouminate/hooks/hooks.json");
    let text = fs::read_to_string(path).expect("read hooks.json");
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse hooks.json");
    json.pointer("/hooks/SessionStart/0/hooks/0/command")
        .and_then(|value| value.as_str())
        .expect("hook command")
        .to_string()
}

fn run_hook_with_fake(script: &str) -> (Option<i32>, std::time::Duration) {
    let bin = tempfile::tempdir().expect("bin dir");
    let fake = bin.path().join("hallouminate");
    fs::write(&fake, script).expect("write fake");
    fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod fake");
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let started = std::time::Instant::now();
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &hook_command()])
        .env("PATH", path)
        .output()
        .expect("run hook");
    (output.status.code(), started.elapsed())
}

#[test]
fn session_start_hook_exits_zero_when_hallouminate_fails() {
    let (code, _) = run_hook_with_fake("#!/bin/sh\necho boom >&2\nexit 7\n");
    assert_eq!(code, Some(0), "a failing index must not fail the hook");
}

#[test]
fn session_start_hook_does_not_hold_the_output_pipes_of_a_blocked_index() {
    let (code, elapsed) = run_hook_with_fake("#!/bin/sh\necho started\nsleep 5\n");
    assert_eq!(code, Some(0));
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "a hook runner that captures stdout and stderr must not wait for the detached index: {elapsed:?}"
    );
}
