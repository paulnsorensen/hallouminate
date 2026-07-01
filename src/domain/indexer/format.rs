//! Per-format ingestion dispatch.
//!
//! `prepare_file` (writer.rs) detects a file's [`Format`] and routes it to a
//! [`FormatHandler`] that owns the full load → chunk → metadata pipeline for
//! that format. The [`HandlerRegistry`] holds one handler per format and is
//! threaded where the single markdown chunker used to be (`DaemonState::open`).
//!
//! Detection is extension-primary: the file extension picks the format, and
//! `file-format`'s magic-byte sniff is the fallback only for extensionless
//! inputs (a known-but-unsupported extension is decisive, never sniffed).
//! Unsupported types and per-handler extraction failures
//! are graceful per-file skips at the call site, never run-aborting errors.

use std::io::Cursor;
use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto_from_rs};
use file_format::FileFormat;
use text_splitter::{ChunkConfig, ChunkSizer, TextSplitter};

use crate::adapters::lance::{PreparedChunk, PreparedFile};
use crate::domain::common::{CorpusConfig, FileRef, HallouminateError, Mtime, Result};
use crate::domain::corpus::{
    ClaimMark, CorpusChunker, Frontmatter, build_line_starts, byte_to_line, extract_claim_marks,
    extract_keywords, extract_summary, marks_to_canonical_json, split_frontmatter,
    strip_claim_marks,
};

use super::writer::file_ref_string;

/// The set of formats Phase 1 can ingest. Extended in later phases (PDF,
/// office-prose, code-aware).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    PlainText,
    Spreadsheet,
}

/// Resolve a file's [`Format`] from its extension first, falling back to a
/// magic-byte sniff for extensionless names only (a known-but-unsupported
/// extension returns `None`, never a sniff).
///
/// Returns `None` for any type Phase 1 does not handle — the caller logs a
/// warning and skips the file.
pub fn detect_format(path: &Path, bytes: &[u8]) -> Option<Format> {
    match format_from_extension(path) {
        // Extension is decisive: `Some(fmt)` when supported, `None` when a
        // known-but-unsupported extension (never sniffed).
        Some(ext_format) => ext_format,
        // No usable extension: fall back to a magic-byte sniff.
        None => detect_by_magic(bytes),
    }
}

/// Classify a path's extension *without reading its bytes*, so the caller can
/// skip a known-unsupported file before any IO.
///
/// - `Some(Some(fmt))` — extension maps to a supported format.
/// - `Some(None)` — extension is present but unsupported: skip, no read needed.
/// - `None` — no usable extension; bytes are required for a magic-byte sniff.
pub fn format_from_extension(path: &Path) -> Option<Option<Format>> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(Format::Markdown),
        "txt" | "text" => Some(Format::PlainText),
        "csv" | "xlsx" | "xls" | "ods" => Some(Format::Spreadsheet),
        // A known-but-unsupported extension is decisive: do NOT fall through
        // to a magic-byte sniff that might mislabel it (e.g. a `.docx` is a
        // ZIP and could be mistaken for a spreadsheet container).
        _ => None,
    })
}

/// Magic-byte fallback for inputs with no usable extension. Maps `file-format`'s
/// content sniff onto the Phase 1 set; everything else is unsupported.
///
/// `file-format` 0.29 detects the binary spreadsheet containers (OOXML xlsx,
/// OLE2 xls, ODF ods) by magic bytes but has **no CSV variant** — CSV is a plain
/// text shape with no signature, so an extensionless CSV sniffs as `PlainText`
/// and routes to the text handler. Markdown likewise has no variant and sniffs
/// as `PlainText`. Both are acceptable: extensionful files never reach here.
fn detect_by_magic(bytes: &[u8]) -> Option<Format> {
    match FileFormat::from_bytes(bytes) {
        FileFormat::OfficeOpenXmlSpreadsheet
        | FileFormat::MicrosoftExcelSpreadsheet
        | FileFormat::OpendocumentSpreadsheet => Some(Format::Spreadsheet),
        FileFormat::PlainText => Some(Format::PlainText),
        _ => None,
    }
}

/// What a handler needs to build a [`PreparedFile`]: the file's path, its raw
/// bytes (read once by the dispatcher), the precomputed content hash, and the
/// per-run metadata frame.
pub struct PrepareCtx<'a> {
    pub corpus: &'a CorpusConfig,
    pub file: &'a FileRef,
    pub mtime: Mtime,
    pub bytes: &'a [u8],
    pub content_hash: String,
    pub indexed_at_ms: i64,
}

