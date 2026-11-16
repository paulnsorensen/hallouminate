//! Operations log: the named stage timings of `daemon_op` rows.

use std::collections::BTreeSet;

use hallouminate_daemon::oplog::{OperationRecord, Outcome, RecordKind, RequestId};
use hallouminate_daemon::{
    AddMarkdownItem, AddMarkdownRequest, DaemonRequest, DaemonRequestPayload, DaemonResponse,
    GroundRequest, ListFilesRequest, Position, connect_at,
};

use crate::common::daemon::DaemonHarness;
use crate::common::oplog_support::{
    Fixture, assert_present, assert_stage_sum_within_duration, dropped_connection_row,
    fixture_with, stage_names, stages, unreachable_ground_row,
};

const GROUND_STAGES: [&str; 7] = [
    "resolve",
    "cold_wait",
    "inventory",
    "search",
    "fuse",
    "rerank",
    "render",
];
const WARM_GROUND_STAGES: [&str; 6] = [
    "resolve",
    "cold_wait",
    "inventory",
    "search",
    "fuse",
    "render",
];
const ADD_MARKDOWN_STAGES: [&str; 8] = [
    "validate",
    "debt_gate_wait",
    "corpus_lock_wait",
    "write_lane_wait",
    "edit",
    "write",
    "ancestor_index",
    "reindex",
];
const WAIT_STAGES: [&str; 3] = ["debt_gate_wait", "corpus_lock_wait", "write_lane_wait"];
const SOFT_DELAY_MS: f64 = 300.0;
const BATCH_ITEMS: usize = 3;

fn request(harness: &DaemonHarness, payload: DaemonRequestPayload) -> DaemonRequest {
    DaemonRequest::new(harness.cwd().to_path_buf(), payload)
}

fn add_markdown(path: &str, content: &str) -> AddMarkdownRequest {
    AddMarkdownRequest {
        corpus: "docs".into(),
        path: path.into(),
        content: content.into(),
        overwrite: false,
        ..Default::default()
    }
}

fn batch_item(path: &str, content: &str) -> AddMarkdownItem {
    AddMarkdownItem {
        path: path.into(),
        content: content.into(),
        overwrite: false,
        under_heading: None,
        position: Position::default(),
        replace_lines: None,
        replace_match: None,
    }
}

fn ground(corpus: &str, query: &str) -> DaemonRequestPayload {
    let request: GroundRequest =
        serde_json::from_value(serde_json::json!({ "query": query, "corpus": corpus }))
            .expect("ground request");
    DaemonRequestPayload::Ground(request)
}

/// Sends `payload` with a fresh request id, asserts success, and returns
/// the `daemon_op` row of that request.
async fn run(harness: &DaemonHarness, payload: DaemonRequestPayload) -> OperationRecord {
    let id = RequestId::generate();
    let client = connect_at(harness.socket()).await.expect("connect");
    let response = client
        .call_raw(request(harness, payload).with_request_id(id.clone()))
        .await
        .expect("daemon response");
    match &response {
        DaemonResponse::Ok { .. } => {}
        DaemonResponse::Err { .. } => panic!("operation failed: {response:?}"),
    }
    let row = harness.row_for(id.as_str()).await;
    assert_eq!(row.outcome, Outcome::Ok);
    row
}

fn stage_ms(row: &OperationRecord, name: &str) -> f64 {
    let Some(elapsed) = stages(row).as_map().get(name) else {
        panic!("row {} has no {name} stage: {:?}", row.op, stage_names(row));
    };
    elapsed.as_f64()
}

