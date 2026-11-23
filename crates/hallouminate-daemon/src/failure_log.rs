//! One tracing event per error response that the daemon sends to a client.

use super::ipc::{DaemonResponse, ErrorKind};

/// Writes one tracing event for an error response that the daemon sends to a
/// client. `op` is the request's operation name when the request parsed.
///
/// The event carries the response message, so its content depends on the
/// level. `Internal` responses log at ERROR; their messages can carry store
/// errors and file paths, and handlers avoid putting request text in them.
/// `InvalidParams` responses log at DEBUG; their messages can echo request
/// values, for example the serde error of a malformed request. `Retryable`
/// responses log at INFO; they are backpressure signals with fixed text.
/// Request bodies are never logged as such.
/// The log escapes control characters without changing the response.
pub(crate) fn log_error_response(op: Option<&'static str>, response: &DaemonResponse) {
    let (kind, message) = match response {
        DaemonResponse::Ok { .. } => return,
        DaemonResponse::Err { kind, message } => (kind, message),
    };
    let message = message.escape_debug();
    match kind {
        ErrorKind::Internal => tracing::error!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed: {message}",
        ),
        ErrorKind::Retryable => tracing::info!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed: {message}",
        ),
        ErrorKind::InvalidParams => tracing::debug!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed: {message}",
        ),
    }
}