/// A format owns its full load → chunk → metadata pipeline behind this trait.
/// Returning `Err` is an extraction failure: the dispatcher turns it into a
/// per-file skip, not a run abort.
pub trait FormatHandler: Send + Sync {
    fn prepare(&self, ctx: &PrepareCtx<'_>) -> Result<PreparedFile>;
}

// ── Markdown ──────────────────────────────────────────────────────────────

/// The verbatim legacy markdown pipeline, now behind [`FormatHandler`]. Output
/// must remain byte-identical to the pre-dispatch `prepare_file` (golden
/// snapshot regression).
pub struct MarkdownHandler {
    chunker: Box<dyn CorpusChunker>,
}

impl MarkdownHandler {
    pub fn new(chunker: Box<dyn CorpusChunker>) -> Self {
        Self { chunker }
    }
}

impl FormatHandler for MarkdownHandler {
    fn prepare(&self, ctx: &PrepareCtx<'_>) -> Result<PreparedFile> {
        let path = ctx.file.as_path();
        let body = std::str::from_utf8(ctx.bytes).map_err(|e| {
            HallouminateError::Indexer(format!("non-utf8 file {}: {e}", path.display()))
        })?;
        // Strip an optional leading frontmatter block before every text pass so
        // it never pollutes chunks, summary, or keywords. `fm_lines` is added
        // back to each chunk's line numbers so citations point at on-disk lines.
        let (frontmatter, content, fm_lines) = split_frontmatter(body);
        let chunks_raw = self.chunker.chunk_text(content);
        // Claim marks are parsed once on the (frontmatter-stripped) body; their
        // lines are body-relative, matching the chunker's body-relative chunk
        // line ranges. Each mark is bucketed into exactly one chunk below.
        let marks = extract_claim_marks(content);
        // Assign each mark to exactly ONE chunk. The naive inclusive range test
        // (`line_start <= m.line <= line_end`) double-buckets when a single
        // over-budget line is split across chunks: every sub-chunk then carries
        // `line_start == line_end == N`, so a mark on line N matches all of them
        // and surfaces N times in `ground`. Pick the LAST matching chunk — a
        // mark's `<!--claim:...-->` text sits at the end of its line, so it
        // belongs to the final sub-chunk of a split line. For an unsplit line
        // exactly one chunk matches, so this is identical to the old behaviour.
        let mark_chunk_idx: Vec<Option<usize>> = marks
            .iter()
            .map(|m| {
                chunks_raw
                    .iter()
                    .rposition(|c| m.line >= c.line_start && m.line <= c.line_end)
            })
            .collect();
        let fallback = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let summary = extract_summary(content, &fallback);
        let keywords = extract_keywords(content);
        let file_ref_str = file_ref_string(ctx.file)?;
        let mut chunks: Vec<PreparedChunk> = Vec::with_capacity(chunks_raw.len());
        for c in chunks_raw {
            // Take only the marks assigned to this chunk (each mark lands in
            // exactly one chunk via `mark_chunk_idx`), then shift their lines by
            // `fm_lines` for the on-disk citation (same offset the chunk's line
            // numbers get below).
            let chunk_marks: Vec<ClaimMark> = marks
                .iter()
                .enumerate()
                .filter(|(mi, _)| mark_chunk_idx[*mi] == Some(c.ord))
                .map(|(_, m)| ClaimMark {
                    line: m.line + fm_lines,
                    ..m.clone()
                })
                .collect();
            chunks.push(PreparedChunk {
                ord: c.ord,
                heading_path: c.heading_path,
                line_start: c.line_start + fm_lines,
                line_end: c.line_end + fm_lines,
                // Strip claim comments from the retrieval text. This single edit
                // cleans both the embedding input and the stored snippet (they
                // share `PreparedChunk.text`); strip preserves line count so the
                // chunk's line numbers and the per-chunk mark filter stay aligned.
                text: strip_claim_marks(&c.text),
                claim_marks: marks_to_canonical_json(&chunk_marks),
            });
        }
        Ok(PreparedFile {
            file_ref: file_ref_str,
            corpus: ctx.corpus.name.clone(),
            mtime_ms: ctx.mtime.0,
            content_hash: ctx.content_hash.clone(),
            summary,
            keywords,
            frontmatter: frontmatter.as_ref().map(Frontmatter::to_canonical_json),
            indexed_at_ms: ctx.indexed_at_ms,
            chunks,
            embeddings: None,
        })
    }
}

// ── Plain text ────────────────────────────────────────────────────────────

/// Plain-text handler: budget-bounded chunks via `text-splitter`'s
/// `TextSplitter`. No frontmatter, no claim marks, no heading breadcrumb.
///
/// Generic over the sizer for the same reason `MarkdownChunker<S>` is:
/// `text-splitter`'s `ChunkSizer` blanket impl on `Box<T>` requires `T: Sized`,
/// so a `Box<dyn ChunkSizer>` cannot configure a splitter. The registry erases
/// the parameter by boxing the *handler* (`Box<dyn FormatHandler>`).
pub struct TextHandler<S: ChunkSizer> {
    splitter: TextSplitter<S>,
}

impl<S: ChunkSizer> TextHandler<S> {
    pub fn new(sizer: S, budget_tokens: usize) -> Self {
        let config: ChunkConfig<S> = ChunkConfig::new(budget_tokens).with_sizer(sizer);
        Self {
            splitter: TextSplitter::new(config),
        }
    }
}

impl<S: ChunkSizer + Send + Sync> FormatHandler for TextHandler<S> {
    fn prepare(&self, ctx: &PrepareCtx<'_>) -> Result<PreparedFile> {
        let path = ctx.file.as_path();
        let body = std::str::from_utf8(ctx.bytes).map_err(|e| {
            HallouminateError::Indexer(format!("non-utf8 file {}: {e}", path.display()))
        })?;
        let line_starts = build_line_starts(body);
        let mut chunks: Vec<PreparedChunk> = Vec::new();
        for (byte_off, slice) in self.splitter.chunk_indices(body) {
            if slice.is_empty() {
                continue;
            }
            let line_start = byte_to_line(byte_off, &line_starts);
            let end_byte = byte_off + slice.len();
            let line_end = if end_byte == 0 {
                line_start
            } else {
                byte_to_line(end_byte - 1, &line_starts)
            };
            chunks.push(PreparedChunk {
                ord: chunks.len(),
                heading_path: Vec::new(),
                line_start,
                line_end,
                text: slice.to_string(),
                claim_marks: None,
            });
        }
        let fallback = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(PreparedFile {
            file_ref: file_ref_string(ctx.file)?,
            corpus: ctx.corpus.name.clone(),
            mtime_ms: ctx.mtime.0,
            content_hash: ctx.content_hash.clone(),
            summary: extract_summary(body, &fallback),
            keywords: extract_keywords(body),
            frontmatter: None,
            indexed_at_ms: ctx.indexed_at_ms,
            chunks,
            embeddings: None,
        })
    }
}

// ── Spreadsheet ───────────────────────────────────────────────────────────

/// Spreadsheet handler: one self-describing chunk per data row. The first row
/// of each sheet is treated as the header (neither calamine nor the CSV reader
/// auto-detects headers); each data row renders as `col: val` lines so the
/// chunk carries its own column context. Breadcrumb is `sheet:row-N`.
pub struct SpreadsheetHandler;

impl FormatHandler for SpreadsheetHandler {
    fn prepare(&self, ctx: &PrepareCtx<'_>) -> Result<PreparedFile> {
        let path = ctx.file.as_path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let chunks = match ext.as_deref() {
            Some("csv") => csv_chunks(ctx.bytes, path)?,
            _ => workbook_chunks(ctx.bytes, path)?,
        };
        let fallback = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Summary/keywords over the concatenated chunk text so a spreadsheet is
        // still lexically discoverable; cheap and bounded by the row text.
        let joined: String = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(PreparedFile {
            file_ref: file_ref_string(ctx.file)?,
            corpus: ctx.corpus.name.clone(),
            mtime_ms: ctx.mtime.0,
            content_hash: ctx.content_hash.clone(),
            summary: extract_summary(&joined, &fallback),
            keywords: extract_keywords(&joined),
            frontmatter: None,
            indexed_at_ms: ctx.indexed_at_ms,
            chunks,
            embeddings: None,
        })
    }
}

/// Render one CSV file as per-row chunks. The header record names every column;
/// each subsequent record becomes one `col: val` chunk under breadcrumb
/// `csv:row-N` (N is the 1-based data-row index).
fn csv_chunks(bytes: &[u8], path: &Path) -> Result<Vec<PreparedChunk>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(bytes);
    // `headers()` borrows `&mut self`; clone so the `records()` loop can reborrow.
    let headers = rdr.headers().map_err(|e| extract_err(path, &e))?.clone();
    let header_vec = header_strs(&headers);
    let mut chunks: Vec<PreparedChunk> = Vec::new();
    for (row_idx, record) in rdr.records().enumerate() {
        let record = record.map_err(|e| extract_err(path, &e))?;
        let cells: Vec<String> = record.iter().map(|s| s.to_string()).collect();
        let text = row_text(&header_vec, &cells);
        if text.is_empty() {
            continue;
        }
        // On-disk line: header is line 1, data rows are 1-based below it.
        push_row_chunk(&mut chunks, "csv", row_idx + 1, row_idx + 2, text);
    }
    Ok(chunks)
}