fn assert_subset(row: &OperationRecord, allowed: &[&str]) {
    for name in stage_names(row) {
        assert!(
            allowed.contains(&name),
            "{} row has stage {name} outside {allowed:?}",
            row.op
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ground_stages() {
    let Fixture { harness, .. } = fixture_with("").await;
    run(
        &harness,
        DaemonRequestPayload::AddMarkdown(add_markdown(
            "cheese.md",
            "# Cheese\n\nHalloumi squeaks when grilled.\n",
        )),
    )
    .await;
    run(&harness, ground("docs", "halloumi")).await;

    let row = run(&harness, ground("docs", "grilled halloumi")).await;
    assert_eq!(row.op, "ground");
    assert_subset(&row, &GROUND_STAGES);
    assert_present(&row, &WARM_GROUND_STAGES);
    assert_stage_sum_within_duration(&row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_markdown_stages_single() {
    let Fixture { harness, .. } = fixture_with("").await;
    let row = run(
        &harness,
        DaemonRequestPayload::AddMarkdown(add_markdown("single.md", "# Single\n\nBody.\n")),
    )
    .await;
    assert_eq!(row.op, "add_markdown");
    assert_subset(&row, &ADD_MARKDOWN_STAGES);
    assert_present(&row, &WAIT_STAGES);
    assert_present(&row, &["validate", "write", "reindex"]);
    assert_stage_sum_within_duration(&row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_markdown_stages_batch() {
    let Fixture { harness, .. } = fixture_with("").await;
    let row = run(
        &harness,
        DaemonRequestPayload::AddMarkdownBatch {
            corpus: "docs".into(),
            items: vec![
                batch_item("one.md", "# One\n\nBody.\n"),
                batch_item("two.md", "# Two\n\nBody.\n"),
            ],
        },
    )
    .await;
    assert_eq!(row.op, "add_markdown_batch");
    assert_subset(&row, &ADD_MARKDOWN_STAGES);
    assert_present(&row, &WAIT_STAGES);
    assert_present(&row, &["validate", "write", "reindex"]);
    assert_stage_sum_within_duration(&row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_operation_stages_list_files() {
    let Fixture { harness, .. } = fixture_with("").await;
    let row = run(
        &harness,
        DaemonRequestPayload::ListFiles(ListFilesRequest {
            corpus: Some("docs".into()),
        }),
    )
    .await;
    assert_eq!(row.op, "list_files");
    assert_eq!(stage_names(&row), BTreeSet::from(["operation", "resolve"]));
    assert_stage_sum_within_duration(&row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_operation_stages_ping_and_shutdown() {
    let Fixture { harness, .. } = fixture_with("").await;
    let ping = run(&harness, DaemonRequestPayload::Ping).await;
    assert_eq!(ping.op, "ping");
    assert_eq!(stage_names(&ping), BTreeSet::from(["operation"]));
    assert_stage_sum_within_duration(&ping);

    let shutdown = run(&harness, DaemonRequestPayload::Shutdown).await;
    assert_eq!(shutdown.op, "shutdown");
    assert_eq!(stage_names(&shutdown), BTreeSet::from(["operation"]));
    assert_stage_sum_within_duration(&shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_entries_absent_when_not_run() {
    let Fixture {
        harness,
        corpus_root,
        ..
    } = fixture_with("[search]\ncold_wait_ms = 0\n").await;
    let whole = run(
        &harness,
        DaemonRequestPayload::AddMarkdown(add_markdown(
            "edit.md",
            "# Edit\n\nThe brine is salty.\n",
        )),
    )
    .await;
    let names = stage_names(&whole);
    assert!(
        !names.contains("edit"),
        "whole-file write ran no edit: {names:?}"
    );
    assert!(
        !names.contains("ancestor_index"),
        "a non-wiki corpus has no ancestor index: {names:?}"
    );

    let edited = run(
        &harness,
        DaemonRequestPayload::AddMarkdown(AddMarkdownRequest {
            replace_match: Some("salty".into()),
            ..add_markdown("edit.md", "briny")
        }),
    )
    .await;
    assert_present(&edited, &["edit"]);
    assert_stage_sum_within_duration(&edited);
    let text = std::fs::read_to_string(corpus_root.join("edit.md")).expect("read edited file");
    assert!(text.contains("briny"), "{text}");

    let row = run(&harness, ground("docs", "brine")).await;
    let names = stage_names(&row);
    assert!(!names.contains("cold_wait"), "cold wait is off: {names:?}");
    assert!(
        !names.contains("rerank"),
        "no crossencoder is set: {names:?}"
    );
    assert_present(&row, &["resolve", "inventory", "search", "fuse", "render"]);
}

#[tokio::test]
async fn stage_entries_unavailable_client_row_has_no_render() {
    for row in [
        unreachable_ground_row().await,
        dropped_connection_row().await,
    ] {
        assert_eq!(row.outcome, Outcome::Unavailable, "{row:?}");
        let RecordKind::ClientCall(fields) = &row.kind else {
            panic!("expected a client_call row: {row:?}");
        };
        assert!(
            !fields.stages.as_map().contains_key("render"),
            "an unanswered call ran no render: {row:?}"
        );
    }
}

/// Forced debt delays each mutation by the soft delay inside
/// `debt_gate_wait`. A batch runs that wait once per item, so its entry is
/// the sum of all of them, not the last one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_entries_sum_repeated_runs() {
    let extra = format!(
        "[daemon]\nmaintenance_interval_secs = 0\ndebt_soft_delay_ms = {}\n",
        SOFT_DELAY_MS as u64
    );
    let Fixture { harness, .. } = fixture_with(&extra).await;
    harness.state().force_hard_debt_for_tests();

    let single = run(
        &harness,
        DaemonRequestPayload::AddMarkdown(add_markdown("alone.md", "# Alone\n\nBody.\n")),
    )
    .await;
    let single_wait = stage_ms(&single, "debt_gate_wait");
    assert!(single_wait >= SOFT_DELAY_MS, "single wait {single_wait} ms");

    let mut items = Vec::new();
    for index in 0..BATCH_ITEMS {
        items.push(batch_item(
            &format!("batch-{index}.md"),
            &format!("# Batch {index}\n\nBody.\n"),
        ));
    }
    let batch = run(
        &harness,
        DaemonRequestPayload::AddMarkdownBatch {
            corpus: "docs".into(),
            items,
        },
    )
    .await;
    let batch_wait = stage_ms(&batch, "debt_gate_wait");
    let expected = SOFT_DELAY_MS * BATCH_ITEMS as f64;
    assert!(
        batch_wait >= expected,
        "batch wait {batch_wait} ms must sum {BATCH_ITEMS} delays of {SOFT_DELAY_MS} ms"
    );
    assert_stage_sum_within_duration(&batch);
}
