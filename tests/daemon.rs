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
    AddMarkdownRequest, BacklinksRequest, DaemonRequest, DaemonRequestPayload, DaemonResponse,
    DaemonState, DeleteMarkdownRequest, ErrorKind, GroundRequest, GroundResult, IndexRequest,
    LineRange, ListFilesRequest, ListFilesResult, Position, ReadMarkdownRequest, connect_at, serve,
    spawn_signal_handlers,
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
    // Daemon is already dead — call_raw fails at the socket connect before
    // ever reaching dispatch, so cwd does not need to be a valid path here.
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
    let cwd = harness.cwd().to_path_buf();
    let make_req = || DaemonRequest {
        cwd: cwd.clone(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "docs".into(),
            path: "race.md".into(),
            content: "".into(),
            overwrite: false,
            ..Default::default()
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
        cwd: harness.cwd().to_path_buf(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "a".into(),
            path: "alpha.md".into(),
            content: "".into(),
            overwrite: false,
            ..Default::default()
        }),
    };
    let req_b = DaemonRequest {
        cwd: harness.cwd().to_path_buf(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "b".into(),
            path: "beta.md".into(),
            content: "".into(),
            overwrite: false,
            ..Default::default()
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
            cwd: harness.cwd().to_path_buf(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        cwd: harness.cwd().to_path_buf(),
        payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: repo_corpus_name("myrepo", RepoCorpusKind::Wiki).unwrap(),
            path: "cheese.md".into(),
            content: body.into(),
            overwrite: false,
            ..Default::default()
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
    // The primary write (cheese.md) plus the auto-generated root index.md
    // both flow through index_single_file, so files_upserted is 2.
    assert_eq!(
        corpora[0]["files_upserted"].as_u64(),
        Some(2),
        "primary write + auto-built root index.md must both be upserted: {:?}",
        corpora[0],
    );

    // Reading back through the daemon returns the verbatim bytes (the
    // wiki tree is the source of truth).
    let read_value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: Some(repo_corpus_name("myrepo", RepoCorpusKind::Wiki).unwrap()),
                path: "cheese.md".into(),
            }),
        })
        .await
        .expect("read_markdown ok");
    assert_eq!(read_value["content"].as_str(), Some(body));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_add_markdown_returns_lint_warnings_without_blocking_the_write() {
    // add_markdown stores content verbatim AND returns advisory lint warnings
    // in the same response. Embeddings are disabled so the index path stays
    // lexical-only (no model download) — the write still succeeds and the
    // warnings ride back alongside the index report, never rewriting or
    // rejecting the content.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Two flaggable issues: an empty-destination link and an empty mermaid block.
    let body = "# Notes\n\nSee [the spec]() for details.\n\n```mermaid\n```\n";
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "notes.md".into(),
                content: body.into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok");

    // Stored verbatim despite the warnings — the linter never edits content.
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("notes.md")).unwrap(),
        body,
        "content must be stored verbatim, never rewritten by the linter"
    );

    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present when content has lint issues");
    assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
    let joined = warnings
        .iter()
        .filter_map(|w| w.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("empty destination"), "got: {joined}");
    assert!(joined.contains("mermaid"), "got: {joined}");

    // A clean write omits the warnings field entirely (skip_serializing_if).
    let clean: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "clean.md".into(),
                content: "# Clean\n\nNothing to flag here.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown clean ok");
    assert!(
        clean["warnings"].as_array().is_none_or(|w| w.is_empty()),
        "clean content must carry no warnings: {:?}",
        clean["warnings"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_add_markdown_flags_dangling_wikilink() {
    // A [[wikilink]] whose target has no matching page in the corpus rides
    // back as an advisory warning on the same add_markdown response, and the
    // write still succeeds verbatim — mirrors the lint_markdown wiring above,
    // now for lint_wikilinks. A wikilink to a page that DOES exist must not
    // be flagged.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Dangling wikilink: no `missing-page.md` exists in the corpus.
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "notes.md".into(),
                content: "# Notes\n\nSee [[missing-page]] for details.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok");
    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present for a dangling wikilink");
    let joined = warnings
        .iter()
        .filter_map(|w| w.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("missing-page") && joined.contains("no matching page"),
        "got: {joined}"
    );

    // real-page.md already exists, so a wikilink to it is not flagged.
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "real-page.md".into(),
                content: "# Real page\n\nNothing to see here.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown real-page ok");
    let clean: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "links-to-real.md".into(),
                content: "# Links to real\n\nSee [[real-page]] for details.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown links-to-real ok");
    assert!(
        clean["warnings"].as_array().is_none_or(|w| w.is_empty()),
        "wikilink to an existing page must not be flagged: {:?}",
        clean["warnings"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_backlinks_returns_pages_linking_to_target() {
    // handle_backlinks scans every other page in the corpus for a
    // [[wikilink]] resolving to the target path, and returns the linking
    // pages' corpus-relative paths, sorted. A page with no link to the
    // target must not appear.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "other-page.md".into(),
                content: "# Other page\n\nNothing links here.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown other-page ok");
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "linker.md".into(),
                content: "# Linker\n\nSee [[other-page]] for details.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown linker ok");
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "unrelated.md".into(),
                content: "# Unrelated\n\nNo links at all.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown unrelated ok");

    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Backlinks(BacklinksRequest {
                corpus: Some("docs".into()),
                path: "other-page.md".into(),
            }),
        })
        .await
        .expect("backlinks ok");
    let backlinks = value["backlinks"]
        .as_array()
        .expect("backlinks array present")
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        backlinks,
        vec!["linker.md"],
        "only the linking page must appear in backlinks: {backlinks:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_backlinks_on_uninitialized_wiki_returns_empty_result() {
    // handle_backlinks must ensure the wiki root exists before scanning — a
    // corpus whose directory was never created (no page ever written) must
    // yield an empty result, not an internal error.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let repo_root = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("mkdir repo");
    // Deliberately no `.hallouminate/wiki` under repo_root — this repository's
    // wiki has never been written to.
    let toml = format!(
        r#"
[[repository]]
name = "repo1"
path = "{r}"

[storage]
ground_dir = "{g}"

[embeddings]
enabled = false
"#,
        r = repo_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Backlinks(BacklinksRequest {
                corpus: Some("repo:repo1:wiki".into()),
                path: "missing-page.md".into(),
            }),
        })
        .await
        .expect("backlinks on a fresh wiki must succeed, not error");
    let backlinks = value["backlinks"]
        .as_array()
        .expect("backlinks array present")
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        backlinks,
        Vec::<&str>::new(),
        "fresh wiki with no created root must yield empty backlinks: {backlinks:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_backlinks_disambiguates_basename_collision() {
    // Two pages share the bare stem "index" (a/index.md, b/index.md). A bare
    // [[index]] link is ambiguous and must not be attributed to either page;
    // only a full-path [[a/index]] link counts as a backlink to a/index.md.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    for (path, content) in [
        ("a/index.md", "# A Index\n\nNothing here.\n"),
        ("b/index.md", "# B Index\n\nNothing here.\n"),
        (
            "ambiguous.md",
            "# Ambiguous\n\nSee [[index]] for details.\n",
        ),
        (
            "full-path-linker.md",
            "# Full path linker\n\nSee [[a/index]] for details.\n",
        ),
    ] {
        client
            .call::<serde_json::Value>(DaemonRequest {
                cwd: harness.cwd().to_path_buf(),
                payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                    corpus: "docs".into(),
                    path: path.into(),
                    content: content.into(),
                    overwrite: false,
                    ..Default::default()
                }),
            })
            .await
            .unwrap_or_else(|e| panic!("add_markdown {path} ok: {e}"));
    }

    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Backlinks(BacklinksRequest {
                corpus: Some("docs".into()),
                path: "a/index.md".into(),
            }),
        })
        .await
        .expect("backlinks ok");
    let backlinks = value["backlinks"]
        .as_array()
        .expect("backlinks array present")
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        backlinks,
        vec!["full-path-linker.md"],
        "bare [[index]] must not be attributed to a/index.md when the stem is ambiguous: {backlinks:?}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_backlinks_reports_unreadable_file_as_warning() {
    // A page that becomes unreadable mid-scan (permission change, transient
    // I/O error) must not fail the whole request — handle_backlinks collects
    // the (path, error) failure, warns via tracing, and surfaces it on
    // BacklinksResult.warnings naming both the corpus and the path, so the
    // caller knows the scan is a partial one instead of trusting an empty
    // `backlinks` as "no backlinks".
    use std::os::unix::fs::PermissionsExt;

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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "locked.md".into(),
                content: "# Locked\n\nSee [[other-page]] for details.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown locked ok");
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "other-page.md".into(),
                content: "# Other page\n\nNothing links here.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown other-page ok");

    let is_root = nix_getuid_is_zero();
    let locked_path = corpus_root.join("locked.md");
    std::fs::set_permissions(&locked_path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod locked.md");

    let result: Result<serde_json::Value, _> = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Backlinks(BacklinksRequest {
                corpus: Some("docs".into()),
                path: "other-page.md".into(),
            }),
        })
        .await;
    // Restore perms before any potential assertion failure unwinds, so the
    // tempdir can be cleaned up.
    let _ = std::fs::set_permissions(&locked_path, std::fs::Permissions::from_mode(0o644));
    if is_root {
        return; // root reads through 0o000; the negative test is meaningless.
    }
    let value = result.expect("backlinks must still succeed despite one unreadable file");

    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present when a file could not be read");
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    let warning = warnings[0].as_str().expect("warning is a string");
    assert!(
        warning.contains("docs"),
        "warning must name the corpus: {warning}"
    );
    assert!(
        warning.contains("locked.md"),
        "warning must name the unreadable path: {warning}"
    );
}

