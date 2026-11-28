---
title: "Operations log"
---

Hallouminate appends one JSON row for each daemon operation and one for each
client call to a local operations log. A row says how an operation ended and
where its time went. This page is the stable contract for **schema version 1**.

## Where the log lives

The log is a set of daily segment files in the state directory,
`$XDG_STATE_HOME/hallouminate` (default `~/.local/state/hallouminate`):

```text
operations-2026-11-16.jsonl
```

Each segment is named `operations-YYYY-MM-DD.jsonl` by UTC date. No file named
`operations.jsonl` exists. Read the log with the glob `operations-*.jsonl`.
Each line is one complete JSON object.
The `operations.lock` file coordinates appends and retention. On Unix, a new
lock file has mode `0600`. New segments still respect the process umask.
Do not remove the lock file while a Hallouminate process uses this state directory.

Two keys in the baseline config control the log:

```toml
[logging]
operations = true                     # default; set false to write no rows
operations_max_total_bytes = 52428800 # default; 50 MiB across all segments
```

Both keys are baseline-owned like the rest of `[logging]`, so a repository
config that sets them is an error. See [Configuration](/hallouminate/config/).
When `[logging] operations` is `false`, the `request_id` is still generated and
returned. Only the daemon deletes old segments, oldest first, and it never
deletes the segment for the current UTC date.

## Two rows per call

A call that reaches the daemon writes two rows that share a `request_id`:

- a `daemon_op` row from the daemon, with the outcome and the stage durations;
- a `client_call` row from the client (the MCP server or the CLI), with the
  handler time, the daemon round-trip time, and the response size.

A call that cannot reach the daemon, or that the client rejects before it sends
the request, writes one `client_call` row. A request that the daemon rejects
before dispatch (unauthorized peer, a line that does not parse, an oversized
line, or invalid UTF-8) writes one `daemon_op` row with outcome
`invalid_params` and an empty `stages` object. A connection that sends no
request line writes no row.

`daemon status`, `daemon stop`, and `daemon restart` each write a `client_call`
row. The internal `Ping` probe of the daemon bootstrap writes none.

## Common fields

Every row has these fields.

| Field | Meaning |
| --- | --- |
| `schema_version` | Schema version of the row. Always `1` in this version. |
| `ts` | End of the operation, RFC 3339 UTC. |
| `kind` | `daemon_op` or `client_call`. |
| `request_id` | 32 lowercase hex characters. Joins the two rows of one call. |
| `op` | Snake_case name of the daemon request: `ground`, `add_markdown`, `add_markdown_batch`, `status`, `ping`, `shutdown`, and so on. `unknown` when the daemon cannot parse the request line. |
| `outcome` | `ok`, `invalid_params`, `internal`, `retryable`, `cancelled`, or `unavailable`. |
| `reason` | Present only when `outcome` is `cancelled`: `shutdown` on a `daemon_op` row, `timeout` on a `client_call` row. Absent on every other row. |
| `duration_ms` | Total time of the operation in milliseconds. On a `client_call` row, the handler time. |
| `corpus` | Corpus name that the request names, or `null` when it names none. |
| `version` | Hallouminate version of the writing process. |

Every duration and count is a JSON number, or `null` where this page says that
no value exists.

### `daemon_op` fields

| Field | Meaning |
| --- | --- |
| `stages` | Object that maps a stage name to milliseconds. |
| `warning_count` | Number of warnings that the operation returned. |
| `warning_codes` | Codes of the coded warnings that the operation returned. An empty list when there are none. |

The `outcome` of a `daemon_op` row is the outcome of the response that the
daemon sends. A response over the IPC frame limit becomes an `invalid_params`
error, and the row records that outcome with the stages that ran.

### `client_call` fields

| Field | Meaning |
| --- | --- |
| `client` | `mcp` for the MCP server, `cli` for a CLI command. |
| `tool` | MCP tool name on an `mcp` row, subcommand name on a `cli` row. |
| `stages` | Object that maps a handler stage name to milliseconds. |
| `daemon_round_trip_ms` | Time from the send of the request to the answer, or `null` when no answer arrived. |
| `response_bytes` | Byte length of the daemon response that the client received, or `null` when no answer arrived. |

The `outcome` of a `client_call` row is the result that the caller receives.
For a call with one daemon request, that is the outcome that the daemon
returned. When the caller result is not `ok`, it replaces a recorded `ok`. A
recorded outcome that is not `ok` stays. When no answer arrives, the outcome
is `unavailable` (the daemon cannot be reached, or the connection drops after
the send) or `cancelled` with `reason` `timeout` (the client RPC timeout
expired). On those rows `daemon_round_trip_ms` and `response_bytes` are
`null`. When the client rejects a call before it sends the request, the row
carries the outcome of that error (for example `invalid_params` for an
oversized `add_markdown` batch), and the same two fields are `null`. A daemon
shutdown that kills an in-flight call gives that call no answer, so its answer
fields are `null`. A `daemon restart` whose shutdown gets an answer but whose
respawn fails gives an `unavailable` client row. That row keeps the round trip
and the response size of the answer that it received. When `daemon restart`
finds no running daemon, its row carries the outcome of the start: `ok` when
the new daemon starts.

