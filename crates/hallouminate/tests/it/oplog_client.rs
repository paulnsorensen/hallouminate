//! Operations log: `client_call` rows of the MCP server and the CLI, and
//! their join with `daemon_op` rows on `request_id`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use hallouminate_config::LoggingConfig;
use hallouminate_daemon::oplog::{
    Client, ClientCall, ClientCallFields, OpLog, OperationRecord, Outcome, Reason, RecordKind,
    RequestId,
};
use hallouminate_daemon::{
    DaemonRequest, DaemonRequestPayload, DaemonRpcError, ErrorKind, connect_at,
};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use crate::common::daemon::{DaemonHarness, read_segment_rows};
use crate::common::oplog_support::{
    assert_present, assert_stage_sum_within_duration, docs_config, dropped_connection_row,
    unreachable_ground_row,
};
use crate::mcp_serve::{Mcp, write_minimal_config, write_repo_config};

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const COLD_WAIT_OFF: &str = "\n[search]\ncold_wait_ms = 0\n";
const FORCED_DEBT: &str =
    "\n[daemon]\nhard_block_wait_secs = 0\nmaintenance_interval_secs = 3600\n";
const ALLOWED_TIME_KEYS: [&str; 2] = ["took_ms", "last_indexed_ms"];
const TIMING_KEYS: [&str; 5] = ["timing", "timings", "stages", "duration", "elapsed"];
const DAEMON_START_BUDGET: Duration = Duration::from_secs(60);
const DAEMON_STOP_BUDGET: Duration = Duration::from_secs(30);
const ROW_WAIT: Duration = Duration::from_secs(5);

struct McpFixture {
    tmp: tempfile::TempDir,
    corpus_root: PathBuf,
    harness: DaemonHarness,
    mcp: Mcp,
}

async fn mcp_fixture(daemon_extra: &str, serve_config: &str) -> McpFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cfg, corpus_root) = docs_config(tmp.path(), daemon_extra);
    let logging = cfg.logging;
    let harness = DaemonHarness::spawn_with_oplog(cfg, logging).await;
    let xdg = tmp.path().join("xdg");
    write_minimal_config(&xdg);
    std::fs::write(xdg.join("hallouminate/config.toml"), serve_config).expect("serve config");
    let mcp = initialized_mcp(&xdg, harness.cwd(), harness.socket()).await;
    McpFixture {
        tmp,
        corpus_root,
        harness,
        mcp,
    }
}

async fn initialized_mcp(xdg: &Path, cwd: &Path, socket: &Path) -> Mcp {
    let mut mcp = Mcp::spawn_with_cwd(xdg, cwd, Some(socket), false).await;
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
    mcp
}

async fn tool(mcp: &mut Mcp, id: u64, name: &str, arguments: Value) -> Value {
    mcp.rpc(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
}

fn add_markdown(path: &str, content: &str) -> Value {
    json!({ "corpus": "docs", "path": path, "content": content })
}

fn checked_request_id(id: &str, response: &Value) -> String {
    let Ok(parsed) = id.parse::<RequestId>() else {
        panic!("request_id {id} is not 32 lowercase hex chars: {response}");
    };
    parsed.as_str().to_owned()
}

fn ok_request_id(response: &Value) -> String {
    assert!(
        response.get("error").is_none(),
        "tool call errored: {response}"
    );
    let Some(id) = response["result"]["structuredContent"]["request_id"].as_str() else {
        panic!("structuredContent carries no request_id: {response}");
    };
    checked_request_id(id, response)
}

fn error_request_id(response: &Value) -> String {
    let Some(id) = response["error"]["data"]["request_id"].as_str() else {
        panic!("error data carries no request_id: {response}");
    };
    checked_request_id(id, response)
}

fn response_request_id(response: &Value) -> String {
    match response.get("error") {
        Some(_error) => error_request_id(response),
        None => ok_request_id(response),
    }
}

fn error_code(response: &Value) -> Option<i64> {
    response["error"]["code"].as_i64()
}

fn row_with_id(rows: &[OperationRecord], request_id: &str) -> OperationRecord {
    let mut found = Vec::new();
    for row in rows {
        if row.request_id.as_str() == request_id {
            found.push(row);
        }
    }
    let [row] = found.as_slice() else {
        panic!("expected one row for {request_id}, got {found:?}");
    };
    (*row).clone()
}

fn client_fields(row: &OperationRecord) -> &ClientCallFields {
    match &row.kind {
        RecordKind::ClientCall(fields) => fields,
        RecordKind::DaemonOp(fields) => panic!("expected a client_call row: {fields:?}"),
    }
}

fn stage_names(fields: &ClientCallFields) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    for name in fields.stages.as_map().keys() {
        names.insert(name.as_str());
    }
    names
}

