# Office prose documents (.docx / .pptx / .odt)

> **Status: DEFERRED / future-phase.** None of this ships in Phase 1
> (plain text + spreadsheets). It captures the office-document crate
> landscape so a later phase inherits the survey instead of re-running it.
> Full claim-level evidence — versions, download counts, known bugs — lives
> in `.cheese/research/office-doc-ingestion/office-doc-ingestion.md`.

Office *prose* documents (Word, PowerPoint, OpenDocument text) are deferred
not because extraction is hard but because **no Rust crate is both mature
and heading-aware** today. The blocking want is a `heading_path`
breadcrumb (the generalization of the markdown `heading_path` at
`chunker.rs:124`), and the one crate that exposes heading levels is two
weeks old. The decision recorded in the Phase 1 spec is to **defer crate
choice to a cook-time spike** against real files.

This page covers prose only. Spreadsheets (.xlsx/.xls/.ods, via `calamine`)
are a Phase 1 format and are not deferred — see the tabular note below.

## The .docx crate landscape — pick your poison

| Crate | Downloads | Maturity | Heading levels? | Notes |
|---|---|---|---|---|
| `docx-rust` | ~2.0M | mature-ish, true read+write | limited (styles module, no high-level heading accessor) | **known panic** on OnlyOffice/LibreOffice files (strict parse, missing `rotate_with_shape` in `GradFill`) |
| `docx-rs` | ~2.5M | writer library with a read path | no | high downloads are write traffic, not read |
| `undoc` | ~7.9K | **~10 days old** at research time, single maintainer | **YES — `HeadingLevel` enum** in its document model; `to_markdown()` emits `#` headings | the only crate enabling a real `heading_path`; pure Rust (zip + quick-xml) |
| `docx-lite` | ~49K | purpose-built text extractor, pure Rust | no | one-liner `extract_text(path)`; minimal deps; no heading metadata |
| `office_oxide` | ~92K | **~1 month old** | no explicit heading enum | only crate covering legacy binary DOC/XLS/PPT alongside modern OOXML; 100% pass-rate claim is **self-reported, unverified** |

The tension in one line: **the mature options (`docx-rust`, `docx-rs`)
give no heading structure, and the only heading-aware option (`undoc`) is
too new to trust blind.** `docx-rust` additionally carries a documented
crash on non-Microsoft-authored files (LibreOffice/OnlyOffice GradFill),
which is exactly the kind of file a real repo contains.

## .pptx — even thinner

PowerPoint prose has a far smaller ecosystem. Only **`undoc`** (`PptxParser`
→ slide sections + text) and **`office_oxide`** expose extractable-text
APIs — both the same very-new crates flagged above. Everything else is a
writer or abandoned. Slide breadcrumb shape: `{filename} > slide {n}`.

## .odt — no trusted crate, raw zip+XML is the fallback

No top candidate explicitly handles OpenDocument text extraction.
`office_oxide` and `omniparse` *list* .odt but neither is verified against
real ODF files. The safe fallback is **parsing the zip directly**: ODF
stores prose in `content.xml` inside the zip; headings are `text:h`
elements carrying a `text:outline-level` attribute (the OOXML equivalent is
`w:p` with a `Heading N` style in `w:pStyle`). That raw parse is
well-understood and gives full control, at the cost of writing the
extraction by hand.

## Tabular office data is not on this page

Spreadsheets (.xlsx/.xls/.ods) are a Phase 1 format, handled by `calamine`
(pure Rust, read-only, auto-detects format). The dominant RAG pattern for
tabular data is **per-row chunking with the header columns repeated in each
chunk** (the LangChain/LlamaIndex/Bedrock consensus), with a breadcrumb of
`{filename} > {sheet_name} > row {n}`. That belongs with the Phase 1
spreadsheet work, not this deferred-prose page — noted here only so the
boundary is explicit.

## Why this is deferred

- **No mature + heading-aware option exists.** Every crate is either
  battle-tested-but-flat (`docx-rust`, `docx-rs`) or
  heading-aware-but-unproven (`undoc`, ~10 days old). There is no safe
  default to encode in a spec.
- **Real-file risk is concrete, not hypothetical.** `docx-rust`'s GradFill
  panic on LibreOffice/OnlyOffice output means crate choice must be tested
  against the messy files repos actually contain.
- **The decision needs a spike, not a guess.** The Phase 1 spec records
  crate choice as deferred to a cook-time spike on real .docx/.pptx/.odt
  files — including whether `undoc`'s `HeadingLevel` round-trips through
  styles defined in `word/styles.xml` vs inline in `w:pStyle`.

## Related

- [multi-format-ingestion](multi-format-ingestion.md) — the parent picture: per-format dispatch and the `CorpusChunker` seam.
- [pdf-ocr-ingestion](pdf-ocr-ingestion.md) — sibling deferred format: text-layer PDF crate tradeoff and the OCR gap.
- [code-aware-chunking](code-aware-chunking.md) — sibling deferred format: tree-sitter `CodeSplitter` and the code breadcrumb.
- [architecture](architecture.md) — where the chunking path lives in the sliced-bread layout.
