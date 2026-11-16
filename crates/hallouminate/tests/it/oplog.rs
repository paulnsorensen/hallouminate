//! Operations log: the writer (segment naming, cross-process appends, the
//! daemon-only size cap) and the `daemon_op` rows of a served daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use hallouminate_config::{Config, LoggingConfig};
use hallouminate_daemon::oplog::{
    Client, ClientCallFields, DaemonOpFields, Millis, OpLog, OperationRecord, Outcome, Reason,
    RecordKind, RequestId, SCHEMA_VERSION, Stages,
};
use hallouminate_daemon::{
    AddMarkdownRequest, DaemonRequest, DaemonRequestPayload, DaemonResponse, ErrorKind,
    GroundRequest, connect_at,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::common::daemon::DaemonHarness;

const CHILD_DIR_ENV: &str = "HALLOUMINATE_OPLOG_TEST_DIR";
const CHILD_ID_ENV: &str = "HALLOUMINATE_OPLOG_TEST_CHILD";
const CHILD_TEST: &str = "oplog::concurrent_append_child";
const CHILDREN: usize = 6;
const ROWS_PER_CHILD: usize = 100;
const PADDING_BYTES: usize = 8 * 1024;
const OLD_SEGMENT_BYTES: usize = 1000;
const IPC_LINE_CAP_BYTES: usize = 4 * 1024 * 1024;
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);
const IN_FLIGHT_POLL: Duration = Duration::from_millis(10);
const CLOSE_SETTLE: Duration = Duration::from_millis(300);

fn record(tool: String, corpus: Option<String>) -> OperationRecord {
    OperationRecord {
        schema_version: SCHEMA_VERSION,
        ts: Utc::now(),
        request_id: RequestId::generate(),
        op: "ground".to_owned(),
        outcome: Outcome::Ok,
        duration_ms: Millis::default(),
        corpus,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        kind: RecordKind::ClientCall(ClientCallFields {
            client: Client::Cli,
            tool,
            stages: Stages::default(),
            daemon_round_trip_ms: None,
            response_bytes: None,
        }),
    }
}

fn config(max_total_bytes: u64) -> LoggingConfig {
    LoggingConfig {
        operations_max_total_bytes: max_total_bytes,
        ..LoggingConfig::default()
    }
}

fn segment_name(date: NaiveDate) -> String {
    format!("operations-{}.jsonl", date.format("%Y-%m-%d"))
}

fn write_old_segment(dir: &Path, date: &str) -> PathBuf {
    let path = dir.join(format!("operations-{date}.jsonl"));
    std::fs::write(&path, vec![b'x'; OLD_SEGMENT_BYTES]).expect("write old segment");
    path
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("dir entry");
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    names
}

fn read_rows(path: &Path) -> Vec<OperationRecord> {
    let text = std::fs::read_to_string(path).expect("read segment");
    assert!(text.ends_with('\n'), "segment must end with a newline");
    let mut rows = Vec::new();
    for line in text.lines() {
        let row: OperationRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("line is not one record ({error}): {line:.80}"));
        rows.push(row);
    }
    rows
}

fn current_segment(dir: &Path, before: NaiveDate, after: NaiveDate) -> PathBuf {
    for date in [after, before] {
        let path = dir.join(segment_name(date));
        if path.exists() {
            return path;
        }
    }
    panic!("no segment for {before} or {after} in {dir:?}");
}

#[test]
#[ignore = "child process of concurrent_process_appends_keep_whole_lines"]
fn concurrent_append_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let child = std::env::var(CHILD_ID_ENV).expect("child id");
    let log = OpLog::in_dir(dir, &config(u64::MAX));
    let padding = "p".repeat(PADDING_BYTES);
    for index in 0..ROWS_PER_CHILD {
        log.append(&record(
            format!("{child}:{index}:{padding}"),
            Some(child.clone()),
        ));
    }
}

