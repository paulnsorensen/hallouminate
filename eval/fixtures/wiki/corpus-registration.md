---
status: reviewed
last_verified: 2026-07-21
confidence: high
sources:
  - crates/hallouminate-domain/src/repository.rs
  - https://github.com/paulnsorensen/hallouminate/issues/219
---
# Corpus registration: [[repository]] vs [[corpus]]

Config declares corpora two ways, and they mean different things. A
`[[repository]]` entry (`RepositoryConfig`,
`crates/hallouminate-domain/src/repository.rs:26-38`) names a git repository
hallouminate manages tenancy for, and derives one or two corpora from it — it
is never itself a corpus name. A `[[corpus]]` entry is a user-declared corpus
with its own explicit name, paths, and globs, with no repository concept
attached.

## What a [[repository]] derives

Every repository derives a `repo:{name}:wiki` corpus rooted at
`<path>/.hallouminate/wiki` (or the `wiki` override) — it always exists
logically, even before any page has been written. A `repo:{name}:corpus`
source-document corpus exists only when the repository declares
`corpus_paths`. `repo_corpus_name`
(`crates/hallouminate-domain/src/repository.rs:69-83`) rejects an empty name
or one containing `:`, since a colon would collide with the `repo:` prefix
and make the derived name unparseable. `RepoCorpusKind`
(`crates/hallouminate-domain/src/repository.rs:44-49`) currently has exactly
two real variants, `Wiki` and `Corpus` — a third, code-oriented variant is
named only in a comment and has no match arm, so it is not a live corpus
today.[^1]

The split matters because a repository's wiki and its source documents have
different write semantics: the wiki is LLM-authored via `add_markdown` and
gets its ancestor `index.md` files refreshed on every write, while the
derived source corpus is read-only from hallouminate's side — it indexes
whatever the repository already has on disk.

## Resolving the default corpus from cwd

A tool call that omits `corpus` resolves against the repository whose `path`
is the deepest ancestor of the client's workspace root —
`default_wiki_for_cwd` (`crates/hallouminate-domain/src/repository.rs:177`)
prefers the most specific matching repository so a nested checkout inside a
larger monorepo-style root still resolves to its own wiki, not the parent's.
This mirrors the same most-specific-root-wins resolution
`worktree-corpus-identity.md` describes for canonical root matching.[^2]

## Why an explicit corpus is required beyond one

When cwd sits under no configured repository and more than one corpus is
reachable — several `[[corpus]]` entries, or a mix of derived and
user-declared corpora — there is no principled way to guess which one a bare
tool call means, so the daemon rejects the call with `InvalidParams` rather
than picking one silently.[^3] Silent selection would be a worse failure mode
than an explicit error: a wrong guess reads and writes the wrong wiki
quietly, while a rejection tells the caller immediately that it must name the
corpus. `list_corpora` exists specifically so a caller in this situation can
discover the available names before retrying with an explicit `corpus`
parameter.

See [worktree-corpus-identity](worktree-corpus-identity.md), [config-layering](config-layering.md), and [mcp-surface](mcp-surface.md).

[^1]: `crates/hallouminate-domain/src/repository.rs:26-83`
[^2]: `crates/hallouminate-domain/src/repository.rs:167-199`
[^3]: `crates/hallouminate-daemon/src/dispatch.rs:368`

_Source: `repository.rs` + `dispatch.rs` · Updated: 2026-07-21_
