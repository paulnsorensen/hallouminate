use std::fs;

use super::chunk::PreparedFile;
use crate::common::{CorpusConfig, FileRef, HallouminateError, Mtime, Result};
use crate::corpus::blake3_bytes;

use super::format::{HandlerRegistry, PrepareCtx, detect_format, format_from_extension};

pub(super) struct WriteRequest<'a> {
    pub corpus: &'a CorpusConfig,
    pub file: &'a FileRef,
    pub mtime: Mtime,
}

/// Dispatch one file to its format handler.
///
/// Decides the format from the extension first: a known-but-unsupported
/// extension is skipped without reading or hashing the file. Only a supported
/// or extensionless name is read (extensionless names still need the bytes for
/// the magic-byte sniff in [`detect_format`]). A real IO failure on a file we
/// do read is a hard error — the caller must not silently drop it from the
/// index — and the full bytes are hashed so any edit re-indexes.
///
/// Returns `Ok(None)` — a graceful per-file skip, logged here — when the type
/// is unsupported or the handler fails to extract content. One bad file never
/// aborts the run. `Ok(Some(_))` is a prepared file ready to embed and store.
pub(super) fn prepare_file(
    req: WriteRequest<'_>,
    registry: &HandlerRegistry,
    indexed_at_ms: i64,
    bytes_override: Option<&[u8]>,
) -> Result<Option<PreparedFile>> {
    let path = req.file.as_path();
    // Skip a known-unsupported extension before any IO — no read, no hash.
    if let Some(None) = format_from_extension(path) {
        tracing::debug!(
            target: "hallouminate::indexer",
            file = %path.display(),
            "skipping file: unsupported format (no handler for its type)"
        );
        return Ok(None);
    }

    let owned_bytes;
    let bytes: &[u8] = match bytes_override {
        Some(b) => b,
        None => {
            owned_bytes = fs::read(path)?;
            &owned_bytes
        }
    };
    // Hash the full file (frontmatter included) so any edit to the block still
    // changes the content hash and triggers a re-index.
    let content_hash = blake3_bytes(bytes);

    let Some(format) = detect_format(path, bytes) else {
        // Reached only for extensionless names that sniff to an unsupported type.
        tracing::debug!(
            target: "hallouminate::indexer",
            file = %path.display(),
            "skipping file: unsupported format (no handler for its type)"
        );
        return Ok(None);
    };

    let ctx = PrepareCtx {
        corpus: req.corpus,
        file: req.file,
        mtime: req.mtime,
        bytes,
        content_hash,
        indexed_at_ms,
    };
    match registry.handler(format).prepare(&ctx) {
        Ok(pf) => Ok(Some(pf)),
        Err(e) => {
            // Extraction failure (corrupt workbook, non-UTF8 text, …) is a
            // per-file skip, not a run abort: log and continue the reindex.
            tracing::warn!(
                target: "hallouminate::indexer",
                file = %path.display(),
                error = %e,
                "skipping file: extraction failed"
            );
            Ok(None)
        }
    }
}

