# Multi-format ingestion

Hallouminate is functionally **markdown-only** today. The walker is
format-agnostic, but everything downstream of it — chunking, summary,
breadcrumbs, the UTF-8 gate — assumes markdown. Adding plain text,
source code, and PDF is not a glob change; it is a per-format dispatch
problem, because the single biggest documented antipattern in mature RAG
pipelines is using one generic splitter for every format. This page
captures what is true of the current pipeline, what tooling is reachable,
the architecture pattern to borrow, and the design questions still open —
so a future agent has the durable picture before touching the indexer.

Full evidence behind the crate and pattern claims lives in two on-disk
research artifacts:

- `.cheese/research/multi-format-ingest/multi-format-ingest.md` — Rust crate survey (text-splitter surface, tree-sitter grammars, PDF extractors, format detectors), with a claim-level source table.
- `.cheese/research/multi-format-rag-ingestion/multi-format-rag-ingestion.md` — RAG pipeline architecture patterns (LangChain / LlamaIndex / unstructured.io loaders and splitters), with a claim-level source table.

## Current state: markdown all the way down

The walker is *not* the constraint. `src/domain/corpus/walker.rs::scan`
(`walker.rs:13`) is glob-driven — `build_globset` over `corpus.globs` —
and empty globs match everything (`scan_with_empty_globs_matches_everything`,
`walker.rs:250`). There is no extension gate in the walker. The
markdown-only behavior comes from two places downstream:

1. **The wiki corpus hardcodes `globs: [\"**/*.md\"]`** in
   `repository_wiki_corpus` (`repository.rs:94`). So the derived
   `repo:NAME:wiki` corpus only ever sees `.md` files regardless of the
   walker's generality.

2. **The whole `prepare_file` pipeline assumes markdown.**
   `src/domain/indexer/writer.rs::prepare_file` (`writer.rs:16`):
   - **UTF-8 gate** — `String::from_utf8(bytes)` (`writer.rs:26`) hard-rejects
     any non-UTF-8 file with a `non-utf8 file` error. PDF bytes never make
     it past this line (regression: `prepare_file_errors_on_non_utf8_file`,
     `writer.rs:168`).
   - **Single chunker** — it takes `&dyn CorpusChunker`, and the *only*
     implementor is `MarkdownChunker` (`chunker.rs:25`), which wraps
     `text_splitter::MarkdownSplitter` over pulldown-cmark
     (`chunker.rs:3`, `chunker.rs:53`).
   - **Markdown-specific enrichment** — `extract_summary`,
     `build_breadcrumbs` (the H1→H3 `heading_path`, `chunker.rs:124`),
     `extract_claim_marks`, and `split_frontmatter` (`writer.rs:5-8`,
     `writer.rs:32-37`) are all markdown semantics.

A reserved intent for code indexing exists, but it is **a comment, not a
live variant**: `RepoCorpusKind` has only `Wiki` and `Corpus`
(`repository.rs:46-47`); line 48 is `// Future: Code maps to
repo:{name}:code if code-aware indexing is added.` Grepping for
`RepoCorpusKind::Code` finds nothing — the `suffix()` match
(`repository.rs:53-56`) has no `Code` arm and would not compile with one.
Treat it as scaffolded intent with no implementation.

## Why this matters: per-format dispatch, not a glob change

The consistent finding across LangChain, LlamaIndex, and unstructured.io
plus the practitioner community: applying one generic splitter
(`RecursiveCharacterTextSplitter`-style) to all file types is the most
common quality antipattern. Markdown loses its heading structure, code
gets split mid-function, PDFs lose page context — each format degrades
differently. So multi-format support is fundamentally about routing each
format to its own splitter and its own metadata, not about widening the
include glob. Hallouminate's `CorpusChunker` trait (`chunker.rs:17`) is
already the seam where that dispatch belongs — it just has one impl today.

## Available tooling (reachable without large new deps)

**Chunking — extend the existing dep, don't add one.** `text-splitter`
`0.32.0` is already locked (`Cargo.lock:6330`) and grouped with
`tokenizers` in dependabot lockstep (see below). It ships three splitters
on the same `ChunkConfig` / `ChunkSizer` we already re-export
(`chunker.rs:12`):

| Splitter | Feature flag | Backed by | Status here |
|---|---|---|---|
| `MarkdownSplitter` | `markdown` | pulldown-cmark | in use |
| `TextSplitter` | none (default) | Unicode segmenter | available today, no flag |
| `CodeSplitter` | `code` + one `tree-sitter-<lang>` crate per language | tree-sitter AST | one feature line + grammar crates |

So plain-text and code-aware chunking are extensions of the current dep,
not new dependencies. `CodeSplitter` bundles the tree-sitter runtime;
each language is a separate `tree-sitter-rust` / `-python` / … grammar
crate (~1–2 MB compiled each) that compiles its C parser via `cc` — so a
**C compiler is required at build time** for code support.