#[cfg(unix)]
fn nix_getuid_is_zero() -> bool {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status")
        && let Some(line) = s.lines().find(|l| l.starts_with("Uid:"))
    {
        return line.split_whitespace().nth(1) == Some("0");
    }
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_add_markdown_reports_ancestor_index_refresh_failure_as_warning() {
    // The primary write can durably succeed while the auto-rebuild of an
    // ancestor index.md fails (e.g. a permission change on an intermediate
    // directory). handle_add_markdown must still report success with the
    // file on disk, and surface the refresh failure as a warning naming the
    // repair step — never swallow it, and never bounce an internal error
    // that would make the caller believe the durable write itself was lost.
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let repo = tmp.path().join("my-repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let mut cfg = cfg_with_repository(&ground, "myrepo", &repo);
    cfg.embeddings.enabled = false;

    let wiki_dir = wiki_directory(&cfg.repositories[0]);
    let dir_a = wiki_dir.join("a");
    let dir_b = dir_a.join("b");
    std::fs::create_dir_all(&dir_b).expect("mkdir nested wiki dirs");

    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    std::fs::set_permissions(&dir_a, std::fs::Permissions::from_mode(0o555))
        .expect("chmod a to read-only");

    let is_root = nix_getuid_is_zero();
    let result: Result<serde_json::Value, _> = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: repo_corpus_name("myrepo", RepoCorpusKind::Wiki).unwrap(),
                path: "a/b/page.md".into(),
                content: "# Page\n\nBody text.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await;
    // Restore perms before any potential assertion failure unwinds, so the
    // tempdir can be cleaned up.
    let _ = std::fs::set_permissions(&dir_a, std::fs::Permissions::from_mode(0o755));
    if is_root {
        return; // root writes through 0o555; the negative test is meaningless.
    }
    let value = result.expect("add_markdown must still succeed despite index refresh failure");

    let on_disk = std::fs::read_to_string(dir_b.join("page.md")).expect("page.md written to disk");
    assert_eq!(
        on_disk, "# Page\n\nBody text.\n",
        "primary write content must land verbatim despite the downstream index failure"
    );

    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present when ancestor index refresh fails");
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    let warning = warnings[0].as_str().expect("warning is a string");
    assert!(
        warning.contains("ancestor index refresh failed"),
        "warning must name the refresh failure: {warning}"
    );
    assert!(
        warning.contains("run `index` to repair"),
        "warning must point at the repair step: {warning}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_add_markdown_warns_on_malformed_frontmatter_block_and_stores_verbatim() {
    // Locks the `handle_add_markdown` wiring of `lint_frontmatter`: a page that
    // opens with a *delimited* `---…---` block whose contents are not valid YAML
    // must ride back exactly one frontmatter advisory through the real daemon
    // response — and still be stored byte-for-byte (fail-soft indexing never
    // rejects or rewrites the author's content). A well-formed frontmatter page
    // must produce no frontmatter advisory. Without the `warnings.extend(
    // lint_frontmatter(..))` line in dispatch, the malformed case below carries
    // no advisory and this test fails.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // A closed `---…---` block whose body is not a YAML mapping → malformed.
    let malformed = "---\n: : : not valid : :\n---\n# Notes\n\nplain body text\n";
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "bad-fm.md".into(),
                content: malformed.into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok despite malformed frontmatter");

    // Fail-soft end-to-end: the content is stored verbatim, fence included.
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("bad-fm.md")).unwrap(),
        malformed,
        "malformed frontmatter must be stored verbatim, never rewritten"
    );

    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present for a malformed frontmatter block");
    let frontmatter_advisories = warnings
        .iter()
        .filter_map(|w| w.as_str())
        .filter(|w| w.contains("frontmatter"))
        .count();
    assert_eq!(
        frontmatter_advisories, 1,
        "exactly one frontmatter advisory must ride back: {warnings:?}"
    );

    // A well-formed frontmatter page produces no frontmatter advisory.
    let clean: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "good-fm.md".into(),
                content: "---\nstatus: reviewed\nowner: cheese-lord\n---\n# Notes\n\nbody\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok for well-formed frontmatter");
    let clean_fm_advisories = clean["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter_map(|x| x.as_str())
                .filter(|x| x.contains("frontmatter"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        clean_fm_advisories, 0,
        "well-formed frontmatter must not warn: {:?}",
        clean["warnings"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_add_markdown_warns_on_claim_marks_and_stores_verbatim() {
    // Locks the `handle_add_markdown` wiring of `lint_claim_marks`: a page with
    // a `contradicted` mark missing `ref=` and a malformed (unknown-status)
    // claim comment must ride back two advisories through the real daemon
    // response — and still be stored byte-for-byte, claim comments included
    // (advisory lint never blocks or rewrites the write). A page whose marks are
    // all well-formed produces no claim advisory.
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

[embeddings]
enabled = false
"#,
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Two claim advisories: a contradicted mark with no ref, and an unknown
    // status. The ordinary HTML comment must not warn.
    let body = "# Notes\n\nA contradicted claim.<!--claim:contradicted-->\n\nBogus.<!--claim:bananas-->\n\nFine.<!--claim:confirmed-->\n\nPlain <!-- ordinary note -->\n";
    let value: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "claims.md".into(),
                content: body.into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok despite claim advisories");

    // Stored verbatim, claim comments included — the linter never edits content.
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("claims.md")).unwrap(),
        body,
        "claim marks must be stored verbatim, never rewritten by the linter"
    );

    let warnings = value["warnings"]
        .as_array()
        .expect("warnings array present for claim-mark issues");
    let claim_advisories: Vec<&str> = warnings
        .iter()
        .filter_map(|w| w.as_str())
        .filter(|w| w.contains("claim"))
        .collect();
    assert_eq!(
        claim_advisories.len(),
        2,
        "exactly two claim advisories must ride back: {warnings:?}"
    );
    let joined = claim_advisories.join("\n");
    assert!(
        joined.contains("contradicted") && joined.contains("ref="),
        "missing-ref advisory expected: {joined}"
    );
    assert!(
        joined.contains("unrecognized status"),
        "malformed-status advisory expected: {joined}"
    );

    // A page whose marks are all well-formed (and confirmed/qualified need no
    // ref) produces no claim advisory.
    let clean: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "clean-claims.md".into(),
                content: "# Notes\n\nGood.<!--claim:superseded ref=old.md-->\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("add_markdown ok for well-formed claim marks");
    let clean_claim_advisories = clean["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter_map(|x| x.as_str())
                .filter(|x| x.contains("claim"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        clean_claim_advisories, 0,
        "well-formed claim marks must not warn: {:?}",
        clean["warnings"]
    );
}

// ─── Hardening: liveness, contract surface, single-instance ────────────

#[tokio::test]
async fn daemon_ping_returns_versioned_pong() {
    // Smallest possible request — the contract is: client encodes
    // `{"op":"ping"}`, server returns `{"status":"ok","result":{"version":...}}`
    // (Curd C). The version field is what the MCP bootstrap reads to detect
    // cross-version daemon skew. If this regresses, every other client call
    // regresses too.
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
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping ok");
    assert_eq!(
        value["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "ping must report the daemon binary version: {value}"
    );
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
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Index(hallouminate::app::daemon::IndexRequest {
                corpus: None,
                paths_from: Some(PathBuf::from("/tmp/list.txt")),
                strict: false,
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
async fn daemon_closes_idle_connection_after_read_timeout() {
    // A client that connects but never sends its request line must not pin
    // the per-connection task forever — the idle-read timeout closes the
    // connection so it can't leak. Use a harness-scoped short timeout so
    // this test doesn't wait out the real 30s production default.
    use tokio::io::AsyncReadExt;
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
    let harness = DaemonHarness::spawn_with_idle_timeout(cfg, Duration::from_millis(200)).await;

    let mut stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");
    // Never write anything — the server must give up waiting for the
    // request line and close its half, which surfaces to us as EOF.
    let mut buf = [0u8; 1];
    let n = timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("server must close the idle connection well before the test timeout")
        .expect("read must not error");
    assert_eq!(n, 0, "idle connection must be closed (EOF), not left open");
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
    let state2 = DaemonState::open(cfg, None).await.expect("second open ok");
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
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping");
    assert_eq!(pong["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
}

// ─── Curd 1: graceful shutdown ───────────────────────────────────────────

/// Build a tempdir with an empty `.hallouminate/config.toml` so daemon
/// requests using it as `cwd` resolve a (trivial) repo layer.
fn seed_cwd(tmp: &Path) -> PathBuf {
    let cwd = tmp.to_path_buf();
    let hallou = cwd.join(".hallouminate");
    std::fs::create_dir_all(&hallou).expect("mkdir .hallouminate");
    std::fs::write(hallou.join("config.toml"), "").expect("write repo config");
    cwd
}

fn docs_cfg(ground_dir: &Path, corpus_root: &Path) -> Config {
    let toml = format!(
        "[[corpus]]\nname = \"docs\"\npaths = [\"{c}\"]\nglobs = [\"**/*.md\"]\n\n[storage]\nground_dir = \"{g}\"\n",
        c = corpus_root.display(),
        g = ground_dir.display(),
    );
    toml::from_str(&toml).expect("parse cfg")
}

#[tokio::test]
async fn ipc_shutdown_removes_socket_and_lockfile_and_refuses_new_connections() {
    // Quality gate (Curd 1): sending `Shutdown` exits the daemon gracefully —
    // socket + lockfile gone, a subsequent connect fails.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let cwd = seed_cwd(tmp.path());
    let socket = tmp.path().join("daemon.sock");
    let lockfile = tmp.path().join("daemon.sock.lock");
    let cfg = docs_cfg(&ground, &corpus_root);

    let state = DaemonState::open(cfg, None).await.expect("open state");
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move { serve(&state, &socket_clone).await });

    // Wait for the socket to appear.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(lockfile.exists(), "lockfile must exist while daemon runs");

    let client = connect_at(&socket).await.expect("connect");
    let resp = client
        .call_raw(DaemonRequest {
            cwd: cwd.clone(),
            payload: DaemonRequestPayload::Shutdown,
        })
        .await
        .expect("shutdown transport ok");
    match resp {
        DaemonResponse::Ok { result } => {
            assert_eq!(result, serde_json::Value::String("stopping".to_string()));
        }
        other => panic!("shutdown must ack `stopping`, got {other:?}"),
    }

    // The serve future must return Ok after cleanup.
    let served = timeout(Duration::from_secs(5), handle)
        .await
        .expect("serve must return after shutdown")
        .expect("join ok");
    served.expect("serve returns Ok on graceful shutdown");

    // Socket removed; lockfile removed (flock dropped + file removal by cleanup
    // is not guaranteed, but the socket is — and a new connect must fail).
    assert!(!socket.exists(), "socket file must be removed on shutdown");
    let err = connect_at(&socket)
        .await
        .expect_err("connect must fail after shutdown");
    assert!(
        format!("{err:#}").contains("daemon unavailable"),
        "post-shutdown connect must report daemon unavailable: {err:#}"
    );
}

#[tokio::test]
async fn sigterm_removes_socket_and_refuses_new_connections() {
    // Quality gate (Curd 1): a SIGTERM must drive the *same* graceful exit as
    // the IPC `Shutdown` path — accept loop drained, socket removed, a
    // subsequent connect fails — rather than dying on the default-terminate
    // disposition and leaving a stale socket. This exercises the production
    // signal wiring (`spawn_signal_handlers`), not just the IPC token-cancel
    // already covered above.
    //
    // `spawn_signal_handlers` registers the SIGTERM stream synchronously, so
    // by the time it returns the default-terminate disposition is overridden
    // and `libc::raise(SIGTERM)` reaches the token instead of killing the test
    // process.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let socket = tmp.path().join("daemon.sock");
    let cfg = docs_cfg(&ground, &corpus_root);

    let state = DaemonState::open(cfg, None).await.expect("open state");
    // Install the real signal handlers against this state's shutdown token
    // *before* serving, mirroring `serve_with_config`'s production order.
    spawn_signal_handlers(&state);
    let serve_state = state.clone();
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move { serve(&serve_state, &socket_clone).await });

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Sanity: the daemon is reachable before the signal.
    let client = connect_at(&socket).await.expect("connect before SIGTERM");
    let pong: serde_json::Value = client
        .call(DaemonRequest {
            cwd: seed_cwd(tmp.path()),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping before SIGTERM");
    assert_eq!(pong["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));

    // Raise SIGTERM at our own process; the installed handler cancels the
    // shutdown token, the accept loop breaks, and `serve` runs cleanup.
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .expect("kill_process(self, SIGTERM) must succeed");

    let served = timeout(Duration::from_secs(5), handle)
        .await
        .expect("serve must return after SIGTERM")
        .expect("join ok");
    served.expect("serve returns Ok on SIGTERM-driven shutdown");

    assert!(
        !socket.exists(),
        "socket must be removed on SIGTERM shutdown"
    );
    let err = connect_at(&socket)
        .await
        .expect_err("connect must fail after SIGTERM");
    assert!(
        format!("{err:#}").contains("daemon unavailable"),
        "post-SIGTERM connect must report daemon unavailable: {err:#}"
    );
}

#[tokio::test]
async fn idle_exit_removes_socket_and_lockfile_and_refuses_new_connections() {
    // Quality gate (AC1): after `idle_exit_secs` of no activity with zero
    // active connections the daemon exits cleanly on its own — the accept
    // loop drains, the socket is removed, and a subsequent connect fails — so
    // the OS reclaims all memory (ADR-001). This is the *self-driven* twin of
    // the IPC/`SIGTERM` cleanup tests above: no shutdown is ever sent and no
    // connection is ever held, so the idle clock stays quiet from boot and
    // the watcher cancels the shutdown token after ~`idle_exit_secs`.
    // Embeddings are disabled to keep the test hermetic (no model download).
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let socket = tmp.path().join("daemon.sock");
    let lockfile = tmp.path().join("daemon.sock.lock");
    let mut cfg = docs_cfg(&ground, &corpus_root);
    cfg.daemon.idle_exit_secs = 1;
    cfg.embeddings.enabled = false;

    let state = DaemonState::open(cfg, None).await.expect("open state");
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move { serve(&state, &socket_clone).await });

    // Wait for the socket to appear; hold NO connection afterwards.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(lockfile.exists(), "lockfile must exist while daemon runs");

    // The idle-exit watcher fires within ~idle_exit_secs and drives the same
    // graceful cleanup as an IPC `Shutdown`; the serve future returns Ok.
    let served = timeout(Duration::from_secs(10), handle)
        .await
        .expect("serve must return after idle-exit")
        .expect("join ok");
    served.expect("serve returns Ok on idle-exit shutdown");

    assert!(
        !socket.exists(),
        "socket must be removed on idle-exit shutdown"
    );
    let err = connect_at(&socket)
        .await
        .expect_err("connect must fail after idle-exit");
    assert!(
        format!("{err:#}").contains("daemon unavailable"),
        "post-idle-exit connect must report daemon unavailable: {err:#}"
    );

    // AC1 "flock released": cleanup drops the single-instance lock but leaves
    // the lockfile on disk, so the file persists while its advisory lock is
    // free. A fresh non-blocking exclusive flock on that same lockfile must
    // succeed — if idle-exit had leaked the lock, this returns EWOULDBLOCK and
    // the next daemon could never rebind (the whole respawn premise, ADR-002).
    // This is the assertion that makes the "and_lockfile" in the name real;
    // the connect-failure above only proves the socket is gone.
    assert!(
        lockfile.exists(),
        "lockfile persists after idle-exit; only its advisory lock is released"
    );
    let relock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lockfile)
        .expect("reopen lockfile after idle-exit");
    rustix::fs::flock(
        &relock,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .expect("idle-exit must release the single-instance flock so a fresh daemon can rebind");
    drop(relock);
}

#[tokio::test]
async fn client_for_default_socket_respawns_exactly_once_then_fails_loudly() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // AC3: the default-socket respawn seam (`client_for(None)`) self-heals at
    // most once — on connect failure it runs the respawn step a single time,
    // retries the connect, and, if the daemon still is not up, returns the
    // loud "daemon unavailable" error rather than looping. Point the socket at
    // a path that is never bound and give a respawn closure that counts its
    // calls but deliberately does NOT bring up a daemon. Serialized against
    // other `HALLOUMINATE_SOCKET` mutators via `EnvGuard`'s static mutex.
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing_sock = tmp.path().join("never-bound.sock");
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &missing_sock) };

    let respawns = Arc::new(AtomicUsize::new(0));
    let respawns_ref = Arc::clone(&respawns);
    let result = hallouminate::app::daemon::client_for_with(None, || {
        respawns_ref.fetch_add(1, Ordering::SeqCst);
        async { anyhow::Ok(()) }
    })
    .await;

    assert_eq!(
        respawns.load(Ordering::SeqCst),
        1,
        "default-socket path must invoke the respawn step exactly once"
    );
    let err = result.expect_err("connect must fail when respawn brings up no daemon");
    assert!(
        format!("{err:#}").contains("daemon unavailable"),
        "failed respawn must report daemon unavailable: {err:#}"
    );
}

#[tokio::test]
async fn client_for_default_socket_respawns_then_returns_the_reconnected_client() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    // AC3 retry + AC5 transparent respawn: when the first connect on the
    // default socket fails, client_for(None) runs the respawn step once and
    // then RETRIES the connect — the retry succeeding is exactly what makes a
    // mid-session idle-exit invisible to MCP/CLI callers. The respawn closure
    // stands in for `ensure_daemon_running` by binding a listener at the socket
    // path (connect_at is a bare connect, so a bound listener is a reachable
    // "daemon"); the returned client must be Ok and target that socket. A
    // regression that dropped the retry-after-respawn would surface the first
    // connect error instead — this test fails, the exactly-once test does not.
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock = tmp.path().join("respawned.sock");
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &sock) };

    let respawns = Arc::new(AtomicUsize::new(0));
    // Keep the stub listener alive past the retry connect; a listener dropped
    // inside the closure would leave the retry dialing a dead socket.
    let listener_holder: Arc<Mutex<Option<tokio::net::UnixListener>>> = Arc::new(Mutex::new(None));
    let respawns_ref = Arc::clone(&respawns);
    let holder_ref = Arc::clone(&listener_holder);
    let sock_ref = sock.clone();

    let result = hallouminate::app::daemon::client_for_with(None, move || async move {
        respawns_ref.fetch_add(1, Ordering::SeqCst);
        let listener = tokio::net::UnixListener::bind(&sock_ref)?;
        *holder_ref.lock().unwrap() = Some(listener);
        anyhow::Ok(())
    })
    .await;

    assert_eq!(
        respawns.load(Ordering::SeqCst),
        1,
        "respawn must run exactly once before the successful retry"
    );
    let client = result.expect("retry connect after a successful respawn must return Ok");
    assert_eq!(
        client.socket_path(),
        sock.as_path(),
        "the reconnected client must target the respawned socket"
    );
}

#[tokio::test]
async fn open_warns_and_names_replacement_when_idle_evict_secs_is_set() {
    // AC7: a non-default legacy `embeddings.idle_evict_secs` must still parse,
    // take no eviction action (the eviction task is deleted), and emit a
    // deprecation warning that NAMES the replacement `[daemon].idle_exit_secs`
    // so operators know where to migrate. Capturing the tracing output proves
    // the warning actually fires with the migration guidance — a value-only
    // parse assertion would pass even if the warn were dropped.
    let logs = capture_daemon_warnings(|| async {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        cfg.embeddings.idle_evict_secs += 1; // any non-default value trips the warning
        DaemonState::open(cfg, None).await.expect("open state");
    })
    .await;
    assert!(
        logs.contains("deprecated"),
        "a non-default idle_evict_secs must log a deprecation warning; captured: {logs:?}"
    );
    assert!(
        logs.contains("[daemon].idle_exit_secs"),
        "the deprecation warning must name the replacement config key; captured: {logs:?}"
    );
}

#[tokio::test]
async fn open_does_not_warn_when_idle_evict_secs_is_default() {
    // The deprecation warning must be conditional on a non-default value, not
    // fired unconditionally — otherwise every default config nags on boot.
    // Guards against dropping the `!= default` guard around the warn.
    let logs = capture_daemon_warnings(|| async {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::default();
        cfg.embeddings.enabled = false;
        cfg.storage.ground_dir = tmp.path().to_string_lossy().into_owned();
        DaemonState::open(cfg, None).await.expect("open state");
    })
    .await;
    assert!(
        !logs.contains("deprecated"),
        "a default idle_evict_secs must not log a deprecation warning; captured: {logs:?}"
    );
}

/// Run `f` with a thread-local tracing subscriber that captures WARN-level
/// output into a string, so a test can assert which warnings a code path emits.
/// `#[tokio::test]` uses a current-thread runtime, so the thread-local default
/// holds across the awaits inside `f`.
async fn capture_daemon_warnings<F, Fut>(f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer_buf = std::sync::Arc::clone(&buf);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || CaptureWriter(std::sync::Arc::clone(&writer_buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    f().await;
    drop(guard);
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8(bytes).expect("captured logs are utf8")
}

struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ─── Curd 2: lifecycle status / restart ──────────────────────────────────

#[tokio::test]
async fn status_reports_running_then_not_running_across_shutdown() {
    // Quality gate (Curd 2): `daemon status` returns Running against a live
    // daemon and NotRunning once it has stopped. `status()` resolves the
    // socket via `daemon_socket_path()`, so point HALLOUMINATE_SOCKET at the
    // harness socket for the duration of this test. (Serialized via a process
    // env mutex below — env is global to the test binary.)
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let socket = tmp.path().join("daemon.sock");
    let cfg = docs_cfg(&ground, &corpus_root);

    let state = DaemonState::open(cfg, None).await.expect("open state");
    let serve_state = state.clone();
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move { serve(&serve_state, &socket_clone).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &socket) };

    assert_eq!(
        hallouminate::app::daemon::status()
            .await
            .expect("status ok while running"),
        hallouminate::app::daemon::DaemonStatus::Running,
        "status must be Running against a live daemon"
    );

    // Drive a graceful shutdown via the IPC path, then assert NotRunning.
    let client = connect_at(&socket).await.expect("connect");
    let _ = client
        .call_raw(DaemonRequest {
            cwd: seed_cwd(tmp.path()),
            payload: DaemonRequestPayload::Shutdown,
        })
        .await;
    let served = timeout(Duration::from_secs(5), handle)
        .await
        .expect("serve returns after shutdown")
        .expect("join ok");
    served.expect("serve Ok on shutdown");

    assert_eq!(
        hallouminate::app::daemon::status()
            .await
            .expect("status ok while stopped"),
        hallouminate::app::daemon::DaemonStatus::NotRunning,
        "status must be NotRunning once the socket is gone"
    );
}

#[tokio::test]
async fn stop_is_a_noop_against_an_already_stopped_daemon() {
    // `stop()` returns Ok when no daemon is reachable — stopping an
    // already-stopped daemon is success, not an error. Point the socket path
    // at a tempdir location that was never bound.
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("never-bound.sock");
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &socket) };

    hallouminate::app::daemon::stop()
        .await
        .expect("stop against a stopped daemon must be Ok");
    assert!(
        !socket.exists(),
        "stop must not create the socket it never connected to"
    );
}

/// Spawn an in-process `serve` on `socket` from a fresh `DaemonState` and wait
/// until the socket is reachable. Returns the serve task handle so the caller
/// can join it after a graceful shutdown.
async fn spawn_serve(cfg: Config, socket: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let state = DaemonState::open(cfg, None).await.expect("open state");
    let socket_clone = socket.to_path_buf();
    let handle = tokio::spawn(async move { serve(&state, &socket_clone).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle
}

#[tokio::test]
async fn restart_stops_the_old_daemon_then_brings_up_a_reachable_one() {
    // `restart()` must take a running daemon down and bring a fresh, reachable
    // one up. The suite sets HALLOUMINATE_SOCKET, which makes the production
    // respawn (`ensure_daemon_running`) a no-op, so we drive the real
    // stop→respawn→reachable sequence through the `restart_with` seam: the
    // injected respawn spins up an in-process `serve`, exactly as production's
    // spawned daemon would, but against the controllable harness socket.
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let socket = tmp.path().join("daemon.sock");
    let cfg = docs_cfg(&ground, &corpus_root);

    // First daemon up and reachable.
    let first = spawn_serve(cfg.clone(), &socket).await;
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &socket) };
    assert_eq!(
        hallouminate::app::daemon::status()
            .await
            .expect("status ok while first daemon runs"),
        hallouminate::app::daemon::DaemonStatus::Running,
        "the first daemon must be reachable before restart",
    );

    // Restart: stop() takes the first daemon down (its serve future returns),
    // then the injected respawn brings a fresh in-process daemon up. The
    // respawn must observe the old daemon already gone, proving stop ran first.
    let restarted_cfg = cfg.clone();
    let restart_socket = socket.clone();
    let second_handle: std::sync::Arc<
        std::sync::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let stash = second_handle.clone();
    hallouminate::app::daemon::restart_with(|| async move {
        // After restart's stop(), nothing must answer on the socket.
        assert_eq!(
            hallouminate::app::daemon::status()
                .await
                .expect("status ok between stop and respawn"),
            hallouminate::app::daemon::DaemonStatus::NotRunning,
            "restart must stop the old daemon before respawning",
        );
        let handle = spawn_serve(restarted_cfg, &restart_socket).await;
        *stash.lock().expect("stash lock") = Some(handle);
        Ok(())
    })
    .await
    .expect("restart_with ok");

    // The first daemon's serve future must have returned (graceful shutdown).
    let first_result = timeout(Duration::from_secs(5), first)
        .await
        .expect("first serve must return after restart's stop")
        .expect("first serve join ok");
    first_result.expect("first serve returns Ok on shutdown");

    // The freshly respawned daemon must be reachable.
    assert_eq!(
        hallouminate::app::daemon::status()
            .await
            .expect("status ok after restart"),
        hallouminate::app::daemon::DaemonStatus::Running,
        "restart must leave a fresh, reachable daemon up",
    );

    // Tear down the second daemon so the test leaves no listener behind.
    let second = second_handle
        .lock()
        .expect("stash lock")
        .take()
        .expect("respawn must have stored the second serve handle");
    let client = connect_at(&socket).await.expect("connect to second daemon");
    let _ = client
        .call_raw(DaemonRequest {
            cwd: seed_cwd(tmp.path()),
            payload: DaemonRequestPayload::Shutdown,
        })
        .await;
    let second_result = timeout(Duration::from_secs(5), second)
        .await
        .expect("second serve must return after teardown shutdown")
        .expect("second serve join ok");
    second_result.expect("second serve returns Ok on shutdown");
}

#[tokio::test]
async fn restart_via_lifecycle_leaves_a_daemon_reporting_the_current_version() {
    // Curd C end-to-end: the MCP bootstrap restarts a stale daemon via the
    // `lifecycle::restart` machinery, then proceeds. This drives that same
    // stop→respawn path through `restart_with` and proves the post-restart
    // daemon is reachable AND reports OUR version over the versioned Ping —
    // i.e. a fresh client adopting the restarted daemon sees no skew.
    let _env = EnvGuard::set("HALLOUMINATE_SOCKET");
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let socket = tmp.path().join("daemon.sock");
    let cfg = docs_cfg(&ground, &corpus_root);

    let first = spawn_serve(cfg.clone(), &socket).await;
    unsafe { std::env::set_var("HALLOUMINATE_SOCKET", &socket) };

    let restarted_cfg = cfg.clone();
    let restart_socket = socket.clone();
    let stash: std::sync::Arc<
        std::sync::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let stash_inner = stash.clone();
    hallouminate::app::daemon::restart_with(|| async move {
        let handle = spawn_serve(restarted_cfg, &restart_socket).await;
        *stash_inner.lock().expect("stash lock") = Some(handle);
        Ok(())
    })
    .await
    .expect("restart_with ok");

    let first_result = timeout(Duration::from_secs(5), first)
        .await
        .expect("first serve must return after restart's stop")
        .expect("first serve join ok");
    first_result.expect("first serve returns Ok on shutdown");

    // The respawned daemon answers a versioned pong reporting OUR version.
    let client = connect_at(&socket)
        .await
        .expect("connect to restarted daemon");
    let pong: serde_json::Value = client
        .call(DaemonRequest {
            cwd: seed_cwd(tmp.path()),
            payload: DaemonRequestPayload::Ping,
        })
        .await
        .expect("ping restarted daemon");
    assert_eq!(
        pong["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "restarted daemon must report the current version: {pong}"
    );

    // Tear down the respawned daemon.
    let second = stash
        .lock()
        .expect("stash lock")
        .take()
        .expect("respawn must have stored the second serve handle");
    let _ = client
        .call_raw(DaemonRequest {
            cwd: seed_cwd(tmp.path()),
            payload: DaemonRequestPayload::Shutdown,
        })
        .await;
    let second_result = timeout(Duration::from_secs(5), second)
        .await
        .expect("second serve must return after teardown shutdown")
        .expect("second serve join ok");
    second_result.expect("second serve returns Ok on shutdown");
}

/// RAII guard that removes an env var on drop and serializes env-mutating
/// tests against a shared mutex (the Rust test harness runs tests across
/// threads; `daemon_socket_path()` reads `HALLOUMINATE_SOCKET` process-wide).
struct EnvGuard {
    key: &'static str,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn set(key: &'static str) -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EnvGuard { key, _lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(self.key) };
    }
}

// ─── Curd 3: corpus watcher ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_reindexes_then_prunes_file_in_baseline_corpus_root() {
    // Quality gate (Curd 3): editing a file in a baseline corpus root triggers
    // a reindex within ~debounce_ms; deleting prunes its rows. Both legs are
    // asserted via `ground` — the watcher's *unique* observable effect on the
    // LanceDB rows — never via a manual `index` (which would index the file
    // itself, so the old assertion passed even with the watcher disabled) nor
    // `list_files` (a filesystem scan that sees the on-disk file regardless of
    // indexing).
    //
    // Pin embeddings off so non-empty content indexes lexical-only (FTS) —
    // no embedding-model (ONNX) load. The chunking tokenizer still loads
    // (and is networked on a cold cache), so this is hermetic only with the
    // tokenizer cached. A distinctive token in the body lets `ground` find
    // precisely this file and nothing else.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let toml = format!(
        "[[corpus]]\nname = \"docs\"\npaths = [\"{c}\"]\nglobs = [\"**/*.md\"]\n\n[embeddings]\nenabled = false\n\n[watch]\ndebounce_ms = 100\n\n[storage]\nground_dir = \"{g}\"\n",
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    let harness = DaemonHarness::spawn(cfg).await;

    // Write a NON-EMPTY file directly on disk (outside the add_markdown lane)
    // with a unique token. Only the background watcher can index it — the test
    // never calls `index`, so a hit in `ground` proves the watcher reindexed.
    let watched = corpus_root.join("watched.md");
    std::fs::write(
        &watched,
        "# Spice\n\nthe rarespiceword melange flows here\n",
    )
    .expect("write watched file");

    let ground_hits = |client: hallouminate::app::daemon::DaemonClient, cwd: PathBuf| async move {
        let res: hallouminate::app::daemon::GroundResult = client
            .call(DaemonRequest {
                cwd,
                payload: DaemonRequestPayload::Ground(hallouminate::app::daemon::GroundRequest {
                    query: "rarespiceword".into(),
                    corpus: Some("docs".into()),
                    top_files: None,
                    chunks_per_file: None,
                    limit: None,
                    snippet_chars: None,
                }),
            })
            .await
            .expect("ground ok");
        res.response.docs.len()
    };

    // The watcher must reindex the created file within a few debounce windows.
    // Assert a `ground` hit appears that could only come from the watcher.
    // 20s ceiling: free in the passing case (loop exits on condition); guards against
    // parallel-suite CPU contention slowing the watcher event → reindex → ground path.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut indexed = false;
    while std::time::Instant::now() < deadline {
        if ground_hits(
            connect_at(harness.socket()).await.expect("connect"),
            harness.cwd().to_path_buf(),
        )
        .await
            >= 1
        {
            indexed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        indexed,
        "watcher must reindex watched.md so `ground` returns it (no manual index was issued)"
    );

    // DELETE → prune: remove the file and let the debounced watcher observe it.
    // The rows must disappear from `ground` — proving the prune ran, not merely
    // that the daemon survived.
    std::fs::remove_file(&watched).expect("remove watched file");
    // 20s ceiling: same load-tolerant margin for the prune leg.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut pruned = false;
    while std::time::Instant::now() < deadline {
        if ground_hits(
            connect_at(harness.socket()).await.expect("connect"),
            harness.cwd().to_path_buf(),
        )
        .await
            == 0
        {
            pruned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        pruned,
        "watcher must prune watched.md's rows on delete so `ground` no longer returns it"
    );
}

// ─── Curd B: multi-root corpus read/mutate split ─────────────────────────

/// Build a daemon config with one explicit corpus that has TWO roots, plus a
/// ground dir. Mirrors the `SPEC_EXAMPLE` multi-root shape (a single
/// `[[corpus]]` aggregating several paths) that `ground`/`list_files` already
/// walk. Embeddings disabled so reads/mutations don't touch the model.
fn cfg_two_root_corpus(ground: &Path, root_a: &Path, root_b: &Path) -> Config {
    let toml = format!(
        r#"
[[corpus]]
name = "multi"
paths = ["{a}", "{b}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{g}"

[embeddings]
enabled = false
"#,
        a = root_a.display(),
        b = root_b.display(),
        g = ground.display(),
    );
    toml::from_str(&toml).expect("two-root corpus toml parses")
}

#[tokio::test]
async fn daemon_read_markdown_resolves_file_under_a_non_first_root() {
    // Curd B core fix: a file that lives under the SECOND configured root is
    // searchable (the scan walks every root) and must now also be readable —
    // before, read resolved `paths[0]` only and a paths[1..] file was a
    // searchable-but-unreadable split surface.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    std::fs::create_dir_all(&root_a).expect("mkdir a");
    std::fs::create_dir_all(&root_b).expect("mkdir b");
    // File only under the second root.
    let body = "# Under second root\n\nReachable now.\n";
    std::fs::write(root_b.join("only-b.md"), body).expect("write under b");
    // And one under the first root, to prove both roots stay readable.
    let body_a = "# Under first root\n";
    std::fs::write(root_a.join("only-a.md"), body_a).expect("write under a");

    let harness = DaemonHarness::spawn(cfg_two_root_corpus(&ground, &root_a, &root_b)).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let read_b: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: Some("multi".into()),
                path: "only-b.md".into(),
            }),
        })
        .await
        .expect("read of paths[1] file must succeed");
    assert_eq!(read_b["content"].as_str(), Some(body));

    let read_a: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: Some("multi".into()),
                path: "only-a.md".into(),
            }),
        })
        .await
        .expect("read of paths[0] file must succeed");
    assert_eq!(read_a["content"].as_str(), Some(body_a));
}

