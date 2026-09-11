//! Integration tests for Ground discovery and non-Ground read behavior in the
//! worktree index provisioning spec (`.cheese/specs/worktree-index-provisioning.md`):
//! Ground over an unseen root registers it with the live WatchRegistry; the
//! watcher pump catches it up in the background. Non-ground reads do not
//! register roots or enqueue catch-up work.
//!
//! The "unseen root" fixture is a request-resolved repo-layer corpus: a
//! directory with its own tracked-style `.hallouminate/config.toml`
//! (`[[repository]] name = "proj" path = "."`), exactly how a git worktree's
//! checked-out repo layer resolves `repo:proj:wiki` at that worktree's own
//! root. Boot-time `catch_up_index` only walks `state.baseline().effective_corpora()`
//! (a static config with no `[[repository]]`/`[[corpus]]` declared here), so
//! this corpus's `CorpusKey` is absent from the boot-time registry and Ground's
//! runtime registration routes it to the live pump.
//!
//! AC-2 (lock-before-write ordering) and AC-4..AC-6 (vector reuse) are
//! covered by daemon catch-up and adapter tests.

use std::path::Path;
use std::time::Duration;

use hallouminate_config::Config;
use hallouminate_daemon::{
    CorpusStatsResult, DaemonRequest, DaemonRequestPayload, GroundRequest, GroundResult,
    ListFilesRequest, ListFilesResult, ReadMarkdownRequest, connect_at,
};

use crate::common::daemon::DaemonHarness;

/// Baseline config with no `[[repository]]`/`[[corpus]]` entries, matching
/// production's rule that a repo-layer-declared repository must never also
/// be declared in the baseline (they'd collide on the derived corpus name).
/// Boot-time `catch_up_index` therefore covers nothing here, isolating
/// behavior under test to the live watcher pump.
fn cfg_baseline(ground_dir: &Path) -> Config {
    let toml = format!(
        r#"
[embeddings]
enabled = false

[storage]
ground_dir = "{ground}"
"#,
        ground = ground_dir.display(),
    );
    toml::from_str(&toml).expect("baseline toml parses")
}

/// Seeds a repo-layer root: a directory carrying its own
/// `.hallouminate/config.toml` declaring itself as `[[repository]]`, the
/// same shape a git worktree checks out for every worktree of the repo
/// (the file is git-tracked, so it exists identically at every worktree's
/// own root -- same corpus name, different canonical root per #288).
fn seed_repo_layer_root(repo_root: &Path, repo_name: &str) -> std::path::PathBuf {
    let hallou_dir = repo_root.join(".hallouminate");
    let wiki_dir = hallou_dir.join("wiki");
    std::fs::create_dir_all(&wiki_dir).expect("mkdir wiki");
    let toml = format!("[[repository]]\nname = \"{repo_name}\"\npath = \".\"\n");
    std::fs::write(hallou_dir.join("config.toml"), toml).expect("write repo-layer config");
    std::fs::write(wiki_dir.join("a.md"), "# A\n\nbody\n").expect("write a.md");
    wiki_dir
}

async fn corpus_stats(client: &hallouminate_daemon::DaemonClient, cwd: &Path) -> CorpusStatsResult {
    client
        .call(DaemonRequest {
            cwd: cwd.to_path_buf(),
            payload: DaemonRequestPayload::CorpusStats { corpus: None },
        })
        .await
        .expect("corpus_stats call")
}

async fn poll_indexed_files_above_zero(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
) -> CorpusStatsResult {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stats = corpus_stats(client, cwd).await;
        if stats.indexed_files > 0 {
            return stats;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "catch-up pass never indexed the corpus within 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn ground(client: &hallouminate_daemon::DaemonClient, cwd: &Path) -> GroundResult {
    client
        .call(DaemonRequest {
            cwd: cwd.to_path_buf(),
            payload: DaemonRequestPayload::Ground(GroundRequest {
                query: "body".to_string(),
                corpus: None,
                top_files: None,
                chunks_per_file: None,
                limit: None,
                snippet_chars: None,
                footnote_mode: Default::default(),
            }),
        })
        .await
        .expect("ground call")
}

// ── AC-1: ground over an unseen root schedules a non-blocking pass ────────

#[tokio::test]
async fn ground_over_an_unseen_root_catches_it_up_in_the_background() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground_dir = tmp.path().join("ground");
    let repo_root = tmp.path().join("worktree");
    std::fs::create_dir_all(&repo_root).expect("mkdir worktree");
    seed_repo_layer_root(&repo_root, "proj");

    let cfg = cfg_baseline(&ground_dir);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let started = std::time::Instant::now();
    let _ground: GroundResult = ground(&client, &repo_root).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "ground must respond promptly without waiting for the background catch-up pass, took {:?}",
        started.elapsed(),
    );

    let stats = poll_indexed_files_above_zero(&client, &repo_root).await;
    assert_eq!(
        stats.indexed_files, 1,
        "the background catch-up pass must index the unseen root's file"
    );

    harness.shutdown().await.expect("daemon shutdown");
}

// ── AC-7: non-ground reads never enqueue catch-up ─────────────────────────

#[tokio::test]
async fn non_ground_reads_do_not_enqueue_catch_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground_dir = tmp.path().join("ground");
    let repo_root = tmp.path().join("worktree");
    std::fs::create_dir_all(&repo_root).expect("mkdir worktree");
    seed_repo_layer_root(&repo_root, "proj");

    let cfg = cfg_baseline(&ground_dir);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let _list: ListFilesResult = client
        .call(DaemonRequest {
            cwd: repo_root.to_path_buf(),
            payload: DaemonRequestPayload::ListFiles(ListFilesRequest { corpus: None }),
        })
        .await
        .expect("list_files call");
    let _stats = corpus_stats(&client, &repo_root).await;
    let _read: serde_json::Value = client
        .call(DaemonRequest {
            cwd: repo_root.to_path_buf(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: None,
                path: "a.md".to_string(),
            }),
        })
        .await
        .expect("read_markdown call");

    // A short grace period for a wrongly-enqueued pass to have run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stats = corpus_stats(&client, &repo_root).await;
    assert_eq!(
        stats.indexed_files, 0,
        "list_files/corpus_stats/read_markdown must never enqueue catch-up"
    );

    harness.shutdown().await.expect("daemon shutdown");
}