fn nanos(ms: f64) -> u64 {
    (ms * 1_000_000.0).round() as u64
}

fn assert_daemon_fields_null(row: &OperationRecord) {
    let fields = client_fields(row);
    assert_eq!(fields.daemon_round_trip_ms, None, "{row:?}");
    assert_eq!(fields.response_bytes, None, "{row:?}");
}

fn assert_unanswered(row: &OperationRecord, outcome: Outcome, stages: &[&str]) {
    assert_eq!(row.outcome, outcome, "{row:?}");
    assert_daemon_fields_null(row);
    let mut expected = BTreeSet::new();
    for stage in stages {
        expected.insert(*stage);
    }
    assert_eq!(stage_names(client_fields(row)), expected, "{row:?}");
}

/// Asserts that the client row and the daemon row of one call agree, and
/// that the client timings nest: the round trip covers the daemon duration,
/// and the stage sum plus the round trip fits in the handler duration.
fn assert_joined(
    client: &OperationRecord,
    daemon: &OperationRecord,
    expected: (Client, &str, Outcome),
) {
    let (client_kind, tool, outcome) = expected;
    match &daemon.kind {
        RecordKind::DaemonOp(_) => {}
        RecordKind::ClientCall(fields) => panic!("expected a daemon_op row: {fields:?}"),
    }
    let fields = client_fields(client);
    assert_eq!(fields.client, client_kind, "{client:?}");
    assert_eq!(fields.tool, tool, "{client:?}");
    assert_eq!(client.request_id, daemon.request_id);
    assert_eq!(client.op, daemon.op);
    assert_eq!(client.corpus, daemon.corpus);
    assert_eq!(client.schema_version, daemon.schema_version);
    assert_eq!(client.version, daemon.version);
    assert_eq!(client.outcome, outcome, "{client:?}");
    assert_eq!(daemon.outcome, outcome, "{daemon:?}");
    let Some(round_trip) = fields.daemon_round_trip_ms else {
        panic!("answered call has a null round trip: {client:?}");
    };
    assert!(
        round_trip.as_f64() >= daemon.duration_ms.as_f64(),
        "round trip {round_trip:?} is below the daemon duration {:?}",
        daemon.duration_ms
    );
    assert!(
        fields.response_bytes.is_some_and(|bytes| bytes > 0),
        "{client:?}"
    );
    assert_eq!(
        stage_names(fields),
        BTreeSet::from(["render", "setup"]),
        "{client:?}"
    );
    let mut sum = nanos(round_trip.as_f64());
    for elapsed in fields.stages.as_map().values() {
        sum += nanos(elapsed.as_f64());
    }
    let duration = nanos(client.duration_ms.as_f64());
    assert!(
        sum <= duration,
        "stage sum plus round trip {sum} ns exceeds duration {duration} ns: {client:?}"
    );
}

