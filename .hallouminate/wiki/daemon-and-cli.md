# Daemon and CLI

## Why there's a daemon

LanceDB does not support concurrent writer processes against the same
table. If both a CLI `index` and an MCP `add_markdown` opened LanceDB
directly they would race on table mutations. The daemon is the single
owner of the LanceDB ground directory, per-corpus mutation locks, and
the repository registry. Every other caller — CLI subcommand, MCP
tool, future agent — dials the daemon over a Unix domain socket.

## Socket location

- Default: `${XDG_CACHE_HOME:-~/.cache}/hallouminate/daemon.sock`
- Per-process override: `HALLOUMINATE_SOCKET` env var
- CLI override: `--socket PATH` on `index`, `ground`, etc.

The daemon takes a flock on `<socket>.lock` to enforce single-instance
ownership. A second `hallouminate daemon` against the same socket
errors out with "another hallouminate daemon already holds …".

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