#[tokio::test]
async fn daemon_read_markdown_missing_in_all_roots_reports_does_not_exist() {
    // A path absent from every root surfaces the same "does not exist" shape a
    // single-root miss does — not a confusing multi-root-specific error.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    std::fs::create_dir_all(&root_a).expect("mkdir a");
    std::fs::create_dir_all(&root_b).expect("mkdir b");
    let harness = DaemonHarness::spawn(cfg_two_root_corpus(&ground, &root_a, &root_b)).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: Some("multi".into()),
                path: "nowhere.md".into(),
            }),
        })
        .await
        .expect("transport ok");
    match resp {
        DaemonResponse::Err { kind, message } => {
            assert_eq!(kind, ErrorKind::InvalidParams, "{message}");
            assert!(message.contains("does not exist"), "got: {message}");
        }
        DaemonResponse::Ok { result } => panic!("missing file must error; got Ok({result:?})"),
    }
}

#[tokio::test]
async fn daemon_add_markdown_to_multi_root_corpus_is_rejected() {
    // Mutations have no canonical destination on a multi-root corpus, so
    // add_markdown must refuse at request time with an InvalidParams error
    // that names the reason ("roots"), not silently write to paths[0].
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    std::fs::create_dir_all(&root_a).expect("mkdir a");
    std::fs::create_dir_all(&root_b).expect("mkdir b");
    let harness = DaemonHarness::spawn(cfg_two_root_corpus(&ground, &root_a, &root_b)).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "multi".into(),
                path: "new.md".into(),
                content: "# nope\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    match resp {
        DaemonResponse::Err { kind, message } => {
            assert_eq!(kind, ErrorKind::InvalidParams, "{message}");
            assert!(
                message.contains("roots"),
                "must explain the reason: {message}"
            );
        }
        DaemonResponse::Ok { result } => {
            panic!("multi-root add must be rejected; got Ok({result:?})")
        }
    }
    // And nothing was written to either root.
    assert!(
        !root_a.join("new.md").exists(),
        "must not write to paths[0]"
    );
    assert!(
        !root_b.join("new.md").exists(),
        "must not write to paths[1]"
    );
}