/// Makes one `add_markdown` call and asserts that it writes exactly one
/// client row, joined to its daemon row with `outcome`. Returns the response.
async fn single_add_markdown_row(fixture: McpFixture, arguments: Value, outcome: Outcome) -> Value {
    let McpFixture {
        tmp: _tmp,
        harness,
        mut mcp,
        ..
    } = fixture;
    let response = tool(&mut mcp, 2, "add_markdown", arguments).await;
    let id = response_request_id(&response);

    let rows = mcp.client_rows();
    let [client] = rows.as_slice() else {
        panic!("one client row per tool call: {rows:?}");
    };
    assert_eq!(client.request_id.as_str(), id);
    assert_eq!(client.op, "add_markdown");
    assert_eq!(client.corpus.as_deref(), Some("docs"), "{client:?}");
    let daemon = harness.row_for(&id).await;
    assert_joined(client, &daemon, (Client::Mcp, "add_markdown", outcome));
    mcp.shutdown().await;
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_ok() {
    let fixture = mcp_fixture("", "").await;
    let response =
        single_add_markdown_row(fixture, add_markdown("ok.md", "# Ok\n"), Outcome::Ok).await;
    assert!(response.get("error").is_none(), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_invalid_params() {
    let fixture = mcp_fixture("", "").await;
    let response = single_add_markdown_row(
        fixture,
        add_markdown("../escape.md", "# Escape\n"),
        Outcome::InvalidParams,
    )
    .await;
    assert_eq!(error_code(&response), Some(INVALID_PARAMS), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_internal() {
    let fixture = mcp_fixture("", "").await;
    fixture.harness.state().write_lane().close();
    let response = single_add_markdown_row(
        fixture,
        add_markdown("internal.md", "# Internal\n"),
        Outcome::Internal,
    )
    .await;
    assert_eq!(error_code(&response), Some(INTERNAL_ERROR), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_retryable() {
    let fixture = mcp_fixture(FORCED_DEBT, "").await;
    fixture.harness.state().force_hard_debt_for_tests();
    let response = single_add_markdown_row(
        fixture,
        add_markdown("retry.md", "# Retry\n"),
        Outcome::Retryable,
    )
    .await;
    assert_eq!(
        response["error"]["data"]["retryable"].as_bool(),
        Some(true),
        "{response}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_batch_read_records_tool_outcome() {
    let McpFixture {
        tmp: _tmp,
        corpus_root,
        harness,
        mut mcp,
    } = mcp_fixture("", "").await;
    std::fs::write(corpus_root.join("a.md"), "# A\n").expect("seed a");
    std::fs::write(corpus_root.join("c.md"), "# C\n").expect("seed c");
    let response = tool(
        &mut mcp,
        2,
        "read_markdown",
        json!({
            "corpus": "docs",
            "items": [{"path": "a.md"}, {"path": "missing.md"}, {"path": "c.md"}]
        }),
    )
    .await;
    let id = ok_request_id(&response);
    let results = response["result"]["structuredContent"]["results"]
        .as_array()
        .expect("results");
    assert_eq!(results[1]["error"]["code"], INVALID_PARAMS, "{response}");

    let rows = mcp.client_rows();
    let [client] = rows.as_slice() else {
        panic!("one client row per batch call: {rows:?}");
    };
    assert_eq!(client.request_id.as_str(), id);
    assert_eq!(client.outcome, Outcome::Ok, "{client:?}");

    let deadline = Instant::now() + ROW_WAIT;
    let daemon = loop {
        let mut joined = Vec::new();
        for row in harness.rows() {
            if row.request_id.as_str() == id {
                joined.push(row);
            }
        }
        if joined.len() >= 3 || Instant::now() >= deadline {
            break joined;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(daemon.len(), 3, "{daemon:?}");
    let mut outcomes = Vec::new();
    let mut daemon_ns = 0;
    for row in &daemon {
        outcomes.push(row.outcome);
        daemon_ns += nanos(row.duration_ms.as_f64());
    }
    outcomes.sort_by_key(|outcome| format!("{outcome:?}"));
    assert_eq!(
        outcomes,
        [Outcome::InvalidParams, Outcome::Ok, Outcome::Ok],
        "{daemon:?}"
    );
    let Some(round_trip) = client_fields(client).daemon_round_trip_ms else {
        panic!("answered batch has a null round trip: {client:?}");
    };
    assert!(
        nanos(round_trip.as_f64()) >= daemon_ns,
        "round trip {round_trip:?} is below the summed daemon durations: {daemon:?}"
    );
    mcp.shutdown().await;
}

/// Collects every `timing`-like key of `value` that is not an allowed time
/// field into `found`.
fn collect_timing_keys(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let timing = key.ends_with("_ms") || TIMING_KEYS.contains(&key.as_str());
                if timing && !ALLOWED_TIME_KEYS.contains(&key.as_str()) {
                    found.push(key.clone());
                }
                collect_timing_keys(nested, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_timing_keys(item, found);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn assert_no_timing(name: &str, payload: &Value) {
    let mut found = Vec::new();
    collect_timing_keys(payload, &mut found);
    assert!(
        found.is_empty(),
        "{name} payload carries timing fields {found:?}: {payload}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_payloads_carry_request_id() {
    let McpFixture {
        tmp: _tmp,
        harness: _harness,
        mut mcp,
        ..
    } = mcp_fixture(COLD_WAIT_OFF, "").await;
    let list = mcp.rpc(2, "tools/list", json!({})).await;
    let Some(listed) = list["result"]["tools"].as_array() else {
        panic!("tools/list returned no tools: {list}");
    };
    let mut exposed = BTreeSet::new();
    for listed_tool in listed {
        let Some(name) = listed_tool["name"].as_str() else {
            panic!("tool without a name: {listed_tool}");
        };
        exposed.insert(name.to_owned());
    }

    let missing = "missing-corpus";
    let calls = [
        (
            "add_markdown",
            add_markdown("page.md", "# Page\n\nThe brine is salty.\n"),
            json!({ "corpus": missing, "path": "page.md", "content": "# Page\n" }),
        ),
        (
            "index",
            json!({ "corpus": "docs" }),
            json!({ "corpus": missing }),
        ),
        (
            "ground",
            json!({ "query": "brine", "corpus": "docs" }),
            json!({ "query": "brine", "corpus": missing }),
        ),
        (
            "list_tree",
            json!({ "corpus": "docs" }),
            json!({ "corpus": missing }),
        ),
        (
            "list_files",
            json!({ "corpus": "docs" }),
            json!({ "corpus": missing }),
        ),
        (
            "read_markdown",
            json!({ "corpus": "docs", "path": "page.md" }),
            json!({ "corpus": missing, "path": "page.md" }),
        ),
        (
            "backlinks",
            json!({ "corpus": "docs", "path": "page.md" }),
            json!({ "corpus": missing, "path": "page.md" }),
        ),
        (
            "corpus_stats",
            json!({ "corpus": "docs" }),
            json!({ "corpus": missing }),
        ),
        (
            "list_corpora",
            json!({}),
            json!({ "cwd": "/nonexistent/hallouminate-request-id-test" }),
        ),
        (
            "delete_markdown",
            json!({ "corpus": "docs", "path": "page.md" }),
            json!({ "corpus": missing, "path": "page.md" }),
        ),
    ];

    let mut called = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut rpc_id = 3;
    for (name, success, failure) in calls {
        let ok = tool(&mut mcp, rpc_id, name, success).await;
        ids.insert(ok_request_id(&ok));
        assert_no_timing(name, &ok["result"]["structuredContent"]);
        let failed = tool(&mut mcp, rpc_id + 1, name, failure).await;
        ids.insert(error_request_id(&failed));
        assert_no_timing(name, &failed["error"]["data"]);
        rpc_id += 2;
        called.insert(name.to_owned());
    }

    assert_eq!(called, exposed, "every exposed tool is called");
    assert_eq!(ids.len(), 2 * called.len(), "each call has its own id");
    mcp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_side_rejection_writes_client_row_only() {
    let McpFixture {
        tmp: _tmp,
        harness,
        mut mcp,
        ..
    } = mcp_fixture("", "").await;

    let content = "a".repeat(FRAME_LIMIT + 1);
    let response = tool(
        &mut mcp,
        2,
        "add_markdown",
        json!({ "corpus": "docs", "items": [{ "path": "big.md", "content": content }] }),
    )
    .await;
    assert_eq!(error_code(&response), Some(INVALID_PARAMS), "{response}");
    let id = error_request_id(&response);

    let client = row_with_id(&mcp.client_rows(), &id);
    assert_eq!(client.op, "add_markdown_batch");
    assert_eq!(client_fields(&client).tool, "add_markdown");
    assert_unanswered(&client, Outcome::InvalidParams, &["setup"]);
    let mut daemon_rows = Vec::new();
    for row in harness.rows() {
        if row.request_id.as_str() == id {
            daemon_rows.push(row);
        }
    }
    assert!(
        daemon_rows.is_empty(),
        "the daemon never saw the request: {daemon_rows:?}"
    );
    mcp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_response_is_invalid_params_on_both_rows() {
    let McpFixture {
        tmp: _tmp,
        corpus_root,
        harness,
        mut mcp,
    } = mcp_fixture("", "").await;
    let content = format!("# Big\n\n{}\n", "a".repeat(FRAME_LIMIT));
    std::fs::write(corpus_root.join("big.md"), content).expect("write big file");

    let response = tool(
        &mut mcp,
        2,
        "read_markdown",
        json!({ "corpus": "docs", "path": "big.md" }),
    )
    .await;
    assert_eq!(error_code(&response), Some(INVALID_PARAMS), "{response}");
    let id = error_request_id(&response);

    let client = row_with_id(&mcp.client_rows(), &id);
    let daemon = harness.row_for(&id).await;
    assert_joined(
        &client,
        &daemon,
        (Client::Mcp, "read_markdown", Outcome::InvalidParams),
    );
    mcp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_log_writes_no_row_and_returns_request_id() {
    let McpFixture {
        tmp: _tmp,
        harness,
        mut mcp,
        ..
    } = mcp_fixture("", "[logging]\noperations = false\n").await;

    let ok = tool(&mut mcp, 2, "add_markdown", add_markdown("ok.md", "# Ok\n")).await;
    let ok_id = ok_request_id(&ok);
    let invalid = tool(
        &mut mcp,
        3,
        "add_markdown",
        add_markdown("../escape.md", "# Escape\n"),
    )
    .await;
    let invalid_id = error_request_id(&invalid);

    assert_eq!(harness.row_for(&ok_id).await.outcome, Outcome::Ok);
    assert_eq!(
        harness.row_for(&invalid_id).await.outcome,
        Outcome::InvalidParams
    );
    let rows = mcp.client_rows();
    assert!(rows.is_empty(), "operations = false wrote rows: {rows:?}");
    mcp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_row_write_failure_keeps_tool_result() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cfg, corpus_root) = docs_config(tmp.path(), "");
    let logging = cfg.logging;
    let harness = DaemonHarness::spawn_with_oplog(cfg, logging).await;
    let xdg = tmp.path().join("xdg");
    write_minimal_config(&xdg);
    let client_state = xdg.join("state-home").join("hallouminate");
    let today = chrono::Utc::now().date_naive();
    let tomorrow = today.succ_opt().expect("tomorrow");
    for date in [today, tomorrow] {
        let blocker = client_state.join(format!("operations-{}.jsonl", date.format("%Y-%m-%d")));
        std::fs::create_dir_all(&blocker).expect("segment-named directory");
    }
    let mut mcp = initialized_mcp(&xdg, harness.cwd(), harness.socket()).await;

    let ok = tool(
        &mut mcp,
        2,
        "add_markdown",
        add_markdown("kept.md", "# Kept\n"),
    )
    .await;
    let ok_id = ok_request_id(&ok);
    assert!(corpus_root.join("kept.md").exists());
    assert_eq!(harness.row_for(&ok_id).await.outcome, Outcome::Ok);

    let invalid = tool(
        &mut mcp,
        3,
        "add_markdown",
        add_markdown("../escape.md", "# Escape\n"),
    )
    .await;
    assert_eq!(error_code(&invalid), Some(INVALID_PARAMS), "{invalid}");
    let invalid_id = error_request_id(&invalid);
    assert_eq!(
        harness.row_for(&invalid_id).await.outcome,
        Outcome::InvalidParams
    );
    for date in [today, tomorrow] {
        let blocker = client_state.join(format!("operations-{}.jsonl", date.format("%Y-%m-%d")));
        assert!(blocker.is_dir(), "the client must not replace {blocker:?}");
    }
    mcp.shutdown().await;
}

/// Sends `status` to a listener that never answers, with a short timeout.
/// Returns the one client row and the error kind that the caller sees.
async fn timed_out_status_row() -> (OperationRecord, ErrorKind) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("silent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
    let client = connect_at(&socket).await.expect("connect");
    let state = tmp.path().join("state");
    let oplog = OpLog::in_dir(&state, &LoggingConfig::default());
    let mut call = ClientCall::start(&oplog, Client::Cli, "status", "status", None);
    call.ready();

    let error = client
        .call_raw_with_timeout_logged(
            DaemonRequest::new(tmp.path().to_path_buf(), DaemonRequestPayload::Status),
            Duration::from_millis(200),
            &mut call,
        )
        .await
        .expect_err("silent daemon must time out");
    let Some(rpc) = error.downcast_ref::<DaemonRpcError>() else {
        panic!("timeout must stay a typed daemon error: {error:#}");
    };
    let kind = rpc.kind;
    call.finish(Outcome::Internal);

    let rows = read_segment_rows(&state);
    let [row] = rows.as_slice() else {
        panic!("expected one client row: {rows:?}");
    };
    (row.clone(), kind)
}

#[tokio::test]
async fn unreachable_daemon_writes_unavailable_row() {
    let row = unreachable_ground_row().await;
    let fields = client_fields(&row);
    assert_eq!(fields.client, Client::Cli);
    assert_eq!(fields.tool, "ground");
    assert_eq!(row.op, "ground");
    assert_unanswered(&row, Outcome::Unavailable, &[]);
}

#[tokio::test]
async fn rpc_timeout_writes_cancelled_timeout_row() {
    let (row, kind) = timed_out_status_row().await;
    assert_eq!(kind, ErrorKind::Retryable);
    assert_unanswered(&row, Outcome::Cancelled(Reason::Timeout), &["setup"]);
}

#[tokio::test]
async fn dropped_connection_writes_unavailable_row() {
    let row = dropped_connection_row().await;
    assert_unanswered(&row, Outcome::Unavailable, &["setup"]);
}

#[tokio::test]
async fn unavailable_and_cancelled_rows_null_daemon_fields() {
    let unavailable = unreachable_ground_row().await;
    let dropped = dropped_connection_row().await;
    let (cancelled, _kind) = timed_out_status_row().await;
    for (row, outcome) in [
        (&unavailable, Outcome::Unavailable),
        (&dropped, Outcome::Unavailable),
        (&cancelled, Outcome::Cancelled(Reason::Timeout)),
    ] {
        assert_eq!(row.outcome, outcome, "{row:?}");
        assert_daemon_fields_null(row);
        let text = serde_json::to_string(row).expect("row json");
        let parsed: Value = serde_json::from_str(&text).expect("row json value");
        assert_eq!(parsed["daemon_round_trip_ms"], Value::Null, "{text}");
        assert_eq!(parsed["response_bytes"], Value::Null, "{text}");
        assert!(
            parsed
                .as_object()
                .is_some_and(|row| row.contains_key("daemon_round_trip_ms")
                    && row.contains_key("response_bytes")),
            "null daemon fields stay present: {text}"
        );
    }
}

async fn run_cli(args: &[&str], socket: &Path, cwd: &Path, xdg: &Path, state_home: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_hallouminate"))
        .args(args)
        .current_dir(cwd)
        .env("HALLOUMINATE_SOCKET", socket)
        .env("XDG_CONFIG_HOME", xdg)
        .env("XDG_STATE_HOME", state_home)
        .output()
        .await
        .expect("spawn hallouminate");
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn oplog_harness(tmp: &Path, extra: &str) -> DaemonHarness {
    let (cfg, _corpus_root) = docs_config(tmp, extra);
    let logging = cfg.logging;
    DaemonHarness::spawn_with_oplog(cfg, logging).await
}

/// Runs `hallouminate ground` against `harness` with a state dir under
/// `dir`, and returns its one client row.
async fn cli_ground_row(harness: &DaemonHarness, dir: &Path) -> OperationRecord {
    let xdg = dir.join("cli-xdg");
    write_minimal_config(&xdg);
    let state_home = dir.join("cli-state-home");
    run_cli(
        &["ground", "brine", "--corpus", "docs"],
        harness.socket(),
        harness.cwd(),
        &xdg,
        &state_home,
    )
    .await;
    let rows = read_segment_rows(&state_home.join("hallouminate"));
    let [row] = rows.as_slice() else {
        panic!("one client row per CLI command: {rows:?}");
    };
    row.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_row_sets_client_and_tool() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let harness = oplog_harness(tmp.path(), COLD_WAIT_OFF).await;

    let row = cli_ground_row(&harness, tmp.path()).await;
    let fields = client_fields(&row);
    assert_eq!(fields.client, Client::Cli, "{row:?}");
    assert_eq!(fields.tool, "ground", "{row:?}");
    assert_eq!(row.op, "ground", "{row:?}");
    assert_eq!(row.corpus.as_deref(), Some("docs"), "{row:?}");
    let daemon = harness.row_for(row.request_id.as_str()).await;
    assert_joined(&row, &daemon, (Client::Cli, "ground", Outcome::Ok));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_stages_setup_and_render() {
    let McpFixture {
        tmp,
        harness,
        mut mcp,
        ..
    } = mcp_fixture(COLD_WAIT_OFF, "").await;
    let response = tool(
        &mut mcp,
        2,
        "add_markdown",
        add_markdown("staged.md", "# Staged\n\nThe brine is salty.\n"),
    )
    .await;
    let id = ok_request_id(&response);
    let mcp_row = row_with_id(&mcp.client_rows(), &id);
    let cli_row = cli_ground_row(&harness, tmp.path()).await;

    for (row, client, tool_name) in [
        (&mcp_row, Client::Mcp, "add_markdown"),
        (&cli_row, Client::Cli, "ground"),
    ] {
        let daemon = harness.row_for(row.request_id.as_str()).await;
        assert_joined(row, &daemon, (client, tool_name, Outcome::Ok));
    }
    mcp.shutdown().await;
}

/// The bootstrap `Ping` probe runs only without `HALLOUMINATE_SOCKET`, and
/// then it dials the account-home socket of the real user. With the
/// isolated socket here it never runs, so this test proves only that the
/// lifecycle commands write their own rows and no `ping` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_commands_write_client_rows_and_probe_writes_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    write_minimal_config(&xdg);
    let state_home = tmp.path().join("state-home");
    let state = state_home.join("hallouminate");
    let first = oplog_harness(&tmp.path().join("first"), "").await;
    let second = oplog_harness(&tmp.path().join("second"), "").await;

    for (args, harness) in [
        (["daemon", "status"], &first),
        (["daemon", "restart"], &first),
        (["daemon", "stop"], &second),
    ] {
        run_cli(&args, harness.socket(), harness.cwd(), &xdg, &state_home).await;
    }

    let rows = read_segment_rows(&state);
    assert_eq!(rows.len(), 3, "one client row per command: {rows:?}");
    let expected = [
        (&first, "status", "status"),
        (&first, "restart", "shutdown"),
        (&second, "stop", "shutdown"),
    ];
    for (row, (harness, tool_name, op)) in rows.iter().zip(expected) {
        assert_eq!(row.op, op, "{row:?}");
        let daemon = harness.row_for(row.request_id.as_str()).await;
        assert_joined(row, &daemon, (Client::Cli, tool_name, Outcome::Ok));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_commands_without_daemon_write_unavailable_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let xdg = tmp.path().join("xdg");
    write_minimal_config(&xdg);
    let state_home = tmp.path().join("state-home");
    let socket = tmp.path().join("absent.sock");

    for args in [["daemon", "status"], ["daemon", "stop"]] {
        run_cli(&args, &socket, tmp.path(), &xdg, &state_home).await;
    }

    let rows = read_segment_rows(&state_home.join("hallouminate"));
    assert_eq!(rows.len(), 2, "one client row per command: {rows:?}");
    for (row, (tool_name, op)) in rows
        .iter()
        .zip([("status", "status"), ("stop", "shutdown")])
    {
        let fields = client_fields(row);
        assert_eq!(fields.client, Client::Cli, "{row:?}");
        assert_eq!(fields.tool, tool_name, "{row:?}");
        assert_eq!(row.op, op, "{row:?}");
        assert_eq!(row.corpus, None, "{row:?}");
        assert_unanswered(row, Outcome::Unavailable, &[]);
    }
}

/// A `hallouminate daemon` child process on an isolated socket.
struct DaemonProcess {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

fn read_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

impl DaemonProcess {
    /// Starts the daemon with its config, state, data, and cache dirs
    /// under `root` and `xdg`, and waits until its socket accepts.
    async fn start(root: &Path, xdg: &Path, state_home: &Path) -> Self {
        let socket = root.join("daemon.sock");
        let log = root.join("daemon.log");
        let stdout = std::fs::File::create(&log).expect("create daemon log");
        let stderr = stdout.try_clone().expect("clone daemon log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hallouminate"))
            .arg("daemon")
            .current_dir(root)
            .env("HALLOUMINATE_SOCKET", &socket)
            .env("XDG_CONFIG_HOME", xdg)
            .env("XDG_STATE_HOME", state_home)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn hallouminate daemon");
        let deadline = Instant::now() + DAEMON_START_BUDGET;
        while connect_at(&socket).await.is_err() {
            if let Some(status) = child.try_wait().expect("poll daemon") {
                panic!(
                    "daemon exited with {status} before it listened: {}",
                    read_log(&log)
                );
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not listen within {DAEMON_START_BUDGET:?}: {}",
                read_log(&log)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { child, socket, log }
    }

    /// Stops the daemon with `hallouminate daemon stop` and waits for the
    /// process to exit successfully.
    async fn stop(mut self, cwd: &Path, xdg: &Path, state_home: &Path) {
        run_cli(&["daemon", "stop"], &self.socket, cwd, xdg, state_home).await;
        let status = tokio::time::timeout(DAEMON_STOP_BUDGET, self.child.wait())
            .await
            .expect("daemon exits after stop")
            .expect("wait for daemon");
        assert!(
            status.success(),
            "daemon exited with {status}: {}",
            read_log(&self.log)
        );
    }
}

/// Waits for the one `client_call` row and the one `daemon_op` row of
/// `request_id` in the segments under `dir`.
async fn joined_rows(dir: &Path, request_id: &str) -> (OperationRecord, OperationRecord) {
    let deadline = Instant::now() + ROW_WAIT;
    loop {
        let rows = read_segment_rows(dir);
        let mut clients = Vec::new();
        let mut daemons = Vec::new();
        for row in &rows {
            if row.request_id.as_str() != request_id {
                continue;
            }
            match &row.kind {
                RecordKind::ClientCall(_) => clients.push(row.clone()),
                RecordKind::DaemonOp(_) => daemons.push(row.clone()),
            }
        }
        if let ([client], [daemon]) = (clients.as_slice(), daemons.as_slice()) {
            return (client.clone(), daemon.clone());
        }
        assert!(
            Instant::now() < deadline,
            "no client and daemon row pair for {request_id}: clients {clients:?}, daemons {daemons:?}, {} rows in all",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_demonstration_joins_rows_on_request_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    write_repo_config(&repo, "demo");
    let wiki = repo.join(".hallouminate/wiki");
    std::fs::create_dir_all(&wiki).expect("mkdir wiki");
    std::fs::write(
        wiki.join("config-layering.md"),
        "# Config layering\n\nThe repository config layering merges the repo layer over the XDG baseline.\n",
    )
    .expect("write wiki page");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(xdg.join("hallouminate")).expect("mkdir config dir");
    std::fs::write(
        xdg.join("hallouminate/config.toml"),
        format!(
            "[storage]\nground_dir = \"{}\"\n\n[embeddings]\nenabled = false\n",
            tmp.path().join("ground").display()
        ),
    )
    .expect("write baseline config");
    // `Mcp` points its child at this state home; the daemon shares it.
    let state_home = xdg.join("state-home");
    let daemon = DaemonProcess::start(tmp.path(), &xdg, &state_home).await;
    let mut mcp = initialized_mcp(&xdg, &repo, &daemon.socket).await;

    let indexed = tool(&mut mcp, 2, "index", json!({ "corpus": "repo:demo:wiki" })).await;
    ok_request_id(&indexed);
    let grounded = tool(
        &mut mcp,
        3,
        "ground",
        json!({ "query": "config layering", "corpus": "repo:demo:wiki" }),
    )
    .await;
    let id = ok_request_id(&grounded);
    let Some(text) = grounded["result"]["content"][0]["text"].as_str() else {
        panic!("ground returned no text: {grounded}");
    };
    assert!(text.contains("config-layering.md"), "no hit: {text}");

    let (client, daemon_row) = joined_rows(&state_home.join("hallouminate"), &id).await;
    assert_eq!(daemon_row.outcome, Outcome::Ok, "{daemon_row:?}");
    assert_present(
        &daemon_row,
        &["resolve", "inventory", "search", "fuse", "render"],
    );
    assert_stage_sum_within_duration(&daemon_row);
    assert_joined(&client, &daemon_row, (Client::Mcp, "ground", Outcome::Ok));
    println!(
        "{}\n{}",
        serde_json::to_string(&daemon_row).expect("daemon row json"),
        serde_json::to_string(&client).expect("client row json")
    );
    mcp.shutdown().await;
    daemon.stop(&repo, &xdg, &state_home).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_log_daemon_process_writes_no_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".hallouminate")).expect("mkdir repo layer");
    std::fs::write(repo.join(".hallouminate/config.toml"), "").expect("write repo layer");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(xdg.join("hallouminate")).expect("mkdir config dir");
    std::fs::write(
        xdg.join("hallouminate/config.toml"),
        format!(
            "[storage]\nground_dir = \"{}\"\n\n[embeddings]\nenabled = false\n\n[logging]\noperations = false\n",
            tmp.path().join("ground").display()
        ),
    )
    .expect("write baseline config");
    let state_home = tmp.path().join("state-home");
    let daemon = DaemonProcess::start(tmp.path(), &xdg, &state_home).await;

    run_cli(
        &["daemon", "status"],
        &daemon.socket,
        &repo,
        &xdg,
        &state_home,
    )
    .await;
    daemon.stop(&repo, &xdg, &state_home).await;

    let state = state_home.join("hallouminate");
    assert!(state.is_dir(), "the daemon ran with this state dir");
    let mut segments = Vec::new();
    for entry in std::fs::read_dir(&state).expect("read state dir") {
        let name = entry.expect("dir entry").file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("operations-") {
            segments.push(name);
        }
    }
    assert!(
        segments.is_empty(),
        "operations = false wrote segments: {segments:?}"
    );
}