A batch `read_markdown` call sends one daemon request per item, and every
request carries the same `request_id`. Thus one `client_call` row joins one
`daemon_op` row per item that reached the daemon. The `client_call` outcome is
the tool result: `ok` when the tool returns its results, also when some items
fail. Each failed item shows its own outcome on its `daemon_op` row. On the
`client_call` row, `daemon_round_trip_ms` is the sum of the round trips of the
answered items, and `response_bytes` is the sum of their response sizes. Both
fields are `null` when no item got an answer, even when the outcome is `ok`.

A tool call with a missing or `null` `cwd` fails parameter decoding before the
tool runs. It writes no row and carries no `request_id`. A relative `cwd` fails
later, in the tool setup. It writes an `invalid_params` `client_call` row, and
the error carries `error.data.request_id`.

The handler stages and `daemon_round_trip_ms` of a `client_call` row sum to at
most `duration_ms`.

## Stage names

| Operation | Stages |
| --- | --- |
| `ground` | `resolve`, `cold_wait`, `inventory`, `search`, `fuse`, `rerank`, `render` |
| `add_markdown`, `add_markdown_batch` | `validate`, `debt_gate_wait`, `corpus_lock_wait`, `write_lane_wait`, `edit`, `write`, `ancestor_index`, `reindex` |
| every other operation | `resolve`, `operation` (`ping` and `shutdown` record only `operation`) |
| `client_call` rows, any tool | `setup`, `render` |

On a `client_call` row, `setup` runs from the handler start until the daemon
client is ready, including a daemon start when one is needed. `render` runs from
the daemon answer until the handler returns. The daemon round trip is the field
`daemon_round_trip_ms`, not a stage.

A stage that does not run has no entry. A stage that runs more than once in one
operation records the sum of its runs; `add_markdown_batch` runs the write
stages once for each item. The stage durations sum to at most `duration_ms`. The
remainder is unattributed time.

## Examples

Each example is one line of a segment file.

A `daemon_op` row with outcome `ok`:

```json
{"schema_version":1,"ts":"2026-11-16T04:12:09.481Z","request_id":"4bf92f3577b34da6a3ce929d0e0e4736","op":"ground","outcome":"ok","duration_ms":212.4,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"daemon_op","stages":{"resolve":3.1,"inventory":6.8,"search":141.7,"fuse":2.2,"rerank":48.9,"render":4.0},"warning_count":1,"warning_codes":["ripgrep-timeout"]}
```

A `daemon_op` row with outcome `invalid_params`, here a request line that does
not parse:

```json
{"schema_version":1,"ts":"2026-11-16T04:13:40.002Z","request_id":"0af7651916cd43dd8448eb211c80319c","op":"unknown","outcome":"invalid_params","duration_ms":0.2,"corpus":null,"version":"0.1.0","kind":"daemon_op","stages":{},"warning_count":0,"warning_codes":[]}
```

A `daemon_op` row with outcome `internal`:

```json
{"schema_version":1,"ts":"2026-11-16T04:15:21.730Z","request_id":"b7ad6b7169203331b5d1a0c4e6f2b8a1","op":"add_markdown","outcome":"internal","duration_ms":57.3,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"daemon_op","stages":{"validate":0.4,"debt_gate_wait":0.1,"corpus_lock_wait":1.2,"write_lane_wait":0.3,"write":52.9},"warning_count":0,"warning_codes":[]}
```

A `daemon_op` row with outcome `retryable`, here a refusal by write
backpressure:

```json
{"schema_version":1,"ts":"2026-11-16T04:16:02.118Z","request_id":"c3f1e8a2d9b44f60a7e5129b8d0c6f34","op":"add_markdown","outcome":"retryable","duration_ms":30012.6,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"daemon_op","stages":{"validate":0.5,"debt_gate_wait":30010.9},"warning_count":0,"warning_codes":[]}
```

A `daemon_op` row with outcome `cancelled`, where the daemon shut down during
the operation:

```json
{"schema_version":1,"ts":"2026-11-16T04:17:45.903Z","request_id":"d41d8cd98f00b204e9800998ecf8427e","op":"ground","outcome":"cancelled","reason":"shutdown","duration_ms":1830.5,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"daemon_op","stages":{"resolve":2.9,"cold_wait":1820.4},"warning_count":0,"warning_codes":[]}
```

A `client_call` row from the MCP server with outcome `ok`. It joins to the first
`daemon_op` example by `request_id` in a real log:

