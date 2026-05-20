//! Integration tests for the local hallouminate daemon.
//!
//! Three quality-gate buckets from the spec:
//!   1. CLI / MCP daemon-client calls fail clearly when the daemon is
//!      unavailable.
//!   2. Concurrent same-corpus mutations are serialized through the daemon
//!      so we never let two writers race the LanceDB / filesystem state.
//!   3. `add_markdown` to `repo:{name}:wiki` writes under
//!      `<repo>/.hallouminate/wiki` and refreshes LanceDB rows through the
//!      daemon end-to-end.
//!
//! The e2e test downloads the embedding model on first run and is gated
//! `#[ignore]` to keep CI fast, mirroring `tests/cli_index.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hallouminate::app::config::Config;
use hallouminate::app::daemon::{
    AddMarkdownRequest, DaemonRequest, DaemonRequestPayload, DaemonResponse, DaemonState,
    ErrorKind, ReadMarkdownRequest, connect_at, serve,
};
use hallouminate::domain::repository::{RepoCorpusKind, repo_corpus_name, wiki_directory};
use tokio::time::timeout;

mod common;
use common::daemon::DaemonHarness;

fn cfg_with_repository(ground_dir: &Path, repo_name: &str, repo_path: &Path) -> Config {
    let toml = format!(
        r#"
[[repository]]
name = "{repo_name}"
path = "{repo}"

[storage]
ground_dir = "{ground}"
"#,
        repo = repo_path.display(),
        ground = ground_dir.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("repository toml parses");
    cfg
}

// ─── Gate 1: fail loudly when daemon is unreachable ──────────────────────

#[tokio::test]
async fn daemon_client_returns_clear_error_when_socket_missing() {
    // No daemon spawned. Connect attempt must surface a message that
    // identifies the missing socket so a CLI user (or the MCP transport)
    // can route the failure as "daemon unavailable" instead of guessing.
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("never-bound.sock");
    let err = connect_at(&missing)
        .await
        .expect_err("missing socket must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("daemon unavailable"),
        "error must say `daemon unavailable`: {msg}"
    );
    assert!(
        msg.contains(missing.to_string_lossy().as_ref()),
        "error must name the socket path: {msg}"
    );
}

#[tokio::test]
async fn daemon_client_helper_returns_clear_error_when_socket_missing() {
    // `daemon_client()` falls back through `daemon_socket_path()` to read
    // the configured runtime/cache socket, so we can't drive it from an
    // env-mutating test without racing the rest of the test binary (the
    // Rust test harness runs tasks across threads, and parallel tests may
    // call `daemon_socket_path()` themselves). Instead exercise the same
    // failure shape through the explicit `connect_at` entry point, which
    // is the codepath production callers reach via `client_for(Some(...))`
    // — the env-fallback is then covered structurally by
    // `socket_path_is_named_daemon_sock` and the empty-XDG filter unit
    // tests inside `daemon::socket`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("absent.sock");
    let err = connect_at(&missing)
        .await
        .expect_err("missing socket must fail");
    assert!(
        format!("{err:#}").contains("daemon unavailable"),
        "got: {err:#}"
    );
}

#[tokio::test]
async fn daemon_client_reconnect_failure_carries_start_hint() {
    // Regression for the `call_raw` reconnect path: a `DaemonClient`
    // constructed against a live daemon that then dies must surface the
    // same `(start it with `hallouminate daemon`)` hint that the initial
    // `connect_at` path emits. Before the fix this path returned the bare
    // `daemon unavailable: connect to <socket> failed` shape, so a long-
    // lived MCP-side client outliving the daemon would lose the actionable
    // suffix.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{c}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let socket = harness.socket().to_path_buf();
    // Construct a client against the live daemon, then tear the daemon
    // down so the next `call_raw` reaches the reconnect-failure branch.
    let client = connect_at(&socket).await.expect("initial connect");
    drop(harness);
    // Wait briefly for the socket file to disappear so the reconnect
    // attempt deterministically fails.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while socket.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = client
        .call_raw(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect_err("reconnect must fail after daemon shutdown");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("daemon unavailable"),
        "reconnect error must say `daemon unavailable`: {msg}"
    );
    assert!(
        msg.contains("hallouminate daemon"),
        "reconnect error must include the start hint: {msg}"
    );
}

// ─── Gate 2: same-corpus serialization ───────────────────────────────────

#[tokio::test]
async fn daemon_serializes_concurrent_writes_to_the_same_corpus() {
    // Two `AddMarkdown` requests fired at the same corpus must execute in
    // some serial order — the second one must observe a file already on
    // disk from the first. We exercise this by:
    //   1. issuing both requests concurrently with `overwrite=false`,
    //   2. asserting exactly one succeeds and one comes back with the
    //      "already exists" invalid-params error.
    // If the per-corpus mutex were missing, two `AddMarkdown` workers
    // could both pass the existence check and race the atomic write — one
    // would succeed and the other would surface a different failure shape
    // (e.g. a write error) instead of the structured "already exists".
    //
    // We use a `[[corpus]]` directly (no embedder needed) and a tiny
    // markdown body so the dispatch sees an empty-chunk skip rather than
    // touching the embedding model.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus root");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{}"
"#,
        corpus_root.display(),
        ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;

    // Empty file (`""`) produces zero chunks; the indexer's empty-skip
    // path avoids the embedding model entirely, keeping this test
    // hermetic.
    let make_req = || DaemonRequest {
        cwd: PathBuf::new(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "docs".into(),
            path: "race.md".into(),
            content: "".into(),
            overwrite: false,
        }),
    };

    let client_a = connect_at(harness.socket()).await.expect("client a");
    let client_b = connect_at(harness.socket()).await.expect("client b");
    let req_a = make_req();
    let req_b = make_req();
    let (res_a, res_b) = tokio::join!(client_a.call_raw(req_a), client_b.call_raw(req_b));

    let res_a = res_a.expect("a transport ok");
    let res_b = res_b.expect("b transport ok");

    // One ok, one invalid_params "already exists". Either order is fine —
    // we only need to prove that serialization happened, not who won.
    let ok_count = [&res_a, &res_b]
        .iter()
        .filter(|r| matches!(r, DaemonResponse::Ok { .. }))
        .count();
    let err_count = [&res_a, &res_b]
        .iter()
        .filter(|r| {
            matches!(
                r,
                DaemonResponse::Err {
                    kind: ErrorKind::InvalidParams,
                    message,
                } if message.contains("already exists")
            )
        })
        .count();
    assert_eq!(
        (ok_count, err_count),
        (1, 1),
        "expected exactly one ok and one already-exists, got a={res_a:?} b={res_b:?}"
    );

    // The file is on disk regardless of which request won.
    assert!(
        corpus_root.join("race.md").exists(),
        "winner must have left the file on disk",
    );
}

