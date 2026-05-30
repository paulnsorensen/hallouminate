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
