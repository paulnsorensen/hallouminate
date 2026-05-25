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
