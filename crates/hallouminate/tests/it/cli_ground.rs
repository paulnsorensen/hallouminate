use std::fs;
use std::path::Path;
use tokio::process::Command;

use hallouminate::cli::{GroundArgs, run_ground};
use hallouminate_config::Config;
use hallouminate_daemon::{
    CorpusEntry, DaemonRequest, DaemonRequestPayload, DaemonResponse, ListCorporaResult, connect_at,
};

use crate::common::daemon::DaemonHarness;

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

    // Run the CLI from an isolated repository so cwd discovery cannot load this checkout's wiki.
    let repo_root = dir.path().join("repo");
    fs::create_dir_all(repo_root.join(".hallouminate")).unwrap();
    // The repo layer only marks the repository root; the baseline config
    // below declares it, and a second declaration is a duplicate corpus.
    fs::write(repo_root.join(".hallouminate/config.toml"), "").unwrap();
    let config_text = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        format!(
            "{config_text}\n[[repository]]\nname = \"fixture-repo\"\npath = {:?}\n",
            repo_root.display().to_string(),
        ),
    )
    .unwrap();
    let harness = DaemonHarness::spawn(load_config(&config_path)).await;
    let socket = harness.socket().to_path_buf();
    let binary = env!("CARGO_BIN_EXE_hallouminate");

    let indexed = Command::new(binary)
        .args(["index", "--config"])
        .arg(&config_path)
        .args(["--socket"])
        .arg(&socket)
        .current_dir(&repo_root)
        .output()
        .await
        .expect("spawn index CLI");
    assert!(
        indexed.status.success(),
        "index CLI failed: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );

    let grounded = Command::new(binary)
        .args(["ground", "spice melange Arrakis", "--config"])
        .arg(&config_path)
        .args(["--socket"])
        .arg(&socket)
        .args(["--corpus", "fixtures", "--format", "json"])
        .current_dir(&repo_root)
        .output()
        .await
        .expect("spawn ground CLI");
    assert!(
        grounded.status.success(),
        "ground CLI failed: {}",
        String::from_utf8_lossy(&grounded.stderr)
    );
    let response: serde_json::Value =
        serde_json::from_slice(&grounded.stdout).expect("ground JSON output");
    let docs = response["docs"].as_object().expect("docs object");
    assert!(!docs.is_empty(), "ground returned no docs");
    let top_path = docs
        .iter()
        .max_by(|(_, a), (_, b)| {
            a["score"]
                .as_f64()
                .partial_cmp(&b["score"].as_f64())
                .unwrap()
        })
        .map(|(path, _)| path)
        .expect("at least one hit");
    assert!(
        top_path.ends_with("arrakis.md"),
        "expected arrakis.md as top hit, got {top_path}"
    );
    for (path, doc) in docs {
        assert_eq!(
            doc["corpus"], "fixtures",
            "doc at {path} must carry corpus stamp"
        );
        let chunks = doc["chunks"].as_array().expect("CLI document chunks");
        assert!(!chunks.is_empty(), "doc at {path} has chunks");
        for chunk in chunks {
            assert!(
                chunk.get("source_text").is_none(),
                "CLI JSON must not expose internal source_text: {chunk}"
            );
        }
    }
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
    client: &hallouminate_daemon::DaemonClient,
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