#[test]
fn concurrent_process_appends_keep_whole_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = std::env::current_exe().expect("test binary path");
    let mut children = Vec::new();
    for child in 0..CHILDREN {
        let process = Command::new(&exe)
            .args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"])
            .env(CHILD_DIR_ENV, dir.path())
            .env(CHILD_ID_ENV, child.to_string())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn child test process");
        children.push(process);
    }
    for mut process in children {
        let status = process.wait().expect("wait for child");
        assert!(status.success(), "child failed: {status}");
    }

    let mut per_child = vec![0usize; CHILDREN];
    let mut total = 0;
    for name in file_names(dir.path()) {
        for row in read_rows(&dir.path().join(name)) {
            let OperationRecord { corpus, kind, .. } = row;
            let child: usize = corpus.expect("child id").parse().expect("numeric child id");
            let RecordKind::ClientCall(ClientCallFields { tool, .. }) = kind else {
                panic!("child wrote a daemon row");
            };
            assert!(tool.len() > PADDING_BYTES, "tool field was cut");
            per_child[child] += 1;
            total += 1;
        }
    }
    assert_eq!(total, CHILDREN * ROWS_PER_CHILD);
    assert_eq!(per_child, vec![ROWS_PER_CHILD; CHILDREN]);
}

#[test]
fn size_cap_deletes_oldest_segments_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let oldest = write_old_segment(dir.path(), "2020-01-01");
    let middle = write_old_segment(dir.path(), "2020-01-02");
    let newest = write_old_segment(dir.path(), "2020-01-03");
    let unrelated = dir.path().join("hallouminate.log");
    std::fs::write(&unrelated, vec![b'l'; 10 * OLD_SEGMENT_BYTES]).expect("write log");

    let row = record("ground".to_owned(), None);
    let row_bytes =
        u64::try_from(serde_json::to_vec(&row).expect("serialize").len() + 1).expect("row length");
    let two_old = u64::try_from(2 * OLD_SEGMENT_BYTES).expect("bytes");
    let daemon = OpLog::in_dir(dir.path(), &config(two_old + row_bytes));
    let before = Utc::now().date_naive();
    daemon.append_and_enforce_cap(&row);
    let after = Utc::now().date_naive();
    let current = current_segment(dir.path(), before, after);

    assert!(!oldest.exists(), "oldest segment must go first");
    assert!(middle.exists(), "cap reached: middle segment must stay");
    assert!(newest.exists(), "cap reached: newest old segment must stay");
    assert!(unrelated.exists(), "non-segment files are outside the cap");
    assert_eq!(read_rows(&current), vec![row]);

    let daemon = OpLog::in_dir(dir.path(), &config(1));
    let second = record("ground".to_owned(), None);
    daemon.append_and_enforce_cap(&second);
    assert!(!middle.exists(), "over the cap: older segments go");
    assert!(!newest.exists(), "over the cap: older segments go");
    assert!(current.exists(), "the current segment is never deleted");

    let client = OpLog::in_dir(dir.path(), &config(1));
    let third = record("status".to_owned(), None);
    client.append(&third);
    let current = current_segment(dir.path(), before, Utc::now().date_naive());
    let rows = read_rows(&current);
    assert_eq!(
        rows.last(),
        Some(&third),
        "rows keep landing in the current segment"
    );
}

#[test]
fn segment_name_is_utc_date_and_clients_delete_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = write_old_segment(dir.path(), "2020-01-01");
    let older = write_old_segment(dir.path(), "2019-12-31");

    let client = OpLog::in_dir(dir.path(), &config(1));
    let row = record("ground".to_owned(), Some("repo:demo:wiki".to_owned()));
    let before = Utc::now().date_naive();
    client.append(&row);
    let after = Utc::now().date_naive();

    let current = current_segment(dir.path(), before, after);
    let name = current
        .file_name()
        .expect("segment name")
        .to_string_lossy()
        .into_owned();
    assert!(
        name == segment_name(before) || name == segment_name(after),
        "segment {name} must be named by the UTC date of the append"
    );
    assert_eq!(read_rows(&current), vec![row]);
    assert!(old.exists(), "a client append must delete no segment");
    assert!(older.exists(), "a client append must delete no segment");
    assert_eq!(file_names(dir.path()).len(), 3);
}