pub(super) fn file_ref_string(file: &FileRef) -> Result<String> {
    file.as_path()
        .to_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| {
            HallouminateError::Indexer(format!(
                "non-utf8 path cannot be stored: {}",
                file.as_path().display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use text_splitter::Characters;

    use super::*;
    use crate::indexer::HandlerRegistry;

    fn corpus() -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            ..Default::default()
        }
    }

    fn registry(budget: usize) -> HandlerRegistry {
        HandlerRegistry::new(Characters, budget)
    }

    /// Run the markdown path the way the indexer does, asserting the file was
    /// prepared (not skipped). Returns the [`PreparedFile`].
    fn prep(path: &Path, registry: &HandlerRegistry, mtime: i64, indexed_at: i64) -> PreparedFile {
        let file = FileRef::new(PathBuf::from(path));
        prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(mtime),
            },
            registry,
            indexed_at,
            None,
        )
        .expect("prepare_file must not hard-error")
        .expect("file must be prepared, not skipped")
    }

    #[test]
    fn prepare_file_reads_chunks_summary_keywords_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.md");
        fs::write(&path, "# Hello\n\nspice melange harvested on Arrakis\n").unwrap();
        let pf = prep(&path, &registry(2000), 42, 1234);
        assert_eq!(pf.corpus, "docs");
        assert_eq!(pf.mtime_ms, 42);
        assert_eq!(pf.indexed_at_ms, 1234);
        assert!(pf.file_ref.ends_with("hello.md"));
        assert!(!pf.chunks.is_empty(), "expected at least one chunk");
        assert!(
            pf.summary.contains("spice")
                || pf.summary.contains("Hello")
                || pf.summary.contains("melange"),
            "summary should reflect content: {:?}",
            pf.summary
        );
        // content_hash is a 64-char blake3 hex
        assert_eq!(pf.content_hash.len(), 64);
    }

    #[test]
    fn prepare_file_skips_non_utf8_markdown_with_no_hard_error() {
        // A non-UTF8 `.md` file routes to the markdown handler, which fails to
        // decode. Under per-file-skip semantics that is a graceful `Ok(None)`
        // (logged), NOT a run-aborting error — one bad file must not crash the
        // reindex of the rest of the corpus.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.md");
        fs::write(&path, &[0xff_u8, 0xfe, 0x00, 0x80][..]).unwrap();
        let file = FileRef::new(PathBuf::from(&path));
        let out = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(0),
            },
            &registry(2000),
            0,
            None,
        )
        .expect("non-utf8 must be a skip, not a hard error");
        assert!(out.is_none(), "non-utf8 file must be skipped (Ok(None))");
    }

    #[test]
    fn prepare_file_extracts_content_hash_that_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(2000);

        let p1 = dir.path().join("v1.md");
        fs::write(&p1, "first content").unwrap();
        let pf1 = prep(&p1, &reg, 1, 0);

        let p2 = dir.path().join("v2.md");
        fs::write(&p2, "second content").unwrap();
        let pf2 = prep(&p2, &reg, 1, 0);

        assert_ne!(pf1.content_hash, pf2.content_hash);
    }

    #[test]
    fn prepare_file_strips_frontmatter_and_offsets_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fm.md");
        // 4 frontmatter lines (1..=4); the heading lands on on-disk line 5.
        fs::write(
            &path,
            "---\nstatus: reviewed\nowner: cheese-lord\n---\n# Heading\n\nspice melange harvested on Arrakis\n",
        )
        .unwrap();
        let pf = prep(&path, &registry(2000), 0, 0);

        // Frontmatter text never leaks into chunk text, summary, or heading paths.
        for c in &pf.chunks {
            assert!(
                !c.text.contains("status:"),
                "chunk leaked frontmatter: {:?}",
                c.text
            );
            assert!(
                !c.text.contains("cheese-lord"),
                "chunk leaked owner: {:?}",
                c.text
            );
            assert!(
                !c.heading_path.iter().any(|h| h.contains("---")),
                "heading path leaked a fence: {:?}",
                c.heading_path
            );
        }
        assert!(
            !pf.summary.contains("status:"),
            "summary leaked fm: {:?}",
            pf.summary
        );

        // The first chunk maps back to the real on-disk heading line (5), not
        // line 1 of the stripped body — proves the fm_lines offset is applied.
        let first = pf.chunks.first().expect("at least one chunk");
        assert_eq!(
            first.line_start, 5,
            "line numbers must map to on-disk lines"
        );

        // The parsed frontmatter rides along as canonical JSON.
        let fm = pf.frontmatter.expect("frontmatter present");
        assert!(fm.contains(r#""status":"reviewed""#), "{fm}");
        assert!(fm.contains(r#""owner":"cheese-lord""#), "{fm}");
    }

    #[test]
    fn prepare_file_hash_covers_frontmatter_so_block_edits_reindex() {
        // The content hash is taken over the *whole file* (frontmatter
        // included), so editing only the frontmatter block still changes the
        // hash and forces a re-index — keeping the stored frontmatter JSON in
        // sync with the page. Two files with an identical body but different
        // frontmatter must therefore hash differently. If the hash were taken
        // over the stripped body instead, these would collide and a
        // frontmatter-only edit would silently leave a stale JSON column.
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(2000);

        let body = "# Heading\n\nidentical body text\n";
        let p1 = dir.path().join("draft.md");
        fs::write(&p1, format!("---\nstatus: draft\n---\n{body}")).unwrap();
        let pf1 = prep(&p1, &reg, 0, 0);

        let p2 = dir.path().join("trusted.md");
        fs::write(&p2, format!("---\nstatus: trusted\n---\n{body}")).unwrap();
        let pf2 = prep(&p2, &reg, 0, 0);

        assert_ne!(
            pf1.content_hash, pf2.content_hash,
            "a frontmatter-only edit must change the content hash to trigger re-index"
        );
    }

    #[test]
    fn prepare_file_without_frontmatter_carries_none_and_no_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.md");
        fs::write(&path, "# Heading\n\nspice melange\n").unwrap();
        let pf = prep(&path, &registry(2000), 0, 0);
        assert!(pf.frontmatter.is_none(), "no block → null column");
        // No frontmatter → zero offset; the heading stays on line 1.
        assert_eq!(pf.chunks.first().unwrap().line_start, 1);
    }

    #[test]
    fn prepare_file_with_malformed_frontmatter_indexes_verbatim_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.md");
        // A delimited block whose body is not a YAML mapping: fail-soft, so the
        // whole file (fence included) is indexed and no frontmatter is stored.
        fs::write(&path, "---\n: : : not valid : :\n---\n# Heading\n\nbody\n").unwrap();
        let pf = prep(&path, &registry(2000), 0, 0);
        assert!(pf.frontmatter.is_none(), "malformed → null column");
        assert!(!pf.chunks.is_empty(), "content still indexes");
    }

    #[test]
    fn marked_long_line_split_across_chunks_buckets_to_exactly_one_chunk() {
        // Finding B (correctness): a single line longer than the chunk budget that
        // also carries a claim mark. If `MarkdownSplitter` split that one line into
        // two chunks with inclusive line ranges that share line N (chunk A's
        // `line_end` == chunk B's `line_start`), the inclusive per-chunk filter
        // would bucket the mark into BOTH chunks and surface it twice in `ground`.
        // This forces the split and asserts the mark lands in exactly one chunk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.md");
        // One physical line, no internal newlines, far over an 8-char budget, with
        // a claim mark at its end (so the mark anchors to line 1).
        let long_line = "word ".repeat(40);
        fs::write(&path, format!("{long_line}<!--claim:confirmed-->\n")).unwrap();
        let pf = prep(&path, &registry(8), 0, 0);

        // The tiny budget must actually split the line into multiple chunks; the
        // mark on line 1 must then be bucketed into exactly one of them.
        assert!(
            pf.chunks.len() >= 2,
            "tiny budget must split the long line into multiple chunks, got {}",
            pf.chunks.len()
        );
        let carrying = pf.chunks.iter().filter(|c| c.claim_marks.is_some()).count();
        assert_eq!(
            carrying, 1,
            "a mark on a split line must surface in exactly one chunk, not be \
             double-bucketed across the inclusive boundary; chunks={:#?}",
            pf.chunks
        );
    }
}
