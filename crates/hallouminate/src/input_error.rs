//! Structural marker for caller-supplied-input errors.
//!
//! Producers in `cli::ground` / `cli::index` construct `InputError(msg)`
//! when the cause is a user-input problem — unknown corpus name, missing
//! `--corpus` when ambiguous, etc. — and convert into `anyhow::Error` via
//! `.into()` (or the `?` operator). The MCP `map_app_error` walks the
//! `anyhow::Error` chain looking for this type via
//! `anyhow::Error::downcast_ref::<InputError>()` to decide between
//! JSON-RPC `-32602 invalid_params` (marker present) and `-32603
//! internal_error` (marker absent).
//!
//! This replaces the previous substring match on `err.to_string()`, which
//! silently regressed any time a producer reworded its message. Anything
//! unmarked is treated as a server fault by design — accidentally
//! un-marking a producer is caught by the `map_app_error` unit tests
//! rather than by silently flipping a real error back to `internal_error`
//! in production.

use std::fmt;

/// Caller-supplied-input error. The wrapped string is the user-facing
/// message; the type itself is the structural marker the MCP adapter
/// downcasts to. `From<InputError> for anyhow::Error` is provided by the
/// blanket impl in `anyhow` (since `InputError: Error + Send + Sync +
/// 'static`), so producer call sites stay one line:
/// `return Err(InputError(format!("…")).into());`.
#[derive(Debug)]
pub struct InputError(pub String);

impl InputError {
    /// Convenience constructor for `Display`-able messages, mirroring
    /// `anyhow!("…")` ergonomics.
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

/// True if `InputError` is present anywhere in the `anyhow::Error` chain —
/// whether it's the head error or buried under further `.context(...)`
/// layers a producer added for traceability. Used by the MCP adapter;
/// lives in the app layer because the marker is an app-level contract
/// between CLI producers and transport adapters.
///
/// `anyhow::Error::downcast_ref` walks the typed `ContextError` chain
/// recursively, peeling into each context's inner error, so this finds
/// the marker even when the producer has wrapped further context on top.
pub fn is_input_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<InputError>().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_input_error_finds_head_marker() {
        let err: anyhow::Error = InputError::new("bad corpus").into();
        assert!(is_input_error(&err));
    }

    #[test]
    fn is_input_error_finds_marker_under_further_context_layers() {
        // Producers commonly add `.with_context(...)` layers after the
        // marker to attach file paths / call-site detail. The marker must
        // remain discoverable when wrapped further up the chain.
        let err: anyhow::Error =
            anyhow::Error::new(InputError::new("bad corpus")).context("while running ground");
        assert!(is_input_error(&err));
    }

    #[test]
    fn is_input_error_returns_false_when_marker_absent() {
        let err: anyhow::Error = anyhow::anyhow!("disk IO blew up");
        assert!(!is_input_error(&err));
    }

    #[test]
    fn is_input_error_returns_false_for_unrelated_context_layers() {
        // Hardening: a `with_context` chain that never includes the marker
        // must still report false even when other anyhow internals are in
        // play. Catches a future bug where the chain walk accidentally
        // matches on `Display`-based heuristics instead of a typed downcast.
        let err: anyhow::Error = anyhow::anyhow!("disk IO blew up")
            .context("opening ground dir")
            .context("during index");
        assert!(!is_input_error(&err));
    }

    #[test]
    fn display_forwards_inner_message_so_existing_callers_keep_working() {
        // Producer-side tests assert `err.to_string().contains("…")`. The
        // marker's Display must surface the wrapped message verbatim so
        // those assertions hold without rewording.
        let err: anyhow::Error = InputError::new("corpus \"docs\" not found in config").into();
        assert!(
            err.to_string().contains("docs") && err.to_string().contains("not found"),
            "InputError must surface its message via Display: {err}"
        );
    }
}
