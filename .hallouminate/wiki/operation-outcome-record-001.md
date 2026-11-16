# Operation log as the outcome record

<certain> Hallouminate records how each operation ended, and where its time went, in a dedicated local log of flat JSON rows. **Accepted design; not implemented.** The draft spec is `operation-outcome-record` (issues #454 and #456).[^1]

## ADR-001: Write operation outcomes to a dedicated, versioned log  [status: accepted; not implemented]

- **Context:** <certain> No Hallouminate record states the outcome of an operation or its stage timing. `took_ms` is one scalar on `ground` only. Session logs from 93 sessions gave wall times (`ground` p50 4.4 s, p95 21 s; `add_markdown` p95 30.3 s) that mix server time with host delivery time. Issue #456 requires a contract that does not depend on a harness log format.[^2]
- **Decision:** <certain> Every process that takes part in a call appends one flat JSON row per operation to the operations log in the Hallouminate state directory. The log is stored as daily segment files, `operations-YYYY-MM-DD.jsonl` by UTC date, and a reader uses the glob `operations-*.jsonl`. Each row carries `schema_version` 1, and the schema is additive only. The log is on by default; `[logging] operations = false` disables it. `[logging] operations_max_total_bytes` (default 50 MiB) caps all segments: the daemon checks the cap after each row that it appends, deletes the oldest segments, and never deletes the segment for the current date. A row holds no query text and no file content. A failed row write never changes the result of the operation.
- **Alternatives:** <certain> OpenTelemetry with a local collector adds a dependency and a second process for a local, single-user tool; the user weighed it and chose the local file. The `request_id` uses the W3C trace-id shape, so an OpenTelemetry layer can be added later. The existing tracing log is a lossy rotating text log with no stable schema, so it cannot carry a contract.[^3] Timing fields in MCP responses alone depend on a harness log that keeps them.
- **Consequences:** <certain> DuckDB reads the segments directly, with no harness log. A new default-on file appears in the state directory. Rows from several processes depend on one-line appends, which the acceptance tests cover on Linux and macOS; Windows is outside the daemon transport.

See [[operation-outcome-record-002]] for the two row kinds and [[operation-outcome-record-003]] for the identifier in tool responses.

[^1]: [Issue #454](https://github.com/paulnsorensen/hallouminate/issues/454); [issue #456](https://github.com/paulnsorensen/hallouminate/issues/456).
[^2]: `crates/hallouminate-domain/src/ground/orchestrate.rs` sets `took_ms`; session-log evidence from the backlog triage of 2026-11-15.
[^3]: `crates/hallouminate/src/logging.rs:34-48`.

_Source: mold session for issues #454 and #456 · Updated: 2026-11-15_