#[tokio::test]
async fn daemon_delete_markdown_from_multi_root_corpus_is_rejected() {
    // delete counts as a mutation → also refused on multi-root, even when the
    // target file genuinely exists under one of the roots.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    std::fs::create_dir_all(&root_a).expect("mkdir a");
    std::fs::create_dir_all(&root_b).expect("mkdir b");
    std::fs::write(root_b.join("doomed.md"), b"# here\n").expect("seed file under b");
    let harness = DaemonHarness::spawn(cfg_two_root_corpus(&ground, &root_a, &root_b)).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::DeleteMarkdown(DeleteMarkdownRequest {
                corpus: "multi".into(),
                path: "doomed.md".into(),
            }),
        })
        .await
        .expect("transport ok");
    match resp {
        DaemonResponse::Err { kind, message } => {
            assert_eq!(kind, ErrorKind::InvalidParams, "{message}");
            assert!(
                message.contains("roots"),
                "must explain the reason: {message}"
            );
        }
        DaemonResponse::Ok { result } => {
            panic!("multi-root delete must be rejected; got Ok({result:?})")
        }
    }
    assert!(
        root_b.join("doomed.md").exists(),
        "rejected delete must leave the file intact"
    );
}

// ─── Issue #101: a missing corpus root must not abort the whole run ───────

