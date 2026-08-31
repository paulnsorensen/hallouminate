//! Integration tests for AC-1, AC-3, and AC-7 of the worktree index
//! provisioning spec (`.cheese/specs/worktree-index-provisioning.md`):
//! `ground` over an unseen corpus root schedules a non-blocking background
//! catch-up pass; a failed pass clears the seen-set so a later `ground`
//! re-enqueues; non-ground read ops never enqueue provisioning.
//!
//! The "unseen root" fixture is a request-resolved repo-layer corpus: a
//! directory with its own tracked-style `.hallouminate/config.toml`
//! (`[[repository]] name = "proj" path = "."`), exactly how a git worktree's
//! checked-out repo layer resolves `repo:proj:wiki` at that worktree's own
//! root. Boot-time `catch_up_index` only walks `state.baseline().effective_corpora()`
//! (a static config with no `[[repository]]`/`[[corpus]]` declared here), so
//! this corpus's `CorpusKey` is never in the boot-time seen-set and
//! `handle_ground`'s `observe` call must enqueue it lazily (#427).
//!
//! AC-2 (lock-before-write ordering) and AC-4..AC-6 (vector reuse) are
//! covered elsewhere: AC-2 by
//! `hallouminate_daemon::state::provisioning_holds_the_mutation_lock_before_writing`,
//! AC-4..AC-6 by the lance adapter's recording-embedder tests.

use std::os::unix::fs::PermissionsExt;
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
/// Boot-time `catch_up_index` therefore covers nothing here, isolating the
/// behavior under test to the lazy provisioner.
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
            "provisioning pass never indexed the corpus within 10s"
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
            }),
        })
        .await
        .expect("ground call")
}

// ── AC-1: ground over an unseen root schedules a non-blocking pass ────────

#[tokio::test]
async fn ground_over_an_unseen_root_provisions_it_in_the_background() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground_dir = tmp.path().join("ground");
    let repo_root = tmp.path().join("worktree");
    std::fs::create_dir_all(&repo_root).expect("mkdir worktree");
    // The request-resolved-store regression guard (provisioning must use the
    // enqueued config's resources, not the baseline's) lives at the unit
    // seam: `state::tests::provisioning_resolves_resources_from_the_enqueued_config_not_baseline`,
    // where a divergent `ground_dir` can be injected without the config
    // merge's scalar-conflict check or process-global env overrides.
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
        "the background provisioning pass must index the unseen root's file"
    );

    harness.shutdown().await.expect("daemon shutdown");
}

// ── AC-3: a failed pass clears the seen-set so a later ground re-enqueues ──

#[tokio::test]
async fn a_failed_provisioning_pass_clears_the_seen_set_for_re_enqueue_on_a_later_ground() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground_dir = tmp.path().join("ground");
    let repo_root = tmp.path().join("worktree");
    std::fs::create_dir_all(&repo_root).expect("mkdir worktree");
    let wiki_dir = seed_repo_layer_root(&repo_root, "proj");

    let cfg = cfg_baseline(&ground_dir);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Make the wiki root unreadable so the first provisioning pass fails.
    // `.hallouminate/config.toml` stays readable, so repo-layer discovery
    // from `repo_root` still resolves the corpus.
    std::fs::set_permissions(&wiki_dir, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    struct RestorePerms(std::path::PathBuf);
    impl Drop for RestorePerms {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = RestorePerms(wiki_dir.clone());

    let _first_ground: GroundResult = ground(&client, &repo_root).await;

    // Give the background pass a moment to run and fail. `corpus_stats`
    // itself walks the disk and would error on the unreadable root, so
    // this only waits — the zero-indexed-files assertion happens after
    // restoring readability below, which also makes `corpus_stats` safe
    // to call again.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Restore readability, then re-ground: the seen-set must have been
    // cleared so this re-enqueues the corpus for provisioning.
    std::fs::set_permissions(&wiki_dir, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
    let stats = corpus_stats(&client, &repo_root).await;
    assert_eq!(
        stats.indexed_files, 0,
        "the failed pass must not have indexed anything before the retry"
    );

    let _second_ground: GroundResult = ground(&client, &repo_root).await;

    let stats = poll_indexed_files_above_zero(&client, &repo_root).await;
    assert_eq!(
        stats.indexed_files, 1,
        "the re-enqueued pass must index the now-readable root's file"
    );

    harness.shutdown().await.expect("daemon shutdown");
}

// ── AC-7: non-ground reads never enqueue provisioning ──────────────────────

#[tokio::test]
async fn non_ground_reads_do_not_enqueue_provisioning() {
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
        "list_files/corpus_stats/read_markdown must never enqueue provisioning"
    );

    harness.shutdown().await.expect("daemon shutdown");
}
