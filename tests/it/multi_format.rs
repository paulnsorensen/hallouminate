//! Phase 1 multi-format ingestion acceptance tests.
//!
//! Exercises the format-dispatch pipeline end to end through `index_corpus`:
//! plain text, CSV, xlsx, and ods all index and ground; unsupported and
//! corrupt in-scope files are skipped (logged) without aborting the run; and a
//! representative markdown fixture indexes byte-identically to the pre-dispatch
//! pipeline (golden-snapshot regression).
//!
//! Spreadsheet fixtures are generated in-process (no committed binaries):
//! rust_xlsxwriter writes a real .xlsx; .ods is a hand-built ZIP. No pure-Rust
//! .xls writer exists, so the .xls path is covered by a detection/routing unit
//! test instead of a generated binary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use hallouminate::adapters::lance::LanceStore;
use hallouminate::domain::common::{CorpusConfig, FileRef, Mtime};
use hallouminate::domain::indexer::{
    Format, HandlerRegistry, PrepareCtx, detect_format, index_corpus,
};
use hallouminate::domain::search::search_with_ripgrep;
use text_splitter::Characters;

use crate::common::StubEmbedder;

const MODEL: &str = "BAAI/bge-small-en-v1.5";

fn corpus(dir: &Path, name: &str, globs: &[&str]) -> CorpusConfig {
    CorpusConfig {
        name: name.into(),
        paths: vec![dir.to_string_lossy().into_owned()],
        globs: globs.iter().map(|g| (*g).to_string()).collect(),
        exclude: vec![],
        global: false,
    }
}

async fn open_store(dir: &Path) -> LanceStore {
    LanceStore::open_or_create(dir, MODEL, false, true, Some(Box::new(StubEmbedder)))
        .await
        .expect("open store")
}