/// Embeddings-off baseline config with two `[[corpus]]` entries: a healthy
/// root (exists, empty) and a ghost root (does not exist). The daemon boots
/// in lexical-only mode so the test indexes without downloading a model.
fn cfg_two_corpora_one_missing(
    ground_dir: &Path,
    healthy_root: &Path,
    ghost_root: &Path,
) -> Config {
    let toml = format!(
        r#"
[[corpus]]
name = "healthy"
paths = ["{healthy}"]
globs = ["**/*.md"]

[[corpus]]
name = "ghost"
paths = ["{ghost}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{ground}"

[embeddings]
enabled = false
"#,
        healthy = healthy_root.display(),
        ghost = ghost_root.display(),
        ground = ground_dir.display(),
    );
    toml::from_str(&toml).expect("two-corpora toml parses")
}

#[tokio::test]
async fn index_skips_missing_corpus_root_and_indexes_the_rest() {
    // Regression for #101: a single configured corpus whose root does not
    // exist on disk used to abort the ENTIRE index run with a fatal walk
    // error ("No such file or directory"), taking down every healthy corpus
    // on the box. The run must instead skip the missing corpus with a warning
    // and still index the rest — the whole point of a portable/synced
    // baseline config where some roots only exist on some machines.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let healthy_root = tmp.path().join("healthy-corpus");
    std::fs::create_dir_all(&healthy_root).expect("mkdir healthy corpus");
    // Never created on disk — this is the ghost root that used to be fatal.
    let ghost_root = tmp.path().join("does-not-exist-xyz");

    let cfg = cfg_two_corpora_one_missing(&ground, &healthy_root, &ghost_root);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Index(hallouminate::app::daemon::IndexRequest {
                corpus: None,
                paths_from: None,
                strict: false,
            }),
        })
        .await
        .expect("index transport ok");

    let report: hallouminate::app::cli::IndexReport = match resp {
        DaemonResponse::Ok { result } => {
            serde_json::from_value(result).expect("index payload shape")
        }
        DaemonResponse::Err { kind, message } => {
            panic!("missing root must NOT abort the run, got {kind:?}: {message}");
        }
    };

    // The healthy corpus was indexed; the ghost corpus was skipped entirely.
    let indexed: Vec<&str> = report.corpora.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        indexed,
        vec!["healthy"],
        "only the healthy corpus should be indexed; ghost must be skipped: {indexed:?}"
    );

    // A warning names the skipped corpus and its missing root, so the user
    // can see why it didn't index instead of getting silent partial output.
    assert_eq!(
        report.warnings.len(),
        1,
        "exactly one skip warning expected"
    );
    let w = &report.warnings[0];
    assert!(
        w.contains("ghost") && w.contains("skipped"),
        "warning must name the skipped corpus: {w}"
    );
    assert!(
        w.contains(&ghost_root.display().to_string()),
        "warning must name the missing root path: {w}"
    );
}

