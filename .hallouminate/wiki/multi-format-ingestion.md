# Multi-format ingestion

Hallouminate is **no longer markdown-only**. The indexer routes five format
families — markdown, plain text, reStructuredText, spreadsheets, and text-layer
PDFs — each to its own handler, following the per-format-dispatch design this
page previously argued for. The single most important nuance: **the wiki corpus
you author into is still markdown-only**; multi-format applies to the non-wiki
corpora (`repo:NAME:corpus` and explicit `[[corpus]]` entries). This page records
what shipped, why, and what remains deferred.

Full research evidence behind the crate/pattern choices lives in two on-disk
artifacts:

- `.cheese/research/multi-format-ingest/multi-format-ingest.md` — Rust crate survey (text-splitter surface, tree-sitter grammars, PDF extractors, format detectors), with a claim-level source table.
- `.cheese/research/multi-format-rag-ingestion/multi-format-rag-ingestion.md` — RAG pipeline architecture patterns (LangChain / LlamaIndex / unstructured.io loaders and splitters), with a claim-level source table.

## What shipped: extension-keyed dispatch, one handler per format

The pipeline is the extension-keyed-loader-registry shape the RAG literature
converges on. Three seams, all in
`crates/hallouminate-domain/src/indexer/format.rs`:

1. **`Format` enum** (`format.rs::Format`): `Markdown`, `PlainText`, `Rst`,
   `Spreadsheet`, and `Pdf`.
2. **`detect_format`** (`format.rs::detect_format`): **extension is decisive**.
   `format_from_extension` maps the extension; a *known-but-unsupported*
   extension returns `None` and is skipped **without a magic-byte sniff** —
   deliberately, so a `.docx` (a ZIP) is never mislabeled as a spreadsheet
   container. Only an extensionless name falls back to `detect_by_magic`
   (`file-format` 0.29 content sniff).
3. **`HandlerRegistry`** (`format.rs::HandlerRegistry`): holds one boxed
   `FormatHandler` per format and dispatches with a total `handler(format)`
   match. The daemon builds it once and threads it through
   `index_single_file` / `catch_up_corpus`
   (`crates/hallouminate-daemon/src/dispatch.rs`).

Extension routing (not magic-byte sniffing) is the right call for a git-repo
indexer because source and doc files have well-known extensions; unstructured.io
is the outlier that sniffs libmagic, and it pays for it. The one place bytes
still matter is extensionless files.

### Format → handler map

| Extension(s) | `Format` | Handler | Chunking |
|---|---|---|---|
| `md`, `markdown` | `Markdown` | `MarkdownHandler` | `MarkdownChunker` (pulldown-cmark, H1→H3 breadcrumbs, frontmatter + claim marks) |
| `txt`, `text`, `adoc`, `asciidoc`, `org` | `PlainText` | `TextHandler<S>` | `text_splitter::TextSplitter` budget windows; **empty `heading_path`**, no frontmatter, no claim marks |
| `rst` | `Rst` | `RstHandler` | `RstChunker`: plain `TextSplitter` windows **plus a section-adornment side-pass** that recovers RST heading breadcrumbs; no frontmatter/claim marks |
| `csv`, `xlsx`, `xls`, `ods` | `Spreadsheet` | `SpreadsheetHandler` | one chunk **per data row**; first row is the header; each row renders `col: val` lines so the chunk self-describes; breadcrumb `sheet:row-N`. CSV via the `csv` crate, workbooks via `calamine` |
| `pdf` | `Pdf` | `PdfHandler<S>` | pure-Rust `pdf-extract` page extraction followed by per-page `TextSplitter` windows; breadcrumb `page:N`; empty pages warn and are omitted |

`.md`/AsciiDoc/`.org` note: AsciiDoc and Org route to the *plain-text*
handler — there is no AsciiDoc/Org structure parser, they just chunk as text.
Markdown remains the only format with frontmatter and `<!--claim:-->` parsing.

## Failure handling: skip one file, never abort the run

`prepare_file` (`crates/hallouminate-domain/src/indexer/writer.rs::prepare_file`)
replaced the old hard UTF-8 gate. The previous behavior — one non-UTF-8 file
erroring the whole `prepare_file` — is gone. Now:

- A known-unsupported extension is skipped **before any IO** (no read, no hash)
  via `format_from_extension` returning `Some(None)`.
- A real IO error on a file it *does* read is still a hard error (the caller
  must not silently drop it).
- An **extraction failure** inside a handler (corrupt workbook, non-UTF-8 text,
  extensionless file that sniffs unsupported) is a per-file skip: logged and
  `Ok(None)`, the reindex continues. One bad file never aborts the run.

## Why per-format dispatch, not a wider glob

The consistent finding across LangChain, LlamaIndex, and unstructured.io: one
generic splitter for all file types is the most common quality antipattern.
Markdown loses heading structure, code splits mid-function, spreadsheets lose
row/column context. So multi-format was never a glob widening — it is routing
each format to its own splitter and its own metadata. The walker was already
format-agnostic (`crates/hallouminate-domain/src/corpus/walker.rs::scan` is
glob-driven with no extension gate); the constraint was always downstream, and
that is exactly where the `FormatHandler` seam now lives.

## Corpus wiring: rides the source corpus, not a `code` corpus

Multi-format extends the existing wiki/source corpus chunking path rather than a
new corpus kind. `RepoCorpusKind` still has only `Wiki` and (source) `Corpus` —
there is **no `Code` variant** (`RepoCorpusKind::Code` finds nothing in the
tree). The derived wiki corpus keeps `globs: ["**/*.md"]`
(`crates/hallouminate-domain/src/repository.rs::repository_wiki_corpus`), so
authored wikis stay markdown-only; the derived `repo:NAME:corpus` source corpus
and user `[[corpus]]` entries are where text, RST, spreadsheet, and PDF files get
indexed when their globs select those extensions.
This resolves the old "corpus wiring" open question.

## Dependencies added

- `file-format` 0.29 — magic-byte sniff for extensionless names. Detects the
  binary spreadsheet containers (OOXML/OLE2/ODF) but has **no CSV or markdown
  variant**, so an extensionless CSV or markdown file sniffs as `PlainText` and
  routes to the text handler — acceptable, since extensionful files never reach
  the sniff.
- `calamine` — workbook reader (`xlsx`/`xls`/`ods`); `csv` crate for CSV.
- `pdf-extract` 0.10 — pure-Rust, page-separated text-layer PDF extraction. Its
  page API supports `page:N` breadcrumbs without Pdfium; layout coordinates and
  OCR remain unavailable.

## Deferred / future-phase

Each remaining format has its own forward-looking page recording why it is deferred.
Text-layer PDF support has shipped; [pdf-ocr-ingestion](pdf-ocr-ingestion.md) now
records the implemented handler and the still-deferred OCR/scanned-PDF subsystem.
- [office-prose-extraction](office-prose-extraction.md) — the immature .docx/.pptx/.odt crate landscape (no mature + heading-aware option; crate choice deferred to a cook-time spike).
- [code-aware-chunking](code-aware-chunking.md) — tree-sitter `CodeSplitter`, the build-time C-compiler tension, and the `{file}::{class}::{fn}` cAST breadcrumb gap. Still deferred: no code handler or `Code` corpus variant exists.

## Related

- [architecture](architecture.md) — where `corpus/`, `indexer/`, and `repository.rs` sit in the sliced-bread layout.
- [corpus-walker](corpus-walker.md) — the format-agnostic walker that never gated on extension.
- [config-layering](config-layering.md) — how `[[repository]]` entries derive `repo:NAME:wiki` and `repo:NAME:corpus`.
