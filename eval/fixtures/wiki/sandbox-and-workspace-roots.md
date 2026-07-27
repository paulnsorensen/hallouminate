---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - crates/hallouminate/src/mcp/tools.rs
  - crates/hallouminate-domain/src/corpus/sandbox.rs
  - https://github.com/paulnsorensen/hallouminate/issues/207
---
# Sandbox boundaries and workspace root discovery

Every corpus operation is bounded by two related but distinct checks: which directory counts as "the workspace" for a client that didn't name a corpus explicitly, and which files a corpus operation is allowed to touch once a corpus is resolved. Both err toward refusing a request outright over guessing or silently narrowing its effect.

## Discovering the workspace root from the client

When an MCP tool call omits `corpus`, the server needs a directory to resolve the default corpus against. `cwd_from_peer` gets that directory from the connected client rather than from the server process's own startup directory whenever it can: if the client advertised the `roots` capability, the server sends `roots/list` (bounded by a two-second timeout) and uses the first returned root's path.[^1] `root_uri_to_path` prefers proper `file://` URL decoding — handling percent-escapes and host segments correctly — and only falls back to treating a bare absolute string as a path when URL parsing fails.[^2] Only when the client has no roots capability, the request times out, or the first root yields no usable path does resolution fall back to the server's own process-startup cwd.[^3]

This matters because a client like an IDE or a coding agent often runs one long-lived MCP server process across many different projects; if the server always used its own startup cwd, every call from a different workspace would silently resolve against the wrong repository's config. Asking the client which workspace it means, and preferring that answer, keeps a single running server correct across simultaneous or sequential workspace switches. The `roots` protocol capability is itself marked deprecated upstream, but it remains the mechanism clients such as Claude Code use today to advertise a workspace directory, so the server keeps relying on it until a replacement lands.[^4]

## A path outside every configured root is refused, not indexed

Once a corpus is resolved, read and write operations that take a relative path resolve it against the corpus's configured root(s) — never against an arbitrary filesystem location. `resolve_read_root` walks every configured root of a multi-root corpus looking for the one that actually contains the requested relative path, using a symlink-safe existence probe; if none of the configured roots contain it, the call returns a not-found error rather than falling through to some other directory on disk.[^5] There is no code path that resolves a relative name against anything other than the corpus's own declared roots — a request for a path outside all of them fails closed with an explicit error, the same shape callers already get for a file that simply doesn't exist, rather than silently indexing or reading whatever happens to sit at that path elsewhere in the filesystem.

A symlink escape gets an even harder stop than a plain miss: `resolve_read_root` treats a symlinked path component or leaf as an immediate, non-retryable error rather than quietly moving on to try the next configured root, because a symlink pointing outside the sandboxed root is a security boundary violation, not an ordinary "wrong root" miss.[^6]

## Why `..` in a relative path is rejected outright

`safe_relative_path` is the single choke point every `add_markdown` / `read_markdown` / `delete_markdown` relative path passes through before it touches the filesystem. It rejects an empty path, an absolute path, any component that isn't a plain filename (`Component::Normal`), a `NUL` byte, a trailing slash, and a `.`/`./` segment — and by rejecting anything that isn't exclusively `Component::Normal` components, it rejects `..` specifically as well.[^7] The comment on the function names the reason directly: this closes "the path-traversal boundary" for the write and read handlers, because `..`, absolute paths, and `.`-segments "would all reach outside the corpus root."[^8] A relative path that resolves outside its declared root defeats the entire purpose of scoping a corpus to a root in the first place — every downstream check (glob include/exclude, symlink rejection, root-scoped delete) assumes the path it's validating is actually inside the root it claims to be under, and `..` is the simplest way to break that assumption.

See [gitignore-and-explicit-roots](gitignore-and-explicit-roots.md), [worktree-corpus-identity](worktree-corpus-identity.md), [architecture](architecture.md).

[^1]: `crates/hallouminate/src/mcp/tools.rs:312-341`
[^2]: `crates/hallouminate/src/mcp/tools.rs:346-354`
[^3]: `crates/hallouminate/src/mcp/tools.rs:324-341`
[^4]: `crates/hallouminate/src/mcp/tools.rs:320-323`; https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2577
[^5]: `crates/hallouminate-domain/src/corpus/sandbox.rs:503-549`
[^6]: `crates/hallouminate-domain/src/corpus/sandbox.rs:541-546`
[^7]: `crates/hallouminate-domain/src/corpus/sandbox.rs:119-156`
[^8]: `crates/hallouminate-domain/src/corpus/sandbox.rs:120-122`

_Source: crates/hallouminate/src/mcp/tools.rs, crates/hallouminate-domain/src/corpus/sandbox.rs · Updated: 2026-07-18_