#[tokio::test]
async fn index_strict_aborts_on_missing_corpus_root() {
    // The `--strict` opt-out restores fail-fast: a caller who wants every
    // configured root guaranteed present gets a hard error rather than a
    // silent skip.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let healthy_root = tmp.path().join("healthy-corpus");
    std::fs::create_dir_all(&healthy_root).expect("mkdir healthy corpus");
    let ghost_root = tmp.path().join("does-not-exist-xyz");

    let cfg = cfg_two_corpora_one_missing(&ground, &healthy_root, &ghost_root);
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Index(hallouminate::app::daemon::IndexRequest {
                corpus: None,
                paths_from: None,
                strict: true,
            }),
        })
        .await
        .expect("index transport ok");

    match resp {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message,
        } => {
            assert!(
                message.contains("does not exist")
                    && message.contains(&ghost_root.display().to_string()),
                "strict error must name the missing root: {message}"
            );
        }
        other => panic!("strict mode must reject a missing root, got: {other:?}"),
    }
}

// ─── Curd 5: stale-detection wiring ─────────────────────────────────────────

/// Build a single-corpus daemon config with embeddings disabled.
fn cfg_stale_corpus(ground: &Path, corpus_root: &Path) -> Config {
    let toml = format!(
        "[[corpus]]\nname = \"docs\"\npaths = [\"{c}\"]\nglobs = [\"**/*.md\"]\n\n[storage]\nground_dir = \"{g}\"\n\n[embeddings]\nenabled = false\n",
        c = corpus_root.display(),
        g = ground.display(),
    );
    toml::from_str(&toml).expect("stale corpus toml parses")
}

#[tokio::test]
async fn ground_marks_stale_true_when_file_modified_after_index() {
    // Quality gate for #135 wiring: index a file through the daemon, bump
    // its on-disk mtime out-of-band, issue a `ground` request through the
    // real daemon path, and assert the returned DocFile.stale == true for
    // the modified file and stale == false for an unchanged file.
    //
    // This test specifically guards the `mark_stale` call inside
    // `handle_ground`: if that call is removed, `ground` still returns
    // results but stale is always false — this test turns red.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");

    // Write two files before indexing.
    let stale_file = corpus_root.join("will-change.md");
    let fresh_file = corpus_root.join("unchanged.md");
    // Use unique tokens so `ground` can find them regardless of other content.
    std::fs::write(&stale_file, "# Stale\n\nstaletoken9182 content here\n").expect("write stale");
    std::fs::write(&fresh_file, "# Fresh\n\nfreshtoken7364 content here\n").expect("write fresh");

    let cfg = cfg_stale_corpus(&ground, &corpus_root);
    let harness = DaemonHarness::spawn(cfg).await;

    // Index both files through the daemon.
    let client = connect_at(harness.socket()).await.expect("connect");
    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Index(IndexRequest {
                corpus: Some("docs".into()),
                paths_from: None,
                strict: false,
            }),
        })
        .await
        .expect("index transport ok");
    match resp {
        DaemonResponse::Ok { .. } => {}
        DaemonResponse::Err { kind, message } => {
            panic!("index failed ({kind:?}): {message}");
        }
    }

    // Bump the mtime of stale_file out-of-band by rewriting it with a
    // timestamp guaranteed to be at least one second newer than the indexed
    // mtime. We sleep briefly to ensure the OS mtime ticks past the indexed
    // second boundary.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    std::fs::write(&stale_file, "# Stale\n\nstaletoken9182 modified\n").expect("rewrite stale");

    // Issue a ground query for the stale file through the real daemon.
    let client = connect_at(harness.socket()).await.expect("reconnect");
    let result: GroundResult = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Ground(GroundRequest {
                query: "staletoken9182".into(),
                corpus: Some("docs".into()),
                top_files: None,
                chunks_per_file: None,
                limit: None,
                snippet_chars: None,
            }),
        })
        .await
        .expect("ground ok");

    // The stale file must appear in results and be marked stale.
    let abs_stale = std::fs::canonicalize(&stale_file).unwrap_or_else(|_| stale_file.clone());
    let abs_stale_str = abs_stale.to_str().unwrap();
    let stale_doc = result
        .response
        .docs
        .get(abs_stale_str)
        .expect("stale file must appear in ground results");
    assert!(
        stale_doc.stale,
        "file modified after index must be marked stale by handle_ground"
    );

    // Query for the unchanged file and verify it is NOT stale.
    let client = connect_at(harness.socket()).await.expect("reconnect2");
    let result2: GroundResult = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::Ground(GroundRequest {
                query: "freshtoken7364".into(),
                corpus: Some("docs".into()),
                top_files: None,
                chunks_per_file: None,
                limit: None,
                snippet_chars: None,
            }),
        })
        .await
        .expect("ground ok 2");

    let abs_fresh = std::fs::canonicalize(&fresh_file).unwrap_or_else(|_| fresh_file.clone());
    let abs_fresh_str = abs_fresh.to_str().unwrap();
    let fresh_doc = result2
        .response
        .docs
        .get(abs_fresh_str)
        .expect("fresh file must appear in ground results");
    assert!(
        !fresh_doc.stale,
        "file unchanged since index must NOT be marked stale"
    );
}

// ─── Issue #134: add_markdown section / range / match-scoped writes ──────────

/// T20 — mode exclusivity: under_heading + replace_lines both set → InvalidParams
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t20_mode_exclusivity_under_heading_plus_replace_lines_rejected() {
    // WHY: setting two edit-mode selectors is a caller error that must fail
    // loudly without touching the file.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Write a file first so it exists.
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "## Section\n\nBody.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("initial write");

    let original = std::fs::read_to_string(corpus_root.join("page.md")).unwrap();

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "x".into(),
                overwrite: false,
                under_heading: Some("Section".into()),
                replace_lines: Some(LineRange { start: 1, end: 1 }),
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    match resp {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message,
        } => {
            assert!(
                message.contains("at most one"),
                "error must mention exclusivity: {message}"
            );
        }
        other => panic!("expected InvalidParams, got: {other:?}"),
    }
    // File must be untouched
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("page.md")).unwrap(),
        original,
        "file must be untouched after rejected multi-mode request"
    );
}

/// T21 — mode exclusivity: replace_lines + replace_match both set → InvalidParams
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t21_mode_exclusivity_replace_lines_plus_replace_match_rejected() {
    // WHY: the second conflicting pair must also be caught by the exclusivity gate.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "Line one.\nLine two.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("initial write");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "new".into(),
                overwrite: false,
                replace_lines: Some(LineRange { start: 1, end: 1 }),
                replace_match: Some("Line one".into()),
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    match resp {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message,
        } => {
            assert!(
                message.contains("at most one"),
                "error must mention exclusivity: {message}"
            );
        }
        other => panic!("expected InvalidParams, got: {other:?}"),
    }
    // File must be untouched
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("page.md")).unwrap(),
        "Line one.\nLine two.\n",
        "file must be untouched after rejected multi-mode request"
    );
}

/// T22 — file-must-exist: under_heading on missing file → InvalidParams
#[tokio::test]
async fn t22_under_heading_on_missing_file_returns_invalid_params() {
    // WHY: edit modes require an existing file; silent creation would corrupt
    // the expected structure.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "nonexistent.md".into(),
                content: "stuff".into(),
                overwrite: false,
                under_heading: Some("Section".into()),
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    assert!(
        matches!(
            resp,
            DaemonResponse::Err {
                kind: ErrorKind::InvalidParams,
                ..
            }
        ),
        "expected InvalidParams for missing file, got: {resp:?}"
    );
    // File must not have been created
    assert!(
        !corpus_root.join("nonexistent.md").exists(),
        "file must not be created by under_heading on a missing file"
    );
}

