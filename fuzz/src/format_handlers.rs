//! Invariants for the `format_handlers` target: the per-format indexer
//! handlers on untrusted file bytes.
//!
//! The indexer runs each handler inside `catch_unwind` and skips a file whose
//! handler panics, so a handler panic is not a finding here. An abort, a stack
//! overflow, a timeout, an out-of-memory, or a broken invariant on a handler's
//! `Ok` output is a finding.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Once;

use hallouminate_domain::common::{CorpusKey, FileRef, Mtime};
use hallouminate_domain::corpus::{
    ClaimMark, MarkdownChunker, extract_claim_marks, split_frontmatter, strip_claim_marks,
};
use hallouminate_domain::indexer::{
    Format, HandlerRegistry, PAGE_BREADCRUMB_PREFIX, PrepareCtx, PreparedChunk, PreparedFile,
    detect_format,
};
use pdf_extract::Document;
use text_splitter::Characters;

const ROW_INFIX: &str = ":row-";
const CSV_SHEET: &str = "csv";

thread_local! {
    static CONTAINED: Cell<bool> = const { Cell::new(false) };
}

static HOOK: Once = Once::new();

/// Wraps the libFuzzer panic hook, which aborts, so that a panic inside
/// [`contained`] unwinds silently. Every other panic still aborts.
fn install_contained_panic_hook() {
    HOOK.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| match CONTAINED.get() {
            true => {}
            false => report(info),
        }));
    });
}

struct Containment {
    previous: bool,
}

impl Containment {
    fn enter() -> Self {
        Self {
            previous: CONTAINED.replace(true),
        }
    }
}

impl Drop for Containment {
    fn drop(&mut self) {
        CONTAINED.set(self.previous);
    }
}

/// Runs `run` the way the indexer runs a handler: a panic returns `None`.
fn contained<T>(run: impl FnOnce() -> T) -> Option<T> {
    install_contained_panic_hook();
    let guard = Containment::enter();
    let outcome = catch_unwind(AssertUnwindSafe(run));
    drop(guard);
    outcome.ok()
}

/// The file type a `format_handlers` input claims to be.
#[derive(Clone, Copy)]
enum FileKind {
    Markdown,
    Text,
    Rst,
    Csv,
    Workbook,
    Pdf,
    Json,
    Jsonl,
}

impl FileKind {
    fn from_byte(byte: u8) -> Self {
        match byte % 8 {
            0 => Self::Markdown,
            1 => Self::Text,
            2 => Self::Rst,
            3 => Self::Csv,
            4 => Self::Workbook,
            5 => Self::Pdf,
            6 => Self::Json,
            7.. => Self::Jsonl,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Markdown => "fuzz.md",
            Self::Text => "fuzz.txt",
            Self::Rst => "fuzz.rst",
            Self::Csv => "fuzz.csv",
            Self::Workbook => "fuzz.xlsx",
            Self::Pdf => "fuzz.pdf",
            Self::Json => "fuzz.json",
            Self::Jsonl => "fuzz.jsonl",
        }
    }

    fn format(self) -> Format {
        match self {
            Self::Markdown => Format::Markdown,
            Self::Text => Format::PlainText,
            Self::Rst => Format::Rst,
            Self::Csv | Self::Workbook => Format::Spreadsheet,
            Self::Pdf => Format::Pdf,
            Self::Json => Format::Json,
            Self::Jsonl => Format::Jsonl,
        }
    }
}

/// The first byte of `data` picks the file type and a small character budget;
/// the rest is the file. Every handler output keeps its documented chunk,
/// citation, claim-mark, and breadcrumb contracts.
pub fn check_format_handlers(data: &[u8]) {
    let Some((first, bytes)) = data.split_first() else {
        return;
    };
    detect_format(Path::new("input"), bytes);
    let kind = FileKind::from_byte(*first);
    let budget = usize::from(*first >> 3) * 4 + 16;
    let registry = HandlerRegistry::new(Characters, budget);
    let corpus_key = CorpusKey {
        name: "fuzz".to_string(),
        canonical_root: PathBuf::from("/fuzz"),
    };
    let file = FileRef::new(PathBuf::from(kind.file_name()));
    let ctx = PrepareCtx {
        corpus_key: &corpus_key,
        file: &file,
        mtime: Mtime(0),
        bytes,
        content_hash: String::new(),
        indexed_at_ms: 0,
    };
    let Some(Ok(prepared)) = contained(|| registry.handler(kind.format()).prepare(&ctx)) else {
        return;
    };
    check_prepared(kind, bytes, budget, &prepared);
}