#[test]
fn size_cap_skips_directories_and_unparsable_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let directory = dir.path().join("operations-2019-01-01.jsonl");
    std::fs::create_dir(&directory).expect("mkdir segment-named directory");
    let notes = dir.path().join("operations-foo.jsonl");
    std::fs::write(&notes, vec![b'n'; 10 * OLD_SEGMENT_BYTES]).expect("write notes");
    let bad_date = dir.path().join("operations-2019-13-40.jsonl");
    std::fs::write(&bad_date, vec![b'b'; OLD_SEGMENT_BYTES]).expect("write bad date");
    let old = write_old_segment(dir.path(), "2020-01-01");

    let row = record("ground".to_owned(), None);
    let row_bytes =
        u64::try_from(serde_json::to_vec(&row).expect("serialize").len() + 1).expect("row length");
    let daemon = OpLog::in_dir(dir.path(), &config(row_bytes));
    let before = Utc::now().date_naive();
    daemon.append_and_enforce_cap(&row);
    let after = Utc::now().date_naive();
    let current = current_segment(dir.path(), before, after);

    assert!(!old.exists(), "the dated segment over the cap must go");
    assert!(directory.is_dir(), "a directory is not a segment");
    assert!(notes.exists(), "an undated name is not a segment");
    assert!(bad_date.exists(), "an invalid date is not a segment");
    assert_eq!(read_rows(&current), vec![row]);
}

// ---- daemon_op rows -------------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    corpus_root: PathBuf,
    harness: DaemonHarness,
}

fn docs_config(tmp: &Path, extra: &str) -> (Config, PathBuf) {
    let corpus_root = tmp.join("corpus");
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
{extra}
"#,
        c = corpus_root.display(),
        g = tmp.join("ground").display(),
    );
    let cfg: Config = toml::from_str(&toml).expect("parse cfg");
    (cfg, corpus_root)
}

async fn fixture_with(extra: &str) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cfg, corpus_root) = docs_config(tmp.path(), extra);
    let logging = cfg.logging;
    let harness = DaemonHarness::spawn_with_oplog(cfg, logging).await;
    Fixture {
        _tmp: tmp,
        corpus_root,
        harness,
    }
}

async fn fixture() -> Fixture {
    fixture_with("").await
}

fn add_markdown(harness: &DaemonHarness, path: &str, content: &str) -> DaemonRequest {
    DaemonRequest::new(
        harness.cwd().to_path_buf(),
        DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            corpus: "docs".into(),
            path: path.into(),
            content: content.into(),
            overwrite: false,
            ..Default::default()
        }),
    )
}

fn ground(harness: &DaemonHarness, corpus: &str, query: &str) -> DaemonRequest {
    let request: GroundRequest =
        serde_json::from_value(serde_json::json!({ "query": query, "corpus": corpus }))
            .expect("ground request");
    DaemonRequest::new(
        harness.cwd().to_path_buf(),
        DaemonRequestPayload::Ground(request),
    )
}

async fn send(harness: &DaemonHarness, request: DaemonRequest) -> DaemonResponse {
    let client = connect_at(harness.socket()).await.expect("connect");
    client.call_raw(request).await.expect("daemon response")
}

fn error_kind(response: &DaemonResponse) -> Option<ErrorKind> {
    match response {
        DaemonResponse::Ok { .. } => None,
        DaemonResponse::Err { kind, .. } => Some(*kind),
    }
}

fn daemon_fields(row: &OperationRecord) -> &DaemonOpFields {
    match &row.kind {
        RecordKind::DaemonOp(fields) => fields,
        RecordKind::ClientCall(fields) => panic!("daemon wrote a client row: {fields:?}"),
    }
}

fn segment_values(dir: &Path) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    for line in segment_text(dir).lines() {
        values.push(serde_json::from_str(line).expect("row is JSON"));
    }
    values
}

