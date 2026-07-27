---
status: reviewed
last_verified: 2026-07-22
confidence: high
sources:
  - crates/hallouminate-domain/src/common.rs
  - crates/hallouminate-domain/src/ground/orchestrate.rs
  - https://github.com/paulnsorensen/hallouminate/issues/236
---
# Error taxonomy

`HallouminateError` (`crates/hallouminate-domain/src/common.rs:151-186`) is
the one error type crossing every fallible operation in the crate. It has six
variants: `Io` (converted automatically from `std::io::Error` via `#[from]`),
`Db` (the vector store failed to open, read, or write a batch), `Embed`
(embedding-model loading or encoding failed), `Config` (a malformed config
file or a rejected repository/corpus entry), `Indexer` (chunking or batch
application failed), and `StoreSchemaStale` (an on-disk store was written at
an older schema version than the running build expects).[^1]

## What each variant is for

`Config` and `Indexer` both carry a free-text `String` rather than a
structured payload, because their callers — config parsing, corpus/repository
validation, chunk-batch application — each fail for reasons specific enough
that a shared enum of sub-cases would just be a second, smaller taxonomy
nested inside the first. `StoreSchemaStale` is the one variant with
structured fields (`found`, `expected`, `ground_dir`) because callers act on
it specifically: opening a store written at an older schema triggers a
rebuild from source, while a newer schema than the running build understands
stays fatal — the version numbers themselves are the decision input, not just
prose for a log line.

## MCP-layer mapping

The MCP adapter narrows this six-variant taxonomy down to two JSON-RPC codes:
`InvalidParams` (`-32602`) for caller-supplied input failures — a bad corpus
name, an unsafe path, a missing required argument — and `Internal` (`-32603`)
for everything else, including any failure that happens before the daemon
returns a typed envelope at all (a transport error, a decode failure, the
daemon being unreachable). That collapse is intentional: an MCP client should
not have to distinguish six internal failure modes to know whether retrying
with different arguments could help, and a transport-level failure should
never be mistaken for a caller mistake.

## Recoverable degradation vs. a hard error

Not every failure inside a request becomes an error at all. `ground`'s
optional reranking pass — a secondary scoring step applied to already-found
hits — runs under a configurable timeout; if it doesn't finish in time, the
orchestration logs a warning, keeps the hits in the order already computed,
and appends a structured `Warning { code: rerank-timeout, message }` to the
response rather than failing the call
(`crates/hallouminate-domain/src/ground/orchestrate.rs:153-172`). The caller
still gets results — worse-ranked, not wrong — and a machine-readable signal
that something was skipped.[^2]

The line the codebase draws is whether the degraded path still returns
correct, if imperfect, results. A rerank timeout leaves every hit's actual
content untouched; only the scoring pass that would have reordered them was
skipped, so returning them anyway cannot mislead the caller about what exists
in the corpus. Contrast that with something like a corpus failing to open at
all (`Db`) or a config file failing to parse (`Config`): there is no partial
result to fall back to that isn't simply wrong, so those stay hard errors.
Put differently — a component that can only make results better is allowed
to fail soft; a component whose failure means the results would be incorrect
or absent is not.

See [architecture](architecture.md), [design-rationale](design-rationale.md), and [worktree-corpus-identity](worktree-corpus-identity.md).

[^1]: `crates/hallouminate-domain/src/common.rs:151-186`
[^2]: `crates/hallouminate-domain/src/ground/orchestrate.rs:153-172`; `crates/hallouminate-domain/src/ground/types.rs:126-131`

_Source: `common.rs` + `ground/orchestrate.rs` · Updated: 2026-07-22_