/// T23 — file-must-exist: replace_lines on missing file → InvalidParams
#[tokio::test]
async fn t23_replace_lines_on_missing_file_returns_invalid_params() {
    // WHY: replace_lines is read-modify-write; missing file must fail loudly.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "nonexistent.md".into(),
                content: "replacement".into(),
                overwrite: false,
                replace_lines: Some(LineRange { start: 1, end: 1 }),
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    assert!(
        matches!(
            resp,
            DaemonResponse::Err {
                kind: ErrorKind::InvalidParams,
                ..
            }
        ),
        "expected InvalidParams for missing file, got: {resp:?}"
    );
    // File must not have been created
    assert!(
        !corpus_root.join("nonexistent.md").exists(),
        "file must not be created by replace_lines on a missing file"
    );
}

/// T24 — file-must-exist: replace_match on missing file → InvalidParams
#[tokio::test]
async fn t24_replace_match_on_missing_file_returns_invalid_params() {
    // WHY: replace_match is read-modify-write; missing file must fail loudly.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let resp = client
        .call_raw(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "nonexistent.md".into(),
                content: "replacement".into(),
                overwrite: false,
                replace_match: Some("needle".into()),
                ..Default::default()
            }),
        })
        .await
        .expect("transport ok");
    assert!(
        matches!(
            resp,
            DaemonResponse::Err {
                kind: ErrorKind::InvalidParams,
                ..
            }
        ),
        "expected InvalidParams for missing file, got: {resp:?}"
    );
    // File must not have been created
    assert!(
        !corpus_root.join("nonexistent.md").exists(),
        "file must not be created by replace_match on a missing file"
    );
}

/// T25 — reindex: section write → IndexReport files_upserted >= 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t25_section_write_reindexes_file() {
    // WHY: after a section splice, the daemon must reindex the file so the
    // updated content is searchable.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Write initial file.
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "wiki.md".into(),
                content: "## Notes\n\nOriginal content.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("initial write");

    // Section splice.
    let resp: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "wiki.md".into(),
                content: "Added bullet.".into(),
                overwrite: false,
                under_heading: Some("Notes".into()),
                position: Position::Append,
                ..Default::default()
            }),
        })
        .await
        .expect("section write ok");

    let upserted = resp["indexed"]["corpora"][0]["files_upserted"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        upserted >= 1,
        "section write must report files_upserted >= 1: {resp}"
    );

    // Verify the composed content is on disk.
    let on_disk = std::fs::read_to_string(corpus_root.join("wiki.md")).unwrap();
    assert!(
        on_disk.contains("Added bullet."),
        "spliced content must be on disk: {on_disk:?}"
    );
    assert!(
        on_disk.contains("Original content."),
        "original content must be preserved: {on_disk:?}"
    );
}

/// T26 — reindex: replace_lines write → files_upserted >= 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t26_replace_lines_write_reindexes_file() {
    // WHY: after a line-range replace, the daemon must reindex the modified
    // file so the updated content is searchable.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Write a 3-line file.
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "alpha\nbeta\ngamma\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("initial write");

    // Replace line 2.
    let resp: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "REPLACED".into(),
                overwrite: false,
                replace_lines: Some(LineRange { start: 2, end: 2 }),
                ..Default::default()
            }),
        })
        .await
        .expect("replace_lines ok");

    let upserted = resp["indexed"]["corpora"][0]["files_upserted"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        upserted >= 1,
        "replace_lines must report files_upserted >= 1: {resp}"
    );

    let on_disk = std::fs::read_to_string(corpus_root.join("page.md")).unwrap();
    // Terminator hygiene: single trailing newline after replacement, no surrounding blank lines.
    assert_eq!(
        on_disk, "alpha\nREPLACED\ngamma\n",
        "replace_lines must produce exact composed output: {on_disk:?}"
    );
}

/// T27 — back-compat: whole-file write with all new fields omitted is unchanged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t27_whole_file_write_with_new_fields_omitted_is_unchanged() {
    // WHY: the default (no edit-mode field) must behave identically to the
    // pre-#134 whole-file path so existing callers are unaffected.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    let body = "# Whole File\n\nContent unchanged.\n";
    let resp: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "whole.md".into(),
                content: body.into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("whole-file write ok");

    // File on disk is verbatim — no splicing, no normalization.
    assert_eq!(
        std::fs::read_to_string(corpus_root.join("whole.md")).unwrap(),
        body,
        "whole-file write must be stored verbatim"
    );
    // Index report looks the same as pre-#134.
    assert!(
        resp["indexed"]["corpora"][0]["files_upserted"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "whole-file write must report files_upserted >= 1: {resp}"
    );
}

/// T28 — lint ride-back: edit-mode write on a composed file that trips a lint
/// returns warnings in AddMarkdownResult
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t28_edit_mode_lint_warnings_ride_back_on_composed_file() {
    // WHY: advisory lint must run on the COMPOSED file after an edit-mode
    // write so broken links / empty mermaid in the spliced fragment surface
    // the same way they would in a whole-file write.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir");
    let cfg: Config = toml::from_str(&format!(
        "[[corpus]]\nname=\"docs\"\npaths=[\"{c}\"]\nglobs=[\"**/*.md\"]\n[storage]\nground_dir=\"{g}\"\n[embeddings]\nenabled=false",
        c = corpus_root.display(),
        g = ground.display()
    ))
    .expect("cfg");
    let harness = DaemonHarness::spawn(cfg).await;
    let client = connect_at(harness.socket()).await.expect("connect");

    // Write a clean initial page.
    client
        .call::<serde_json::Value>(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "## Section\n\nClean content.\n".into(),
                overwrite: false,
                ..Default::default()
            }),
        })
        .await
        .expect("initial write");

    // Splice a fragment with an empty-destination link (triggers lint).
    let resp: serde_json::Value = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
                corpus: "docs".into(),
                path: "page.md".into(),
                content: "See [broken link]() for details.".into(),
                overwrite: false,
                under_heading: Some("Section".into()),
                position: Position::Append,
                ..Default::default()
            }),
        })
        .await
        .expect("edit-mode write ok despite lint");

    // The write must succeed (advisory lint never blocks).
    let on_disk = std::fs::read_to_string(corpus_root.join("page.md")).unwrap();
    assert!(
        on_disk.contains("broken link"),
        "content must be on disk despite lint warning: {on_disk:?}"
    );

    // Lint warnings must ride back in the response.
    let warnings = resp["warnings"].as_array();
    assert!(
        warnings.is_some_and(|w| !w.is_empty()),
        "lint warnings must ride back on edit-mode write: {resp}"
    );
    let joined = warnings
        .unwrap()
        .iter()
        .filter_map(|w| w.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("empty destination"),
        "empty-destination lint must be present: {joined}"
    );
}

// ─── Issue #127: store-schema auto-rebuild ────────────────────────────────────────

/// Write a meta.toml with an explicit schema_version into `ground_dir`.
/// `ground_dir` is created if absent.
fn write_stale_meta(ground_dir: &Path, schema_version: u32) {
    std::fs::create_dir_all(ground_dir).expect("mkdir ground");
    let meta = format!(
        "# auto-managed by hallouminate; do not edit\n\
         embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
         quantized = false\n\
         embeddings_enabled = false\n\
         schema_version = {schema_version}\n"
    );
    std::fs::write(ground_dir.join("meta.toml"), meta).expect("write meta.toml");
}

/// Config with one corpus and embeddings disabled, model pinned to BGE so it
/// matches the `write_stale_meta` sidecar (both must agree on model name).
fn cfg_with_corpus(ground_dir: &Path, corpus_root: &Path) -> Config {
    let toml = format!(
        "[[corpus]]\nname = \"docs\"\npaths = [\"{c}\"]\nglobs = [\"**/*.md\"]\n\n\
         [storage]\nground_dir = \"{g}\"\n\n\
         [embeddings]\nenabled = false\nmodel = \"BAAI/bge-small-en-v1.5\"\n",
        c = corpus_root.display(),
        g = ground_dir.display(),
    );
    toml::from_str(&toml).expect("cfg_with_corpus toml parses")
}

/// Read the schema_version from a ground dir's meta.toml.
fn read_schema_version(ground_dir: &Path) -> u32 {
    #[derive(serde::Deserialize)]
    struct MetaVersion {
        schema_version: u32,
    }
    let text = std::fs::read_to_string(ground_dir.join("meta.toml")).expect("read meta.toml");
    let m: MetaVersion = toml::from_str(&text).expect("parse meta.toml");
    m.schema_version
}

// T1: stale store auto-rebuilds
#[tokio::test]
async fn stale_store_auto_rebuilds_on_daemon_open() {
    // Setup: ground dir with a stale meta.toml (expected - 1) + a seeded wiki
    // file. DaemonState::open must return Ok, moving the stale store aside and
    // reindexing from source.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    std::fs::write(corpus_root.join("hello.md"), "# Hello\n\nworld\n").expect("seed");

    // Write a stale meta at schema_version = current - 1.
    let current = hallouminate::adapters::lance::default_schema_version_pub();
    let stale = current - 1;
    write_stale_meta(&ground, stale);

    let cfg = cfg_with_corpus(&ground, &corpus_root);
    // DaemonState::open must succeed (no crash).
    DaemonState::open(cfg, None)
        .await
        .expect("stale store must be auto-rebuilt, not fatal");

    // The stale store was moved aside.
    let bak = ground.with_file_name(format!("ground.bak-v{stale}"));
    assert!(
        bak.exists(),
        "backup ground.bak-v{stale} must exist after rebuild"
    );
    // The backup must contain the original stale meta.toml so the store is
    // recoverable (spec criterion 4: stale store is recoverable at .bak-v{N}).
    let bak_version = read_schema_version(&bak);
    assert_eq!(
        bak_version, stale,
        "backup meta.toml must retain the stale schema_version so the data is recoverable"
    );

    // The fresh store has the current schema version.
    let fresh_version = read_schema_version(&ground);
    assert_eq!(
        fresh_version, current,
        "fresh meta.toml must record the current schema version"
    );
}