// ── Plain text ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn plain_text_corpus_indexes_and_grounds() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    fs::write(
        corpus_dir.path().join("notes.txt"),
        "The xenoblat protocol coordinates the harvest.\nA second paragraph about the same xenoblat machinery.\n",
    )
    .unwrap();
    // A `.text` file is also plain text and must index.
    fs::write(
        corpus_dir.path().join("more.text"),
        "Standalone quibblefax line of plain prose.\n",
    )
    .unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.txt", "**/*.text"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index plain text");
    assert_eq!(stats.files_upserted, 2, "both text files indexed");
    assert!(stats.chunks_inserted >= 2);

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "xenoblat", 5)
        .await
        .expect("search");
    assert!(
        hits.iter().any(|h| h.file_ref.ends_with("notes.txt")),
        "plain-text chunk must be retrievable: {:?}",
        hits.iter().map(|h| h.file_ref.clone()).collect::<Vec<_>>()
    );
    // Plain text carries no heading breadcrumb.
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("notes.txt"))
        .unwrap();
    assert!(
        hit.heading_path.is_empty(),
        "plain text must have no heading_path: {:?}",
        hit.heading_path
    );
}

// ── CSV ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn csv_indexes_one_self_describing_chunk_per_row() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    fs::write(
        corpus_dir.path().join("fruit.csv"),
        "name,qty,note\napple,10,crisp wozzlefruit\nbanana,5,ripe yellow\n",
    )
    .unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.csv"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index csv");
    assert_eq!(stats.files_upserted, 1);
    // Two data rows → two chunks (header row is not a chunk).
    assert_eq!(stats.chunks_inserted, 2, "one chunk per data row");

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "wozzlefruit", 5)
        .await
        .expect("search csv");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("fruit.csv"))
        .expect("csv row must be retrievable");

    // Self-describing: every column rendered inline as `col: val`.
    assert!(hit.text.contains("name: apple"), "row text: {:?}", hit.text);
    assert!(hit.text.contains("qty: 10"), "row text: {:?}", hit.text);
    assert!(
        hit.text.contains("note: crisp wozzlefruit"),
        "row text: {:?}",
        hit.text
    );
    // Breadcrumb is `csv:row-N` (1-based data-row index).
    assert_eq!(
        hit.heading_path,
        vec!["csv:row-1".to_string()],
        "first data row breadcrumb"
    );
    // line_range points at the true on-disk line: header is line 1, so the
    // first data row is line 2.
    assert_eq!(
        (hit.line_start, hit.line_end),
        (2, 2),
        "csv line_range is the on-disk line, not the row ordinal"
    );
}

// ── XLSX ──────────────────────────────────────────────────────────────────

fn write_xlsx(path: &Path) {
    use rust_xlsxwriter::Workbook;
    let mut wb = Workbook::new();
    let sheet = wb.add_worksheet();
    sheet.set_name("Inventory").unwrap();
    sheet.write_row(0, 0, ["name", "qty", "note"]).unwrap();
    sheet
        .write_row(1, 0, ["widget", "42", "shiny grobblet"])
        .unwrap();
    sheet
        .write_row(2, 0, ["gadget", "7", "plain sproket"])
        .unwrap();
    wb.save(path).unwrap();
}

#[tokio::test]
async fn xlsx_indexes_each_sheet_row_with_sheet_and_row_metadata() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    write_xlsx(&corpus_dir.path().join("inv.xlsx"));

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.xlsx"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index xlsx");
    assert_eq!(stats.files_upserted, 1);
    assert_eq!(stats.chunks_inserted, 2, "two data rows → two chunks");

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "grobblet", 5)
        .await
        .expect("search xlsx");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("inv.xlsx"))
        .expect("xlsx row retrievable");
    assert!(hit.text.contains("name: widget"), "{:?}", hit.text);
    assert!(hit.text.contains("note: shiny grobblet"), "{:?}", hit.text);
    // Breadcrumb carries the real sheet name and row index.
    assert_eq!(hit.heading_path, vec!["Inventory:row-1".to_string()]);
}

// ── ODS ───────────────────────────────────────────────────────────────────

// Single-line table structure on purpose: calamine's ODS reader expects a
// `table-cell` element immediately inside `table-row` and chokes on the
// whitespace text nodes that pretty-printed indentation introduces.
const ODS_CONTENT_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="Catalog"><table:table-row><table:table-cell office:value-type="string"><text:p>name</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>note</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>flange</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>rare snibblet item</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

const ODS_MANIFEST_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;

fn write_ods(path: &Path) {
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    let file = fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // `mimetype` must be the first entry and stored uncompressed (ODS spec).
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    // calamine reads META-INF/manifest.xml first (password check) and errors if absent.
    zip.start_file("META-INF/manifest.xml", deflated).unwrap();
    zip.write_all(ODS_MANIFEST_XML).unwrap();
    zip.start_file("content.xml", deflated).unwrap();
    zip.write_all(ODS_CONTENT_XML).unwrap();
    zip.finish().unwrap();
}

#[tokio::test]
async fn ods_indexes_rows_with_sheet_and_row_metadata() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    write_ods(&corpus_dir.path().join("cat.ods"));

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.ods"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index ods");
    assert_eq!(stats.files_upserted, 1);
    assert_eq!(stats.chunks_inserted, 1, "one data row → one chunk");

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "snibblet", 5)
        .await
        .expect("search ods");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("cat.ods"))
        .expect("ods row retrievable");
    assert!(hit.text.contains("name: flange"), "{:?}", hit.text);
    assert!(
        hit.text.contains("note: rare snibblet item"),
        "{:?}",
        hit.text
    );
    assert_eq!(hit.heading_path, vec!["Catalog:row-1".to_string()]);
}

// ── Graceful per-file skips ────────────────────────────────────────────────

#[tokio::test]
async fn unsupported_extension_is_skipped_and_rest_of_corpus_indexes() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    // One supported file and one unsupported type, both swept by a broad glob.
    fs::write(
        corpus_dir.path().join("good.md"),
        "# Good\n\nThe flibberwidget endures.\n",
    )
    .unwrap();
    fs::write(
        corpus_dir.path().join("photo.png"),
        [0x89, 0x50, 0x4e, 0x47],
    )
    .unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index must not abort on an unsupported file");
    assert_eq!(stats.files_upserted, 1, "the markdown file still indexes");
    assert_eq!(
        stats.files_skipped_unreadable, 1,
        "the unsupported .png is skipped as unreadable, not indexed"
    );
    assert_eq!(
        stats.files_skipped_empty, 0,
        "an unsupported type must not be miscounted as truncate-to-empty"
    );

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "flibberwidget", 5)
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.file_ref.ends_with("good.md")));
}

#[tokio::test]
async fn corrupt_xlsx_is_skipped_without_panic_and_indexer_continues() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    // A `.xlsx` (in-scope, supported extension) whose bytes are not a valid
    // workbook: the handler's extraction fails and the file is skipped.
    fs::write(
        corpus_dir.path().join("broken.xlsx"),
        b"this is definitely not a real xlsx zip",
    )
    .unwrap();
    fs::write(
        corpus_dir.path().join("ok.csv"),
        "name,note\nbolt,sturdy zonkbolt fastener\n",
    )
    .unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.xlsx", "**/*.csv"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("a corrupt in-scope file must skip, not error the run");
    assert_eq!(stats.files_upserted, 1, "the valid csv still indexes");
    assert_eq!(
        stats.files_skipped_unreadable, 1,
        "the corrupt xlsx is skipped as unreadable"
    );
    assert_eq!(
        stats.files_skipped_empty, 0,
        "an extraction failure must not be miscounted as truncate-to-empty"
    );

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "zonkbolt", 5)
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.file_ref.ends_with("ok.csv")));
}

