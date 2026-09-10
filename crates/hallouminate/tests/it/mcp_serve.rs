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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hallouminate_config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::common::daemon::DaemonHarness;

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct Mcp {
    child: Child,
    // `Option` so the cooperative `shutdown` path can `take()` and drop
    // stdin to signal EOF without moving out of a struct that owns a
    // `Drop` impl — see the impl block below.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    // Directory `rpc()` auto-injects as `arguments.cwd` on `tools/call`
    // requests that omit it. Set from `spawn_with_cwd`'s `cwd` argument.
    cwd: PathBuf,
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
        Self::spawn_with_cwd(xdg_config_home, xdg_config_home, daemon_socket, true).await
    }

    async fn spawn_with_cwd(
        xdg_config_home: &Path,
        cwd: &Path,
        daemon_socket: Option<&Path>,
        seed_repo_config_in_cwd: bool,
    ) -> Self {
        // Most MCP tests exercise the fallback cwd path: set the child's cwd
        // to a tempdir with an empty repo layer so daemon discovery resolves
        // cleanly. Empty TOML → `Config::default()` → trivial merge against
        // whichever baseline the per-test daemon was booted with.
        if seed_repo_config_in_cwd {
            let hallou_dir = cwd.join(".hallouminate");
            std::fs::create_dir_all(&hallou_dir).expect("mkdir .hallouminate");
            std::fs::write(hallou_dir.join("config.toml"), "")
                .expect("write empty repo-layer config");
        }

        let bin = env!("CARGO_BIN_EXE_hallouminate");
        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .current_dir(cwd)
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
            cwd: cwd.to_path_buf(),
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

    /// Injects `cwd` into `tools/call` arguments that omit it, then dispatches
    /// via `rpc_raw`. Keeps the ~15 pre-existing call sites exercising the
    /// directory the test harness set up without per-call-site edits. Tests
    /// asserting the `cwd` contract itself (AC-1/AC-2/AC-3/AC-4) must pass
    /// `cwd` explicitly or use `rpc_raw` so this injection is never the thing
    /// under test.
    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        let params = self.inject_cwd(method, params);
        self.rpc_raw(id, method, params).await
    }

    fn inject_cwd(&self, method: &str, mut params: Value) -> Value {
        if method == "tools/call"
            && let Some(args) = params.get_mut("arguments").and_then(Value::as_object_mut)
            && !args.contains_key("cwd")
        {
            args.insert("cwd".to_string(), json!(self.cwd.to_string_lossy()));
        }
        params
    }

    async fn rpc_raw(&mut self, id: u64, method: &str, params: Value) -> Value {
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

    async fn rpc_batch(&mut self, requests: Vec<(u64, Value)>) -> Vec<Value> {
        for (id, params) in &requests {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": params,
            }))
            .await;
        }
        let mut responses = Vec::with_capacity(requests.len());
        while responses.len() < requests.len() {
            let response = self.recv().await;
            if let Some(id) = response.get("id").and_then(Value::as_u64)
                && requests.iter().any(|(expected, _)| *expected == id)
                && !responses.iter().any(|seen: &Value| seen["id"] == id)
            {
                responses.push(response);
            }
        }
        responses.sort_by_key(|response| response["id"].as_u64().unwrap_or_default());
        responses
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

