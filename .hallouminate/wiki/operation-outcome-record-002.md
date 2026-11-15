# Two operation rows per call, joined by a request identifier

<certain> One call to the Hallouminate daemon writes two rows to the operations log: a `daemon_op` row from the daemon and a `client_call` row from the MCP server or the CLI. Both rows share one `request_id`. **Accepted design; not implemented.**

## ADR-002: Write a daemon row and a client row for each call  [status: accepted; not implemented]

- **Context:** <certain> The daemon can time its own stages, but it cannot see the MCP handler time, the response serialization, or a call that never reaches it. A client that gives up leaves no trace of how the daemon ended the operation. The write-path and rerank designs need to know which side of the socket is slow.
- **Decision:** <certain> The client generates the `request_id` (32 lowercase hex characters) and sends it in an optional `DaemonRequest.request_id` field; the daemon generates one when the field is absent. The daemon row carries `op`, `outcome`, `duration_ms`, and the stage durations, which are `tracing` spans under one root span. The client row carries `client`, `tool`, the handler time, `daemon_round_trip_ms`, `serialize_ms`, and `response_bytes`. The outcome values are `ok`, `invalid_params`, `internal`, `retryable`, `cancelled`, and `unavailable`. `unavailable` appears only on a client row. `cancelled` carries `reason`: `shutdown` on a daemon row and `timeout` on a client row. A call that the client rejects before the send writes one client row. A field with no measured value is null.
- **Alternatives:** <certain> One daemon row with client timings sent on the next request loses the last call of a session and every call that cannot reach the daemon. A daemon row alone cannot locate time outside the daemon.
- **Consequences:** <certain> The row count doubles, and a reader joins on `request_id`. `DaemonRequest` has no `deny_unknown_fields`, so the optional field is additive.[^1] A daemon that is older than the client writes no row, so the reader sees a client row alone. The client must classify its own timeout as `cancelled` before `call_raw_with_timeout` turns it into the retryable error that the caller receives.[^2]

See [[operation-outcome-record-001]] for the log itself and [[operation-outcome-record-003]] for the identifier in tool responses.

[^1]: `crates/hallouminate-daemon/src/ipc.rs:8-21,40-44`.
[^2]: `crates/hallouminate-daemon/src/client.rs::call_raw_with_timeout`.

_Source: mold session for issues #454 and #456 · Updated: 2026-11-15_