#[tokio::test]
async fn bulk_reindex_retains_last_good_rows_when_file_becomes_unreadable() {
    // apply.rs's unreadable-skip branch must never evict a previously-indexed
    // file's rows on re-index (distinct from the truncate-to-empty eviction
    // path) — exercised here through the bulk `index_corpus` path rather than
    // the single-file daemon dispatch path.
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    let file = corpus_dir.path().join("data.csv");
    fs::write(&file, "name,note\nbolt,sturdy fastener\n").unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.csv"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats1 = index_corpus(&corpus, &store, &registry)
        .await
        .expect("first index of a valid csv must succeed");
    assert_eq!(stats1.files_upserted, 1, "the valid csv indexes");
    let rows_before = store.count_rows().await.unwrap();
    assert!(rows_before > 0, "the valid csv must produce indexed rows");

    // Bump mtime forward so the plan re-visits the file, then corrupt it.
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    fs::write(&file, b"\xff\xfe\x00 not,a valid\x00 csv").unwrap();
    std::fs::File::open(&file)
        .unwrap()
        .set_modified(bumped)
        .unwrap();

    let stats2 = index_corpus(&corpus, &store, &registry)
        .await
        .expect("a corrupt re-extraction must not hard-error");
    assert_eq!(
        stats2.files_skipped_unreadable, 1,
        "a corrupt re-extraction is an unreadable skip"
    );
    assert_eq!(
        stats2.files_deleted, 0,
        "a present-but-unreadable file must NOT be evicted from the index"
    );
    let rows_after = store.count_rows().await.unwrap();
    assert_eq!(
        rows_after, rows_before,
        "last-good rows must survive a transient parse failure on re-index"
    );
}

// ── Detection / routing unit checks ────────────────────────────────────────

#[test]
fn detect_format_keys_on_extension_for_every_phase1_type() {
    let cases: &[(&str, Format)] = &[
        ("a.md", Format::Markdown),
        ("a.markdown", Format::Markdown),
        ("a.txt", Format::PlainText),
        ("a.text", Format::PlainText),
        ("a.csv", Format::Spreadsheet),
        ("a.xlsx", Format::Spreadsheet),
        ("a.xls", Format::Spreadsheet),
        ("a.ods", Format::Spreadsheet),
        ("A.MD", Format::Markdown), // case-insensitive extension
        ("A.CSV", Format::Spreadsheet),
    ];
    for (name, want) in cases {
        let got = detect_format(Path::new(name), b"irrelevant content");
        assert_eq!(got, Some(*want), "{name} should detect as {want:?}");
    }
    // A known-but-unsupported extension is decisively skipped (no magic-byte
    // fallthrough that might mislabel a different ZIP/OOXML container).
    assert_eq!(detect_format(Path::new("a.docx"), b"PK\x03\x04"), None);
    assert_eq!(detect_format(Path::new("a.png"), &[0x89, 0x50]), None);
}

