# hallouminate wiki — index

This wiki is what an LLM working in the `hallouminate` repo writes to and
reads from when it wants to remember things across sessions. It lives at
`.hallouminate/wiki/` and is indexed as the `repo:hallouminate:wiki`
corpus, separate from the source-code corpus (`repo:hallouminate:corpus`)
and the per-session reports under `.cheese/` (corpus `cheese-local`).

## Topics

- [architecture](architecture.md) — five-crate sliced-bread workspace (app, daemon, config, domain, adapters), dependency direction, entry points.
- [blocking-inference-offload](blocking-inference-offload.md) — which CPU-bound daemon paths hop off tokio workers and which still run inline; coverage gaps (#217, #219).
- [claim-provenance-marks](claim-provenance-marks.md) — inline `<!--claim:STATUS-->` marks parsed at index time, stored per chunk in Lance and surfaced in `ground`; how they differ from page-level frontmatter.
- [code-aware-chunking](code-aware-chunking.md) — deferred/future-phase plan for tree-sitter source-code indexing (not shipped today).
- [config-layering](config-layering.md) — XDG baseline plus repo-layer merge; how a single daemon serves many repos.
- [corpus-walker](corpus-walker.md) — gitignore-aware corpus walking and the explicit-root opt-in escape hatch.
- [daemon-and-cli](daemon-and-cli.md) — why there's a daemon, the JSON-line socket protocol, the CLI subcommand surface.
- [debt-observed-test-isolation](debt-observed-test-isolation.md) — why the process-wide `debt::OBSERVED` static makes any Hard-recording test break concurrent maintenance-defer tests.
- [design-rationale](design-rationale.md) — what hallouminate deliberately *is* and *isn't*, and why — the "why this design not that one" page.
- [mcp-surface](mcp-surface.md) — the ten MCP tools the LLM uses to author and search wikis.
- [multi-format-ingestion](multi-format-ingestion.md) — why hallouminate is markdown-only today, the per-format dispatch pattern (text/code/PDF), reachable tooling, and the open design questions before extending the indexer.
- [office-prose-extraction](office-prose-extraction.md) — deferred/future-phase plan for .docx/.pptx/.odt prose extraction (not shipped today).
- [ort-arena-retention](ort-arena-retention.md) — why session eviction never reclaimed embedder memory: upstream ONNX Runtime BFCArena retention; superseded by daemon idle-exit.
- [pdf-ocr-ingestion](pdf-ocr-ingestion.md) — deferred/future-phase plan for PDF and OCR ingestion (not shipped today).
- [racy-mtime-smudge](racy-mtime-smudge.md) — why stored file mtimes are deliberately smudged by one millisecond at the write seam rather than fixing the equality gates.
- [release-ceremony](release-ceremony.md) — release-flow gotchas the scripts don't tell you, learned cutting real releases.
- [wiki-conventions](wiki-conventions.md) — how to author entries in *this* wiki without contradicting the indexer's expectations.
- [worktree-corpus-identity](worktree-corpus-identity.md) — indexing the same corpus from two git worktrees deletes each other's index rows (#215); mechanism, symptoms, fix direction.
- [worktree-dev-gotchas](worktree-dev-gotchas.md) — environment traps for agents in isolated worktrees: tilth edits leaking to the parent repo, and `/tmp` scratch builds (disk quota, cargo wrapper exit 134).

## How to use this index

`index.md` is a table of contents, not a topic. Add new pages to the
list above (alphabetical inside the list), keeping a one-line gloss per
entry. Anything substantive belongs in a topic file.

If you read this index and don't see the topic you need, run
`list_files` against the `repo:hallouminate:wiki` corpus first — the
index may be out of date relative to the directory.
