---
status: reviewed
last_verified: 2026-07-20
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/149
  - crates/hallouminate-domain/src/corpus/sandbox.rs
---
# Path safety

Every filesystem access the daemon performs on behalf of a caller — reading a corpus file, writing through `add_markdown`, deleting through `delete_markdown` — goes through a sandbox layer that rejects symlinks outright, replaces file contents atomically on write, and resolves relative paths against a corpus' declared root rather than the process' cwd.[^1]

## Why symlinks are rejected, not followed

A corpus root is meant to be a bounded set of files: whatever the configured `paths` and `globs` say it contains. A symlink inside that root can point anywhere on the filesystem the daemon process has access to, so following it on read would let a corpus silently vacuum in content its config never named, and following it on write would let a write meant for one file land on whatever the link currently resolves to — including a target outside the corpus root entirely, changed after the link was created. The sandbox does not try to distinguish a "safe" symlink (one that resolves back inside the same root) from an unsafe one; every symlink encountered during a read or write is rejected with a caller-facing error. That's a simpler invariant to hold than a resolve-and-recheck loop, and it closes the door on a class of race — a symlink retargeted between the check and the use — that a resolve-then-verify approach would still be exposed to.[^2]

## Atomic replacement on write

`add_markdown` writes to a temporary file in the same directory as the target and renames it into place, rather than truncating and rewriting the target file in place. A rename within the same filesystem is atomic: any reader of the target path sees either the old complete content or the new complete content, never a partially-written file. This matters because the daemon serves concurrent read requests (`ground`, `read_markdown`) while a write may be in flight — see [daemon-and-cli](daemon-and-cli.md) for how read and write ops are allowed to run concurrently against different corpora. Without atomic replacement, a `ground` search racing an `add_markdown` write could read a half-written file and either chunk garbage or crash the parser.

## Why relative paths resolve against a declared root

A path argument like `path: "notes/design.md"` passed to `add_markdown` or `read_markdown` is resolved against the corpus' configured root — never against the daemon process' own working directory. The daemon is a long-lived process serving many repositories and many corpora over its lifetime; it has no single meaningful "current directory" for a caller's relative path to mean anything relative to. Anchoring resolution to the corpus root instead makes the same relative path mean the same file regardless of which repository the caller happened to be sitting in when the request was sent, and it makes the escape check tractable: after resolving against a known root, the sandbox can reject any resolved path that normalizes to something outside that root, including `../` traversal and absolute-path overrides smuggled into the `path` argument.[^3]

## What class of escape this closes

Taken together, these three checks close the class of escape where a caller — a malformed request, a compromised MCP client, a copy-pasted path from the wrong repo — causes the daemon to read or write a file outside the corpus it was told to operate on. That covers:

- `../../../etc/passwd`-style traversal in a `path` argument;
- an absolute path substituted for what was expected to be corpus-relative;
- a symlink planted inside a corpus root that resolves outside it, whether pre-existing or created between requests;
- a write racing a concurrent read of the same file.

It does not cover a corpus root itself being misconfigured to point somewhere sensitive — that's a config-time decision the operator makes, not something the sandbox can second-guess at request time. See [config-layering](config-layering.md) for how corpus roots are configured.

See [corpus-walker](corpus-walker.md), [daemon-and-cli](daemon-and-cli.md), [mcp-surface](mcp-surface.md).

[^1]: `crates/hallouminate-domain/src/corpus/sandbox.rs:1-90`; https://github.com/paulnsorensen/hallouminate/issues/149
[^2]: `crates/hallouminate-domain/src/corpus/sandbox.rs:40-66`
[^3]: `crates/hallouminate-domain/src/corpus/sandbox.rs:14-39`

_Source: issue #149 · Updated: 2026-07-20_