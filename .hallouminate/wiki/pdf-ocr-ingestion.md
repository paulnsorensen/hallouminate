# PDF and OCR ingestion

> **Status: DEFERRED / future-phase.** None of this ships in Phase 1
> (plain text + spreadsheets). This page records the durable shape of the
> PDF problem so a later phase starts from the decided constraints, not a
> blank slate. The crate-level evidence behind every claim lives in
> `.cheese/research/multi-format-ingest/multi-format-ingest.md` and
> `.cheese/research/multi-format-rag-ingestion/multi-format-rag-ingestion.md`,
> each with a claim-level source table.

PDF support splits into two genuinely different problems: **text-layer
PDFs** (a crate-choice decision) and **scanned PDFs** (an OCR-engine
decision). They are deferred for different reasons; conflating them is
the trap.

## The two crate options, and the tradeoff between them

Adding PDF is a new dependency either way — neither crate is reachable by
extending `text-splitter` the way plain-text and code chunking are.

| Crate | Native dep | Page metadata | When to reach for it |
|---|---|---|---|
| `pdf-extract` (0.10.0) | none — pure Rust (builds on `lopdf`) | **none natively** | the default starting point |
| `pdfium-render` (0.9.2) | **~20 MB `libpdfium`** (Chromium's Pdfium C++ lib) at runtime | page-level metadata + layout fidelity | only if `pdf-extract`'s fidelity / metadata proves inadequate |

`pdf-extract` is one call (`extract_text_from_mem(&bytes)`), highest
download signal, no toolchain cost — but it extracts in content-stream
order (may misorder multi-column layouts) and exposes **no page-level
metadata**. `pdfium-render` fixes both but binds a native runtime library,
which fights the prebuilt-binary install path (`/install`) that today needs
no toolchain on supported targets. `lopdf` is structural manipulation, not
a text extractor — its `extract_text` is order-unaware and secondary. Not
the right tool.

The forced choice: **a `page:N` breadcrumb requires the heavier crate.**
The markdown `heading_path` (`chunker.rs:124`) generalizes to a `page:{n}`
breadcrumb for PDF — but `pdf-extract` cannot supply the page number, so a
navigable PDF breadcrumb means taking on the native `pdfium` dep. That is
the core deferred tradeoff: native-dep-vs-coarse-metadata.

## Where PDF bytes enter: the UTF-8 gate must be bypassed

`prepare_file` (`writer.rs:16`) reads the file (`writer.rs:22`), hashes the
raw bytes (`writer.rs:25`), then hard-rejects non-UTF-8 with
`String::from_utf8(bytes)` at `writer.rs:26` (`non-utf8 file` error,
guarded by `prepare_file_errors_on_non_utf8_file` at `writer.rs:168`). PDF
is binary — its bytes never survive that line.

The fix is **not** to loosen the gate. Text extraction must run *before*
the gate, slotting between the byte read (`writer.rs:22`) and the UTF-8
conversion (`writer.rs:26`): a PDF is detected, decoded to text by the PDF
crate, and only the extracted text flows into the existing markdown-style
chunking path. The gate stays intact for everything that is genuinely
expected to be UTF-8. Note the hash at `writer.rs:25` is over the raw
bytes and must stay that way — re-index triggering depends on hashing the
on-disk file, not the extracted text.

## Scanned PDFs / OCR — a separate engine, explicitly out of scope

A scanned PDF has **no text layer**. Every text extractor — `pdf-extract`
or `pdfium-render` alike — returns empty or garbage on it. There is no
crate-choice that fixes this; it needs an entirely separate capability: an
OCR engine.

The documented pipeline pattern is a two-stage fallback: detect
near-empty extraction for a page, then route that page to OCR (the
standard stack is rasterize-then-Tesseract). This is a whole subsystem —
a new heavy dependency, image rasterization, and per-page routing logic —
not a knob on the PDF crate.

**Decision recorded: OCR is out of scope as its own future sub-problem.**
This is a per-repo wiki/RAG indexer; repos rarely contain scanned PDFs, so
a clear "text-layer PDF only" limitation is the likely acceptable stance.
The gap is named here deliberately and left un-researched at the crate
level — when OCR becomes real work, it earns its own page and its own
spike, rather than being bolted onto the text-layer PDF effort.

## Why this is deferred

1. **New dependency.** PDF is not an extension of the existing
   `text-splitter` dep; it adds a crate no matter which option wins.
2. **Native-dep-vs-metadata tradeoff is unresolved.** Page breadcrumbs
   require `pdfium`'s ~20 MB native lib, which conflicts with the
   no-toolchain install path. That call needs a deliberate decision, not a
   default.
3. **OCR is a separate engine concern.** Scanned PDFs are a different
   problem with a different (much heavier) solution; folding it into the
   text-layer work would mis-scope both.

## Related

- [multi-format-ingestion](multi-format-ingestion.md) — the parent picture: why multi-format is per-format dispatch, not a glob change, and where the `CorpusChunker` seam sits.
- [office-prose-extraction](office-prose-extraction.md) — sibling deferred format: the immature .docx/.pptx/.odt crate landscape.
- [code-aware-chunking](code-aware-chunking.md) — sibling deferred format: tree-sitter `CodeSplitter` and the `{file}::{class}::{fn}` breadcrumb.
- [architecture](architecture.md) — where `indexer/writer.rs` and the chunking path sit in the sliced-bread layout.
