---
status: reviewed
last_verified: 2026-07-19
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/174
  - https://github.com/paulnsorensen/hallouminate/issues/201
---
# Socket protocol

Every caller that isn't the daemon itself — the CLI, the MCP stdio server, any future client — talks to the daemon over a Unix domain socket using a JSON-lines request/response framing: one request object per line in, one response object per line out, then the connection closes. There is no persistent multiplexed session and no in-band request id, because each connection carries exactly one request/response pair.[^1]

## Why line-delimited JSON over a binary protocol

A binary framing (length-prefixed protobuf, bincode, a custom header) would save bytes on the wire, but the daemon and its clients live in the same binary and the same release — there's no cross-language boundary that would justify a schema-compiled format. JSON-lines is trivially inspectable with `nc` or `socat` during debugging, requires no code-generation step when a request shape changes, and one line in / one line out maps directly onto the connect-send-recv-close lifecycle every client already uses. The cost — slower parsing and no compact binary encoding — doesn't matter at the request volumes and payload sizes a local developer daemon actually sees.

This is deliberately not a wire-compatibility-hardened protocol. There is no `version` envelope field and no `#[serde(deny_unknown_fields)]`, so a client built against one release talking to a daemon from another is not a supported configuration — see [daemon-and-cli](daemon-and-cli.md) for the compatibility caveat. Adding a real third-party client is the trigger for introducing an explicit version negotiation handshake, not something to build speculatively now.

## Request and response shape

A request line is an envelope with the caller's `cwd` and an inner `payload` naming the operation:

```json
{"cwd": "/path/to/client/cwd", "payload": {"op": "ground", "query": "..."}}
```

The daemon walks `cwd` on every request to resolve the active repo-layer config and merge it with the boot-time baseline, so the same daemon process can correctly serve requests from several repositories without being told which one is active out of band.

A response line is either a success or an error envelope:

```json
{"status": "ok", "result": {...}}
{"status": "err", "kind": "invalid_params", "message": "..."}
```

`kind` is a small closed set of error categories (caller-input failures, internal faults) rather than a per-operation error type, because every client — the CLI's error formatter, the MCP transport's JSON-RPC code mapping — needs to make the same coarse decision: is this the caller's fault, or the daemon's.[^2]

## Request deadlines

Every request carries a bounded wait on the client side: the client gives up on a hung daemon rather than blocking a CLI invocation or an MCP tool call indefinitely. A request that exceeds its deadline surfaces as a transport-level error to the caller, distinct from a `status: err` response — the daemon may still be working on it, but the client has stopped waiting. This matters most for scan-triggering operations (`index`, `add_markdown` against a large corpus), where the daemon-side work is itself wrapped in its own supervisory deadline; see [concurrency-and-supervision](concurrency-and-supervision.md) for how the two deadlines relate.

## CLI and MCP are both just clients

The CLI's `ground`, `index`, and `hook` subcommands, and the MCP server's tool handlers, do not talk to LanceDB, the corpus walker, or any domain type directly. Each constructs a request envelope, opens a connection with `client_for(...)`, writes one JSON line, reads one JSON line back, and renders the result in whatever shape its transport expects — a formatted CLI outline, or an MCP `structuredContent` object. Neither surface has privileged access to daemon internals; the protocol is the entire contract between them. This is what makes `hallouminate serve`'s auto-spawn-on-missing-daemon behavior safe to add without touching CLI code, and it's what let the daemon and application crates be split into separate crates without changing either surface's call sites — see [architecture](architecture.md).

See [daemon-and-cli](daemon-and-cli.md), [mcp-surface](mcp-surface.md), [concurrency-and-supervision](concurrency-and-supervision.md).

[^1]: `crates/hallouminate-daemon/src/socket.rs:1-40`; https://github.com/paulnsorensen/hallouminate/issues/174
[^2]: `crates/hallouminate-daemon/src/protocol.rs:1-58`; https://github.com/paulnsorensen/hallouminate/issues/201

_Source: issues #174 and #201 · Updated: 2026-07-19_