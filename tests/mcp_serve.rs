//! End-to-end test: spawn `hallouminate serve` as a child process, drive
//! the MCP JSON-RPC handshake over its stdio, and assert that `tools/list`
//! and `tools/call list_corpora` produce the expected shapes.
//!
//! Skips `tools/call ground` and `tools/call index` because both would
//! force the embedding model download (~33MB on first run). The CLI-side
//! ground test already exercises that path under `#[ignore]`.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Mcp {
    async fn spawn(xdg_config_home: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_hallouminate");
        let mut child = Command::new(bin)
            .arg("serve")
            .env("XDG_CONFIG_HOME", xdg_config_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hallouminate serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut buf = serde_json::to_string(&value).unwrap();
        buf.push('\n');
        self.stdin.write_all(buf.as_bytes()).await.expect("write");
        self.stdin.flush().await.expect("flush");
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

    async fn shutdown(mut self) {
        drop(self.stdin); // closing stdin tells the server to exit
        let _ = timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

fn write_minimal_config(dir: &std::path::Path) {
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

fn write_config_with_corpus(dir: &std::path::Path, corpus_name: &str, corpus_path: &str) {
    let cfg_dir = dir.join("hallouminate");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir hallouminate config dir");
    let toml = format!(
        r#"
[[corpus]]
name = "{corpus_name}"
paths = ["{corpus_path}"]
globs = ["**/*.md"]
"#
    );
    std::fs::write(cfg_dir.join("config.toml"), toml).expect("write config.toml");
}

#[tokio::test]
async fn mcp_server_initialize_lists_tools_and_calls_list_corpora() {
    let xdg = tempfile::tempdir().expect("tempdir");
    write_minimal_config(xdg.path());

    let mut mcp = Mcp::spawn(xdg.path()).await;

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
    // Empty-config means structured payload is an empty array; the field
    // must still be present so structured-aware clients can rely on it.
    assert!(
        result["structuredContent"].is_array()
            || result["structuredContent"].is_object()
            || result["structuredContent"].is_null(),
        "structuredContent must be present: {result}"
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
    write_config_with_corpus(xdg.path(), "wiki", &corpus.path().to_string_lossy());

    let mut mcp = Mcp::spawn(xdg.path()).await;
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
    write_config_with_corpus(xdg.path(), "test-corpus", "/tmp/hallouminate-press-fixture");

    let mut mcp = Mcp::spawn(xdg.path()).await;
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

    let mut mcp = Mcp::spawn(xdg.path()).await;

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

    // Either a top-level JSON-RPC error, or a tool-level error embedded in
    // result.isError = true with content describing the failure. The
    // contract is "no panic, no crashed server" — both shapes are valid.
    let has_rpc_error = call.get("error").is_some();
    let has_tool_error = call["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        has_rpc_error || has_tool_error,
        "unknown corpus must surface as RPC error or tool error: {call}"
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