#[test]
fn detect_format_falls_back_to_magic_bytes_for_extensionless_files() {
    // No extension → sniff. UTF-8 text sniffs as plain text.
    assert_eq!(
        detect_format(Path::new("README"), b"just some plain ascii text\n"),
        Some(Format::PlainText)
    );
    // Random binary with no recognized signature is unsupported.
    assert_eq!(
        detect_format(Path::new("blob"), &[0x00, 0x01, 0x02, 0xff, 0xfe]),
        None
    );
}

// ── Markdown golden-snapshot regression ────────────────────────────────────

/// The markdown handler must produce output byte-identical to the pre-dispatch
/// pipeline. This pins the full prepared shape (chunk count, text, heading_path,
/// line ranges, claim marks, frontmatter, summary) for a representative fixture
/// with frontmatter + headings + a claim mark, so any drift in the markdown path
/// fails loudly.
#[test]
fn markdown_handler_golden_snapshot_is_byte_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("page.md");
    // 3 frontmatter lines (1..=3) → fm offset 3. Heading on line 4, body token
    // on line 6, claim mark on line 6.
    let body = "---\nstatus: reviewed\n---\n# Spice\n\nThe melange flows.<!--claim:confirmed-->\n";
    fs::write(&path, body).unwrap();

    let registry = HandlerRegistry::new(Characters, 2000);
    let file = FileRef::new(PathBuf::from(&path));
    let bytes = fs::read(&path).unwrap();
    let detected = detect_format(&path, &bytes);
    assert_eq!(detected, Some(Format::Markdown));

    let ctx = PrepareCtx {
        corpus: &corpus(dir.path(), "docs", &["**/*.md"]),
        file: &file,
        mtime: Mtime(7),
        bytes: &bytes,
        content_hash: "deadbeef".into(),
        indexed_at_ms: 99,
    };
    let pf = registry
        .handler(Format::Markdown)
        .prepare(&ctx)
        .expect("markdown prepare");

    // Exactly one chunk for this short page.
    assert_eq!(pf.chunks.len(), 1, "one chunk: {:#?}", pf.chunks);
    let c = &pf.chunks[0];
    // heading_path is the H1, verbatim.
    assert_eq!(c.heading_path, vec!["Spice".to_string()]);
    // Claim comment stripped from the retrieval text; line count preserved.
    assert!(
        !c.text.contains("<!--claim:"),
        "claim comment must be stripped: {:?}",
        c.text
    );
    assert!(c.text.contains("The melange flows."), "{:?}", c.text);
    // Frontmatter stripped from the body but parsed into the JSON column.
    assert!(!c.text.contains("status:"), "fm leaked: {:?}", c.text);
    let fm = pf.frontmatter.as_deref().expect("frontmatter present");
    assert!(
        fm.contains(r#""status":"reviewed""#),
        "frontmatter parsed to canonical JSON: {fm}"
    );
    // The claim mark rides on the chunk at its on-disk line (6 = 3 fm + 3 body).
    assert!(
        c.claim_marks
            .as_deref()
            .is_some_and(|j| j.contains("confirmed") && j.contains("\"line\":6")),
        "claim mark must anchor to on-disk line 6: {:?}",
        c.claim_marks
    );
    // Line range maps to on-disk lines (heading on 4, body on 6).
    assert_eq!(c.line_start, 4, "chunk starts at on-disk heading line");
    assert_eq!(c.line_end, 6, "chunk ends at on-disk body line");
    // Metadata frame is threaded from the ctx unchanged.
    assert_eq!(pf.mtime_ms, 7);
    assert_eq!(pf.indexed_at_ms, 99);
    assert_eq!(pf.content_hash, "deadbeef");
    assert!(pf.summary.contains("melange") || pf.summary.contains("Spice"));
}

