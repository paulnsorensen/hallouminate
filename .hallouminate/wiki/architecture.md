# Architecture

Hallouminate uses a Sliced Bread layout — vertical slices with public
APIs at slice boundaries, no cross-slice peeks at internals. Three
top-level concerns.

## `src/app/` — orchestration

The application layer. Owns:

- `cli/` and `cli.rs` — clap-derived subcommands (`index`, `ground`, `serve`, `daemon`, `config`, `hook`)
- `daemon/` — the long-lived RPC daemon: `bootstrap.rs`, `client.rs`, `dispatch.rs`, `ipc.rs`, `mod.rs`, `server.rs`, `socket.rs`, `state.rs`
- `config.rs` — XDG baseline parser plus repo-layer merge (see `config-layering.md`)
- `logging.rs` — `tracing-subscriber` bootstrap with a rolling appender under the XDG state dir
- `xdg.rs` — centralized XDG path resolution (config, cache, state)
- `input_error.rs` — caller-input error shape distinct from internal errors

App depends on `domain` and `adapters`. App composes them — it does not
own pure logic.

## `src/domain/` — pure logic

No I/O dependencies beyond filesystem walks and hashing. Slices:

- `corpus/` — `chunker.rs`, `walker.rs`, `hasher.rs`, `sandbox.rs`, `snippet.rs`, `summary.rs`, `keywords.rs`
- `embeddings/` — `embedder.rs` (fastembed wrapper), `index.rs`
- `ground/` — `orchestrate.rs`, `bucket.rs`, `format.rs`, `types.rs`, `index.rs`
- `indexer/` — `plan.rs`, `apply.rs`, `writer.rs`, `index.rs`
- `repository.rs` — `RepositoryConfig` plus the `effective_corpora` derivation that turns each `[[repository]]` into `repo:NAME:wiki` and `repo:NAME:corpus`
- `common.rs` and `common/paths.rs` — shared types: `Mtime`, `FileRef`, `HallouminateError`, `expand_tilde`, `canonicalize_or_passthrough`
- `search.rs` — read-side query types

Domain has no dependency on app or adapters.

## `src/adapters/` — external systems

- `lance.rs` — LanceDB vector storage adapter
- `mcp/server.rs` and `mcp/tools.rs` — the rmcp-based stdio MCP server

Adapters depend on domain (for types) but not on app.

## Dependency direction

`adapters → domain ← app → adapters`. Domain is the stable core; app
composes adapter implementations with domain orchestration; adapters
plug external systems into domain ports.

## Entry points

- `src/main.rs` — process entry; calls `hallouminate::app::run()`.
- `src/lib.rs` — library facade for tests and downstream callers.
- `src/app.rs` — top-level `run()`: parses CLI, dispatches to the right subcommand.

## Testing

- Unit tests live alongside their module (`#[cfg(test)] mod tests`).
- Integration tests live in `tests/` and use a `DaemonHarness` fixture (`tests/common/daemon.rs`) that spins up a daemon on a temp socket per test.
