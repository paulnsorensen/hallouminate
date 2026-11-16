//! Operations log: one flat JSON row per operation.
//!
//! Rows go to daily segment files `operations-YYYY-MM-DD.jsonl` (UTC date of
//! the append) in the Hallouminate state directory. Each append opens the
//! current segment, writes one line, and closes it. Only the daemon deletes
//! old segments, through [`OpLog::append_and_enforce_cap`].

mod client_call;
mod record;
mod request_id;
mod row_layer;
mod writer;

pub(crate) use client_call::Answer;
pub use client_call::ClientCall;
pub use record::{
    Client, ClientCallFields, DaemonOpFields, InvalidMillis, Millis, OperationRecord, Outcome,
    Reason, RecordKind, SCHEMA_VERSION, Stages,
};
pub use request_id::{InvalidRequestId, RequestId};
pub use row_layer::{
    ROOT_SPAN, STAGE_SPAN, TARGET, UNKNOWN_OP, layer, record_outcome, record_warnings, root_span,
    stage,
};
pub use writer::OpLog;
