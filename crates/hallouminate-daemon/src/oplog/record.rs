use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::RequestId;

/// Schema version of the rows that this crate writes.
pub const SCHEMA_VERSION: u32 = 1;

/// One row of the operations log, version 1.
///
/// The kind-specific fields live in [`RecordKind`], so a row of one kind
/// cannot carry the fields of the other kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    /// Row schema version, [`SCHEMA_VERSION`] for rows written by this crate.
    pub schema_version: u32,
    /// End of the operation, serialized as RFC 3339 UTC.
    pub ts: DateTime<Utc>,
    /// Id that joins the client row and the daemon row of one call.
    pub request_id: RequestId,
    /// Snake_case name of the daemon request variant, or `unknown`.
    pub op: String,
    /// Outcome of the operation, with its `reason` when cancelled.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Total time of the operation.
    pub duration_ms: Millis,
    /// Corpus that the request names, or `None` (JSON null).
    pub corpus: Option<String>,
    /// Hallouminate version of the writing process.
    pub version: String,
    /// The `kind` field and the fields that belong to that kind.
    #[serde(flatten)]
    pub kind: RecordKind,
}

/// The `kind` of a row and its kind-specific fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordKind {
    /// Row written by the daemon for one operation.
    DaemonOp(DaemonOpFields),
    /// Row written by a client (MCP server or CLI) for one call.
    ClientCall(ClientCallFields),
}

/// Fields of a `daemon_op` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonOpFields {
    /// Stage name to duration.
    pub stages: Stages,
    /// Number of warnings that the operation returned.
    pub warning_count: u64,
    /// Codes of the coded warnings that the operation returned.
    pub warning_codes: Vec<String>,
}

/// Fields of a `client_call` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCallFields {
    /// Process that wrote the row.
    pub client: Client,
    /// MCP tool name, or CLI subcommand name.
    pub tool: String,
    /// Handler stage name to duration.
    pub stages: Stages,
    /// Daemon round-trip time, or `None` (JSON null) when no answer arrived.
    pub daemon_round_trip_ms: Option<Millis>,
    /// Byte length of the daemon response, or `None` (JSON null) when no answer arrived.
    pub response_bytes: Option<u64>,
}

/// Process that writes a `client_call` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    /// The MCP server, `hallouminate serve`.
    Mcp,
    /// A CLI command.
    Cli,
}

/// Outcome of an operation.
///
/// Serializes as the `outcome` field, plus a `reason` field only for
/// [`Outcome::Cancelled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "reason", rename_all = "snake_case")]
pub enum Outcome {
    /// The operation succeeded.
    Ok,
    /// The request failed validation.
    InvalidParams,
    /// The operation failed internally.
    Internal,
    /// The daemon refused the operation for now; a retry can succeed.
    Retryable,
    /// The operation stopped before it completed.
    Cancelled(Reason),
    /// The client could not reach the daemon.
    Unavailable,
}

/// Why an operation was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The daemon shut down during the operation.
    Shutdown,
    /// The client RPC timeout expired.
    Timeout,
}

/// A non-negative duration in milliseconds, serialized as a JSON number.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Millis(f64);

impl Millis {
    /// Converts a [`Duration`] to milliseconds.
    pub fn from_duration(duration: Duration) -> Self {
        Self(duration.as_nanos() as f64 / 1_000_000.0)
    }

    /// Returns the number of milliseconds.
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

/// Error returned when a value is not a finite, non-negative [`Millis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMillis;

impl fmt::Display for InvalidMillis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("milliseconds must be a finite, non-negative number")
    }
}

impl std::error::Error for InvalidMillis {}

impl TryFrom<f64> for Millis {
    type Error = InvalidMillis;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() || value < 0.0 {
            return Err(InvalidMillis);
        }
        Ok(Self(value))
    }
}

impl From<Millis> for f64 {
    fn from(value: Millis) -> Self {
        value.0
    }
}

/// Stage name to duration, ordered by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Stages(BTreeMap<String, Millis>);

impl Stages {
    /// Adds `elapsed` to the stage `name`; a repeated stage records the sum of its runs.
    pub fn add(&mut self, name: &str, elapsed: Millis) {
        let total = self.0.entry(name.to_owned()).or_default();
        *total = Millis(total.0 + elapsed.0);
    }