```json
{"schema_version":1,"ts":"2026-11-16T04:12:09.495Z","request_id":"4bf92f3577b34da6a3ce929d0e0e4736","op":"ground","outcome":"ok","duration_ms":231.9,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"client_call","client":"mcp","tool":"ground","stages":{"setup":4.6,"render":2.3},"daemon_round_trip_ms":224.8,"response_bytes":18734}
```

A `client_call` row from the MCP server with outcome `invalid_params`, here an
oversized `add_markdown` batch that
the client rejects before it sends the request. No `daemon_op` row exists for
it:

```json
{"schema_version":1,"ts":"2026-11-16T04:20:11.640Z","request_id":"e2c8a4f01b7d49c3a65f0d8e3b9a7c15","op":"add_markdown_batch","outcome":"invalid_params","duration_ms":0.9,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"client_call","client":"mcp","tool":"add_markdown","stages":{"setup":0.7},"daemon_round_trip_ms":null,"response_bytes":null}
```

A `client_call` row with outcome `internal`:

```json
{"schema_version":1,"ts":"2026-11-16T04:15:21.744Z","request_id":"b7ad6b7169203331b5d1a0c4e6f2b8a1","op":"add_markdown","outcome":"internal","duration_ms":71.0,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"client_call","client":"mcp","tool":"add_markdown","stages":{"setup":3.9,"render":0.6},"daemon_round_trip_ms":62.8,"response_bytes":212}
```

A `client_call` row with outcome `retryable`:

```json
{"schema_version":1,"ts":"2026-11-16T04:16:02.131Z","request_id":"c3f1e8a2d9b44f60a7e5129b8d0c6f34","op":"add_markdown","outcome":"retryable","duration_ms":30025.4,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"client_call","client":"mcp","tool":"add_markdown","stages":{"setup":4.1,"render":0.5},"daemon_round_trip_ms":30016.2,"response_bytes":190}
```

A `client_call` row with outcome `cancelled` and `reason` `timeout`, where the
client RPC timeout expired. The two daemon fields are `null`:

```json
{"schema_version":1,"ts":"2026-11-16T04:25:30.077Z","request_id":"f6a1c0d9e8b747d2a3c4b5e6f7081920","op":"ground","outcome":"cancelled","reason":"timeout","duration_ms":60004.7,"corpus":"repo:hallouminate:wiki","version":"0.1.0","kind":"client_call","client":"mcp","tool":"ground","stages":{"setup":3.8},"daemon_round_trip_ms":null,"response_bytes":null}
```

A `client_call` row with outcome `unavailable`, where the daemon cannot be
reached. It has no `render` stage:

```json
{"schema_version":1,"ts":"2026-11-16T04:30:02.512Z","request_id":"9a8b7c6d5e4f40312a3b4c5d6e7f8091","op":"status","outcome":"unavailable","duration_ms":15.3,"corpus":null,"version":"0.1.0","kind":"client_call","client":"cli","tool":"status","stages":{"setup":15.1},"daemon_round_trip_ms":null,"response_bytes":null}
```

## Join the two rows with DuckDB

DuckDB reads the segments directly. This query shows, for each call with an
`ok` outcome, the server time, the client time, and the time that passed
outside the daemon. A batch can be `ok` when some items got no answer. For
such a batch, `daemon_ms` includes the unanswered items, so the transport time
can be low or negative. It sums the `daemon_op` rows of each
`request_id` first, because a batch call has one `daemon_op` row per item:

```sql
WITH ops AS (
    SELECT *
    FROM read_json_auto('~/.local/state/hallouminate/operations-*.jsonl',
                        format = 'newline_delimited', union_by_name = true)
),
daemon AS (
    SELECT request_id, SUM(duration_ms) AS daemon_ms
    FROM ops
    WHERE kind = 'daemon_op'
    GROUP BY request_id
)
SELECT c.request_id,
       c.tool,
       c.outcome,
       d.daemon_ms,
       c.duration_ms AS handler_ms,
       c.daemon_round_trip_ms - d.daemon_ms AS transport_ms
FROM ops AS c
JOIN daemon AS d ON c.request_id = d.request_id
WHERE c.kind = 'client_call' AND c.outcome = 'ok'
  AND c.daemon_round_trip_ms IS NOT NULL
ORDER BY handler_ms DESC;
```

## What the timings measure

Timings measure work inside the Hallouminate processes. They exclude host and
harness delivery time, such as the time that an agent harness takes to hand a
tool call to the MCP server or to show the result. A wall time from a harness
log is therefore longer than `duration_ms`. The MCP library encodes the response
after the handler returns, so version 1 has no serialization time.

## What a row never holds

A row holds no query text and no file content. It holds names, counts, codes,
and durations only.

## Compatibility

The schema is additive only. A later version adds fields and never changes the
meaning of an existing field. Readers must ignore fields that they do not know,
and can use `schema_version` to find rows of a version that they understand.
