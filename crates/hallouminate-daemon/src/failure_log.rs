//! One tracing event per error response that the daemon sends to a client.

use std::borrow::Cow;

use super::ipc::{DaemonResponse, ErrorKind};

/// Writes one tracing event for an error response that the daemon sends to a
/// client. `op` is the request's operation name when the request parsed.
///
/// Only Internal responses include the escaped response message. INFO and
/// DEBUG events omit it because it can contain request values.
pub(crate) fn log_error_response(op: Option<&'static str>, response: &DaemonResponse) {
    let (kind, message) = match response {
        DaemonResponse::Ok { .. } => return,
        DaemonResponse::Err { kind, message } => (kind, message),
    };
    match kind {
        ErrorKind::Internal => tracing::error!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed: {}", escape_controls(message),
        ),
        ErrorKind::Retryable => tracing::info!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed",
        ),
        ErrorKind::InvalidParams => tracing::debug!(
            target: "hallouminate::daemon",
            op,
            kind = ?kind,
            "request failed",
        ),
    }
}

pub(super) fn escape_controls(message: &str) -> Cow<'_, str> {
    let Some(first_control) = message.find(char::is_control) else {
        return Cow::Borrowed(message);
    };
    let (prefix, suffix) = message.split_at(first_control);
    let mut escaped = String::with_capacity(message.len());
    escaped.push_str(prefix);
    for character in suffix.chars() {
        if character.is_control() {
            escaped.extend(character.escape_debug());
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}