/// Render an xlsx/xls/ods workbook as per-row chunks across every sheet. The
/// first row of each sheet is the header; each later row becomes one `col: val`
/// chunk under breadcrumb `{sheet}:row-N`.
fn workbook_chunks(bytes: &[u8], path: &Path) -> Result<Vec<PreparedChunk>> {
    let cursor = Cursor::new(bytes);
    let mut workbook = open_workbook_auto_from_rs(cursor).map_err(|e| extract_err(path, &e))?;
    let sheet_names = workbook.sheet_names().to_vec();
    let mut chunks: Vec<PreparedChunk> = Vec::new();
    for name in &sheet_names {
        let range = workbook
            .worksheet_range(name)
            .map_err(|e| extract_err(path, &e))?;
        let mut rows = range.rows();
        let Some(header_row) = rows.next() else {
            continue;
        };
        let headers: Vec<String> = header_row.iter().map(cell_to_string).collect();
        for (row_idx, row) in rows.enumerate() {
            let cells: Vec<String> = row.iter().map(cell_to_string).collect();
            let text = row_text(&headers, &cells);
            if text.is_empty() {
                continue;
            }
            // No on-disk line for binary formats: `line` is the per-sheet row ordinal.
            push_row_chunk(&mut chunks, name, row_idx + 1, row_idx + 1, text);
        }
    }
    Ok(chunks)
}