fn segment_text(dir: &Path) -> String {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).expect("state dir") {
        let path = entry.expect("dir entry").path();
        names.push(path);
    }
    names.sort();
    let mut text = String::new();
    for path in names {
        text.push_str(&std::fs::read_to_string(path).expect("read segment"));
    }
    text
}

fn value_for<'a>(values: &'a [serde_json::Value], request_id: &str) -> &'a serde_json::Value {
    for value in values {
        if value["request_id"] == request_id {
            return value;
        }
    }
    panic!("no row for {request_id}");
}

async fn rows_with_op(harness: &DaemonHarness, op: &str, count: usize) -> Vec<OperationRecord> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut matching = Vec::new();
        for row in harness.rows() {
            if row.op == op {
                matching.push(row);
            }
        }
        if matching.len() >= count {
            return matching;
        }
        if std::time::Instant::now() > deadline {
            panic!("expected {count} {op} rows, found {}", matching.len());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn send_raw_line(harness: &DaemonHarness, line: &[u8]) -> DaemonResponse {
    let mut stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");
    let mut written = 0;
    for chunk in line.chunks(64 * 1024) {
        let Ok(()) = stream.write_all(chunk).await else {
            break;
        };
        written += chunk.len();
    }
    assert!(written > 0, "no request bytes reached the daemon");
    let mut reader = BufReader::new(&mut stream);
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut response))
        .await
        .expect("daemon answers a rejected line promptly")
        .expect("read response");
    serde_json::from_str(response.trim_end()).expect("response is JSON")
}

fn assert_rejected_row(row: &OperationRecord) {
    assert_eq!(row.outcome, Outcome::InvalidParams);
    assert_eq!(row.op, "unknown");
    assert_eq!(row.corpus, None);
    assert_request_id_shape(row.request_id.as_str());
    assert!(daemon_fields(row).stages.as_map().is_empty());
}

