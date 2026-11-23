//! Helpers that the operations-log suites share: the served-daemon fixture,
//! stage assertions on `daemon_op` rows, and client rows of failed calls.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hallouminate::cli::{GroundArgs, cmd_ground};
use hallouminate_config::{Config, LoggingConfig};
use hallouminate_daemon::oplog::{
    Client, ClientCall, OpLog, OperationRecord, Outcome, RecordKind, Stages,
};
use hallouminate_daemon::{DaemonRequest, DaemonRequestPayload, connect_at};
use tokio::io::AsyncBufReadExt;

use super::daemon::{DaemonHarness, read_segment_rows};

pub struct Fixture {
    // Bind this guard before moving out the harness. The harness must drop first.
    pub _tmp: tempfile::TempDir,
    pub corpus_root: PathBuf,
    pub harness: DaemonHarness,
}

pub fn docs_config(tmp: &Path, extra: &str) -> (Config, PathBuf) {
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

pub async fn fixture_with(extra: &str) -> Fixture {
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

pub fn stages(row: &OperationRecord) -> &Stages {
    match &row.kind {
        RecordKind::DaemonOp(fields) => &fields.stages,
        RecordKind::ClientCall(fields) => panic!("daemon wrote a client row: {fields:?}"),
    }
}

pub fn stage_names(row: &OperationRecord) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    for name in stages(row).as_map().keys() {
        names.insert(name.as_str());
    }
    names
}

pub fn assert_stage_sum_within_duration(row: &OperationRecord) {
    let mut sum = 0.0;
    for elapsed in stages(row).as_map().values() {
        sum += elapsed.as_f64();
    }
    let duration = row.duration_ms.as_f64();
    assert!(
        sum <= duration,
        "{} stage sum {sum} ms exceeds duration {duration} ms: {:?}",
        row.op,
        stages(row)
    );
}

pub fn assert_present(row: &OperationRecord, required: &[&str]) {
    let names = stage_names(row);
    for name in required {
        assert!(
            names.contains(name),
            "{} row lacks stage {name}: {names:?}",
            row.op
        );
    }
}

/// Runs `cmd_ground` against a socket path with no daemon and returns its
/// one client row.
pub async fn unreachable_ground_row() -> OperationRecord {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "").expect("write config");
    let state = tmp.path().join("state");
    let oplog = OpLog::in_dir(&state, &LoggingConfig::default());

    cmd_ground(
        GroundArgs {
            query: "anything".into(),
            config: Some(config),
            socket: Some(tmp.path().join("absent.sock")),
            ..Default::default()
        },
        &oplog,
    )
    .await
    .expect_err("missing daemon socket must fail");

    let rows = read_segment_rows(&state);
    let [row] = rows.as_slice() else {
        panic!("expected one client row: {rows:?}");
    };
    row.clone()
}

/// Sends `status` to a listener that reads the request and closes the
/// connection without an answer. Returns the one client row.
pub async fn dropped_connection_row() -> OperationRecord {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("dropping.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
        }
    });
    let client = connect_at(&socket).await.expect("connect");
    let state = tmp.path().join("state");
    let oplog = OpLog::in_dir(&state, &LoggingConfig::default());
    let mut call = ClientCall::start(&oplog, Client::Cli, "status", "status", None);
    call.ready();

    client
        .call_raw_with_timeout_logged(
            DaemonRequest::new(tmp.path().to_path_buf(), DaemonRequestPayload::Status),
            Duration::from_secs(5),
            &mut call,
        )
        .await
        .expect_err("dropped connection must fail");
    call.finish(Outcome::Internal);
    server.abort();

    let rows = read_segment_rows(&state);
    let [row] = rows.as_slice() else {
        panic!("expected one client row: {rows:?}");
    };
    row.clone()
}
