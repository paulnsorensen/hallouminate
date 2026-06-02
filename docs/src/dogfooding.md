# Dogfooding: our own wiki

Hallouminate maintains its own wiki **with hallouminate**. The repo declares
itself as a `[[repository]]`, so its knowledge base is searchable as the
`repo:hallouminate:wiki` corpus from any checkout. The wiki lives at
[`.hallouminate/wiki/`](https://github.com/paulnsorensen/hallouminate/tree/main/.hallouminate/wiki)
and is the canonical, durable record of how the project actually works — the
source these docs are distilled from.

Two corpora, two lifecycles:

| Where | Indexed as | Lifecycle | Holds |
|---|---|---|---|
| `.hallouminate/wiki/` | `repo:hallouminate:wiki` | durable across sessions | architecture, conventions, protocols, "why this design" notes |
| `.cheese/` | `cheese-local` | transient per-task | per-task agent reports |

## What's in the wiki

The entries are written for an LLM working in the repo — they carry
`file:line` and commit citations these human-facing docs summarize:

- **architecture** — the sliced-bread layout and dependency direction.
- **mcp-surface** — the nine MCP tools, params, and error mapping.
- **daemon-and-cli** — why there's a daemon, the JSON-line socket protocol,
  the CLI surface, and the lock order.
- **corpus-walker** — gitignore-aware corpus walking and the explicit-root
  opt-in.
- **config-layering** — the XDG baseline plus repo-layer merge.
- **wiki-conventions** — how to author entries without contradicting the
  indexer.

## Read it the way an LLM would

If you have hallouminate installed and the MCP server registered, an agent
working in this repo queries the wiki directly:

```text
list_tree   { corpus: "repo:hallouminate:wiki" }
ground      { corpus: "repo:hallouminate:wiki", query: "why is there a daemon" }
read_markdown { corpus: "repo:hallouminate:wiki", path: "daemon-and-cli.md" }
```

From the CLI:

```sh
hallouminate ground "socket resolution order" --corpus repo:hallouminate:wiki
```

## Keeping it current

The repo's [`AGENTS.md`](https://github.com/paulnsorensen/hallouminate/blob/main/AGENTS.md)
instructs every coding agent to refresh the wiki **after a change lands on
`main`** — but only when the change altered durable knowledge (architecture,
conventions, protocols, the MCP tool surface, a "why this design" decision).
Routine bug fixes and transient per-task output stay out; that's what
`.cheese/` is for.

Updates go through the MCP (`read_markdown` → `add_markdown` with
`overwrite: true`), not raw file edits, so the LanceDB index and the ancestor
`index.md` link lists stay in sync. When the wiki *is* edited on disk directly,
re-sync with:

```sh
hallouminate index --corpus repo:hallouminate:wiki
```

That loop — author through the tool, search through the tool, keep the index
honest — is the product proving itself on its own source.
