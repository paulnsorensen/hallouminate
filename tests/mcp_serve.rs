//! End-to-end test: spawn `hallouminate serve` as a child process, drive
//! the MCP JSON-RPC handshake over its stdio, and assert that `tools/list`
//! and `tools/call list_corpora` produce the expected shapes.
//!
//! Most tests skip `tools/call ground` and `tools/call index` because
//! both would force the embedding model download (~33MB on first run).
//! The `add_markdown` end-to-end test reuses the developer's already-
//! downloaded model from the default `cache_dir`; on a fresh machine it
//! will pay the one-time download cost.
//!
//! Spec contract: every stateful MCP tool dispatches through the local
//! daemon over a Unix socket. Each test that needs tool work spawns a
//! per-test daemon (`DaemonHarness`) and sets `HALLOUMINATE_SOCKET` on the
//! child `serve` process so the MCP tools dial that socket. The handshake
//! itself (initialize / tools/list) does not need a daemon — pure protocol
//! plumbing — so tests that only exercise the handshake skip the harness.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use hallouminate::app::config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

mod common;
use common::daemon::DaemonHarness;

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct Mcp {
    child: Child,
    // `Option` so the cooperative `shutdown` path can `take()` and drop
    // stdin to signal EOF without moving out of a struct that owns a
    // `Drop` impl — see the impl block below.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Mcp {
    /// RAII safety net: if an assertion panics before `shutdown()` is
    /// reached, the cooperative shutdown path is skipped — without this
    /// drop guard the child `hallouminate serve` process would leak past
    /// the test process exit. `start_kill` is the synchronous, non-async
    /// kill primitive on `tokio::process::Child`, suitable from `Drop`.
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Mcp {
    /// Spawn `hallouminate serve` with `XDG_CONFIG_HOME` pointed at the
    /// per-test config dir. Optionally set `HALLOUMINATE_SOCKET` so the
    /// MCP tools dial the per-test daemon harness instead of the
    /// developer's real daemon socket.
    async fn spawn(xdg_config_home: &Path, daemon_socket: Option<&Path>) -> Self {
        let bin = env!("CARGO_BIN_EXE_hallouminate");
        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .env("XDG_CONFIG_HOME", xdg_config_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(socket) = daemon_socket {
            cmd.env("HALLOUMINATE_SOCKET", socket);
        } else {
            // Defensive: don't accidentally leak the dev's daemon into the
            // test sandbox. Tests that don't pass a socket are tests that
            // shouldn't dial a daemon at all (handshake-only), so point at
            // a per-process /dev/null-equivalent path that will fail loudly
            // if any tool call slips through.
            cmd.env(
                "HALLOUMINATE_SOCKET",
                std::env::temp_dir().join(format!(
                    "hallouminate-mcp-test-no-daemon-{}.sock",
                    std::process::id()
                )),
            );
        }
        let mut child = cmd.spawn().expect("spawn hallouminate serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut buf = serde_json::to_string(&value).unwrap();
        buf.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin not yet closed");
        stdin.write_all(buf.as_bytes()).await.expect("write");
        stdin.flush().await.expect("flush");
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        timeout(READ_TIMEOUT, self.stdout.read_line(&mut line))
            .await
            .expect("response within timeout")
            .expect("read line ok");
        assert!(!line.is_empty(), "server closed stdout before reply");
        serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!("invalid JSON from server: {e}; line: {line}");
        })
    }

    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        loop {
            let msg = self.recv().await;
            // Filter notifications (no id) and only return the matching response.
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return msg;
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await;
    }

    async fn shutdown(&mut self) {
        // Take + drop stdin to send EOF to the server; the `Drop` impl
        // would do the same on panic by killing the child outright.
        self.stdin.take();
        let _ = timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

fn write_minimal_config(dir: &Path) {
    let cfg_dir = dir.join("hallouminate");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir hallouminate config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        // No `[[corpus]]` entries: the list_corpora tool just emits an
        // empty list. Avoids any disk/index side effects in the test.
        "# minimal test config\n",
    )
    .expect("write config.toml");
}

fn write_config_with_corpus(dir: &Path, corpus_name: &str, corpus_path: &str) -> Config {
    // Pin `storage.ground_dir` to a per-test path under `dir` so the daemon
    // doesn't open the developer's real `~/.local/share/hallouminate/ground`
    // (which would couple every MCP test to the host's existing `meta.toml`
    // and pollute it with test mutations). Mirrors `write_config_with_corpus_and_ground`'s
    // contract — just derives the ground dir from `dir` instead of taking it as a separate arg.
    let cfg_dir = dir.join("hallouminate");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir hallouminate config dir");
    let ground_dir = dir.join("ground");
    let toml = format!(
        r#"
[[corpus]]
name = "{corpus_name}"
paths = ["{corpus_path}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{ground}"
"#,
        ground = ground_dir.display(),
    );
    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(&cfg_path, &toml).expect("write config.toml");
    toml::from_str(&toml).expect("parse config")
}

/// Like `write_config_with_corpus` but pins `[storage].ground_dir` to a
/// per-test tmpdir so the integration test never touches the developer's
/// `~/.local/share/hallouminate/ground`. Embedding cache is left at the
/// default `~/.cache/hallouminate/fastembed` so the test reuses any
/// already-downloaded model.
fn write_config_with_corpus_and_ground(
    dir: &Path,
    corpus_name: &str,
    corpus_path: &str,
    ground_dir: &Path,
) -> Config {
    let cfg_dir = dir.join("hallouminate");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir hallouminate config dir");
    let toml = format!(
        r#"
[[corpus]]
name = "{corpus_name}"
paths = ["{corpus_path}"]
globs = ["**/*.md"]

[storage]
ground_dir = "{ground}"
"#,
        ground = ground_dir.display(),
    );
    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(&cfg_path, &toml).expect("write config.toml");
    toml::from_str(&toml).expect("parse config")
}

fn load_minimal_config(dir: &Path) -> Config {
    // Daemon opens LanceDB at startup regardless of corpus count, so even a
    // "minimal" config needs a tempdir ground or we'd hit the developer's
    // real `~/.local/share/hallouminate/ground` and either create/validate
    // its `meta.toml` or fail with a real-store model mismatch.
    let mut cfg = Config::default();
    cfg.storage.ground_dir = dir.join("ground").to_string_lossy().into_owned();
    cfg
}

#[tokio::test]
async fn mcp_server_initialize_lists_tools_and_calls_list_corpora() {
    let xdg = tempfile::tempdir().expect("tempdir");
    write_minimal_config(xdg.path());
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;

    // 1. initialize — required first message in the MCP handshake.
    let init = mcp
        .rpc(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
            }),
        )
        .await;
    assert_eq!(init["jsonrpc"], "2.0", "initialize response: {init}");
    assert!(init.get("error").is_none(), "initialize errored: {init}");
    let result = &init["result"];
    assert!(result.is_object(), "result must be an object: {init}");
    assert!(
        result["serverInfo"]["name"]
            .as_str()
            .unwrap_or("")
            .contains("hallouminate"),
        "serverInfo.name should mention hallouminate: {result}"
    );

    // MCP protocol requires `notifications/initialized` after the response.
    mcp.notify("notifications/initialized", json!({})).await;

    // 2. tools/list — must surface all registered tools.
    let list = mcp.rpc(2, "tools/list", json!({})).await;
    assert!(list.get("error").is_none(), "tools/list errored: {list}");
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools array present");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    for expected in [
        "ground",
        "index",
        "list_corpora",
        "list_files",
        "add_markdown",
        "read_markdown",
        "delete_markdown",
    ] {
        assert!(
            names.contains(&expected),
            "tool `{expected}` missing from {names:?}"
        );
    }
    // Each tool must carry an inputSchema — MCP-aware clients need it to
    // form valid `tools/call` arguments.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("?");
        assert!(
            tool["inputSchema"].is_object(),
            "tool `{name}` missing inputSchema: {tool}"
        );
    }

    // 3. tools/call list_corpora — exercises the full request/response
    //    round-trip without forcing the embedding-model download path.
    //    The daemon (harness) replies with an empty list because the
    //    daemon's config (also minimal) has no corpora.
    let call = mcp
        .rpc(
            3,
            "tools/call",
            json!({"name": "list_corpora", "arguments": {}}),
        )
        .await;
    assert!(call.get("error").is_none(), "tools/call errored: {call}");
    let result = &call["result"];
    assert!(
        result["content"].is_array(),
        "content must be an array: {result}"
    );
    let structured = &result["structuredContent"];
    if let Some(arr) = structured.as_array() {
        assert!(
            arr.is_empty(),
            "no corpora configured in test fixture — expected empty: {arr:?}"
        );
    }

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_list_files_surfaces_corpus_files_without_indexing() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    std::fs::create_dir_all(corpus.path().join("wiki/concepts")).expect("mkdir");
    std::fs::write(corpus.path().join("wiki/overview.md"), "# Overview\n").expect("write");
    std::fs::write(
        corpus.path().join("wiki/concepts/attention.md"),
        "# Attention\n",
    )
    .expect("write");
    std::fs::write(corpus.path().join("wiki/ignore.txt"), "ignore").expect("write txt");
    let cfg = write_config_with_corpus(xdg.path(), "wiki", &corpus.path().to_string_lossy());
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({"name": "list_files", "arguments": {"corpus": "wiki"}}),
        )
        .await;
    assert!(call.get("error").is_none(), "tools/call errored: {call}");
    let result = &call["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text content present");
    assert!(text.contains("wiki/overview.md"), "text content: {text:?}");
    assert!(
        text.contains("wiki/concepts/attention.md"),
        "text content: {text:?}"
    );
    assert!(!text.contains("ignore.txt"), "text content: {text:?}");

    let structured = result["structuredContent"]
        .as_array()
        .expect("structured payload is an array");
    let paths: Vec<&str> = structured
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["wiki/concepts/attention.md", "wiki/overview.md"]
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_list_corpora_surfaces_configured_corpora_with_names_and_paths() {
    // Strengthen the round-trip: a non-empty config must surface each
    // corpus by name in both the text `content` and the structured
    // payload, with `paths` carried through verbatim. Catches regressions
    // where the tool serializes an empty array or drops the paths field.
    let xdg = tempfile::tempdir().expect("tempdir");
    let cfg =
        write_config_with_corpus(xdg.path(), "test-corpus", "/tmp/hallouminate-press-fixture");
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({"name": "list_corpora", "arguments": {}}),
        )
        .await;
    let result = &call["result"];

    // Text content surfaces the corpus name (newline-delimited list).
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text content present");
    assert!(
        text.contains("test-corpus"),
        "corpus name missing from text content: {text:?}"
    );

    // Structured payload is an array of {name, paths} objects.
    let structured = result["structuredContent"]
        .as_array()
        .expect("structured payload is an array");
    assert_eq!(
        structured.len(),
        1,
        "expected exactly one corpus: {structured:?}"
    );
    let entry = &structured[0];
    assert_eq!(
        entry["name"].as_str(),
        Some("test-corpus"),
        "structured entry name: {entry:?}"
    );
    let paths = entry["paths"].as_array().expect("paths is an array");
    assert_eq!(paths.len(), 1, "one path configured: {paths:?}");
    assert_eq!(
        paths[0].as_str(),
        Some("/tmp/hallouminate-press-fixture"),
        "path carried verbatim: {paths:?}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_server_returns_error_for_unknown_corpus_without_panicking() {
    // Regression: an unknown corpus argument must surface as a JSON-RPC
    // error response, not as a crashed server. Uses the `list_corpora`
    // path indirectly via `ground` with a missing corpus — `ground` exits
    // before touching the embedder when the corpus name doesn't match.
    let xdg = tempfile::tempdir().expect("tempdir");
    write_minimal_config(xdg.path());
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;

    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {"query": "x", "corpus": "ghost"}
            }),
        )
        .await;

    // Caller-input failures (unknown corpus) must come back as
    // `-32602 invalid_params`. A regression to `-32603 internal_error` or
    // a panic must be visible here.
    let error = call.get("error").unwrap_or_else(|| {
        panic!("unknown corpus must surface as a top-level JSON-RPC error, got: {call}")
    });
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "invalid_params code expected, got: {error}"
    );

    // Server must still be alive after the error — send a second request
    // to prove it didn't die.
    let alive = mcp.rpc(3, "tools/list", json!({})).await;
    assert!(
        alive["result"]["tools"].is_array(),
        "server died after error: {alive}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_tool_call_fails_loudly_when_daemon_unreachable() {
    // Spec contract: MCP transport must NOT auto-start the daemon, and
    // must surface a clear "daemon unavailable" message when the dial
    // fails. The handshake itself does not touch the daemon (only
    // tools/call does), so initialize + tools/list succeed even with no
    // daemon. The first stateful tool call comes back with an error.
    let xdg = tempfile::tempdir().expect("tempdir");
    write_minimal_config(xdg.path());
    // Deliberately do NOT spawn a DaemonHarness. `Mcp::spawn(None)` points
    // the child at a guaranteed-missing socket.
    let mut mcp = Mcp::spawn(xdg.path(), None).await;

    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({"name": "list_corpora", "arguments": {}}),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("daemon-down must surface as a JSON-RPC error, got: {call}"));
    // Daemon-unreachable is server-side internal_error (the user can't
    // fix a missing daemon by changing their arguments), not -32602.
    assert_eq!(
        error["code"].as_i64(),
        Some(-32603),
        "daemon-unavailable must use internal_error: {error}"
    );
    let msg = error["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("daemon unavailable"),
        "error message must mention daemon-unavailable: {msg}"
    );
    assert!(
        msg.contains("hallouminate daemon"),
        "error message must hint at how to start the daemon: {msg}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the real embedder and may download a model on first run"]
