use std::fs;
use std::path::Path;

use hallouminate::adapters::lance::LanceStore;
use hallouminate::app::cli::{IndexArgs, cmd_index, run_index};
use hallouminate::app::config::Config;

mod common;
use common::daemon::DaemonHarness;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

fn write_config(config_path: &Path, corpus_root: &Path, ground_dir: &Path, cache_dir: &Path) {
    let toml = format!(
        r#"
[[corpus]]
name = "fixtures"
paths = [{root:?}]
globs = ["**/*.md"]

[embeddings]
model     = "BAAI/bge-small-en-v1.5"
cache_dir = {cache:?}

[storage]
ground_dir = {dir:?}
"#,
        root = corpus_root.to_string_lossy().to_string(),
        cache = cache_dir.to_string_lossy().to_string(),
        dir = ground_dir.to_string_lossy().to_string(),
    );
    fs::write(config_path, toml).expect("write config");
}

fn load_config(config_path: &Path) -> Config {
    let text = fs::read_to_string(config_path).expect("read config");
    toml::from_str(&text).expect("parse config")
}

fn seed_fixtures(root: &Path) {
    fs::write(
        root.join("alpha.md"),
        "# Alpha doc\n\nThe spice must flow throughout the corpus.\n",
    )
    .unwrap();
    fs::write(
        root.join("beta.md"),
        "# Beta notes\n\nWitness the indexer pipeline.\n",
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
async fn cmd_index_indexes_fixture_corpus_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let ground_dir = dir.path().join("ground");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir);

    // Spec contract: CLI commands are daemon clients. Spawn a daemon over a
    // per-test socket so the index call dispatches through it instead of
    // opening LanceDB directly.
    let cfg = load_config(&config_path);
    let harness = DaemonHarness::spawn(cfg).await;

    cmd_index(IndexArgs {
        config: Some(config_path),
        socket: Some(harness.socket().to_path_buf()),
        ..Default::default()
    })
    .await
    .expect("first index run");

    // Re-open the LanceStore at the same ground dir the daemon used and
    // assert chunks landed. (Reading the store directly is safe here
    // because the daemon doesn't hold an exclusive lock — it just owns
    // mutations. Listing rows is a read.)
    let store = LanceStore::open_or_create(&ground_dir, MODEL)
        .await
        .expect("reopen ground dir");
    let rows = store.count_rows().await.expect("count rows");
    assert!(
        rows >= 2,
        "expected at least 2 chunks (one per fixture file), got {rows}"
    );
}

#[tokio::test]
async fn run_index_fails_loudly_when_daemon_unreachable() {
    // The spec's daemon-startup contract: do NOT auto-start the daemon.
    // CLI commands must surface a clear "daemon unavailable" error when
    // the socket is missing, with the hint pointing at `hallouminate
    // daemon`. Mirrors `tests/daemon.rs::daemon_client_returns_clear_error_when_socket_missing`
    // but exercises the CLI surface that user-facing transports actually
    // dispatch through.
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    fs::write(corpus_root.join("a.md"), "# A\n").unwrap();
    let ground_dir = dir.path().join("ground");
    let config_path = dir.path().join("config.toml");
    let cache_dir = dir.path().join("cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir);
    let missing_socket = dir.path().join("absent.sock");

    let err = run_index(IndexArgs {
        config: Some(config_path),
        socket: Some(missing_socket.clone()),
        ..Default::default()
    })
    .await
    .expect_err("missing daemon socket must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("daemon unavailable"),
        "error must signal `daemon unavailable`: {msg}"
    );
    assert!(
        msg.contains("hallouminate daemon"),
        "error must hint at how to start the daemon: {msg}"
    );
    assert_eq!(
        msg.matches("daemon unavailable").count(),
        1,
        "`daemon unavailable` prefix must appear exactly once (no double-wrap): {msg}"
    );
}