// ── Hardening: spreadsheet cell shape boundaries ────────────────────────────

/// A numeric spreadsheet cell must render as the bare number (`qty: 42`), not
/// the `f64` debug form (`42.0`) or the `Data::Float(..)` enum form. The earlier
/// xlsx test wrote every cell as a string, so the `Data::Float`/`Data::Int`
/// rendering path in `cell_to_string` was never exercised; this pins it.
#[tokio::test]
async fn xlsx_numeric_cell_renders_as_bare_number_not_float_debug() {
    use rust_xlsxwriter::Workbook;
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    let path = corpus_dir.path().join("nums.xlsx");
    {
        let mut wb = Workbook::new();
        let sheet = wb.add_worksheet();
        sheet.set_name("Stock").unwrap();
        sheet.write_string(0, 0, "item").unwrap();
        sheet.write_string(0, 1, "qty").unwrap();
        sheet.write_string(1, 0, "grommit numwidget").unwrap();
        // A real numeric cell (not a string) → calamine yields Data::Float(42.0).
        sheet.write_number(1, 1, 42).unwrap();
        wb.save(&path).unwrap();
    }

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.xlsx"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index numeric xlsx");
    assert_eq!(stats.chunks_inserted, 1);

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "numwidget", 5)
        .await
        .expect("search numeric xlsx");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("nums.xlsx"))
        .expect("numeric xlsx row retrievable");
    assert!(
        hit.text.contains("qty: 42"),
        "numeric cell must render as a bare integer: {:?}",
        hit.text
    );
    assert!(
        !hit.text.contains("42.0") && !hit.text.contains("Float"),
        "numeric cell must not leak the f64 debug or enum form: {:?}",
        hit.text
    );
}

/// A workbook with more than one sheet must emit chunks for every sheet, with the
/// row index restarting at 1 per sheet and the breadcrumb carrying each sheet's
/// own name. The single-sheet test could not catch a regression that, say, kept a
/// running row counter across sheets or only read the first sheet.
#[tokio::test]
async fn xlsx_multi_sheet_indexes_every_sheet_with_per_sheet_row_index() {
    use rust_xlsxwriter::Workbook;
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    let path = corpus_dir.path().join("multi.xlsx");
    {
        let mut wb = Workbook::new();
        let alpha = wb.add_worksheet();
        alpha.set_name("Alpha").unwrap();
        alpha.write_row(0, 0, ["name", "note"]).unwrap();
        alpha
            .write_row(1, 0, ["a-one", "first alphathing"])
            .unwrap();
        let beta = wb.add_worksheet();
        beta.set_name("Beta").unwrap();
        beta.write_row(0, 0, ["name", "note"]).unwrap();
        beta.write_row(1, 0, ["b-one", "first betathing"]).unwrap();
        beta.write_row(2, 0, ["b-two", "second betathing"]).unwrap();
        wb.save(&path).unwrap();
    }

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.xlsx"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index multi-sheet xlsx");
    // Alpha: 1 data row; Beta: 2 data rows → 3 chunks total.
    assert_eq!(stats.chunks_inserted, 3, "every sheet's data rows index");

    // The first data row of the SECOND sheet must carry `Beta:row-1`, proving the
    // row index resets per sheet rather than running 1,2,3 across the workbook.
    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "first betathing", 10)
        .await
        .expect("search beta sheet");
    let beta_hit = hits
        .iter()
        .find(|h| h.text.contains("first betathing"))
        .expect("beta sheet row retrievable");
    assert_eq!(
        beta_hit.heading_path,
        vec!["Beta:row-1".to_string()],
        "second sheet's first data row must be row-1 with the Beta breadcrumb"
    );

    let ahits = search_with_ripgrep(&store, "docs", &corpus.paths, "first alphathing", 10)
        .await
        .expect("search alpha sheet");
    assert!(
        ahits
            .iter()
            .any(|h| h.heading_path == vec!["Alpha:row-1".to_string()]),
        "first sheet must contribute an Alpha:row-1 chunk: {:?}",
        ahits
            .iter()
            .map(|h| h.heading_path.clone())
            .collect::<Vec<_>>()
    );
}

