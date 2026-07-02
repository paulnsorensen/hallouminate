# hallouminate wiki — index

This wiki is what an LLM working in the `hallouminate` repo writes to and
reads from when it wants to remember things across sessions. It lives at
`.hallouminate/wiki/` and is indexed as the `repo:hallouminate:wiki`
corpus, separate from the source-code corpus (`repo:hallouminate:corpus`)
and the per-session reports under `.cheese/` (corpus `cheese-local`).

## Topics

- [architecture](architecture.md) — sliced-bread layout: `app/`, `domain/`, `adapters/`, dependency direction, entry points.
- [mcp-surface](mcp-surface.md) — the ten MCP tools the LLM uses to author and search wikis.
- [daemon-and-cli](daemon-and-cli.md) — why there's a daemon, the JSON-line socket protocol, the CLI subcommand surface.
- [corpus-walker](corpus-walker.md) — gitignore-aware corpus walking and the explicit-root opt-in escape hatch.
- [multi-format-ingestion](multi-format-ingestion.md) — why hallouminate is markdown-only today, the per-format dispatch pattern (text/code/PDF), reachable tooling, and the open design questions before extending the indexer.
- [claim-provenance-marks](claim-provenance-marks.md) — inline `<!--claim:STATUS-->` marks parsed at index time, stored per chunk in Lance and surfaced in `ground`; how they differ from page-level frontmatter.
- [config-layering](config-layering.md) — XDG baseline plus repo-layer merge; how a single daemon serves many repos.
- [wiki-conventions](wiki-conventions.md) — how to author entries in *this* wiki without contradicting the indexer's expectations.
- [worktree-dev-gotchas](worktree-dev-gotchas.md) — environment traps for agents in isolated worktrees: tilth edits leaking to the parent repo, and `/tmp` scratch builds (disk quota, cargo wrapper exit 134).

## How to use this index

`index.md` is a table of contents, not a topic. Add new pages to the
list above (alphabetical inside the list), keeping a one-line gloss per
entry. Anything substantive belongs in a topic file.

If you read this index and don't see the topic you need, run
`list_files` against the `repo:hallouminate:wiki` corpus first — the
index may be out of date relative to the directory.
