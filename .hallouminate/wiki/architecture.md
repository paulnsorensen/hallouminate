# Architecture

Hallouminate is a three-crate ports-and-adapters workspace with Sliced Bread
applied inside the core: business-capability modules expose crust facades, while
the workspace crates enforce dependency direction. It is not a crate-per-feature
layout.[^1]

## `crates/hallouminate/` — application and driving adapters

The application crate owns process orchestration and inbound transports:

- `cli.rs` and `cli/` — clap commands and user-facing command handlers
- `daemon/` — the long-lived local RPC daemon, request dispatch, lifecycle,
  filesystem watching, and application resource composition
- `mcp.rs` and `mcp/` — the rmcp stdio server; MCP drives application use
  cases, so it belongs beside the CLI rather than among driven adapters
- `config.rs` — XDG baseline parsing plus repository-layer merge
- `logging.rs` and `xdg.rs` — process-wide runtime wiring
- `input_error.rs` — caller-input error marker

`src/lib.rs` is the crate root and exports `run()`; `src/main.rs` is the
thin binary entry point.[^2]

## `crates/hallouminate-domain/` — application core

The core is organized by capability:

- `corpus` — markdown chunking, validation, filesystem walking, sandboxed
  corpus file operations, summaries, keywords, frontmatter, and claim marks
- `embeddings` — supported-model identity policy
- `ground` — search orchestration, bucketing, response types, and rendering
- `indexer` — scan/plan/apply orchestration, format handlers, DTOs, and the
  domain-owned `ChunkStore` port
- `search` — hybrid-search policy, crossencoder port, and exact-match fusion
- `repository`, `discovery`, and `footnotes` — repository-derived corpora,
  bounded wiki discovery, and footnote resolution
- `common` — shared value and error types

Slice root modules are the intended crusts. Consumers should import from
`hallouminate_domain::<slice>`, not a slice's child module. Most child modules
are private and their intentional API is re-exported by the crust.[^3]

The crate is the application core rather than a strictly side-effect-free
domain model: corpus walking/sandboxing and the current ripgrep implementation
perform filesystem or process I/O here. This is a current boundary exception;
strict Sliced Bread conformance would move those mechanisms behind domain-owned
ports with driven implementations in the adapters crate.[^4]

## `crates/hallouminate-adapters/` — driven external systems

- `lance.rs` — LanceDB persistence and the `ChunkStore` implementation
- `embedder.rs` — fastembed passage/query embedding
- `crossencoder.rs` — fastembed reranking

Adapters depend on domain ports and types, never on the application crate.[^5]

## Dependency direction

```text
hallouminate-adapters -> hallouminate-domain <- hallouminate
                                              |
                                              -> hallouminate-adapters
```

Cargo workspace metadata enforces this direction: the application depends on
both lower crates, adapters depend on domain, and domain has no workspace-crate
dependency.[^6]

## Closed boundary seams

The tokenizer seam is closed: `RequestResources` stores
`hallouminate_domain::corpus::Tokenizer`, re-exported at the domain crust
rather than depending on `tokenizers` directly. The application crate no
longer declares a `tokenizers` dependency.[^7]

The maintenance seam is also closed: `LanceStore::maintain` now accepts
adapter-owned `MaintenanceOptions` with `std::time::Duration` and returns
adapter-owned `MaintenanceStats`, so the application crate no longer depends
on LanceDB. New adapter APIs should follow this pattern.[^8]

## Testing

Unit tests live beside their modules. Integration tests are under
`crates/hallouminate/tests/it/`, with `main.rs` as the shared harness and one
module per concern.[^9]

[^1]: `Cargo.toml:1-34`; `crates/hallouminate-domain/src/lib.rs:7-15`.
[^2]: `crates/hallouminate/src/lib.rs:1-20`; `crates/hallouminate/src/main.rs:1-4`; commit `a0a530a` (PR #238).
[^3]: `crates/hallouminate-domain/src/corpus.rs:1-60`; `crates/hallouminate-domain/src/indexer.rs:1-14`; commit `64669f9` (PR #239).
[^4]: `crates/hallouminate-domain/src/corpus/sandbox.rs:1-46`; `crates/hallouminate-domain/src/search/ripgrep.rs:1-64`.
[^5]: `crates/hallouminate-adapters/src/lib.rs:1-13`; `crates/hallouminate-adapters/src/lance.rs:17-23`.
[^6]: `crates/hallouminate/Cargo.toml:15-17`; `crates/hallouminate-adapters/Cargo.toml:10-12`; `crates/hallouminate-domain/Cargo.toml:10-51`.
[^7]: `crates/hallouminate-domain/src/corpus/chunker.rs:10-13`; `crates/hallouminate/src/daemon/state.rs:151-156`.
[^8]: `crates/hallouminate-adapters/src/lance.rs:39-51,754-782`; `crates/hallouminate/src/daemon/state.rs:500-529`; `crates/hallouminate/Cargo.toml:15-58`.
[^9]: `crates/hallouminate/tests/it/main.rs:1-16`.
