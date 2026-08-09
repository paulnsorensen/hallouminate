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
- [daemon-canonical-identity-001](daemon-canonical-identity-001.md) — why the default daemon socket is resolved from the OS account record instead of `XDG_RUNTIME_DIR` (#320).
- [daemon-canonical-identity-002](daemon-canonical-identity-002.md) — the one ordered canonical→legacy discovery seam every client and lifecycle command shares, and the bounded migration window (#323 retires it).
- [daemon-canonical-identity-003](daemon-canonical-identity-003.md) — store-lock owner diagnostics: what the `store.lock` metadata names, and why a second advisory guard proves it is current.
- [debt-observed-test-isolation](debt-observed-test-isolation.md) — why the process-wide `debt::OBSERVED` static makes any Hard-recording test break concurrent maintenance-defer tests.
- [design-rationale](design-rationale.md) — what hallouminate deliberately *is* and *isn't*, and why — the "why this design not that one" page.
- [domain-model](domain-model.md) — the `search_text`, root-aware `CorpusKey`, and schema-v4 model that shipped in #290 (issue #288's design).
- [eval-baseline-rg-provenance-adrs](eval-baseline-rg-provenance-adrs.md) — why the eval was re-baselined with the divergence unexplained, and why ripgrep's version is recorded but never enforced.
- [eval-harness-gotchas](eval-harness-gotchas.md) — operational traps running the ground-retrieval eval: ripgrep as an uninstalled hard dependency, the warning-code taxonomy three crates own, the discarded warmup sweep, and ETXTBSY in parallel tests.
- [ground-search-evaluation](ground-search-evaluation.md) — discriminative file/chunk relevance metrics, latency thresholds, small-reranker comparison, and scheduled regression gate.
- [ground-search-quality](ground-search-quality.md) — issue #288's spec, shipped in #290: indexed-text cleanup, worktree isolation, eval-gated reranking (default stayed opt-in), and authoring context.
- [ground-search-quality-adrs](ground-search-quality-adrs.md) — five decisions and their research basis for issue #288; ADR-001/002/003/005 shipped in #290, ADR-004's reranker default stayed opt-in.
- [ground-signal-fusion-adrs](ground-signal-fusion-adrs.md) — the post-#290 ranking audit: why weighted RRF over four peer signals moved into the domain layer, why the shipped 0.5/0.5 literal weights were kept, and why the literal signals only reorder the FTS/vector pool.
- [log](log.md) — append-only ingest log: what each wiki-ingest run wrote, merged, skipped as a near-duplicate, or flagged as conflicting, and why.
- [mcp-surface](mcp-surface.md) — the eleven MCP tools the LLM uses to author and search wikis, including `add_markdown`'s surgical edit modes and the multi-root read/write asymmetry.
- [multi-format-ingestion](multi-format-ingestion.md) — why hallouminate is markdown-only today, the per-format dispatch pattern (text/code/PDF), reachable tooling, and the open design questions before extending the indexer.
- [office-prose-extraction](office-prose-extraction.md) — deferred/future-phase plan for .docx/.pptx/.odt prose extraction (not shipped today).
- [ort-arena-retention](ort-arena-retention.md) — why session eviction never reclaimed embedder memory: upstream ONNX Runtime BFCArena retention; superseded by daemon idle-exit.
- [pdf-ocr-ingestion](pdf-ocr-ingestion.md) — deferred/future-phase plan for PDF and OCR ingestion (not shipped today).
- [racy-mtime-smudge](racy-mtime-smudge.md) — why stored file mtimes are deliberately smudged by one millisecond at the write seam rather than fixing the equality gates.
- [release-ceremony](release-ceremony.md) — release-flow gotchas the scripts don't tell you, learned cutting real releases.
- [search-reliability-loop-001](search-reliability-loop-001.md) — ADR-001: verify authored knowledge with retrieval probes frozen *before* drafting, and give durable external research its own corpus-local source page.
- [search-reliability-loop-002](search-reliability-loop-002.md) — ADR-002: separate the production regression gate (`just eval`) from reranker measurement (`just eval-measure`); supersedes ground-search-quality ADR-004's selection thresholds.
- [search-reliability-loop-003](search-reliability-loop-003.md) — ADR-003: rerank the indexed `search_text` while `SearchHit.text` stays the display/evidence contract.
- [sources/](sources/index.md) — corpus-local source pages for durable external research, each with a fixed indexed identity spine (title, URL, attribution, limitations).
- [wiki-conventions](wiki-conventions.md) — how to author entries in *this* wiki without contradicting the indexer's expectations.
- [worktree-corpus-identity](worktree-corpus-identity.md) — #215's root-scoped delete fix, the #290 root-aware identity that closed the sibling-worktree search leak, and #304's retired-root garbage collection.
- [worktree-dev-gotchas](worktree-dev-gotchas.md) — environment traps for agents in isolated worktrees: tilth edits leaking to the parent repo, and `/tmp` scratch builds (disk quota, cargo wrapper exit 134).

## How to use this index

`index.md` is a table of contents, not a topic. Add new pages to the
list above (alphabetical inside the list), keeping a one-line gloss per
entry. Anything substantive belongs in a topic file.

If you read this index and don't see the topic you need, run
`list_files` against the `repo:hallouminate:wiki` corpus first — the
index may be out of date relative to the directory.