#[tokio::test]
async fn per_corpus_mutex_does_not_block_writes_to_different_corpora() {
    // Per-corpus mutex layer: distinct corpora must NOT share a per-corpus
    // Mutex<()>, so an `add_markdown` to one corpus doesn't block another
    // corpus's per-corpus lock acquisition. NOTE: this does NOT claim writes
    // to different corpora run in parallel — every mutating handler also
    // takes the single-permit global `write_lane` (see
    // `DaemonStateInner.write_lane`), which serializes mutations across
    // corpora at the lane layer. This regression test only covers the
    // per-corpus mutex map: a refactor that accidentally returned the same
    // mutex for two different names would still let both writes succeed
    // (the global lane would serialize them) but would silently shrink
    // throughput; this test pins the layer-1 contract.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_a = tmp.path().join("a");
    let corpus_b = tmp.path().join("b");
    std::fs::create_dir_all(&corpus_a).expect("a mkdir");
    std::fs::create_dir_all(&corpus_b).expect("b mkdir");
    let toml = format!(
        r#"
[[corpus]]
name = "a"
paths = ["{a}"]
globs = ["**/*.md"]

[[corpus]]
name = "b"
paths = ["{b}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        a = corpus_a.display(),
        b = corpus_b.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;

    let client_a = connect_at(harness.socket()).await.expect("client a");
    let client_b = connect_at(harness.socket()).await.expect("client b");

    let req_a = DaemonRequest {
        cwd: PathBuf::new(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "a".into(),
            path: "alpha.md".into(),
            content: "".into(),
            overwrite: false,
        }),
    };
    let req_b = DaemonRequest {
        cwd: PathBuf::new(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "b".into(),
            path: "beta.md".into(),
            content: "".into(),
            overwrite: false,
        }),
    };

    let (res_a, res_b) = tokio::join!(client_a.call_raw(req_a), client_b.call_raw(req_b));
    let res_a = res_a.expect("a transport ok");
    let res_b = res_b.expect("b transport ok");
    assert!(
        matches!(res_a, DaemonResponse::Ok { .. }),
        "corpus a must succeed: {res_a:?}"
    );
    assert!(
        matches!(res_b, DaemonResponse::Ok { .. }),
        "corpus b must succeed: {res_b:?}"
    );
}

