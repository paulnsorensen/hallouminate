# Request identifier in every MCP tool response

<certain> Every Hallouminate MCP tool returns the `request_id` of its call, so a session log joins exactly to the rows of the operations log. The response gains no timing field. **Accepted design; not implemented.**

## ADR-003: Return the request identifier, and no timing block, from every tool  [status: accepted; not implemented]

- **Context:** <certain> The operations log holds the outcome and timing of each call. A person or an agent who reads a session log needs one exact key to find those rows. Each tool response also costs tokens on every call.
- **Decision:** <certain> A successful tool call returns `request_id` in its structured payload, and a failed call returns it in the error data. No tool gains a new timing field, and the `took_ms` field of `ground` stays unchanged. The identifier is returned when the operations log is disabled.
- **Alternatives:** <certain> A full timing block in each response costs tokens on every call and repeats what the log holds. A response with no identifier forces a join on timestamps, which is inexact when calls overlap.
- **Consequences:** <certain> Each response grows by about 40 characters. Every MCP tool payload gains one field; a search of `crates/hallouminate/src` finds no declared output schema that the field would break. The MCP library must carry error data for every error kind, and only the retryable kind carries data today, so this point is unverified.[^1]

See [[operation-outcome-record-001]] for the log and [[operation-outcome-record-002]] for the two row kinds.

[^1]: `crates/hallouminate/src/mcp/tools.rs` (`tool_ok` builds every success payload; the retryable error carries error data).

_Source: mold session for issues #454 and #456 · Updated: 2026-11-15_