fn check_prepared(kind: FileKind, bytes: &[u8], budget: usize, prepared: &PreparedFile) {
    check_chunk_shape(kind, &prepared.chunks);
    match kind {
        FileKind::Markdown => check_markdown(bytes, budget, prepared),
        FileKind::Text | FileKind::Rst | FileKind::Json | FileKind::Jsonl => {
            let lines = utf8_lines(bytes);
            for chunk in &prepared.chunks {
                assert!(
                    cited_window(&lines, chunk).contains(&chunk.text),
                    "cited lines {}..={} do not hold the chunk",
                    chunk.line_start,
                    chunk.line_end
                );
            }
        }
        FileKind::Pdf => check_pdf(bytes, &prepared.chunks),
        FileKind::Csv => check_csv(bytes, &prepared.chunks),
        FileKind::Workbook => {
            for chunk in &prepared.chunks {
                row_breadcrumb(chunk);
            }
        }
    }
}

/// Ords count up from zero, line ranges are ordered and 1-indexed, and the
/// overlap prefix is held by an earlier chunk of the same section.
fn check_chunk_shape(kind: FileKind, chunks: &[PreparedChunk]) {
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ord, index, "chunk ord is not sequential");
        match kind {
            // Stripping claim marks can empty a chunk that held only a mark.
            FileKind::Markdown => {}
            FileKind::Text
            | FileKind::Rst
            | FileKind::Csv
            | FileKind::Workbook
            | FileKind::Pdf
            | FileKind::Json
            | FileKind::Jsonl => assert!(!chunk.text.is_empty(), "empty chunk"),
        }
        assert!(
            1 <= chunk.line_start && chunk.line_start <= chunk.line_end,
            "bad line range {}..={}",
            chunk.line_start,
            chunk.line_end
        );
        assert!(
            chunk.text.is_char_boundary(chunk.overlap_bytes),
            "overlap {} is past the text or inside a character",
            chunk.overlap_bytes
        );
        if chunk.overlap_bytes == 0 {
            continue;
        }
        let held = &chunk.text[..chunk.overlap_bytes];
        let mut found = false;
        for earlier in &chunks[..index] {
            if earlier.heading_path == chunk.heading_path && earlier.text.contains(held) {
                found = true;
                break;
            }
        }
        assert!(found, "no earlier chunk of the section holds the overlap");
    }
}

fn utf8_lines(bytes: &[u8]) -> Vec<&str> {
    let Ok(body) = std::str::from_utf8(bytes) else {
        panic!("a text handler accepted non-UTF-8 bytes");
    };
    body.split_inclusive('\n').collect()
}

/// The on-disk lines that `chunk` cites.
fn cited_window(lines: &[&str], chunk: &PreparedChunk) -> String {
    assert!(
        chunk.line_end <= lines.len(),
        "cited line {} is past the file end {}",
        chunk.line_end,
        lines.len()
    );
    lines[chunk.line_start - 1..chunk.line_end].concat()
}

