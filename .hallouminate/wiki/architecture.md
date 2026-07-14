# Architecture

Hallouminate uses a Sliced Bread layout — vertical slices with public
APIs at slice boundaries, no cross-slice peeks at internals. Three
top-level concerns.

## `src/app/` — orchestration and driving adapters

The application layer. Owns:

- `cli/` and `cli.rs` — clap-derived subcommands (`index`, `ground`, `serve`, `daemon`, `config`, `hook`)
- `daemon/` — the long-lived RPC daemon: `bootstrap.rs`, `client.rs`, `dispatch.rs`, `ipc.rs`, `mod.rs`, `server.rs`, `socket.rs`, `state.rs`
- `mcp/` — the rmcp-based stdio MCP server; it is a driving adapter beside the CLI, not a driven external-system adapter[^1]
- `config.rs` — XDG baseline parser plus repo-layer merge (see `config-layering.md`)
- `logging.rs` — `tracing-subscriber` bootstrap with a rolling appender under the XDG state dir
- `xdg.rs` — centralized XDG path resolution (config, cache, state)
- `input_error.rs` — caller-input error shape distinct from internal errors

App depends on `domain` and `adapters`. App composes them — it does not
own pure logic. MCP belongs here because it receives external requests and
drives application use cases; putting it under `adapters/` blurred driving
and driven boundaries.[^1]

## `src/domain/` — pure logic

No dependency on app, LanceDB, Arrow, fastembed, or other concrete
infrastructure. Slices:

- `corpus/` — `chunker.rs`, `walker.rs`, `hasher.rs`, `sandbox.rs`, `snippet.rs`, `summary.rs`, `keywords.rs`
- `embeddings/` — model-identity policy; concrete inference lives behind adapter-owned implementations
- `ground/` — `orchestrate.rs`, `bucket.rs`, `format.rs`, `types.rs`, `index.rs`
- `indexer/` — `plan.rs`, `apply.rs`, `writer.rs`, `index.rs`, plus the `ChunkStore` port and domain-owned chunk/search DTOs[^2]
- `repository.rs` — `RepositoryConfig` plus the `effective_corpora` derivation that turns each `[[repository]]` into `repo:NAME:wiki` and `repo:NAME:corpus`
- `common.rs` and `common/paths.rs` — shared types: `Mtime`, `FileRef`, `HallouminateError`, `expand_tilde`, `canonicalize_or_passthrough`
- `search.rs` — read-side query types and the crossencoder policy port

Domain slice facades re-export the types and operations app consumers need;
application code imports through those facades rather than reaching into child
modules.[^3]

## `src/adapters/` — driven external systems

- `lance.rs` — LanceDB persistence plus the `ChunkStore` implementation
- `embedder.rs` — fastembed passage/query embedding
- `crossencoder.rs` — fastembed reranking

This directory contains driven implementations called by the application.
Adapters depend on domain ports and types, but not on app.[^2]

## Dependency direction

`adapters → domain ← app → adapters`. Domain is the stable core; app
composes adapter implementations with domain orchestration; driven adapters
implement domain ports. Driving adapters such as CLI and MCP live in app.

## Entry points

- `src/main.rs` — process entry; calls `hallouminate::app::run()`.
- `src/lib.rs` — library facade for tests and downstream callers.
- `src/app.rs` — top-level `run()`: parses CLI, dispatches to the right subcommand.

## Testing

- Unit tests live alongside their module (`#[cfg(test)] mod tests`).
- Integration tests share one harness at `tests/it/main.rs`, with one module per test concern and common fixtures under `tests/it/common/`.[^4]

[^1]: `src/app.rs:1-10`; `src/app/mcp.rs:1-11`; commit `c67ee057` (landed via PR #237).
[^2]: `src/domain/indexer/store.rs:1-27`; `src/adapters.rs:1-6`; `src/adapters/lance.rs:1213-1238`.
[^3]: `src/domain/corpus.rs:40-50`; `src/domain/indexer.rs:10-14`; `src/app/daemon/state.rs:37-40`; commit `348846d`.
[^4]: `tests/it/main.rs`; commit `6194e572`.
