use std::fs;

use crate::adapters::lance::{PreparedChunk, PreparedFile};
use crate::domain::common::{CorpusConfig, FileRef, HallouminateError, Mtime, Result};
use crate::domain::corpus::{
    CorpusChunker, Frontmatter, blake3_bytes, extract_keywords, extract_summary, split_frontmatter,
};

pub(super) struct WriteRequest<'a> {
    pub corpus: &'a CorpusConfig,
    pub file: &'a FileRef,
    pub mtime: Mtime,
}

pub(super) fn prepare_file(
    req: WriteRequest<'_>,
    chunker: &dyn CorpusChunker,
    indexed_at_ms: i64,
) -> Result<PreparedFile> {
    let path = req.file.as_path();
    let bytes = fs::read(path)?;
    // Hash the full file (frontmatter included) so any edit to the block still
    // changes the content hash and triggers a re-index.
    let hash = blake3_bytes(&bytes);
    let body = String::from_utf8(bytes).map_err(|e| {
        HallouminateError::Indexer(format!("non-utf8 file {}: {e}", path.display()))
    })?;
    // Strip an optional leading frontmatter block before every text pass so it
    // never pollutes chunks, summary, or keywords. `fm_lines` is added back to
    // each chunk's line numbers so citations point at the real on-disk lines.
    let (frontmatter, content, fm_lines) = split_frontmatter(&body);
    let chunks_raw = chunker.chunk_text(content);
    let fallback = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let summary = extract_summary(content, &fallback);
    let keywords = extract_keywords(content);
    let file_ref_str = file_ref_string(req.file)?;
    let mut chunks: Vec<PreparedChunk> = Vec::with_capacity(chunks_raw.len());
    for c in chunks_raw {
        chunks.push(PreparedChunk {
            ord: c.ord,
            heading_path: c.heading_path,
            line_start: c.line_start + fm_lines,
            line_end: c.line_end + fm_lines,
            text: c.text,
        });
    }
    Ok(PreparedFile {
        file_ref: file_ref_str,
        corpus: req.corpus.name.clone(),
        mtime_ms: req.mtime.0,
        content_hash: hash,
        summary,
        keywords,
        frontmatter: frontmatter.as_ref().map(Frontmatter::to_canonical_json),
        indexed_at_ms,
        chunks,
        embeddings: None,
    })
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
    use std::path::PathBuf;

    use text_splitter::Characters;

    use super::*;
    use crate::domain::corpus::MarkdownChunker;

    fn corpus() -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            ..Default::default()
        }
    }

    #[test]
    fn prepare_file_reads_chunks_summary_keywords_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.md");
        fs::write(&path, "# Hello\n\nspice melange harvested on Arrakis\n").unwrap();
        let chunker = MarkdownChunker::new(Characters, 2000);
        let file = FileRef::new(PathBuf::from(&path));
        let pf = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(42),
            },
            &chunker,
            1234,
        )
        .expect("prepare_file");
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
        // embeddings start as None; apply.rs fills in Some(..) in ON mode.
        assert!(pf.embeddings.is_none());
        // content_hash is a 64-char blake3 hex
        assert_eq!(pf.content_hash.len(), 64);
    }

    #[test]
    fn prepare_file_errors_on_non_utf8_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.md");
        fs::write(&path, &[0xff_u8, 0xfe, 0x00, 0x80][..]).unwrap();
        let chunker = MarkdownChunker::new(Characters, 2000);
        let file = FileRef::new(PathBuf::from(&path));
        let err = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .expect_err("must reject non-utf8");
        let msg = err.to_string();
        assert!(msg.contains("non-utf8"), "{msg}");
    }

    #[test]
    fn prepare_file_extracts_content_hash_that_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let chunker = MarkdownChunker::new(Characters, 2000);

        let p1 = dir.path().join("v1.md");
        fs::write(&p1, "first content").unwrap();
        let pf1 = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &FileRef::new(PathBuf::from(&p1)),
                mtime: Mtime(1),
            },
            &chunker,
            0,
        )
        .unwrap();

        let p2 = dir.path().join("v2.md");
        fs::write(&p2, "second content").unwrap();
        let pf2 = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &FileRef::new(PathBuf::from(&p2)),
                mtime: Mtime(1),
            },
            &chunker,
            0,
        )
        .unwrap();

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
        let chunker = MarkdownChunker::new(Characters, 2000);
        let file = FileRef::new(PathBuf::from(&path));
        let pf = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .unwrap();

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
        let chunker = MarkdownChunker::new(Characters, 2000);

        let body = "# Heading\n\nidentical body text\n";
        let p1 = dir.path().join("draft.md");
        fs::write(&p1, format!("---\nstatus: draft\n---\n{body}")).unwrap();
        let pf1 = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &FileRef::new(PathBuf::from(&p1)),
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .unwrap();

        let p2 = dir.path().join("trusted.md");
        fs::write(&p2, format!("---\nstatus: trusted\n---\n{body}")).unwrap();
        let pf2 = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &FileRef::new(PathBuf::from(&p2)),
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .unwrap();

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
        let chunker = MarkdownChunker::new(Characters, 2000);
        let file = FileRef::new(PathBuf::from(&path));
        let pf = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .unwrap();
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
        let chunker = MarkdownChunker::new(Characters, 2000);
        let file = FileRef::new(PathBuf::from(&path));
        let pf = prepare_file(
            WriteRequest {
                corpus: &corpus(),
                file: &file,
                mtime: Mtime(0),
            },
            &chunker,
            0,
        )
        .expect("malformed frontmatter must not error the index run");
        assert!(pf.frontmatter.is_none(), "malformed → null column");
        assert!(!pf.chunks.is_empty(), "content still indexes");
    }
}
