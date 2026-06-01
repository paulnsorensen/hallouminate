# AGENTS.md

Project-specific guidance for coding agents working in `hallouminate`.

## Keep the wiki current after each land

This repo dogfoods its own wiki at `.hallouminate/wiki/` (corpus
`repo:hallouminate:wiki`). After a change lands on `main`, refresh the
wiki **if the change altered durable knowledge** — architecture,
conventions, protocols, the MCP tool surface, or a "why this design not
that one" decision. Routine bug fixes and transient per-task output do
not belong in the wiki; that's what `.cheese/` is for.

Use the hallouminate MCP tools, not raw file edits, so the LanceDB
index and the ancestor `index.md` link lists stay in sync:

1. `list_tree` / `ground "<topic>"` — find the page that should change.
2. `read_markdown` — read current content before clobbering.
3. `add_markdown { overwrite: true }` — write the update (the daemon
   rewrites ancestor `index.md` link lists for you).

Follow `.hallouminate/wiki/wiki-conventions.md` for slug, H1, and
one-topic-per-file rules.

## Local Rust skills

Three skills live under `.agents/skills/`. Apply them when working in Rust:

- **`rust-style`** — coding style. Apply automatically whenever you write
  or modify any Rust code: `for` loops over iterator chains, `let ... else`
  for early returns, variable shadowing over renaming, newtypes over bare
  strings, enums over `bool` params, explicit/exhaustive matching (no `_`
  wildcards, no `matches!`), explicit destructuring, and minimal comments.
- **`rustdoc`** — doc-comment conventions (RFC 1574). Apply when writing
  `///` doc comments on public items: summary sentences, the standard
  section headings (`# Examples`, `# Panics`, `# Errors`, `# Safety`),
  type references, and examples.
- **`rust-analyzer-ssr`** — structural search and replace. Use when making
  the same AST-level change across many call sites (API migrations, UFCS
  ↔ method calls, struct-literal ↔ constructor), where a semantic,
  type-aware transform beats text find-and-replace.

## Wiki skills (for hallouminate users)

Three skills under `plugins/hallouminate/skills/` (the distributable plugin
pack) drive the wiki lifecycle through the hallouminate MCP tools. Each runs an **opus reasoning root** with **haiku
fan-out** sub-agents for parallel retrieval/drafting.

- **`wiki-init`** — bootstrap an empty wiki by interviewing the user with
  Socratic, behavior-first questioning (ACTA: task diagram → knowledge audit
  → simulation), then fans out haiku drafters to write one-topic-per-file
  entries via `add_markdown`.
- **`wiki-query`** — answer a question strictly from the wiki, every claim
  carrying a `path:line` citation. Haiku sub-agents fan out one `ground`
  search per sub-question; the opus root synthesizes and verifies citations.
- **`wiki-ingest`** — fold new sources/facts into an existing wiki: route to
  the page each claim extends, merge, create-new only when novel, and never
  blend contradictions. Haiku locates via `ground`; the opus root decides
  and writes.
