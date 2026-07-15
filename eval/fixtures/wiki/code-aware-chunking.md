# Code-aware ingestion (tree-sitter)

> **Status: DEFERRED / future-phase.** Source-code indexing is **not** in
> Phase 1 (plain text + spreadsheets) — it was explicitly cut from the
> current scope. This page records why AST-aware code chunking is the right
> target and what it costs, so the later phase that picks it up inherits the
> reasoning. Full evidence lives in
> `.cheese/research/multi-format-ingest/multi-format-ingest.md` (crate
> surface) and
> `.cheese/research/multi-format-rag-ingestion/multi-format-rag-ingestion.md`
> (chunking research + cAST).

The good news: code chunking is an **extension of an existing dependency**,
not a new one. The cost is a **build-time C compiler**, which is the real
reason it is deferred.

## Accuracy note: there is NO `RepoCorpusKind::Code` variant

Read this first, because the name is misleading. `RepoCorpusKind`
(`repository.rs:45`) has exactly **two** real variants: `Wiki`
(`repository.rs:46`) and `Corpus` (`repository.rs:47`). Line 48 is a
**comment**, not code:

```rust
// Future: Code maps to repo:{name}:code if code-aware indexing is added.
```

The `suffix()` match (`repository.rs:52-58`) has arms only for `Wiki` and
`Corpus` — adding a `Code` variant would fail to compile until that match
gained an arm. Treat `repo:{name}:code` as scaffolded *intent*, not a live
path. Whether code rides a new `repo:{name}:code` corpus or extends the
existing chunking path is an open wiring decision, not a settled one.

## The chunker already exists — it's behind a feature flag

`text-splitter` `0.32.0` is already locked (`Cargo.lock`) and already used
for markdown chunking (`chunker.rs`). It ships a tree-sitter-backed
**`CodeSplitter`** on the same `ChunkConfig` / `ChunkSizer` surface
(`chunker.rs:12`) the `MarkdownChunker` already uses. Turning it on needs:

1. the `code` feature flag on `text-splitter`, plus
2. one `tree-sitter-<lang>` grammar crate **per language** (Rust, Python, …).

Each grammar crate (~1–2 MB compiled) ships a **C parser compiled via `cc`**.
That is the catch: **code support requires a C compiler at build time.**

### Grammar version pinning — lockstep, like the existing tokenizers pair

Each `tree-sitter-<lang>` grammar must match the tree-sitter runtime
version that `text-splitter` bundles, or the crate graph splits into two
incompatible tree-sitter versions. This is the same coupled-bump hazard the
repo already manages for `text-splitter` + `tokenizers`, grouped as
`text-processing` in dependabot so they bump in one PR
(`.github/dependabot.yml:22-25`, with the WHY at lines 20-21). Code grammars
would join that lockstep discipline.

## Why AST chunking, not a separator list

Two tiers exist in production:

- **Separator lists** (LangChain `from_language`) — zero extra deps, splits
  on syntactic boundary tokens. Breaks silently when a function exceeds the
  chunk size, producing orphaned bodies. No structural metadata.
- **True AST splitting** (tree-sitter / `CodeSplitter`) — splits at AST node
  boundaries, **never mid-syntax**, and recurses into inner nodes for
  over-budget functions.

The **cAST** paper (arxiv 2506.15655, "cAST: Enhancing Code RAG with
Structural Chunking via AST") shows AST-structural chunking consistently
beats both fixed-size and separator-list approaches on code-retrieval
benchmarks. `text-splitter`'s `CodeSplitter` is the AST tier — the quality
win is why code chunking is worth the C-compiler cost, not a separator-list
shortcut.

## The code breadcrumb — the cAST open gap

The markdown `heading_path` (`chunker.rs:124`) generalizes directly to a
**`{file}::{class}::{fn}` code breadcrumb**. But no framework produces it
out of the box: LlamaIndex's `CodeSplitter` and tree-sitter alike give you
AST-bounded chunks **without** lifting the enclosing function/class name
into structured metadata. cAST names this "contextual awareness" and flags
it as an open improvement area.

Producing the breadcrumb means an **extra tree-sitter traversal** per
chunk: walk up to the nearest enclosing `function_definition` /
`class_definition` node and read its `name` child. It is the same enrichment
shape as markdown's `heading_path` and PDF's `page:{n}`, just sourced from a
parse tree instead of heading tokens — and it is not free.

## Why this is deferred

1. **Build-time C compiler tension.** tree-sitter grammars compile a C
   parser via `cc`, requiring a C toolchain at build time. That fights the
   prebuilt-binary install path (`/install`), which today needs no toolchain
   on supported targets. Resolving that tension is a deliberate decision.
2. **Code was explicitly cut from current scope.** Phase 1 is plain text +
   spreadsheets; code is intentionally out, with `repo:{name}:code` left as
   a comment-only placeholder (`repository.rs:48`).

## Related

- [multi-format-ingestion](multi-format-ingestion.md) — the parent picture: per-format dispatch, the `CorpusChunker` seam, and the full `text-splitter` splitter table.
- [pdf-ocr-ingestion](pdf-ocr-ingestion.md) — sibling deferred format: text-layer PDF crate tradeoff and the OCR gap.
- [office-prose-extraction](office-prose-extraction.md) — sibling deferred format: the immature .docx/.pptx/.odt crate landscape.
- [architecture](architecture.md) — where `repository.rs` and the chunking path sit in the sliced-bread layout.
