# Architecture

Hallouminate uses a [Sliced Bread](https://github.com/paulnsorensen/hallouminate)
layout — vertical slices with public APIs at slice boundaries, no cross-slice
peeks at internals. Three top-level concerns under `src/`.

## Layers

### `src/app/` — orchestration

The application layer composes domain logic with adapters. It owns the
clap-derived CLI (`cli.rs`), the long-lived `daemon/`, config parsing and the
XDG/repo-layer merge (`config.rs`), logging, and XDG path resolution. App
depends on `domain` and `adapters`; it does not own pure logic.

### `src/domain/` — pure logic

No I/O beyond filesystem walks and hashing. Slices: `corpus/` (chunker, walker,
hasher, sandbox, snippet), `embeddings/` (the fastembed wrapper), `ground/`
(search orchestration and result formatting), `indexer/` (plan/apply/write),
plus shared types in `common.rs`. Domain has no dependency on app or adapters.

### `src/adapters/` — external systems

`lance.rs` is the LanceDB vector-storage adapter; `mcp/` is the rmcp-based
stdio MCP server. Adapters depend on domain for types, but not on app.

The dependency direction is `adapters → domain ← app → adapters`: domain is
the stable core, app composes adapter implementations with domain
orchestration.

## Why there's a daemon

LanceDB does not support concurrent writer processes against the same table.
If a CLI `index` and an MCP `add_markdown` both opened LanceDB directly, they
would race on table mutations. The daemon is the single owner of the LanceDB
ground directory, the per-corpus mutation locks, and the repository registry.
Every other caller — CLI subcommand, MCP tool, future agent — dials the daemon
over a Unix domain socket.

### Socket location

The socket path resolves in this order (`src/app/daemon/socket.rs`):

1. `HALLOUMINATE_SOCKET` env var — per-process override.
2. `$XDG_RUNTIME_DIR/hallouminate/daemon.sock` — the default when a runtime
   dir exists.
3. `${XDG_CACHE_HOME:-~/.cache}/hallouminate/daemon.sock` — fallback.

The daemon takes a flock on `<socket>.lock` to enforce single-instance
ownership; a second daemon on the same socket errors out.

### Wire protocol

JSON-lines over the socket: one request line in, one response line out, then
the connection closes. The request carries the client's `cwd`, which the
daemon walks on every request to discover the active repo-layer config and
merge it with the boot baseline. That's how one daemon serves many repos with
different configs.

Mutating ops (`index`, `add_markdown`, `delete_markdown`) take the per-corpus
mutation lock and then a global write-lane semaphore, in that order. Read ops
skip both and run concurrently.

## Entry points

- `src/main.rs` — process entry; calls `hallouminate::app::run()`.
- `src/lib.rs` — library facade for tests and downstream callers.
- `src/app.rs` — top-level `run()`: parses the CLI and dispatches.

## A living example

This repo's own architecture notes are also maintained as a hallouminate wiki
at `.hallouminate/wiki/` — see [Dogfooding](./dogfooding.md). The wiki entries
carry `file:line` and commit citations that this page summarizes.