/// A CSV row with more cells than the header has must NOT silently drop the
/// extra cell: the handler synthesizes a positional `col_N` key (spec:
/// "no cell is silently dropped"). A blank cell inside a row is omitted from the
/// `col: val` rendering, but the populated cells around it still render.
#[tokio::test]
async fn csv_ragged_row_uses_col_n_fallback_and_omits_blank_cells() {
    let store_dir = tempfile::tempdir().unwrap();
    let corpus_dir = tempfile::tempdir().unwrap();
    // Header has 2 columns; the data row has 3 cells (the 3rd has no header) and
    // a blank middle cell.
    fs::write(
        corpus_dir.path().join("ragged.csv"),
        "name,note\nsprocket,,extra raggedcell value\n",
    )
    .unwrap();

    let corpus = corpus(corpus_dir.path(), "docs", &["**/*.csv"]);
    let store = open_store(store_dir.path()).await;
    let registry = HandlerRegistry::new(Characters, 1500);

    let stats = index_corpus(&corpus, &store, &registry)
        .await
        .expect("index ragged csv");
    assert_eq!(stats.chunks_inserted, 1, "one data row → one chunk");

    let hits = search_with_ripgrep(&store, "docs", &corpus.paths, "raggedcell", 5)
        .await
        .expect("search ragged csv");
    let hit = hits
        .iter()
        .find(|h| h.file_ref.ends_with("ragged.csv"))
        .expect("ragged csv row retrievable");
    // Named column survives.
    assert!(hit.text.contains("name: sprocket"), "{:?}", hit.text);
    // The header-less third cell is preserved under a positional key, not dropped.
    assert!(
        hit.text.contains("col_3: extra raggedcell value"),
        "header-less cell must fall back to a positional col_N key: {:?}",
        hit.text
    );
    // The blank middle cell (`note`) is omitted entirely — no empty `note:` line.
    assert!(
        !hit.text.contains("note:"),
        "a blank cell must be omitted, not rendered as an empty value: {:?}",
        hit.text
    );
}

// ── Hardening: detection deviations (cook-flagged) ──────────────────────────

/// Cook-flagged deviation: `file-format` 0.29 has no CSV variant, so an
/// EXTENSIONLESS csv-shaped file sniffs as plain text (not Spreadsheet) and
/// routes to the text handler — documented graceful degradation. This pins that
/// contract so a future detection change can't quietly reclassify it.
#[test]
fn detect_format_extensionless_csv_shaped_content_is_plain_text_not_spreadsheet() {
    let csv_shaped = b"name,qty,note\napple,10,crisp\nbanana,5,ripe\n";
    assert_eq!(
        detect_format(Path::new("data_no_ext"), csv_shaped),
        Some(Format::PlainText),
        "an extensionless CSV has no magic signature and must degrade to PlainText"
    );
}

/// The magic-byte fallback must classify a real binary spreadsheet container
/// (OOXML xlsx) as `Spreadsheet` even with no extension. The existing detection
/// tests only covered the PlainText and unsupported magic branches; this pins
/// the Spreadsheet magic branch.
#[test]
fn detect_format_extensionless_xlsx_bytes_sniff_as_spreadsheet() {
    use rust_xlsxwriter::Workbook;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.xlsx");
    {
        let mut wb = Workbook::new();
        let sheet = wb.add_worksheet();
        sheet.write_row(0, 0, ["a", "b"]).unwrap();
        sheet.write_row(1, 0, ["1", "2"]).unwrap();
        wb.save(&path).unwrap();
    }
    let bytes = fs::read(&path).unwrap();
    // No extension on the lookup path → forces the magic-byte branch.
    assert_eq!(
        detect_format(Path::new("workbook_no_ext"), &bytes),
        Some(Format::Spreadsheet),
        "a real OOXML container must sniff as Spreadsheet via magic bytes"
    );
}
