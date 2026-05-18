use std::fs;
use std::path::Path;

use hallouminate::app::cli::{GroundArgs, IndexArgs, cmd_index, run_ground};
use hallouminate::app::config::Config;

mod common;
use common::daemon::DaemonHarness;

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
        root.join("arrakis.md"),
        "# Arrakis\n\n## Spice melange\n\nThe spice must flow across the dunes of Arrakis.\n",
    )
    .unwrap();
    fs::write(
        root.join("caladan.md"),
        "# Caladan\n\n## House Atreides\n\nDuke Leto rules the watery world far from the desert.\n",
    )
    .unwrap();
    fs::write(
        root.join("giedi.md"),
        "# Giedi Prime\n\n## House Harkonnen\n\nA brutal industrial homeworld with no spice.\n",
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
async fn cmd_ground_returns_targeted_file_as_top_hit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let ground_dir = dir.path().join("ground");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir);

    // Spec contract: CLI commands are daemon clients. Spawn a daemon over a
    // per-test socket so both `cmd_index` and `run_ground` dispatch through
    // it instead of opening LanceDB directly. The harness lives for the
    // whole test so the index work and the query both hit the same daemon
    // instance — that's the spec's "one process owns mutations" invariant.
    let cfg = load_config(&config_path);
    let harness = DaemonHarness::spawn(cfg).await;
    let socket = harness.socket().to_path_buf();

    cmd_index(IndexArgs {
        config: Some(config_path.clone()),
        socket: Some(socket.clone()),
        ..Default::default()
    })
    .await
    .expect("index fixture corpus");

    let response = run_ground(GroundArgs {
        query: "spice melange Arrakis".into(),
        config: Some(config_path),
        socket: Some(socket),
        ..Default::default()
    })
    .await
    .expect("run ground");

    assert!(!response.docs.is_empty(), "ground returned no docs");
    assert_eq!(response.query, "spice melange Arrakis", "query echoed back");
    assert!(
        response.stats.hits >= response.docs.len(),
        "stats.hits ({}) must be >= bucketed docs ({})",
        response.stats.hits,
        response.docs.len()
    );
    let (top_path, top_doc) = response
        .docs
        .iter()
        .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap())
        .expect("at least one hit");
    assert!(
        top_path.ends_with("arrakis.md"),
        "expected arrakis.md as top hit, got {top_path}"
    );
    // Corpus stamping: orchestrator must stamp every doc with the resolved
    // corpus name (the only [[corpus]] in the fixture is "fixtures").
    for (path, doc) in &response.docs {
        assert_eq!(
            doc.corpus, "fixtures",
            "doc at {path} must carry corpus stamp"
        );
    }
    let chunk = top_doc
        .chunks
        .first()
        .expect("top doc has at least one chunk");
    assert!(
        chunk.line_range[0] >= 1 && chunk.line_range[1] >= chunk.line_range[0],
        "line_range looks malformed: {:?}",
        chunk.line_range
    );
    assert!(
        !chunk.chunk_id.is_empty(),
        "chunk_id must be a non-empty blake3-derived string"
    );
}

#[tokio::test]
async fn run_ground_fails_loudly_when_daemon_unreachable() {
    // Spec contract: ground must surface a clear "daemon unavailable" error
    // pointing at `hallouminate daemon` when the socket is missing, instead
    // of silently opening LanceDB directly (which is exactly the
    // multi-process race the daemon exists to prevent).
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    let ground_dir = dir.path().join("ground");
    let config_path = dir.path().join("config.toml");
    let cache_dir = dir.path().join("cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir);
    let missing_socket = dir.path().join("absent.sock");

    let err = run_ground(GroundArgs {
        query: "anything".into(),
        config: Some(config_path),
        socket: Some(missing_socket),
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