fn assert_request_id_shape(id: &str) {
    assert_eq!(id.len(), 32, "{id}");
    for byte in id.bytes() {
        assert!(
            byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
            "request id must be lowercase hex: {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_row_carries_common_fields() {
    let Fixture { harness, .. } = fixture().await;
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "common.md", "# Common\n\nBody.\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(error_kind(&response), None, "{response:?}");

    let row = harness.row_for(id.as_str()).await;
    let OperationRecord {
        schema_version,
        ts,
        request_id,
        op,
        outcome,
        duration_ms,
        corpus,
        version,
        kind,
    } = row;
    assert_eq!(schema_version, SCHEMA_VERSION);
    assert!(ts <= Utc::now());
    assert_eq!(request_id, id);
    assert_eq!(op, "add_markdown");
    assert_eq!(outcome, Outcome::Ok);
    assert!(duration_ms.as_f64() > 0.0);
    assert_eq!(corpus.as_deref(), Some("docs"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    let RecordKind::DaemonOp(DaemonOpFields { warning_count, .. }) = kind else {
        panic!("daemon wrote a client row");
    };
    assert_eq!(warning_count, 0);

    let ping = RequestId::generate();
    let response = send(
        &harness,
        DaemonRequest::new(harness.cwd().to_path_buf(), DaemonRequestPayload::Ping)
            .with_request_id(ping.clone()),
    )
    .await;
    assert_eq!(error_kind(&response), None, "{response:?}");
    let row = harness.row_for(ping.as_str()).await;
    assert_eq!(row.op, "ping");
    assert_eq!(row.corpus, None);
    assert_eq!(row.outcome, Outcome::Ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_outcome_ok() {
    let Fixture {
        harness,
        corpus_root,
        ..
    } = fixture().await;
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "ok.md", "# Ok\n\nFine.\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(error_kind(&response), None, "{response:?}");
    assert!(corpus_root.join("ok.md").exists());
    assert_eq!(harness.row_for(id.as_str()).await.outcome, Outcome::Ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_outcome_invalid_params() {
    let Fixture { harness, .. } = fixture().await;
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "../escape.md", "# Escape\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(
        error_kind(&response),
        Some(ErrorKind::InvalidParams),
        "{response:?}"
    );
    let row = harness.row_for(id.as_str()).await;
    assert_eq!(row.outcome, Outcome::InvalidParams);
    assert_eq!(row.corpus.as_deref(), Some("docs"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_outcome_internal() {
    let Fixture { harness, .. } = fixture().await;
    harness.state().write_lane().close();
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "internal.md", "# Internal\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(
        error_kind(&response),
        Some(ErrorKind::Internal),
        "{response:?}"
    );
    assert_eq!(
        harness.row_for(id.as_str()).await.outcome,
        Outcome::Internal
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_outcome_retryable() {
    let Fixture { harness, .. } =
        fixture_with("\n[daemon]\nhard_block_wait_secs = 0\nmaintenance_interval_secs = 3600\n")
            .await;
    harness.state().force_hard_debt_for_tests();
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "retry.md", "# Retry\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(
        error_kind(&response),
        Some(ErrorKind::Retryable),
        "{response:?}"
    );
    assert_eq!(
        harness.row_for(id.as_str()).await.outcome,
        Outcome::Retryable
    );
}
/// Waits until the daemon dispatches one more request than `before`, so the
/// request holds an outcome guard.
async fn wait_for_dispatch(harness: &DaemonHarness, before: u64) {
    let deadline = Instant::now() + IN_FLIGHT_WAIT;
    while harness.state().dispatched_requests_for_tests() == before {
        assert!(
            Instant::now() < deadline,
            "the request did not reach dispatch"
        );
        tokio::time::sleep(IN_FLIGHT_POLL).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_outcome_cancelled_shutdown() {
    let Fixture { mut harness, .. } = fixture().await;
    let lane = harness.state().write_lane();
    let permits = u32::try_from(lane.available_permits()).expect("permit count");
    let held = lane.acquire_many_owned(permits).await.expect("hold lane");
    let id = RequestId::generate();
    let request = add_markdown(&harness, "stalled.md", "# Stalled\n").with_request_id(id.clone());
    let client = connect_at(harness.socket()).await.expect("connect");
    let dispatched = harness.state().dispatched_requests_for_tests();
    let call = tokio::spawn(async move { client.call_raw(request).await });
    wait_for_dispatch(&harness, dispatched).await;

    harness.abort_in_flight();
    let row = harness.row_for(id.as_str()).await;
    assert_eq!(row.outcome, Outcome::Cancelled(Reason::Shutdown));
    assert_eq!(row.op, "add_markdown");
    let call = call.await.expect("client task");
    assert!(call.is_err(), "an aborted operation sends no response");
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_without_id_gets_generated_id() {
    let Fixture { harness, .. } = fixture().await;
    for path in ["one.md", "two.md"] {
        let response = send(&harness, add_markdown(&harness, path, "# Page\n")).await;
        assert_eq!(error_kind(&response), None, "{response:?}");
    }
    let rows = rows_with_op(&harness, "add_markdown", 2).await;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_request_id_shape(row.request_id.as_str());
    }
    assert_ne!(rows[0].request_id, rows[1].request_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_write_failure_keeps_operation_result() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cfg, corpus_root) = docs_config(tmp.path(), "");
    let not_a_dir = tmp.path().join("state-is-a-file");
    std::fs::write(&not_a_dir, "occupied").expect("write blocker");
    let log = OpLog::in_dir(&not_a_dir, &cfg.logging);
    let harness = DaemonHarness::spawn_with_oplog_writer(cfg, log).await;

    let response = send(&harness, add_markdown(&harness, "kept.md", "# Kept\n")).await;
    assert_eq!(error_kind(&response), None, "{response:?}");
    assert!(corpus_root.join("kept.md").exists());
    let response = send(
        &harness,
        DaemonRequest::new(harness.cwd().to_path_buf(), DaemonRequestPayload::Ping),
    )
    .await;
    assert_eq!(error_kind(&response), None, "{response:?}");
    assert_eq!(
        std::fs::read_to_string(&not_a_dir).expect("blocker"),
        "occupied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_values_are_well_formed() {
    let Fixture { harness, .. } = fixture().await;
    let id = RequestId::generate();
    send(
        &harness,
        add_markdown(&harness, "shape.md", "# Shape\n").with_request_id(id.clone()),
    )
    .await;
    harness.row_for(id.as_str()).await;

    let values = segment_values(harness.state_dir());
    assert!(!values.is_empty());
    for value in &values {
        assert_eq!(value["schema_version"], 1, "{value}");
        assert_eq!(value["kind"], "daemon_op", "{value}");
        let ts = value["ts"].as_str().expect("ts string");
        let parsed = chrono::DateTime::parse_from_rfc3339(ts).expect("RFC 3339 ts");
        assert_eq!(parsed.offset().local_minus_utc(), 0, "ts must be UTC: {ts}");
        assert_request_id_shape(value["request_id"].as_str().expect("id string"));
        let duration = value["duration_ms"].as_f64().expect("duration number");
        assert!(duration >= 0.0, "{value}");
        assert!(value["stages"].is_object(), "{value}");
        assert!(value["warning_count"].is_u64(), "{value}");
        assert!(value["warning_codes"].is_array(), "{value}");
        assert!(
            value.get("corpus").is_some(),
            "corpus is null, not absent: {value}"
        );
        assert!(value.get("reason").is_none(), "{value}");
    }
    let row = value_for(&values, id.as_str());
    assert_eq!(row["op"], "add_markdown");
    assert_eq!(row["outcome"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warnings_are_counted_and_coded() {
    let Fixture {
        harness,
        corpus_root: outside_root,
        ..
    } = fixture().await;
    let lint = RequestId::generate();
    let body = "# Notes\n\nSee [the spec]() for details.\n\n```mermaid\n```\n";
    let response = send(
        &harness,
        add_markdown(&harness, "notes.md", body).with_request_id(lint.clone()),
    )
    .await;
    let DaemonResponse::Ok { result } = response else {
        panic!("add_markdown failed: {response:?}");
    };
    assert_eq!(result["warnings"].as_array().expect("warnings").len(), 2);
    let row = harness.row_for(lint.as_str()).await;
    let DaemonOpFields {
        warning_count,
        warning_codes,
        ..
    } = daemon_fields(&row);
    assert_eq!(*warning_count, 2);
    assert!(warning_codes.is_empty(), "free-text warnings have no code");

    let inside_root = harness.cwd().join("inside");
    std::fs::create_dir_all(&inside_root).expect("mkdir inside");
    std::fs::write(inside_root.join("page.md"), "# Page\n\nAlpha.\n").expect("write page");
    std::fs::write(
        harness.cwd().join(".hallouminate/config.toml"),
        format!(
            "[[corpus]]\nname = \"mixed\"\npaths = [\"{}\", \"{}\"]\nglobs = [\"**/*.md\"]\n",
            inside_root.display(),
            outside_root.display()
        ),
    )
    .expect("write repo layer");
    let search = RequestId::generate();
    let response = send(
        &harness,
        ground(&harness, "docs", "alpha").with_request_id(search.clone()),
    )
    .await;
    let DaemonResponse::Ok { result } = response else {
        panic!("ground failed: {response:?}");
    };
    let warnings = result
        .pointer("/response/warnings")
        .and_then(serde_json::Value::as_array)
        .expect("ground warnings");
    let mut expected = Vec::new();
    for warning in warnings {
        expected.push(warning["code"].as_str().expect("code").to_owned());
    }
    assert!(
        !expected.is_empty(),
        "a repo corpus path outside the repo must warn: {warnings:?}"
    );
    let row = harness.row_for(search.as_str()).await;
    let DaemonOpFields {
        warning_count,
        warning_codes,
        ..
    } = daemon_fields(&row);
    assert_eq!(
        *warning_count,
        u64::try_from(warnings.len()).expect("count")
    );
    assert_eq!(warning_codes, &expected);
}

#[test]
fn outcome_classified_from_row_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = OpLog::in_dir(dir.path(), &config(u64::MAX));
    let outcomes = [
        Outcome::Ok,
        Outcome::InvalidParams,
        Outcome::Internal,
        Outcome::Retryable,
        Outcome::Cancelled(Reason::Shutdown),
        Outcome::Cancelled(Reason::Timeout),
        Outcome::Unavailable,
    ];
    for outcome in outcomes {
        let mut row = record("ground".to_owned(), None);
        row.outcome = outcome;
        log.append(&row);
    }
    let mut classified = Vec::new();
    for value in segment_values(dir.path()) {
        let outcome = value["outcome"].as_str().expect("outcome string");
        let reason = value
            .get("reason")
            .map(|reason| reason.as_str().expect("reason"));
        let class = match (outcome, reason) {
            ("ok", None) => Outcome::Ok,
            ("invalid_params", None) => Outcome::InvalidParams,
            ("internal", None) => Outcome::Internal,
            ("retryable", None) => Outcome::Retryable,
            ("cancelled", Some("shutdown")) => Outcome::Cancelled(Reason::Shutdown),
            ("cancelled", Some("timeout")) => Outcome::Cancelled(Reason::Timeout),
            ("unavailable", None) => Outcome::Unavailable,
            (outcome, reason) => panic!("unclassifiable row: {outcome} {reason:?}"),
        };
        classified.push(class);
    }
    assert_eq!(classified, outcomes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_hold_no_query_text_or_content() {
    let Fixture { harness, .. } = fixture().await;
    let content = "# Secret\n\nmarmalade-sentinel-content and [broken]()\n";
    let write = RequestId::generate();
    send(
        &harness,
        add_markdown(&harness, "secret-page.md", content).with_request_id(write.clone()),
    )
    .await;
    let search = RequestId::generate();
    send(
        &harness,
        ground(&harness, "docs", "zanzibar-sentinel-query").with_request_id(search.clone()),
    )
    .await;
    harness.row_for(write.as_str()).await;
    harness.row_for(search.as_str()).await;

    let text = segment_text(harness.state_dir());
    for secret in [
        "marmalade-sentinel-content",
        "zanzibar-sentinel-query",
        "secret-page",
        "empty destination",
    ] {
        assert!(!text.contains(secret), "row text leaked {secret:?}: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_written_when_operations_key_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cfg, _corpus_root) = docs_config(tmp.path(), "\n[logging]\nmax_file_bytes = 1048576\n");
    assert!(cfg.logging.operations, "operations defaults to on");
    let logging = cfg.logging;
    let harness = DaemonHarness::spawn_with_oplog(cfg, logging).await;
    let id = RequestId::generate();
    send(
        &harness,
        DaemonRequest::new(harness.cwd().to_path_buf(), DaemonRequestPayload::Ping)
            .with_request_id(id.clone()),
    )
    .await;
    assert_eq!(harness.row_for(id.as_str()).await.op, "ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_request_unauthorized_peer() {
    let Fixture {
        harness,
        corpus_root,
        ..
    } = fixture().await;
    let euid = rustix::process::geteuid().as_raw();
    harness
        .state()
        .set_authorized_uid_for_tests(euid.wrapping_add(1));
    let id = RequestId::generate();
    let response = send(
        &harness,
        add_markdown(&harness, "denied.md", "# Denied\n").with_request_id(id.clone()),
    )
    .await;
    assert_eq!(
        error_kind(&response),
        Some(ErrorKind::InvalidParams),
        "{response:?}"
    );
    assert!(!corpus_root.join("denied.md").exists());
    let row = harness.row_for(id.as_str()).await;
    assert_eq!(row.outcome, Outcome::InvalidParams);
    assert_eq!(row.op, "add_markdown");
    assert_eq!(row.corpus.as_deref(), Some("docs"));
    assert!(daemon_fields(&row).stages.as_map().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_request_unparsable_line() {
    let Fixture { harness, .. } = fixture().await;
    let response = send_raw_line(&harness, b"{not json\n").await;
    assert_eq!(error_kind(&response), Some(ErrorKind::InvalidParams));
    let rows = rows_with_op(&harness, "unknown", 1).await;
    assert_eq!(rows.len(), 1);
    assert_rejected_row(&rows[0]);

    let response = send_raw_line(&harness, b"\xff\xfe\n").await;
    assert_eq!(error_kind(&response), Some(ErrorKind::InvalidParams));
    let rows = rows_with_op(&harness, "unknown", 2).await;
    assert_eq!(rows.len(), 2);
    assert_rejected_row(&rows[1]);
    assert_ne!(rows[0].request_id, rows[1].request_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_request_oversized_line() {
    let Fixture { harness, .. } = fixture().await;
    let line = vec![b'a'; IPC_LINE_CAP_BYTES + 64 * 1024];
    let response = send_raw_line(&harness, &line).await;
    assert_eq!(error_kind(&response), Some(ErrorKind::InvalidParams));
    let rows = rows_with_op(&harness, "unknown", 1).await;
    assert_eq!(rows.len(), 1);
    assert_rejected_row(&rows[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_request_malformed_request_id() {
    let Fixture {
        harness,
        corpus_root,
        ..
    } = fixture().await;
    let request = add_markdown(&harness, "bad-id.md", "# Bad id\n");
    let malformed = [
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef",
        "0123456789abcdef0123456789abcdeg",
    ];
    for (index, id) in malformed.iter().enumerate() {
        let mut value = serde_json::to_value(&request).expect("request value");
        value["request_id"] = serde_json::json!(id);
        let mut line = serde_json::to_vec(&value).expect("request line");
        line.push(b'\n');
        let response = send_raw_line(&harness, &line).await;
        assert_eq!(
            error_kind(&response),
            Some(ErrorKind::InvalidParams),
            "{id}: {response:?}"
        );
        let rows = rows_with_op(&harness, "unknown", index + 1).await;
        assert_eq!(rows.len(), index + 1);
        let row = &rows[index];
        assert_rejected_row(row);
        assert_ne!(row.request_id.as_str(), id.to_lowercase(), "{row:?}");
    }
    assert!(!corpus_root.join("bad-id.md").exists());

    let ping = RequestId::generate();
    let response = send(
        &harness,
        DaemonRequest::new(harness.cwd().to_path_buf(), DaemonRequestPayload::Ping)
            .with_request_id(ping.clone()),
    )
    .await;
    assert_eq!(error_kind(&response), None, "{response:?}");
    assert_eq!(harness.row_for(ping.as_str()).await.op, "ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_keeps_operation_outcome() {
    let Fixture {
        harness,
        corpus_root,
        ..
    } = fixture().await;
    let lane = harness.state().write_lane();
    let permits = u32::try_from(lane.available_permits()).expect("permit count");
    let held = lane.acquire_many_owned(permits).await.expect("hold lane");
    let id = RequestId::generate();
    let request = add_markdown(&harness, "left.md", "# Left\n").with_request_id(id.clone());
    let mut line = serde_json::to_vec(&request).expect("serialize");
    line.push(b'\n');
    let dispatched = harness.state().dispatched_requests_for_tests();
    let mut stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");
    stream.write_all(&line).await.expect("send request");
    wait_for_dispatch(&harness, dispatched).await;
    drop(stream);
    drop(held);

    let row = harness.row_for(id.as_str()).await;
    assert_eq!(row.outcome, Outcome::Ok);
    assert!(corpus_root.join("left.md").exists());
    let values = segment_values(harness.state_dir());
    assert!(value_for(&values, id.as_str()).get("reason").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_without_request_writes_no_row() {
    let Fixture { harness, .. } = fixture().await;
    let stream = UnixStream::connect(harness.socket())
        .await
        .expect("connect");
    drop(stream);
    tokio::time::sleep(CLOSE_SETTLE).await;

    let barrier = RequestId::generate();
    send(
        &harness,
        DaemonRequest::new(harness.cwd().to_path_buf(), DaemonRequestPayload::Ping)
            .with_request_id(barrier.clone()),
    )
    .await;
    harness.row_for(barrier.as_str()).await;
    for row in harness.rows() {
        assert_eq!(row.op, "ping", "only pings ran: {row:?}");
    }
}