fn header_strs(headers: &csv::StringRecord) -> Vec<String> {
    headers.iter().map(|s| s.to_string()).collect()
}

/// One self-describing row: `col: val` per non-empty column, newline-joined.
/// A value with no matching header falls back to a positional `col_N` key so no
/// cell is silently dropped.
fn row_text(headers: &[String], cells: &[String]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(cells.len());
    for (i, val) in cells.iter().enumerate() {
        if val.trim().is_empty() {
            continue;
        }
        let key = headers
            .get(i)
            .filter(|h| !h.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("col_{}", i + 1));
        lines.push(format!("{key}: {val}"));
    }
    lines.join("\n")
}

/// Push one row as a chunk. `row` is the 1-based per-sheet data-row ordinal
/// used in the breadcrumb; `line` is the value surfaced as `line_range`.
///
/// For CSV `line` is the true on-disk line (header row + 1-based data lines).
/// For binary xlsx/ods there is no on-disk line concept, so `line_range` is the
/// per-sheet row ordinal rather than a file line.
fn push_row_chunk(
    chunks: &mut Vec<PreparedChunk>,
    sheet: &str,
    row: usize,
    line: usize,
    text: String,
) {
    let ord = chunks.len();
    chunks.push(PreparedChunk {
        ord,
        heading_path: vec![format!("{sheet}:row-{row}")],
        line_start: line,
        line_end: line,
        text,
        claim_marks: None,
    });
}

/// Display a single spreadsheet cell as a string. `Data` implements `Display`,
/// so empty cells render as `""` and every scalar gets its natural rendering.
fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        other => other.to_string(),
    }
}

fn extract_err(path: &Path, e: &dyn std::fmt::Display) -> HallouminateError {
    HallouminateError::Indexer(format!("extract {}: {e}", path.display()))
}

// ── Registry ──────────────────────────────────────────────────────────────

/// Holds one [`FormatHandler`] per [`Format`]. Constructed fresh per indexing
/// operation via the daemon's `make_registry` (cheap: a cloned tokenizer and
/// thin handler wrappers), then threaded through the indexer where the single
/// markdown chunker used to be. Handlers are boxed behind `dyn FormatHandler`
/// so the registry is not generic over the sizer (which would ripple `<S>` into
/// every indexer signature).
pub struct HandlerRegistry {
    markdown: Box<dyn FormatHandler>,
    text: Box<dyn FormatHandler>,
    spreadsheet: Box<dyn FormatHandler>,
}

impl HandlerRegistry {
    /// Build a registry over a single shared sizer (cloned into each text
    /// splitter). `S: Clone` so the markdown and plain-text splitters can each
    /// own a copy; production passes a `tokenizers::Tokenizer`, tests pass
    /// `Characters`.
    pub fn new<S>(sizer: S, budget_tokens: usize) -> Self
    where
        S: ChunkSizer + Clone + Send + Sync + 'static,
    {
        let markdown_chunker: Box<dyn CorpusChunker> = Box::new(
            crate::domain::corpus::MarkdownChunker::new(sizer.clone(), budget_tokens),
        );
        Self {
            markdown: Box::new(MarkdownHandler::new(markdown_chunker)),
            text: Box::new(TextHandler::new(sizer, budget_tokens)),
            spreadsheet: Box::new(SpreadsheetHandler),
        }
    }

    /// The handler for a detected format. Total over the Phase 1 set.
    pub fn handler(&self, format: Format) -> &dyn FormatHandler {
        match format {
            Format::Markdown => self.markdown.as_ref(),
            Format::PlainText => self.text.as_ref(),
            Format::Spreadsheet => self.spreadsheet.as_ref(),
        }
    }
}
