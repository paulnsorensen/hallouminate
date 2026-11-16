use std::time::{Duration, Instant};

use chrono::Utc;

use super::{
    Client, ClientCallFields, Millis, OpLog, OperationRecord, Outcome, RecordKind, RequestId,
    SCHEMA_VERSION, Stages,
};

const SETUP_STAGE: &str = "setup";
const RENDER_STAGE: &str = "render";

/// The daemon answer that one call received.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Answer {
    pub(crate) round_trip: Duration,
    pub(crate) response_bytes: u64,
}

/// One `client_call` row in progress: the handler of one MCP tool call or
/// one CLI command that calls the daemon.
///
/// The `setup` stage runs from [`ClientCall::start`] until
/// [`ClientCall::ready`]. The `render` stage runs from the last daemon answer
/// until [`ClientCall::finish`].
#[derive(Debug)]
pub struct ClientCall {
    log: OpLog,
    client: Client,
    tool: String,
    op: String,
    corpus: Option<String>,
    request_id: RequestId,
    start: Instant,
    ready_at: Option<Instant>,
    answered_at: Option<Instant>,
    outcome: Option<Outcome>,
    round_trip: Duration,
    response_bytes: u64,
}

impl ClientCall {
    /// Starts the handler clock and generates the request id.
    pub fn start(log: &OpLog, client: Client, tool: &str, op: &str, corpus: Option<&str>) -> Self {
        Self {
            log: log.clone(),
            client,
            tool: tool.to_owned(),
            op: op.to_owned(),
            corpus: corpus.map(str::to_owned),
            request_id: RequestId::generate(),
            start: Instant::now(),
            ready_at: None,
            answered_at: None,
            outcome: None,
            round_trip: Duration::ZERO,
            response_bytes: 0,
        }
    }

    /// Id that the client sends with each request of this call.
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Ends the `setup` stage: the daemon client is ready.
    pub fn ready(&mut self) {
        if self.ready_at.is_none() {
            self.ready_at = Some(Instant::now());
        }
    }

    /// Records an outcome that the client decides without a daemon answer.
    pub fn unanswered(&mut self, outcome: Outcome) {
        self.record(outcome, None);
    }

    /// Sets the row outcome to the result that the caller receives. It
    /// replaces the outcomes of the single daemon calls, for a call that
    /// sends more than one request or recovers from a failed one. A later
    /// [`Self::finish`] still keeps a recorded outcome that is not `ok`.
    pub fn settle(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }

    /// Records the outcome of one daemon call. The first outcome that is not
    /// `ok` stays the outcome of the row.
    pub(crate) fn record(&mut self, outcome: Outcome, answer: Option<Answer>) {
        match self.outcome {
            None | Some(Outcome::Ok) => self.outcome = Some(outcome),
            Some(
                Outcome::InvalidParams
                | Outcome::Internal
                | Outcome::Retryable
                | Outcome::Cancelled(_)
                | Outcome::Unavailable,
            ) => {}
        }
        let Some(Answer {
            round_trip,
            response_bytes,
        }) = answer
        else {
            return;
        };
        self.round_trip += round_trip;
        self.response_bytes += response_bytes;
        self.answered_at = Some(Instant::now());
    }