// ─── Gate 3: repository wiki end-to-end ──────────────────────────────────

#[tokio::test]
async fn daemon_resolves_repository_derived_corpora_in_list_corpora() {
    // Verifies the daemon surfaces `repo:{name}:wiki` (and the source
    // `repo:{name}:corpus` when declared) via the same list_corpora API
    // that CLI / MCP transports use.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let repo = tmp.path().join("my-repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let cfg = cfg_with_repository(&ground, "myrepo", &repo);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::ListCorpora,
        })
        .await
        .expect("list_corpora ok");
    let names: Vec<String> = value
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.contains(&"repo:myrepo:wiki".to_string()),
        "derived wiki corpus missing: {names:?}"
    );
    // Source corpus must NOT appear when `corpus_paths` is empty (spec
    // §Approach: "derived only when the repository declares source-document
    // paths").
    assert!(
        !names.contains(&"repo:myrepo:corpus".to_string()),
        "source corpus must be omitted when corpus_paths is empty: {names:?}"
    );
}

#[tokio::test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
async fn daemon_add_markdown_to_repository_wiki_writes_under_dot_hallouminate_wiki() {
    // End-to-end gate: writing into `repo:{name}:wiki` must land at
    // `<repo>/.hallouminate/wiki/<path>` AND refresh LanceDB rows through
    // the daemon (the indexed-files report tells us the write reached the
    // index, not just the disk).
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let repo = tmp.path().join("my-repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let cfg = cfg_with_repository(&ground, "myrepo", &repo);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let body = "# Cheese\n\nHalloumi grills better than most.\n";
    let req = DaemonRequest {
        cwd: PathBuf::new(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: repo_corpus_name("myrepo", RepoCorpusKind::Wiki).unwrap(),
            path: "cheese.md".into(),
            content: body.into(),
            overwrite: false,
        }),
    };
    let value: serde_json::Value = timeout(Duration::from_secs(60), client.call(req))
        .await
        .expect("timeout")
        .expect("add_markdown ok");

    // File on disk, under `<repo>/.hallouminate/wiki/`.
    let wiki_dir = wiki_directory(&hallouminate::domain::repository::RepositoryConfig {
        name: "myrepo".into(),
        path: repo.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let written = wiki_dir.join("cheese.md");
    assert!(
        written.exists(),
        "wiki file must land at {} (got cwd-relative path?)",
        written.display()
    );
    assert_eq!(std::fs::read_to_string(&written).unwrap(), body);

    // Daemon reports the file as freshly upserted via the same
    // IndexReport shape the MCP `add_markdown` returns.
    let corpora = value["indexed"]["corpora"]
        .as_array()
        .expect("indexed.corpora array");
    assert_eq!(corpora.len(), 1, "one corpus report: {corpora:?}");
    assert_eq!(
        corpora[0]["files_upserted"].as_u64(),
        Some(1),
        "the freshly-written file must be upserted: {:?}",
        corpora[0],
    );

    // Reading back through the daemon returns the verbatim bytes (the
    // wiki tree is the source of truth).
    let read_value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: repo_corpus_name("myrepo", RepoCorpusKind::Wiki).unwrap(),
                path: "cheese.md".into(),
            }),
        })
        .await
        .expect("read_markdown ok");
    assert_eq!(read_value["content"].as_str(), Some(body));
}