async fn mcp_add_markdown_writes_reindexes_and_rejects_unsafe_inputs() {
    // End-to-end coverage of the `add_markdown` JSON-RPC handler — write
    // path, parent dir creation, reindex side effect, overwrite gate, and
    // path-escape rejection through `-32602`. Reuses the user's
    // already-downloaded embedding model via the default `cache_dir`
    // (`~/.cache/hallouminate/fastembed`); pins `ground_dir` to a tmpdir
    // so the test never pollutes the developer's real ground directory.
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");
    let cfg = write_config_with_corpus_and_ground(
        xdg.path(),
        "wiki",
        &corpus.path().to_string_lossy(),
        ground.path(),
    );
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    // 1. add_markdown into a subdir that does not yet exist — exercises
    //    parent-dir creation and the atomic write path.
    let content =
        "# Photosynthesis\nPhotosynthesis converts sunlight into chemical energy in plant cells.\n";
    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "add_markdown",
                "arguments": {
                    "corpus": "wiki",
                    "path": "bio/cells/photosynthesis.md",
                    "content": content,
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "add_markdown errored: {call}");
    let result = &call["result"];
    assert_ne!(
        result["isError"].as_bool(),
        Some(true),
        "add_markdown returned tool-level error: {result}"
    );

    // File on disk has the exact content; parent dir was created.
    let written = corpus.path().join("bio/cells/photosynthesis.md");
    assert!(written.exists(), "file not created: {}", written.display());
    assert_eq!(
        std::fs::read_to_string(&written).expect("read written file"),
        content,
    );
    assert!(
        corpus.path().join("bio/cells").is_dir(),
        "parent directory was not created"
    );

    // Structured payload reports the indexed file.
    let structured = &result["structuredContent"];
    assert_eq!(
        structured["corpus"].as_str(),
        Some("wiki"),
        "structured.corpus: {structured}"
    );
    assert_eq!(
        structured["path"].as_str(),
        Some("bio/cells/photosynthesis.md"),
        "structured.path: {structured}"
    );
    let corpora = structured["indexed"]["corpora"]
        .as_array()
        .expect("indexed.corpora is an array");
    assert_eq!(corpora.len(), 1, "indexed.corpora: {corpora:?}");
    assert_eq!(
        corpora[0]["files_upserted"].as_u64(),
        Some(1),
        "the freshly-written file must show as upserted: {:?}",
        corpora[0]
    );

    // 2. ground over the same corpus must surface a chunk from the new
    //    file — proves the reindex side effect actually landed in LanceDB.
    let call = mcp
        .rpc(
            3,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {
                    "query": "photosynthesis converts sunlight",
                    "corpus": "wiki",
                    "top_files": 5,
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "ground errored: {call}");
    let result = &call["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .expect("ground text content");
    assert!(
        text.contains("photosynthesis.md"),
        "ground outline must reference the freshly-written file: {text:?}"
    );

    // 3. second add_markdown to the same path WITHOUT overwrite must fail
    //    with `invalid_params` (-32602).
    let call = mcp
        .rpc(
            4,
            "tools/call",
            json!({
                "name": "add_markdown",
                "arguments": {
                    "corpus": "wiki",
                    "path": "bio/cells/photosynthesis.md",
                    "content": "# clobber attempt\n",
                }
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("second add_markdown must error, got: {call}"));
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "overwrite=false rejection must use invalid_params: {error}"
    );
    // File content must be untouched after the rejection.
    assert_eq!(
        std::fs::read_to_string(&written).expect("read after reject"),
        content,
    );

    // 4. parent-escape `../escape.md` must also surface -32602.
    let call = mcp
        .rpc(
            5,
            "tools/call",
            json!({
                "name": "add_markdown",
                "arguments": {
                    "corpus": "wiki",
                    "path": "../escape.md",
                    "content": "# escape attempt\n",
                }
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("path-escape must error, got: {call}"));
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "path-escape rejection must use invalid_params: {error}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_read_markdown_returns_verbatim_content_and_rejects_unsafe_inputs() {
    // read_markdown does not touch the embedder; this test runs offline.
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let body = "# Halloumi\n\nA grilling cheese.\n";
    std::fs::create_dir_all(corpus.path().join("cheeses")).expect("mkdir");
    std::fs::write(corpus.path().join("cheeses/halloumi.md"), body).expect("seed");
    let cfg = write_config_with_corpus(xdg.path(), "wiki", &corpus.path().to_string_lossy());
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    // Happy path — full file content round-trips.
    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/halloumi.md"}
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "read_markdown errored: {call}");
    let result = &call["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text content present");
    assert_eq!(text, body, "verbatim content mismatch");
    let structured = &result["structuredContent"];
    assert_eq!(structured["corpus"].as_str(), Some("wiki"));
    assert_eq!(structured["path"].as_str(), Some("cheeses/halloumi.md"));
    assert_eq!(structured["bytes"].as_u64(), Some(body.len() as u64));
    assert_eq!(structured["content"].as_str(), Some(body));

    // Missing file → invalid_params (-32602), not internal_error.
    let call = mcp
        .rpc(
            3,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/gone.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("missing file must error, got: {call}"));
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "missing-file: {error}"
    );

    // Parent-escape → invalid_params.
    let call = mcp
        .rpc(
            4,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "../escape.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("path-escape must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "escape: {error}");

    mcp.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_read_markdown_rejects_symlink_inside_corpus() {
    use std::os::unix::fs::symlink;
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    std::fs::write(outside.path(), "secret\n").expect("write outside");
    symlink(outside.path(), corpus.path().join("leak.md")).expect("symlink");
    let cfg = write_config_with_corpus(xdg.path(), "wiki", &corpus.path().to_string_lossy());
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "leak.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("symlink read must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "symlink: {error}");
    let msg = error["message"].as_str().unwrap_or("");
    assert!(msg.contains("symlink"), "message: {msg}");

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_delete_markdown_unlinks_file_and_errors_on_repeat() {
    // delete_markdown opens the LanceStore but never builds an embedder, so
    // it runs offline. The store starts empty — delete-by-ref simply removes
    // zero rows when no index has been built yet.
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");
    std::fs::create_dir_all(corpus.path().join("cheeses")).expect("mkdir");
    let target = corpus.path().join("cheeses/halloumi.md");
    std::fs::write(&target, "# Halloumi\n").expect("seed");
    let cfg = write_config_with_corpus_and_ground(
        xdg.path(),
        "wiki",
        &corpus.path().to_string_lossy(),
        ground.path(),
    );
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/halloumi.md"}
            }),
        )
        .await;
    assert!(
        call.get("error").is_none(),
        "delete_markdown errored: {call}"
    );
    let structured = &call["result"]["structuredContent"];
    assert_eq!(structured["corpus"].as_str(), Some("wiki"));
    assert_eq!(structured["path"].as_str(), Some("cheeses/halloumi.md"));
    assert!(!target.exists(), "file should be unlinked");

    // Second delete on the same path → invalid_params (file gone).
    let call = mcp
        .rpc(
            3,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/halloumi.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("re-delete must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "re-delete: {error}");

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_delete_markdown_rejects_parent_escape() {
    // Parent-escape paths must be caught by `safe_relative_path` before any
    // syscall, matching the contract `add_markdown` already enforces.
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    std::fs::write(outside.path(), "secret\n").expect("write outside");
    let cfg = write_config_with_corpus_and_ground(
        xdg.path(),
        "wiki",
        &corpus.path().to_string_lossy(),
        ground.path(),
    );
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {"corpus": "wiki", "path": "../escape.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("parent-escape must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "escape: {error}");
    assert!(
        outside.path().exists(),
        "outside file must not be touched by failed delete"
    );

    mcp.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_delete_markdown_rejects_symlink_inside_corpus() {
    // A symlink whose target is OUTSIDE the corpus must not be unlinked —
    // and the target file itself must survive untouched.
    use std::os::unix::fs::symlink;
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    std::fs::write(outside.path(), "secret\n").expect("write outside");
    symlink(outside.path(), corpus.path().join("leak.md")).expect("symlink");
    let cfg = write_config_with_corpus_and_ground(
        xdg.path(),
        "wiki",
        &corpus.path().to_string_lossy(),
        ground.path(),
    );
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {"corpus": "wiki", "path": "leak.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("symlink delete must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "symlink: {error}");
    let msg = error["message"].as_str().unwrap_or("");
    assert!(msg.contains("symlink"), "message: {msg}");
    // The symlink itself and its target must both survive.
    assert!(
        corpus.path().join("leak.md").exists(),
        "symlink must not be unlinked"
    );
    assert!(outside.path().exists(), "target must not be touched");

    mcp.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_delete_markdown_rejects_intermediate_symlinked_directory() {
    // A symlinked intermediate directory (e.g. `corpus/cheeses` → /private/etc)
    // must not let `delete_markdown` reach files outside the corpus.
    // Pre-hardening, `tokio::fs::remove_file` would follow the dir symlink.
    use std::os::unix::fs::symlink;
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");
    let outside_dir = tempfile::tempdir().expect("outside dir");
    let outside_file = outside_dir.path().join("victim.md");
    std::fs::write(&outside_file, "do not delete\n").expect("seed victim");
    symlink(outside_dir.path(), corpus.path().join("cheeses")).expect("symlink dir");
    let cfg = write_config_with_corpus_and_ground(
        xdg.path(),
        "wiki",
        &corpus.path().to_string_lossy(),
        ground.path(),
    );
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/victim.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("intermediate symlink must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "intermediate: {error}");
    assert!(
        outside_file.exists(),
        "file behind symlinked dir must not be unlinked"
    );

    mcp.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_read_markdown_rejects_intermediate_symlinked_directory() {
    // Same shape as the delete test, for read: a symlinked intermediate
    // directory must not let `read_markdown` exfiltrate files outside the
    // corpus. Pre-hardening, `tokio::fs::read` would follow the dir symlink.
    use std::os::unix::fs::symlink;
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let outside_dir = tempfile::tempdir().expect("outside dir");
    std::fs::write(outside_dir.path().join("secret.md"), "secret contents\n")
        .expect("seed secret");
    symlink(outside_dir.path(), corpus.path().join("cheeses")).expect("symlink dir");
    let cfg = write_config_with_corpus(xdg.path(), "wiki", &corpus.path().to_string_lossy());
    let harness = DaemonHarness::spawn(cfg).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
    mcp.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "hallouminate-test", "version": "0.0.0"}
        }),
    )
    .await;
    mcp.notify("notifications/initialized", json!({})).await;

    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "cheeses/secret.md"}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("intermediate symlink must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "intermediate: {error}");

    mcp.shutdown().await;
}