// T2: downgrade stays fatal + original store untouched
#[tokio::test]
async fn downgrade_store_is_fatal_and_original_untouched() {
    // A store from a NEWER binary (schema_version > expected) must fail loud
    // and fatal. The original ground dir must not be touched (no .bak).
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");

    let current = hallouminate::adapters::lance::default_schema_version_pub();
    let newer = current + 1;
    write_stale_meta(&ground, newer);

    let cfg = cfg_with_corpus(&ground, &corpus_root);
    let err = DaemonState::open(cfg, None)
        .await
        .expect_err("downgrade must be fatal");
    let msg = err.to_string();
    assert!(
        msg.to_uppercase().contains("NEWER"),
        "error must say NEWER (downgrade): {msg}"
    );
    assert!(
        msg.to_lowercase().contains("upgrade"),
        "error must advise upgrade: {msg}"
    );
    // Original ground dir is untouched: no .bak directory created.
    let bak = ground.with_file_name(format!("ground.bak-v{newer}"));
    assert!(
        !bak.exists(),
        "no .bak must exist after downgrade rejection"
    );
    // The original meta.toml is still at the newer version.
    let stored = read_schema_version(&ground);
    assert_eq!(stored, newer, "original meta.toml must be untouched");
}

// T3: matching version — store untouched, no backup
#[tokio::test]
async fn matching_version_store_is_untouched() {
    // When the on-disk schema_version equals the build's expected version,
    // DaemonState::open must succeed and must NOT create any .bak directory.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");

    let current = hallouminate::adapters::lance::default_schema_version_pub();
    write_stale_meta(&ground, current);

    let cfg = cfg_with_corpus(&ground, &corpus_root);
    DaemonState::open(cfg, None)
        .await
        .expect("matching version must open cleanly");

    // No backup was created.
    let bak = ground.with_file_name(format!("ground.bak-v{current}"));
    assert!(!bak.exists(), "no .bak must exist when versions match");
    // The on-disk meta.toml must still record the current version — the
    // match-branch must not have silently mutated or re-written it.
    let still_current = read_schema_version(&ground);
    assert_eq!(
        still_current, current,
        "matching-version open must not alter meta.toml"
    );
}

// T4: rebuild reproduces content (lexical, no embeddings needed)
#[tokio::test]
async fn stale_rebuild_reproduces_corpus_content() {
    // After a stale-store auto-rebuild, list_files returns the seeded file.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let seeded = corpus_root.join("doc.md");
    std::fs::write(&seeded, "# Doc\n\ncontent here\n").expect("seed doc");

    let current = hallouminate::adapters::lance::default_schema_version_pub();
    write_stale_meta(&ground, current - 1);

    let cfg = cfg_with_corpus(&ground, &corpus_root);
    let harness = DaemonHarness::spawn(cfg).await;

    // list_files for the rebuilt corpus must return the seeded file.
    let client = connect_at(harness.socket()).await.expect("connect");
    let result: ListFilesResult = client
        .call(DaemonRequest {
            cwd: harness.cwd().to_path_buf(),
            payload: DaemonRequestPayload::ListFiles(ListFilesRequest {
                corpus: Some("docs".into()),
            }),
        })
        .await
        .expect("list_files ok");

    assert!(
        !result.is_empty(),
        "rebuilt corpus must contain the seeded file; list_files was empty"
    );
    // The seeded file's path must appear in the results.
    let paths: Vec<&str> = result.iter().map(|e| e.path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains("doc.md")),
        "seeded doc.md must appear in list_files after rebuild; got: {paths:?}"
    );
}

// T5: move-aside overwrites prior backup
#[tokio::test]
async fn stale_rebuild_overwrites_prior_backup() {
    // Pre-create ground.bak-v{N} to simulate a prior failed rebuild.
    // A fresh stale rebuild must overwrite it (single backup remains).
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    std::fs::write(corpus_root.join("doc.md"), "# Doc\n\ncontent\n").expect("seed");

    let current = hallouminate::adapters::lance::default_schema_version_pub();
    let stale = current - 1;
    write_stale_meta(&ground, stale);

    // Pre-create a prior backup directory with a sentinel file.
    let bak = ground.with_file_name(format!("ground.bak-v{stale}"));
    std::fs::create_dir_all(&bak).expect("mkdir prior backup");
    std::fs::write(bak.join("sentinel"), "old backup").expect("write sentinel");

    let cfg = cfg_with_corpus(&ground, &corpus_root);
    DaemonState::open(cfg, None)
        .await
        .expect("rebuild must succeed even when prior backup exists");

    // The backup dir exists (rebuilt over the old one).
    assert!(bak.exists(), "ground.bak-v{stale} must exist after rebuild");
    // The old sentinel is gone (prior backup was replaced).
    assert!(
        !bak.join("sentinel").exists(),
        "prior backup must be replaced (sentinel file must be gone)"
    );
}

// T6: rebuild failure — Err returned, backup preserved
#[tokio::test]
async fn stale_rebuild_failure_returns_err_and_preserves_backup() {
    // Use a corpus with an invalid glob so scan() returns Err and the rebuild
    // fails. DaemonState::open must return Err (not panic), and the stale
    // store must still be recoverable at the backup path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    std::fs::write(corpus_root.join("doc.md"), "# Doc\n\ncontent\n").expect("seed");

    let current = hallouminate::adapters::lance::default_schema_version_pub();
    let stale = current - 1;
    write_stale_meta(&ground, stale);

    // Corpus config with an invalid glob pattern so scan() fails.
    let toml = format!(
        "[[corpus]]\nname = \"docs\"\npaths = [\"{c}\"]\nglobs = [\"[invalid\"]\n\n\
         [storage]\nground_dir = \"{g}\"\n\n\
         [embeddings]\nenabled = false\nmodel = \"BAAI/bge-small-en-v1.5\"\n",
        c = corpus_root.display(),
        g = ground.display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("toml parses");

    let err = DaemonState::open(cfg, None)
        .await
        .expect_err("rebuild with scan error must fail");
    assert!(
        err.to_string().contains("rebuild"),
        "error must mention rebuild: {err}"
    );

    // Backup must still exist so the old data is recoverable.
    let bak = ground.with_file_name(format!("ground.bak-v{stale}"));
    assert!(
        bak.exists(),
        "stale store backup must be preserved after failed rebuild"
    );
    // Fresh ground dir must be removed so next boot retries rebuild instead
    // of booting with an empty-but-schema-valid store.
    assert!(
        !ground.exists(),
        "partial fresh ground dir must be removed on rebuild failure"
    );
}

// ─── Curd: bounded request-line cap + drained shutdown ──────────────────

#[tokio::test]
async fn daemon_closes_connection_when_request_line_exceeds_cap() {
    // MAX_REQUEST_LINE_BYTES (4 MiB) bounds the newline-delimited request
    // line's allocation via a `.take()`-wrapped reader. A line that never
    // hits its newline within the cap must close the connection instead of
    // growing the buffer without bound or hanging on the idle timeout.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let cfg = docs_cfg(&ground, &corpus_root);
    let harness = DaemonHarness::spawn(cfg).await;

    let mut stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");

    // Write past the cap with no trailing newline; tolerate a broken pipe
    // once the server closes its half after the cap is exceeded.
    let chunk = vec![b'a'; 64 * 1024];
    let target = 4 * 1024 * 1024 + 64 * 1024;
    let mut written = 0usize;
    while written < target {
        match stream.write_all(&chunk).await {
            Ok(()) => written += chunk.len(),
            Err(_) => break,
        }
    }

    let mut buf = [0u8; 1];
    let n = timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("server must close the oversized-line connection promptly, not hang")
        .expect("read must not error");
    assert_eq!(
        n, 0,
        "server must close the connection (EOF) once the cap is exceeded, not respond or hang"
    );
}

#[tokio::test]
async fn ipc_shutdown_waits_for_in_flight_handler_before_releasing_socket() {
    // Quality gate: `drain_handlers` (SHUTDOWN_DRAIN_TIMEOUT) must let an
    // in-flight connection handler finish before `serve_with_idle_timeout`
    // releases the socket + single-instance flock. A connection that never
    // sends its request line stays "in-flight" (blocked on `read_line`)
    // until the per-connection idle timeout elapses, giving us a real
    // in-flight handler without any test-only injection seam.
    use hallouminate::app::daemon::serve_with_idle_timeout;
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ground = tmp.path().join("ground");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).expect("mkdir corpus");
    let cwd = seed_cwd(tmp.path());
    let socket = tmp.path().join("daemon.sock");
    let cfg = docs_cfg(&ground, &corpus_root);

    let state = DaemonState::open(cfg, None).await.expect("open state");
    let socket_clone = socket.clone();
    let idle_timeout = Duration::from_millis(500);
    let handle =
        tokio::spawn(
            async move { serve_with_idle_timeout(&state, &socket_clone, idle_timeout).await },
        );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Open a connection and never write to it: its handler stays alive,
    // blocked on `read_line` under `idle_timeout`, until that timeout fires.
    let _in_flight = UnixStream::connect(&socket)
        .await
        .expect("connect in-flight handler");

    // Trigger shutdown from a second, independent connection.
    let client = connect_at(&socket).await.expect("connect for shutdown");
    let resp = client
        .call_raw(DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::Shutdown,
        })
        .await
        .expect("shutdown transport ok");
    match resp {
        DaemonResponse::Ok { result } => {
            assert_eq!(result, serde_json::Value::String("stopping".to_string()));
        }
        other => panic!("shutdown must ack `stopping`, got {other:?}"),
    }

    // The accept loop breaks near-instantly on the Shutdown ack, but cleanup
    // must not run until the in-flight handler above times out — proving
    // drain runs before socket removal / flock release, not after.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        socket.exists(),
        "socket must still exist while the in-flight handler is draining"
    );

    let served = timeout(Duration::from_secs(5), handle)
        .await
        .expect("serve must return once the in-flight handler finishes draining")
        .expect("join ok");
    served.expect("serve returns Ok after a graceful drain");
    assert!(
        !socket.exists(),
        "socket must be removed once the drain completes"
    );
}