    /// Writes the row with [`OpLog::append`]. `fallback` is the outcome of the
    /// caller result. A `fallback` that is not `ok` replaces a recorded `ok`.
    /// A recorded outcome that is not `ok` stays.
    pub fn finish(self, fallback: Outcome) {
        let end = Instant::now();
        let Self {
            log,
            client,
            tool,
            op,
            corpus,
            request_id,
            start,
            ready_at,
            answered_at,
            outcome,
            round_trip,
            response_bytes,
        } = self;
        let outcome = match outcome {
            Some(Outcome::Ok) | None => fallback,
            Some(
                recorded @ (Outcome::InvalidParams
                | Outcome::Internal
                | Outcome::Retryable
                | Outcome::Cancelled(_)
                | Outcome::Unavailable),
            ) => recorded,
        };
        let mut stages = Stages::default();
        if let Some(ready_at) = ready_at {
            stages.add(SETUP_STAGE, Millis::from_duration(ready_at - start));
        }
        if let Some(answered_at) = answered_at {
            stages.add(RENDER_STAGE, Millis::from_duration(end - answered_at));
        }
        let (daemon_round_trip_ms, response_bytes) = match answered_at {
            Some(_) => (
                Some(Millis::from_duration(round_trip)),
                Some(response_bytes),
            ),
            None => (None, None),
        };
        let record = OperationRecord {
            schema_version: SCHEMA_VERSION,
            ts: Utc::now(),
            request_id,
            op,
            outcome,
            duration_ms: Millis::from_duration(end - start),
            corpus,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            kind: RecordKind::ClientCall(ClientCallFields {
                client,
                tool,
                stages,
                daemon_round_trip_ms,
                response_bytes,
            }),
        };
        log.append(&record);
    }
}

#[cfg(test)]
mod tests {
    use hallouminate_config::LoggingConfig;

    use super::*;
    use crate::oplog::Reason;

    fn answer() -> Answer {
        Answer {
            round_trip: Duration::from_millis(5),
            response_bytes: 42,
        }
    }

    fn written_row(dir: &std::path::Path) -> OperationRecord {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            let text = std::fs::read_to_string(&path).expect("read segment");
            for line in text.lines() {
                rows.push(serde_json::from_str::<OperationRecord>(line).expect("row"));
            }
        }
        assert_eq!(rows.len(), 1, "one row per call");
        rows.remove(0)
    }

    fn call(dir: &std::path::Path) -> ClientCall {
        let log = OpLog::in_dir(dir, &LoggingConfig::default());
        ClientCall::start(&log, Client::Mcp, "ground", "ground", None)
    }

    fn client_fields(row: OperationRecord) -> ClientCallFields {
        let OperationRecord { kind, .. } = row;
        let RecordKind::ClientCall(fields) = kind else {
            panic!("expected a client_call row");
        };
        fields
    }

    #[test]
    fn client_error_after_daemon_ok_replaces_the_recorded_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut call = call(dir.path());
        call.record(Outcome::Ok, Some(answer()));
        call.finish(Outcome::Internal);
        let row = written_row(dir.path());
        assert_eq!(row.outcome, Outcome::Internal);
    }

    #[test]
    fn recorded_non_ok_outcome_survives_an_ok_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut call = call(dir.path());
        call.record(Outcome::Retryable, Some(answer()));
        call.finish(Outcome::Ok);
        let row = written_row(dir.path());
        assert_eq!(row.outcome, Outcome::Retryable);
    }

    #[test]
    fn answer_fields_survive_a_later_unavailable_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut call = call(dir.path());
        call.record(Outcome::Ok, Some(answer()));
        call.unanswered(Outcome::Unavailable);
        call.settle(Outcome::Unavailable);
        call.finish(Outcome::Unavailable);
        let row = written_row(dir.path());
        assert_eq!(row.outcome, Outcome::Unavailable);
        let fields = client_fields(row);
        assert!(fields.daemon_round_trip_ms.is_some());
        assert_eq!(fields.response_bytes, Some(42));
        let stages = serde_json::to_value(&fields.stages).expect("stages");
        assert!(stages.get(RENDER_STAGE).is_some(), "render stage kept");
    }

    #[test]
    fn unanswered_call_has_no_answer_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut call = call(dir.path());
        call.unanswered(Outcome::Cancelled(Reason::Timeout));
        call.finish(Outcome::Internal);
        let fields = client_fields(written_row(dir.path()));
        assert_eq!(fields.daemon_round_trip_ms, None);
        assert_eq!(fields.response_bytes, None);
    }
}