    /// Returns the stage durations by name.
    pub fn as_map(&self) -> &BTreeMap<String, Millis> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: Outcome, kind: RecordKind) -> OperationRecord {
        OperationRecord {
            schema_version: SCHEMA_VERSION,
            ts: DateTime::parse_from_rfc3339("2026-11-16T01:02:03.5Z")
                .expect("timestamp")
                .with_timezone(&Utc),
            request_id: "0123456789abcdef0123456789abcdef".parse().expect("id"),
            op: "ground".to_owned(),
            outcome,
            duration_ms: Millis::from_duration(Duration::from_micros(12_500)),
            corpus: None,
            version: "0.11.1".to_owned(),
            kind,
        }
    }

    fn daemon_kind() -> RecordKind {
        let mut stages = Stages::default();
        stages.add("search", Millis::from_duration(Duration::from_millis(2)));
        stages.add("search", Millis::from_duration(Duration::from_millis(3)));
        RecordKind::DaemonOp(DaemonOpFields {
            stages,
            warning_count: 1,
            warning_codes: vec!["stale_index".to_owned()],
        })
    }

    fn client_kind() -> RecordKind {
        RecordKind::ClientCall(ClientCallFields {
            client: Client::Mcp,
            tool: "ground".to_owned(),
            stages: Stages::default(),
            daemon_round_trip_ms: None,
            response_bytes: None,
        })
    }

    #[test]
    fn daemon_row_has_common_and_daemon_fields_only() {
        let row = record(Outcome::Ok, daemon_kind());
        let value = serde_json::to_value(&row).expect("serialize");
        let expected = serde_json::json!({
            "schema_version": 1,
            "ts": "2026-11-16T01:02:03.500Z",
            "kind": "daemon_op",
            "request_id": "0123456789abcdef0123456789abcdef",
            "op": "ground",
            "outcome": "ok",
            "duration_ms": 12.5,
            "corpus": null,
            "version": "0.11.1",
            "stages": {"search": 5.0},
            "warning_count": 1,
            "warning_codes": ["stale_index"],
        });
        assert_eq!(value, expected);
        let back: OperationRecord = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, row);
    }

    #[test]
    fn client_row_writes_null_for_missing_values_and_reason_when_cancelled() {
        let row = record(Outcome::Cancelled(Reason::Timeout), client_kind());
        let value = serde_json::to_value(&row).expect("serialize");
        let expected = serde_json::json!({
            "schema_version": 1,
            "ts": "2026-11-16T01:02:03.500Z",
            "kind": "client_call",
            "request_id": "0123456789abcdef0123456789abcdef",
            "op": "ground",
            "outcome": "cancelled",
            "reason": "timeout",
            "duration_ms": 12.5,
            "corpus": null,
            "version": "0.11.1",
            "client": "mcp",
            "tool": "ground",
            "stages": {},
            "daemon_round_trip_ms": null,
            "response_bytes": null,
        });
        assert_eq!(value, expected);
        let back: OperationRecord = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, row);
    }

    #[test]
    fn every_outcome_round_trips() {
        for outcome in [
            Outcome::Ok,
            Outcome::InvalidParams,
            Outcome::Internal,
            Outcome::Retryable,
            Outcome::Cancelled(Reason::Shutdown),
            Outcome::Cancelled(Reason::Timeout),
            Outcome::Unavailable,
        ] {
            let row = record(outcome, daemon_kind());
            let line = serde_json::to_string(&row).expect("serialize");
            let back: OperationRecord = serde_json::from_str(&line).expect("deserialize");
            assert_eq!(back.outcome, outcome, "{line}");
            let value: serde_json::Value = serde_json::from_str(&line).expect("json");
            let has_reason = value.get("reason").is_some();
            let cancelled = match outcome {
                Outcome::Cancelled(_) => true,
                Outcome::Ok
                | Outcome::InvalidParams
                | Outcome::Internal
                | Outcome::Retryable
                | Outcome::Unavailable => false,
            };
            assert_eq!(has_reason, cancelled, "{line}");
        }
    }

    #[test]
    fn rejects_reason_on_non_cancelled_and_negative_durations() {
        let mut value = serde_json::to_value(record(Outcome::Ok, daemon_kind())).expect("value");
        value["reason"] = serde_json::json!("timeout");
        assert!(serde_json::from_value::<OperationRecord>(value).is_err());

        let mut value = serde_json::to_value(record(Outcome::Ok, daemon_kind())).expect("value");
        value["duration_ms"] = serde_json::json!(-1.0);
        assert!(serde_json::from_value::<OperationRecord>(value).is_err());
    }
}