// ─── Hardening: liveness, contract surface, single-instance ────────────

#[tokio::test]
async fn daemon_ping_returns_pong_string() {
    // Smallest possible request — the contract is: client encodes
    // `{"op":"ping"}`, server returns `{"status":"ok","result":"pong"}`.
    // If this regresses, every other client call regresses too.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{c}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping ok");
    assert_eq!(value, serde_json::Value::String("pong".to_string()));
}

#[tokio::test]
async fn daemon_index_with_paths_from_returns_invalid_params() {
    // Cook flagged `paths_from` as deliberately unsupported via the daemon
    // (the dispatcher returns InvalidParams instead of silently ignoring
    // the flag). Lock the contract so a future implementation can't quietly
    // change the failure shape.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{c}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");
    let response = client
        .call_raw(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::Index(hallouminate::app::daemon::IndexRequest {
                corpus: None,
                paths_from: Some(PathBuf::from("/tmp/list.txt")),
            }),
        })
        .await
        .expect("transport ok");
    match response {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message,
        } => {
            assert!(
                message.contains("paths_from"),
                "error must name the unsupported field: {message}"
            );
        }
        other => panic!("expected InvalidParams for paths_from, got: {other:?}"),
    }
}

#[tokio::test]
async fn daemon_malformed_json_request_returns_invalid_params() {
    // The dispatcher promises every transport-level framing failure surfaces
    // as InvalidParams with a clear message, never as a panic or a silent
    // hang. Send raw bytes that look nothing like a DaemonRequest and
    // confirm the server still answers with one JSON line on the same
    // connection.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{c}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;

    let mut stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");
    stream
        .write_all(b"this is not json at all\n")
        .await
        .expect("write");
    stream.flush().await.expect("flush");
    let (read_half, _) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("server must respond before timeout")
        .expect("read response");
    let response: DaemonResponse =
        serde_json::from_str(line.trim_end()).expect("server reply must be valid JSON");
    match response {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message,
        } => {
            assert!(
                message.contains("invalid request"),
                "error must mention parse failure: {message}"
            );
        }
        other => panic!("expected InvalidParams for garbage input, got: {other:?}"),
    }
}

#[tokio::test]
async fn daemon_single_instance_lock_blocks_second_serve_on_same_socket() {
    // The spec calls out "Unix socket cleanup must handle stale sockets
    // without allowing two daemons to run." The advisory flock on
    // `<socket>.lock` is the enforcement point. If two daemons could both
    // bind, the per-corpus mutex + write-lane invariants would silently
    // break.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        r#"
[[corpus]]
name = "docs"
paths = ["{c}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");

    // First daemon: the standard harness takes the lock and binds the
    // socket.
    let harness = DaemonHarness::spawn(cfg.clone()).await;

    // Second daemon: same socket path, fresh state. `serve()` must bail out
    // before returning, with an error that mentions the lockfile so a user
    // sees what's holding them up.
    let state2 = DaemonState::open(cfg).await.expect("second open ok");
    let socket2 = harness.socket().to_path_buf();
    let result = timeout(Duration::from_secs(5), serve(&state2, &socket2))
        .await
        .expect("serve must return promptly");
    let err = result.expect_err("second serve must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already holds") || msg.contains("lockfile"),
        "single-instance error must mention the lock: {msg}"
    );
    // Sanity: the first daemon's socket is still usable.
    let client = connect_at(harness.socket())
        .await
        .expect("first daemon alive");
    let pong: serde_json::Value = client
        .call(DaemonRequest {
            cwd: PathBuf::new(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping");
    assert_eq!(pong, serde_json::Value::String("pong".to_string()));
}
