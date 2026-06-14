use std::fs;
use std::path::Path;

use hallouminate::app::cli::{GroundArgs, IndexArgs, cmd_index, run_ground};
use hallouminate::app::config::Config;
use hallouminate::app::daemon::{
    CorpusEntry, DaemonRequest, DaemonRequestPayload, DaemonResponse, ListCorporaResult, connect_at,
};

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

/// AC #6 from `.cheese/specs/repo-config-discovery.md`: editing a repo's
/// `.hallouminate/config.toml` does NOT require a daemon restart — the next
/// request must reflect the edit. Drives the daemon directly via
/// `DaemonClient` rather than `cmd_ground`, so the test never has to mutate
/// the process-wide CWD (which would race other parallel test threads).
#[tokio::test]
async fn repo_config_edit_takes_effect_without_daemon_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo_root = dir.path().to_path_buf();

    // Repo-layer config declares its own corpus rooted under <repo>/notes.
    let notes_dir = repo_root.join("notes");
    fs::create_dir_all(&notes_dir).expect("mkdir notes");
    let repo_cfg_dir = repo_root.join(".hallouminate");
    fs::create_dir_all(&repo_cfg_dir).expect("mkdir .hallouminate");
    let repo_cfg_path = repo_cfg_dir.join("config.toml");
    fs::write(
        &repo_cfg_path,
        format!(
            r#"
[[corpus]]
name = "notes"
paths = [{notes:?}]
globs = ["**/*.md"]
"#,
            notes = notes_dir.to_string_lossy().to_string(),
        ),
    )
    .expect("write repo config");

    // Baseline (XDG-equivalent) config the daemon boots with: a separate
    // corpus, a tempdir ground/cache, no overlap with the repo's corpus.
    let baseline_corpus_root = dir.path().join("baseline-corpus");
    fs::create_dir_all(&baseline_corpus_root).expect("mkdir baseline corpus");
    let ground_dir = dir.path().join("ground");
    let cache_dir = dir.path().join("cache");
    let baseline_cfg_path = dir.path().join("baseline.toml");
    write_config(
        &baseline_cfg_path,
        &baseline_corpus_root,
        &ground_dir,
        &cache_dir,
    );
    // Rename baseline's sole corpus so it never collides with the repo
    // layer's `"notes"`. `write_config` hardcodes the name `"fixtures"` so
    // we already have the disjoint-name shape; assert it for future-proofing.
    let baseline_cfg = load_config(&baseline_cfg_path);
    assert!(
        baseline_cfg.corpora.iter().all(|c| c.name != "notes"),
        "baseline must not pre-declare the corpus the repo layer introduces"
    );

    let harness = DaemonHarness::spawn(baseline_cfg).await;
    let client = connect_at(harness.socket())
        .await
        .expect("connect to daemon");

    // First request: repo config declares `notes`, daemon must surface it.
    let corpora = list_corpora_at(&client, &repo_root).await;
    assert!(
        corpora.iter().any(|c| c.name == "notes"),
        "repo-declared corpus must be visible without daemon restart: {corpora:?}"
    );

    // Edit the repo config in place — add a second corpus under <repo>/wiki.
    let wiki_dir = repo_root.join("wiki");
    fs::create_dir_all(&wiki_dir).expect("mkdir wiki");
    fs::write(
        &repo_cfg_path,
        format!(
            r#"
[[corpus]]
name = "notes"
paths = [{notes:?}]
globs = ["**/*.md"]

[[corpus]]
name = "wiki"
paths = [{wiki:?}]
globs = ["**/*.md"]
"#,
            notes = notes_dir.to_string_lossy().to_string(),
            wiki = wiki_dir.to_string_lossy().to_string(),
        ),
    )
    .expect("rewrite repo config");

    // Second request, same daemon — the edit must be reflected without a
    // restart.
    let corpora = list_corpora_at(&client, &repo_root).await;
    let names: Vec<&str> = corpora.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"notes") && names.contains(&"wiki"),
        "edit to repo config must be visible on the next request: {names:?}"
    );
}

/// Helper: issue a `ListCorpora` request against the daemon with an explicit
/// `cwd` so the dispatcher can run repo-config discovery against the chosen
/// directory. Pulls the typed payload out of `DaemonResponse::Ok`.
async fn list_corpora_at(
    client: &hallouminate::app::daemon::DaemonClient,
    cwd: &Path,
) -> ListCorporaResult {
    let resp = client
        .call_raw(DaemonRequest {
            cwd: cwd.to_path_buf(),
            payload: DaemonRequestPayload::ListCorpora,
        })
        .await
        .expect("list_corpora transport ok");
    match resp {
        DaemonResponse::Ok { result } => {
            serde_json::from_value::<Vec<CorpusEntry>>(result).expect("list_corpora payload shape")
        }
        DaemonResponse::Err { kind, message } => {
            panic!("list_corpora returned {kind:?}: {message}");
        }
    }
}
