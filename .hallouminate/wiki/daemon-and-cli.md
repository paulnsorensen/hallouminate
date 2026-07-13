# Daemon and CLI

## Why there's a daemon

LanceDB does not support concurrent writer processes against the same
table. If both a CLI `index` and an MCP `add_markdown` opened LanceDB
directly they would race on table mutations. The daemon is the single
owner of the LanceDB ground directory, per-corpus mutation locks, and
the repository registry. Every other caller — CLI subcommand, MCP
tool, future agent — dials the daemon over a Unix domain socket.

## Socket location

Resolved in this order (`src/app/daemon/socket.rs`):

1. `HALLOUMINATE_SOCKET` env var — per-process override.
2. `$XDG_RUNTIME_DIR/hallouminate/daemon.sock` — the default when a
   runtime dir exists.
3. `${XDG_CACHE_HOME:-~/.cache}/hallouminate/daemon.sock` — fallback.

`--socket PATH` on `index`, `ground`, etc. overrides per-invocation.

The daemon takes a flock on `<socket>.lock` to enforce single-instance
ownership. A second `hallouminate daemon` against the same socket
errors out with "another hallouminate daemon already holds …".

**Divergence gotcha (#218).** Single-instance is per *socket path*, not
per machine. Clients launched from environments that disagree on
`XDG_RUNTIME_DIR` (systemd user session vs detached shell) resolve
different paths, each auto-spawns, each wins its own flock → two fully
resident daemons, each with a loaded embedding model. Observed live
(2026-07-13): a stale `~/.cache/hallouminate/daemon.sock` alongside the
active `/run/user/…` socket is the tell. Store co-ownership is guarded
by #205's single-owner store flock; the memory doubling remains.

**Cold start (#220).** A client that misses the connect probe spawns a
daemon candidate and polls its socket for one 30s window with no retry
(`src/app/daemon/bootstrap.rs:24,42-110`). flock losers exit *before*
loading the model or opening LanceDB
(`src/app/daemon/server.rs:70-93,463-484`) — a stampede of candidates
is cheap by design; the risk is spurious client startup failures when
the winner's open takes >30s under system load.

## Wire protocol

JSON-lines over the socket: one request line in, one response line
out, the connection closes. There's no in-band correlation id because
the request/response pair maps 1:1 to the connection.

Request envelope:

```json
{
  "cwd": "/path/to/client/cwd",
  "payload": {"op": "ground", "query": "…"}
}
```

Response envelope (success):

```json
{"status":"ok","result":{...}}
```

Response envelope (error):

```json
{"status":"err","kind":"invalid_params","message":"..."}
```

`cwd` is the client's working directory at request time. The daemon
walks it on every request to discover the active repo-layer config
(`.hallouminate/config.toml`) and merges it with the boot baseline.
That's how a single daemon can serve many repos with different configs.

## Wire compatibility

v1 ships from a single binary — CLI, MCP, and daemon are all the same
executable. There is no protocol-version envelope and no
`#[serde(deny_unknown_fields)]`. Cross-version IPC (a client from one
release talking to a daemon from another) is not a supported
configuration. A future standalone client (third-party Python client,
out-of-process agent) must first add an explicit `version: u32` and a
negotiation handshake.

## CLI subcommands

| Command | Purpose |
|---|---|
| `hallouminate index [--corpus NAME]` | bulk index one or all corpora |
| `hallouminate ground "<query>" [...]` | semantic search; `--format outline\|json\|json-pretty`, `--full`, `--top-files N`, `--chunks-per-file N`, `--limit N`, `--snippet-chars N` |
| `hallouminate serve` | stdio MCP server (auto-spawns daemon if down) |
| `hallouminate daemon [--config PATH]` | run the daemon in the foreground |
| `hallouminate config init\|show\|validate\|download` | config inspection / scaffolding |
| `hallouminate hook install\|uninstall [--repo PATH]` | per-repo discovery hook install |

## Write-lane and per-corpus locks

Mutating ops (`Index`, `AddMarkdown`, `DeleteMarkdown`) take the
per-corpus mutation lock and then a global write-lane semaphore in
that order. Read ops (`Ping`, `ListCorpora`, `ListFiles`,
`ReadMarkdown`, `Ground`) skip both and run concurrently. The lock
order is invariant — taking write-lane first then per-corpus would be
the classic deadlock recipe.

Verified request-concurrency model (2026-07-13 audit):

- Task-per-connection, capped by a 64-permit semaphore acquired before
  spawn (`src/app/daemon/server.rs:48,295`); 4MiB request-line cap and
  30s read/write IO timeouts per connection (`server.rs:36-42,398-449`).
- Resources (LanceDB store, tokenizer, embedder) are cached per
  `ResourceKey = (ground_dir, model, quantized, enabled)`
  (`src/app/daemon/state.rs:121-136`), built lazily behind a keyed build
  lock, never evicted (ADR-001: the ONNX arena can't release anyway).
- Embeds serialize per key on the cached embedder's mutex
  (`state.rs:156-172`); grounds across keys run genuinely parallel. A
  bulk index holds the per-key embedder guard for its entire run — see
  [blocking-inference-offload](blocking-inference-offload.md) (#219),
  and #216 for the missing client-side RPC timeout that turns a busy
  daemon into indefinitely hung callers.

_Source: multi-instance concurrency audit, `.cheese/concurrency-audit/notes.md` (branch `claude/fix-concurrency`) · Updated: 2026-07-13 · Supersedes: —_