**PDF text extraction** (a new dep either way):

- `pdf-extract` (v0.10.0) — pure Rust, no OCR, single `extract_text_from_mem` call, highest download signal. The natural starting point. Extracts in content-stream order (may misorder multi-column layouts); scanned PDFs return empty.
- `pdfium-render` (v0.9.2) — better layout fidelity, but binds Google's Pdfium C++ lib (~20 MB native runtime dep). Reach for it only if `pdf-extract`'s fidelity proves inadequate.
- `lopdf` is structural manipulation, **not** a text-extraction library — its `extract_text` is secondary and order-unaware. Not the right tool.

**Format detection** for routing: `infer` (pure-Rust magic bytes) detects
PDF reliably, but **neither `infer` nor `file-format` can distinguish
source code from plain text** — both are just UTF-8 text at the byte
level. Routing code vs text needs an extension hint (a two-pass
extension-then-bytes strategy).

## Architecture pattern to borrow

LangChain, LlamaIndex, and unstructured.io converge on the same shape:

> **extension-keyed loader registry → normalized `Document(text, metadata)` → format-dispatched splitter**

with the **loader and the splitter as two independently-tuned stages**.
Mixing them (chunking during extraction) makes chunk parameters
impossible to tune without re-running extraction. unstructured.io is the
one outlier on detection — it sniffs magic bytes via libmagic rather than
trusting the extension; for a git-repo indexer, extension routing is
generally safe because source files have well-known extensions.

Two specifics worth carrying:

- **Code chunking via tree-sitter AST beats separator lists.** The cAST paper (2025) and LlamaIndex's `CodeSplitter` both show AST-boundary splitting (never mid-syntax, recurses into inner nodes for over-budget functions) outperforms language separator lists on code retrieval.
- **No framework injects function/class name as structured metadata by default.** This is exactly the generalization of our markdown `heading_path` (`chunker.rs:124`) to a code breadcrumb like `{file}::{class}::{fn}`, and PDF's to `page:{n}`. It requires an extra tree-sitter traversal to find the enclosing declaration node — it is not free.

## Open design questions

A parallel `/mold` spec session is deciding these; recorded here so the
decisions land against a stable problem statement.

1. **Code `heading_path` equivalent** — first-class `heading_path` field (generalize the existing markdown one) vs an optional enrichment pass.
2. **PDF crate choice** — `pdf-extract` vs `pdfium-render`, and whether OCR / scanned-PDF support is explicitly out of scope (repos rarely contain scanned PDFs; a \"text-layer only\" limitation may be acceptable).
3. **Build-time C compiler** — tree-sitter grammars and `pdfium` both add native build requirements. Weigh against the prebuilt-binary install path (`/install`), which currently needs no toolchain on supported targets.
4. **Default routing for unknown extensions** — skip, treat as plain text, or error.
5. **Per-format metadata schema** — whether to enforce one unified field name across `heading_path` (markdown), `page:{n}` (PDF), and `{file}::{class}::{fn}` (code), or let each format carry its own.
6. **Grammar version pinning** — each `tree-sitter-<lang>` grammar must track the tree-sitter runtime `text-splitter` bundles, the same lockstep already enforced for `text-splitter` + `tokenizers` in the `text-processing` dependabot group (`.github/dependabot.yml:20-25`).
7. **Corpus wiring** — whether multi-format rides the reserved `repo:{name}:code` path (the `// Future: Code` intent) or extends the existing wiki/corpus chunking path.

## Deferred / future-phase research

Phase 1 ships plain text + spreadsheets. The formats below are **out of
Phase 1** — each has its own forward-looking page recording why it is
deferred and what a later phase inherits:

- [pdf-ocr-ingestion](pdf-ocr-ingestion.md) — text-layer PDF crate tradeoff (`pdf-extract` vs native `pdfium-render` for a `page:{n}` breadcrumb), the UTF-8-gate bypass, and OCR / scanned-PDF as a separate out-of-scope engine.
- [office-prose-extraction](office-prose-extraction.md) — the immature .docx/.pptx/.odt crate landscape (no mature + heading-aware option; crate choice deferred to a cook-time spike).
- [code-aware-chunking](code-aware-chunking.md) — tree-sitter `CodeSplitter`, the build-time C-compiler tension, and the `{file}::{class}::{fn}` cAST breadcrumb gap (note: `RepoCorpusKind::Code` is a comment, not a variant).

## Related

- [architecture](architecture.md) — where `corpus/`, `indexer/`, and `repository.rs` sit in the sliced-bread layout.
- [corpus-walker](corpus-walker.md) — the format-agnostic walker that already does *not* gate on extension.
- [config-layering](config-layering.md) — how `[[repository]]` entries derive `repo:NAME:wiki` and `repo:NAME:corpus`.