fn write_repo_config(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(dir.join(".hallouminate")).expect("mkdir .hallouminate");
    std::fs::write(
        dir.join(".hallouminate/config.toml"),
        format!(
            r#"
[[repository]]
name = "{name}"
path = "."
"#
        ),
    )
    .expect("write repo config");
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
    cfg.embeddings.enabled = false;
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
    let corpora = structured["corpora"]
        .as_array()
        .expect("structuredContent.corpora is an array");
    assert!(
        corpora.is_empty(),
        "no corpora configured in test fixture — expected empty: {corpora:?}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_corpus_stats_reports_selection_warnings() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
    let ground_dir = tempfile::tempdir().expect("ground tempdir");
    let corpus_root = corpus_dir.path().to_string_lossy();
    let config_toml = format!(
        r#"
[[corpus]]
name = "wiki"
paths = ["{corpus_root}"]
globs = ["docs/**/*.md"]
exclude = ["drafts/**"]

[embeddings]
enabled = false

[storage]
ground_dir = "{ground}"
"#,
        ground = ground_dir.path().display(),
    );
    let config_dir = xdg.path().join("hallouminate");
    std::fs::create_dir_all(&config_dir).expect("mkdir config dir");
    std::fs::write(config_dir.join("config.toml"), &config_toml).expect("write config");
    let cfg: Config = toml::from_str(&config_toml).expect("parse config");
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
            json!({"name": "corpus_stats", "arguments": {"corpus": "wiki"}}),
        )
        .await;
    assert!(call.get("error").is_none(), "corpus_stats errored: {call}");

    let result = &call["result"];
    let structured = &result["structuredContent"];
    let root = corpus_dir
        .path()
        .canonicalize()
        .expect("canonical corpus root");
    let warnings = vec![
        format!(
            "corpus \"wiki\" root {}: include pattern \"docs/**/*.md\" matched no files",
            root.display()
        ),
        format!(
            "corpus \"wiki\" root {}: exclude pattern \"drafts/**\" matched no files",
            root.display()
        ),
    ];
    assert_eq!(structured["corpus"], "wiki");
    assert_eq!(structured["indexed_files"], 0);
    assert_eq!(structured["total_chunks"], 0);
    assert_eq!(structured["last_indexed_ms"], Value::Null);
    assert_eq!(structured["unindexed_files"], 0);
    assert_eq!(structured["warnings"], json!(warnings));

    let text = result["content"][0]["text"]
        .as_str()
        .expect("corpus_stats text");
    for warning in warnings {
        assert!(
            text.contains(&warning),
            "text must include warning: {warning}"
        );
    }

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_list_requires_cwd_on_every_tool() {
    // AC-1: every advertised MCP tool schema requires `cwd`.
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

    let list = mcp.rpc(2, "tools/list", json!({})).await;
    assert!(list.get("error").is_none(), "tools/list errored: {list}");
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools array present");
    assert!(!tools.is_empty(), "no tools advertised: {list}");
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("?");
        let required = tool["inputSchema"]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("tool `{name}` inputSchema.required missing: {tool}"));
        let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(
            required.contains(&"cwd"),
            "tool `{name}` does not require `cwd`: {required:?}"
        );
    }

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_tool_call_without_cwd_fails_before_corpus_operation() {
    // AC-1: calls without a valid `cwd` fail before any corpus operation.
    // Uses `rpc_raw` so `rpc()`'s auto-injection can't supply the missing
    // argument under test. An ABSENT field fails in the schema layer, which
    // reports `isError` on the tool result; a PRESENT but invalid value
    // reaches `validate_cwd` and returns `-32602`
    // (`mcp_tool_invalid_cwd_never_falls_back` covers that path). Either way
    // no corpus data comes back.
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
        .rpc_raw(
            2,
            "tools/call",
            json!({"name": "list_corpora", "arguments": {}}),
        )
        .await;
    assert!(
        call.to_string().contains("cwd"),
        "the failure must name the missing `cwd` field: {call}"
    );
    assert_eq!(
        call["result"]["isError"].as_bool(),
        Some(true),
        "missing cwd must be reported as an error: {call}"
    );
    assert!(
        call["result"].get("structuredContent").is_none(),
        "missing cwd must not return corpus data: {call}"
    );

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

    let structured = result["structuredContent"]["files"]
        .as_array()
        .expect("structuredContent.files is an array");
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

    // Structured payload is { corpora: [{name, paths}, …] }.
    let structured = result["structuredContent"]["corpora"]
        .as_array()
        .expect("structuredContent.corpora is an array");
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
async fn mcp_tool_explicit_cwd_governs_resolution_not_process_cwd() {
    // AC-3: the process's own working directory never wins over an explicit
    // `cwd`. Spawn the child with a process cwd (`home`) that is NOT a repo
    // at all, but pass an explicit valid `cwd` pointing at a real repo
    // workspace on the tool call — the resolved corpus must come from the
    // argument, proving process cwd did not win.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let repo = tempfile::tempdir().expect("repo tempdir");
    let workspace = repo.path().join("packages/api");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    write_minimal_config(xdg.path());
    write_repo_config(repo.path(), "workspace");
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn_with_cwd(xdg.path(), home.path(), Some(harness.socket()), false).await;
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
                "name": "list_corpora",
                "arguments": {"cwd": workspace.to_string_lossy()}
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "tools/call errored: {call}");
    let corpora = call["result"]["structuredContent"]["corpora"]
        .as_array()
        .expect("structuredContent.corpora is an array");
    let names: Vec<&str> = corpora
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert_eq!(names, vec!["repo:workspace:wiki"]);

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_tool_invalid_cwd_never_falls_back() {
    // AC-3: an invalid `cwd` never falls back to anything. Process cwd IS a
    // valid repo (`repo`) so a fallback-to-process-cwd bug would silently
    // succeed here — asserting -32602 with no result proves the invalid
    // argument is rejected outright, never quietly substituted.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let repo = tempfile::tempdir().expect("repo tempdir");
    write_minimal_config(xdg.path());
    write_repo_config(repo.path(), "fallback");
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn_with_cwd(xdg.path(), repo.path(), Some(harness.socket()), false).await;
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

    let nonexistent = repo.path().join("does-not-exist");
    let call = mcp
        .rpc_raw(
            2,
            "tools/call",
            json!({
                "name": "list_corpora",
                "arguments": {"cwd": nonexistent.to_string_lossy()}
            }),
        )
        .await;
    let error = call
        .get("error")
        .unwrap_or_else(|| panic!("invalid cwd must error, got: {call}"));
    assert_eq!(error["code"].as_i64(), Some(-32602), "invalid cwd: {error}");
    assert!(
        call.get("result").is_none(),
        "invalid cwd must not fall back to a result: {call}"
    );

    mcp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_two_worktrees_with_identical_corpus_names_never_cross_read_or_write() {
    // AC-2: one MCP session targets two worktrees that derive the SAME corpus
    // name from their own repo layer. Only the per-request `cwd` distinguishes
    // them, so a stale or shared directory would silently serve the wrong
    // worktree's bytes. The fixture disables embeddings, so writes stay offline.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let worktree_a = tempfile::tempdir().expect("worktree a");
    let worktree_b = tempfile::tempdir().expect("worktree b");
    write_minimal_config(xdg.path());
    write_repo_config(worktree_a.path(), "proj");
    write_repo_config(worktree_b.path(), "proj");
    let body_a = "# A\n\nWorktree A content.\n";
    let body_b = "# B\n\nWorktree B content, deliberately different.\n";
    let wiki_a = worktree_a.path().join(".hallouminate/wiki");
    let wiki_b = worktree_b.path().join(".hallouminate/wiki");
    std::fs::create_dir_all(&wiki_a).expect("mkdir wiki a");
    std::fs::create_dir_all(&wiki_b).expect("mkdir wiki b");
    std::fs::write(wiki_a.join("notes.md"), body_a).expect("seed a");
    std::fs::write(wiki_b.join("notes.md"), body_b).expect("seed b");
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn_with_cwd(xdg.path(), xdg.path(), Some(harness.socket()), false).await;
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

    // Send both reads before receiving either response to exercise concurrent
    // same-session requests with identical corpus names.
    let reads = mcp
        .rpc_batch(vec![
            (
                2,
                json!({"name":"read_markdown","arguments":{"cwd":worktree_a.path().to_string_lossy(),"corpus":"repo:proj:wiki","path":"notes.md"}}),
            ),
            (
                3,
                json!({"name":"read_markdown","arguments":{"cwd":worktree_b.path().to_string_lossy(),"corpus":"repo:proj:wiki","path":"notes.md"}}),
            ),
        ])
        .await;
    for (call, expected, worktree) in [
        (&reads[0], body_a, worktree_a.path()),
        (&reads[1], body_b, worktree_b.path()),
    ] {
        assert!(call.get("error").is_none(), "read_markdown errored: {call}");
        let structured = &call["result"]["structuredContent"];
        assert_eq!(structured["corpus"].as_str(), Some("repo:proj:wiki"));
        assert_eq!(
            structured["content"].as_str(),
            Some(expected),
            "worktree {} must serve its own content, not the sibling's",
            worktree.display()
        );
    }

    // Send both writes before receiving either response. Embeddings stay
    // disabled in this fixture, so the regression remains offline.
    let writes = mcp
        .rpc_batch(vec![
            (
                4,
                json!({"name":"add_markdown","arguments":{"cwd":worktree_a.path().to_string_lossy(),"corpus":"repo:proj:wiki","path":"written.md","content":"# Written A\n"}}),
            ),
            (
                5,
                json!({"name":"add_markdown","arguments":{"cwd":worktree_b.path().to_string_lossy(),"corpus":"repo:proj:wiki","path":"written.md","content":"# Written B\n"}}),
            ),
        ])
        .await;
    assert!(
        writes.iter().all(|call| call.get("error").is_none()),
        "writes errored: {writes:?}"
    );
    assert_eq!(
        std::fs::read_to_string(wiki_a.join("written.md")).expect("written A"),
        "# Written A\n"
    );
    assert_eq!(
        std::fs::read_to_string(wiki_b.join("written.md")).expect("written B"),
        "# Written B\n"
    );

    // Existing escape check remains below.

    // A rejected mutation aimed at worktree A must not touch either worktree.
    let escape = mcp
        .rpc(
            6,
            "tools/call",
            json!({
                "name": "delete_markdown",
                "arguments": {
                    "cwd": worktree_a.path().to_string_lossy(),
                    "corpus": "repo:proj:wiki",
                    "path": "../escape.md"
                }
            }),
        )
        .await;
    let error = escape
        .get("error")
        .unwrap_or_else(|| panic!("parent escape must error, got: {escape}"));
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "parent escape: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(wiki_a.join("notes.md")).expect("read a"),
        body_a,
        "worktree A must be unchanged after a rejected write"
    );
    assert_eq!(
        std::fs::read_to_string(wiki_b.join("notes.md")).expect("read b"),
        body_b,
        "worktree B must be unchanged after a rejected write aimed at A"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test]
async fn mcp_read_markdown_defaults_corpus_to_repo_wiki_when_omitted() {
    // read_markdown does not touch the embedder; this test runs offline.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let repo = tempfile::tempdir().expect("repo tempdir");
    let workspace = repo.path().join("packages/api");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    write_minimal_config(xdg.path());
    write_repo_config(repo.path(), "workspace");
    let body = "# Halloumi\n\nA grilling cheese.\n";
    let wiki = repo.path().join(".hallouminate/wiki");
    std::fs::create_dir_all(wiki.join("cheeses")).expect("mkdir wiki");
    std::fs::write(wiki.join("cheeses/halloumi.md"), body).expect("seed wiki file");
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn_with_cwd(xdg.path(), home.path(), Some(harness.socket()), false).await;
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

    // Omitting `corpus` must resolve to the wiki of the repo containing the
    // request's explicit `cwd` — not fail with a missing-field schema error.
    // Process cwd (`home`) is not a repo at all, proving the explicit `cwd`
    // argument, not process cwd, governs resolution.
    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {
                    "cwd": workspace.to_string_lossy(),
                    "path": "cheeses/halloumi.md"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "read_markdown errored: {call}");
    let result = &call["result"];
    let structured = &result["structuredContent"];
    assert_eq!(structured["corpus"].as_str(), Some("repo:workspace:wiki"));
    assert_eq!(structured["path"].as_str(), Some("cheeses/halloumi.md"));
    assert_eq!(structured["content"].as_str(), Some(body));

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_read_markdown_rejects_absolute_path_argument() {
    // AC-4: document arguments are corpus-relative. An absolute path is not a
    // permitted document argument even when it names a real file, and even
    // when that file sits inside the corpus root — absolute paths identify
    // results in provenance fields, never inputs.
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let inside = corpus.path().join("halloumi.md");
    std::fs::write(&inside, "# Halloumi\n").expect("seed corpus file");
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

    for (id, absolute) in [
        (2, inside.to_string_lossy().into_owned()),
        (3, "/etc/passwd".to_string()),
    ] {
        let call = mcp
            .rpc(
                id,
                "tools/call",
                json!({
                    "name": "read_markdown",
                    "arguments": {"corpus": "wiki", "path": absolute}
                }),
            )
            .await;
        let error = call
            .get("error")
            .unwrap_or_else(|| panic!("absolute path {absolute} must error, got: {call}"));
        assert_eq!(
            error["code"].as_i64(),
            Some(-32602),
            "absolute path {absolute}: {error}"
        );
        assert!(
            call.get("result").is_none(),
            "absolute path {absolute} must not return a result: {call}"
        );
    }

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
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "intermediate: {error}"
    );
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
    std::fs::write(outside_dir.path().join("secret.md"), "secret contents\n").expect("seed secret");
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
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "intermediate: {error}"
    );

    mcp.shutdown().await;
}

// ── footnote-mode tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_tools_list_excludes_get_footnote_and_includes_backlinks() {
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

    let list = mcp.rpc(2, "tools/list", json!({})).await;
    assert!(list.get("error").is_none(), "tools/list errored: {list}");
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        !names.contains(&"get_footnote"),
        "`get_footnote` was removed from the MCP surface but is still in the tool list: {names:?}"
    );
    assert!(
        names.contains(&"backlinks"),
        "`backlinks` missing from tool list: {names:?}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_initialize_instructions_carry_backlinks_cue_not_get_footnote() {
    let xdg = tempfile::tempdir().expect("tempdir");
    write_minimal_config(xdg.path());
    let harness = DaemonHarness::spawn(load_minimal_config(xdg.path())).await;

    let mut mcp = Mcp::spawn(xdg.path(), Some(harness.socket())).await;
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
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions present");
    assert!(
        instructions.contains("backlinks"),
        "instructions must mention `backlinks`: {instructions:?}"
    );
    assert!(
        instructions.contains("before editing"),
        "instructions must carry the backlinks edit-safety cue: {instructions:?}"
    );
    assert!(
        !instructions.contains("get_footnote"),
        "instructions still mention the removed `get_footnote` tool: {instructions:?}"
    );

    mcp.notify("notifications/initialized", json!({})).await;
    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_read_markdown_footnotes_only_returns_definition_block() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let body = "# Article\n\nClaim[^1] is well supported.\n\n[^1]: Author 2024, src/foo.rs:42\n";
    std::fs::write(corpus.path().join("article.md"), body).expect("seed");
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

    // footnotes: "only" — text block should contain only the definition line(s).
    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {
                    "corpus": "wiki",
                    "path": "article.md",
                    "footnotes": "only"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "read_markdown errored: {call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text present");
    assert!(
        text.contains("[^1]:"),
        "footnote definition missing from 'only' result: {text:?}"
    );
    assert!(
        !text.contains("# Article"),
        "body should not appear in 'only' result: {text:?}"
    );
    // structured content stays verbatim
    let structured_content = call["result"]["structuredContent"]["content"]
        .as_str()
        .expect("structured content present");
    assert_eq!(
        structured_content, body,
        "structured content must stay verbatim regardless of footnotes mode"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_read_markdown_footnotes_exclude_strips_markers_and_defs() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let body = "# Article\n\nClaim[^1] is true.\n\n[^1]: Source URL.\n";
    std::fs::write(corpus.path().join("article.md"), body).expect("seed");
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
                "arguments": {
                    "corpus": "wiki",
                    "path": "article.md",
                    "footnotes": "exclude"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "read_markdown errored: {call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text present");
    assert!(
        !text.contains("[^"),
        "no footnote markers/defs should appear in 'exclude' result: {text:?}"
    );
    assert!(
        text.contains("Claim is true."),
        "body text should survive 'exclude': {text:?}"
    );

    mcp.shutdown().await;
}

#[tokio::test]
async fn mcp_read_markdown_footnotes_include_omitted_is_verbatim() {
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let body = "# Article\n\nClaim[^1].\n\n[^1]: Evidence.\n";
    std::fs::write(corpus.path().join("article.md"), body).expect("seed");
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

    // No `footnotes` param — default is "include" = verbatim.
    let call = mcp
        .rpc(
            2,
            "tools/call",
            json!({
                "name": "read_markdown",
                "arguments": {"corpus": "wiki", "path": "article.md"}
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "read_markdown errored: {call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text present");
    assert_eq!(
        text, body,
        "omitted footnotes param must pass content verbatim"
    );

    mcp.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the real embedder and may download a model on first run"]
async fn mcp_ground_footnotes_exclude_strips_markers() {
    // Verifies that `ground` with `footnotes:"exclude"` returns snippets and an
    // outline with no `[^...]` markers, while the default (omitted) case
    // returns them intact. Requires the real embedder (model download).
    let xdg = tempfile::tempdir().expect("tempdir");
    let corpus = tempfile::tempdir().expect("corpus tempdir");
    let ground = tempfile::tempdir().expect("ground tempdir");

    // Seed a markdown file with a footnote reference and definition.
    let body = "# Evidence\n\nThe sky is blue.[^src] and remains visible.\n\nInline literal `[^literal]`.\n\n~~~\nFenced literal [^fenced]\n~~~\n\n[^src]: physics/optics.rs:42\n\nAfter the citation.";
    std::fs::write(corpus.path().join("evidence.md"), body).expect("seed");

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

    // Index first so ground has something to search.
    let idx = mcp
        .rpc(
            2,
            "tools/call",
            json!({"name": "index", "arguments": {"corpus": "wiki"}}),
        )
        .await;
    assert!(idx.get("error").is_none(), "index errored: {idx}");

    // Case 1: footnotes:"exclude" — real markers are removed, while code literals survive.
    let call = mcp
        .rpc(
            3,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {
                    "query": "sky is blue",
                    "corpus": "wiki",
                    "footnotes": "exclude"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "ground errored: {call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("ground text content");
    assert!(
        !text.contains("[^src]"),
        "real footnote markers must be absent from exclude outline: {text:?}"
    );
    assert!(
        text.contains("remains visible") && text.contains("After the citation"),
        "body after marker and definition must survive exclude: {text:?}"
    );
    assert!(
        text.contains("`[^literal]`") && text.contains("[^fenced]"),
        "inline-code and fenced-code markers must survive exclude: {text:?}"
    );
    let docs = call["result"]["structuredContent"]["docs"]
        .as_object()
        .expect("exclude docs object");
    assert!(!docs.is_empty(), "exclude must return the fixture document");
    let exclude_doc = docs.values().next().expect("exclude fixture document");
    let exclude_chunks = exclude_doc["chunks"]
        .as_array()
        .expect("exclude chunks array");
    assert!(
        !exclude_chunks.is_empty(),
        "exclude must return fixture chunks"
    );
    for chunk in exclude_chunks {
        assert!(
            chunk.get("source_text").is_none(),
            "internal source_text must not cross MCP response boundary: {chunk}"
        );
        let snippet = chunk["snippet"].as_str().expect("exclude snippet");
        assert!(
            !snippet.contains("[^src]"),
            "real footnote marker must be absent from exclude snippet: {snippet:?}"
        );
    }

    // Case 1b: the same exclusion with a small limit proves trimming follows filtering.
    let call = mcp
        .rpc(
            4,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {
                    "query": "sky is blue",
                    "corpus": "wiki",
                    "footnotes": "exclude",
                    "snippet_chars": 24
                }
            }),
        )
        .await;
    assert!(
        call.get("error").is_none(),
        "trimmed ground errored: {call}"
    );
    let trimmed_docs = call["result"]["structuredContent"]["docs"]
        .as_object()
        .expect("trimmed exclude docs object");
    assert!(!trimmed_docs.is_empty(), "trimmed exclude must return docs");
    for doc in trimmed_docs.values() {
        let chunks = doc["chunks"].as_array().expect("trimmed chunks array");
        assert!(!chunks.is_empty(), "trimmed exclude must return chunks");
        for chunk in chunks {
            let snippet = chunk["snippet"].as_str().expect("trimmed snippet");
            assert_eq!(
                snippet, "# Evidence The sky is b…",
                "snippet_chars must trim the filtered snippet at the exact boundary"
            );
        }
    }

    // Case 2: footnotes omitted (default = include) — markers present.
    let call = mcp
        .rpc(
            5,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {
                    "query": "sky is blue",
                    "corpus": "wiki"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "ground errored: {call}");
    let docs = call["result"]["structuredContent"]["docs"]
        .as_object()
        .expect("include docs object");
    assert!(!docs.is_empty(), "include must return the fixture document");
    let mut found_marker = false;
    for doc in docs.values() {
        let chunks = doc["chunks"].as_array().expect("include chunks array");
        assert!(!chunks.is_empty(), "include must return fixture chunks");
        for chunk in chunks {
            assert!(
                chunk.get("source_text").is_none(),
                "internal source_text must not cross include response boundary: {chunk}"
            );
            let snippet = chunk["snippet"].as_str().expect("include snippet");
            found_marker |= snippet.contains("[^src]");
        }
    }
    assert!(
        found_marker,
        "footnote marker must appear in default (include) ground result: {docs:?}"
    );

    // Case 3: footnotes:"only" — definitions remain while body text is removed.
    let call = mcp
        .rpc(
            6,
            "tools/call",
            json!({
                "name": "ground",
                "arguments": {
                    "query": "sky is blue",
                    "corpus": "wiki",
                    "footnotes": "only"
                }
            }),
        )
        .await;
    assert!(call.get("error").is_none(), "ground only errored: {call}");
    let only_docs = call["result"]["structuredContent"]["docs"]
        .as_object()
        .expect("only docs object");
    assert!(
        !only_docs.is_empty(),
        "only must return the fixture document"
    );
    let only_doc = only_docs.values().next().expect("only fixture document");
    let only_chunks = only_doc["chunks"].as_array().expect("only chunks array");
    assert!(!only_chunks.is_empty(), "only must return fixture chunks");
    let mut found_definition = false;
    for chunk in only_chunks {
        assert!(
            chunk.get("source_text").is_none(),
            "internal source_text must not cross only response boundary: {chunk}"
        );
        let snippet = chunk["snippet"].as_str().expect("only snippet");
        found_definition |= snippet.contains("[^src]: physics/optics.rs:42");
        assert!(
            !snippet.contains("remains visible"),
            "only snippets must not contain body text: {snippet:?}"
        );
    }
    assert!(
        found_definition,
        "only snippets must contain the definition: {only_chunks:?}"
    );

    mcp.shutdown().await;
}
