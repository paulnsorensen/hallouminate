use hallouminate_daemon::oplog::{OperationRecord, Outcome, Reason, RecordKind};

const PAGE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../site/src/content/docs/operations-log.md"
));

fn documented_rows() -> Vec<OperationRecord> {
    let mut rows = Vec::new();
    let mut in_json_block = false;
    for line in PAGE.lines() {
        if line.starts_with("```") {
            in_json_block = line == "```json";
            continue;
        }
        if !in_json_block {
            continue;
        }
        let row: OperationRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("documented row does not parse: {error}: {line}"));
        rows.push(row);
    }
    rows
}

#[test]
fn oplog_documented_examples_parse() {
    let rows = documented_rows();

    let mut daemon_outcomes: Vec<Outcome> = Vec::new();
    let mut client_outcomes: Vec<Outcome> = Vec::new();
    for row in &rows {
        match row.kind {
            RecordKind::DaemonOp(_) => daemon_outcomes.push(row.outcome),
            RecordKind::ClientCall(_) => client_outcomes.push(row.outcome),
        }
    }

    for expected in [
        Outcome::Ok,
        Outcome::InvalidParams,
        Outcome::Internal,
        Outcome::Retryable,
        Outcome::Cancelled(Reason::Shutdown),
    ] {
        assert!(
            daemon_outcomes.contains(&expected),
            "no daemon_op example for {expected:?}"
        );
    }
    for expected in [
        Outcome::Ok,
        Outcome::InvalidParams,
        Outcome::Internal,
        Outcome::Retryable,
        Outcome::Cancelled(Reason::Timeout),
        Outcome::Unavailable,
    ] {
        assert!(
            client_outcomes.contains(&expected),
            "no client_call example for {expected:?}"
        );
    }

    for phrase in [
        "operations-*.jsonl",
        "operations-YYYY-MM-DD.jsonl",
        "$XDG_STATE_HOME/hallouminate",
        "[logging]",
        "operations = true",
        "operations_max_total_bytes = 52428800",
        "baseline-owned",
        "The schema is additive only",
        "harness delivery time",
        "holds no query text and no file content",
    ] {
        assert!(PAGE.contains(phrase), "page lacks `{phrase}`");
    }
}
