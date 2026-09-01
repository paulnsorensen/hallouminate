# PDF and OCR ingestion

> **Status: text-layer PDF support shipped; OCR remains deferred.** PDF files selected by corpus globs are extracted page by page through the existing format-handler seam. The managed repository wiki remains Markdown-only.

PDF support is two different capabilities: **text-layer extraction**, now implemented, and **scanned-document OCR**, still out of scope. An image-only page has no text for a PDF parser to recover.

## Implemented text-layer path

`Format::Pdf` is selected by a case-insensitive `.pdf` extension or `file-format`'s `PortableDocumentFormat` magic-byte result (`crates/hallouminate-domain/src/indexer/format.rs::format_from_extension`; `format.rs::detect_by_magic`). `HandlerRegistry` owns a `PdfHandler` alongside Markdown, text, RST, and spreadsheet handlers (`crates/hallouminate-domain/src/indexer/format.rs::HandlerRegistry`).

`PdfHandler` uses pure-Rust `pdf-extract` 0.10 (`crates/hallouminate-domain/Cargo.toml:47-57`). This corrects the earlier research claim that `pdf-extract` could not preserve page boundaries: v0.10 exposes a public `output_doc_page(doc, output, page_num)` entry point, so `page:N` breadcrumbs do **not** require Pdfium. `extract_pdf_pages` loads the document once, then loops `doc.get_pages()`'s 1-based page numbers and calls `output_doc_page` once per page with a fresh `PlainTextOutput<&mut String>`, so each page's extracted text starts from clean splitter/line state. Pdfium remains relevant only if layout coordinates or higher-fidelity visual reading order become requirements.

Each page is split independently with the registry's existing `TextSplitter`. Chunks use `page:N` as `heading_path`, page-local extracted-text line ranges, and the normal summary, keyword, search-text, raw-byte hash, and indexing metadata pipeline (`crates/hallouminate-domain/src/indexer/format.rs::PdfHandler::prepare`). No LanceDB schema migration is involved.

## Failure and partial-document policy

Empty pages are omitted. If other pages contain text, the PDF is indexed and one warning records the 1-based empty page numbers. If every page is empty, the handler returns an extraction error so the file is counted as unreadable rather than as a valid empty document (`crates/hallouminate-domain/src/indexer/format.rs::PdfHandler::prepare`).

Corrupt and encrypted PDFs also return contextual extraction errors. The indexer's existing per-file isolation skips them without aborting valid sibling files. The dispatcher continues hashing raw on-disk bytes before handler extraction, so PDF changes retain normal reindex behavior (`crates/hallouminate-domain/src/indexer/writer.rs:27-88`).

Hallouminate does not use `pdf-extract`'s convenience page iterator because that API treats a later-page decode error as normal end-of-document. Instead `extract_pdf_pages` calls `output_doc_page` explicitly per page and propagates any page's error with `?`, so a malformed later page fails the whole file rather than silently truncating it (`crates/hallouminate-domain/src/indexer/format.rs::extract_pdf_pages`). The third-party decoder contains panic paths for malformed page objects; panic isolation now lives at the shared per-file dispatcher (`crates/hallouminate-domain/src/indexer/writer.rs::prepare_file`), wrapping every `FormatHandler::prepare` call, not just the PDF one, and converting a caught panic into the same extraction-failure error path as an `Err` return. A malformed later page therefore cannot leave a partially indexed PDF or abort sibling indexing.

## Known limits

- Text follows PDF content-stream order; complex multi-column layouts may be misordered.
- Page breadcrumbs are available, but layout coordinates and bounding boxes are not.
- Ground's `line_range` values are page-local extracted-text lines, not literal lines in the binary file.
- Binary PDFs do not contribute ripgrep matches; their indexed text still participates in FTS, vector, and term-containment ranking.
- Password configuration is not supported.

## Scanned PDFs and OCR

OCR remains a separate future subsystem. The likely shape is page rasterization followed by an OCR engine for pages with no extractable text, with explicit language packs, packaging, caching, confidence policy, and failure telemetry. It should not be folded into the text-layer handler without a separate design and dependency decision.

## Configuration

PDF ingestion is opt-in through ordinary corpus globs, for example `globs = ["**/*.md", "**/*.pdf"]` or repository `corpus_globs`. The derived `repo:<name>:wiki` corpus stays fixed to `**/*.md`; a repository source corpus preserves its configured globs (`crates/hallouminate-domain/src/repository.rs:87-124`).

## Related

- [multi-format-ingestion](multi-format-ingestion.md) — per-format dispatch and the shared handler seam.
- [office-prose-extraction](office-prose-extraction.md) — deferred office-prose formats.
- [code-aware-chunking](code-aware-chunking.md) — deferred tree-sitter-aware code chunking.
- [architecture](architecture.md) — indexer placement in the sliced-bread layout.