/// Markdown citations point at the on-disk lines, frontmatter included, that
/// hold the chunk; claim marks decode, sit in their chunk's lines, and are
/// never duplicated.
fn check_markdown(bytes: &[u8], budget: usize, prepared: &PreparedFile) {
    let lines = utf8_lines(bytes);
    let body = lines.concat();
    let (frontmatter, content, fm_lines) = split_frontmatter(&body);
    match (&prepared.frontmatter, frontmatter) {
        (Some(json), Some(_)) => {
            let decoded = serde_json::from_str::<serde_json::Value>(json);
            assert!(decoded.is_ok(), "frontmatter JSON does not decode");
        }
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => panic!("frontmatter presence changed"),
    }

    // The handler strips claim marks from each raw chunk, so the raw chunks
    // are the witness for what the cited lines must hold.
    let raw = MarkdownChunker::new(Characters, budget).chunk(content);
    assert_eq!(raw.len(), prepared.chunks.len(), "chunk count changed");
    let mut sources = Vec::new();
    for mark in extract_claim_marks(content) {
        sources.push(ClaimMark {
            line: mark.line + fm_lines,
            ..mark
        });
    }
    let mut cited = Vec::new();
    for (raw, chunk) in raw.iter().zip(&prepared.chunks) {
        assert_eq!(
            chunk.text,
            strip_claim_marks(&raw.text),
            "chunk text is not the stripped raw chunk"
        );
        assert!(
            cited_window(&lines, chunk).contains(&raw.text),
            "cited lines {}..={} do not hold the chunk",
            chunk.line_start,
            chunk.line_end
        );
        let Some(json) = &chunk.claim_marks else {
            continue;
        };
        let Ok(marks) = serde_json::from_str::<Vec<ClaimMark>>(json) else {
            panic!("claim marks JSON does not decode: {json}");
        };
        assert!(!marks.is_empty(), "claim marks JSON is an empty list");
        for mark in marks {
            assert!(
                chunk.line_start <= mark.line && mark.line <= chunk.line_end,
                "claim mark line {} is outside its chunk {}..={}",
                mark.line,
                chunk.line_start,
                chunk.line_end
            );
            cited.push(mark);
        }
    }
    for mark in &cited {
        assert!(
            occurrences(&cited, mark) <= occurrences(&sources, mark),
            "claim mark {mark:?} is duplicated or not in the file"
        );
    }
}

fn occurrences(marks: &[ClaimMark], wanted: &ClaimMark) -> usize {
    let mut count = 0;
    for mark in marks {
        if mark == wanted {
            count += 1;
        }
    }
    count
}

/// PDF chunks carry a `page:N` breadcrumb for a real page.
fn check_pdf(bytes: &[u8], chunks: &[PreparedChunk]) {
    let Ok(document) = Document::load_mem(bytes) else {
        panic!("the handler loaded a PDF that lopdf now rejects");
    };
    let pages = document.get_pages().len();
    for chunk in chunks {
        let [crumb] = chunk.heading_path.as_slice() else {
            panic!("PDF breadcrumb {:?} is not one page", chunk.heading_path);
        };
        let Some(page) = crumb.strip_prefix(PAGE_BREADCRUMB_PREFIX) else {
            panic!("PDF breadcrumb {crumb:?} is not a page");
        };
        let Ok(page) = page.parse::<usize>() else {
            panic!("PDF breadcrumb {crumb:?} has no page number");
        };
        assert!(
            1 <= page && page <= pages,
            "PDF page {page} is outside 1..={pages}"
        );
    }
}

/// CSV rows cite a line inside the file under a `csv:row-N` breadcrumb.
fn check_csv(bytes: &[u8], chunks: &[PreparedChunk]) {
    let mut lines = 1;
    for byte in bytes {
        if *byte == b'\n' {
            lines += 1;
        }
    }
    for chunk in chunks {
        let sheet = row_breadcrumb(chunk);
        assert_eq!(sheet, CSV_SHEET, "CSV breadcrumb names a sheet");
        assert!(
            chunk.line_end <= lines,
            "CSV row line {} is past the file end {lines}",
            chunk.line_end
        );
    }
}

/// A spreadsheet chunk has one `{sheet}:row-N` breadcrumb with N from 1.
/// Returns the sheet name.
fn row_breadcrumb(chunk: &PreparedChunk) -> &str {
    let [crumb] = chunk.heading_path.as_slice() else {
        panic!("row breadcrumb {:?} is not one row", chunk.heading_path);
    };
    let Some((sheet, row)) = crumb.rsplit_once(ROW_INFIX) else {
        panic!("row breadcrumb {crumb:?} has no row");
    };
    let Ok(row) = row.parse::<usize>() else {
        panic!("row breadcrumb {crumb:?} has no row number");
    };
    assert!(row >= 1, "row breadcrumb {crumb:?} counts from zero");
    sheet
}
