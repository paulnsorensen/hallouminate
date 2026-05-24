use std::fs;

use crate::adapters::lance::{PreparedChunk, PreparedFile};
use crate::domain::common::{CorpusConfig, FileRef, HallouminateError, Mtime, Result};
use crate::domain::corpus::{blake3_bytes, extract_keywords, extract_summary, CorpusChunker};

pub(super) struct WriteRequest<'a> {
    pub corpus: &'a CorpusConfig,
    pub file: &'a FileRef,
    pub mtime: Mtime,
}

/// Read a file from disk, chunk it, extract metadata. Returns a
/// `PreparedFile` with `embeddings: None` — the caller (`apply`) fills in
/// `Some(vectors)` in ON mode, or leaves `None` to write null embeddings in
/// OFF mode, before passing the batch to `LanceStore::apply_batch`.
pub(super) fn prepare_file(
    req: WriteRequest<'_>,
    chunker: &dyn CorpusChunker,
    indexed_at_ms: i64,
) -> Result<PreparedFile> {
    let path = req.file.as_path();
    let bytes = fs::read(path)?;
    let hash = blake3_bytes(&bytes);
    let body = String::from_utf8(bytes).map_err(|e| {
        HallouminateError::Indexer(format!("non-utf8 file {}: {e}", path.display()))
    })?;
    let chunks_raw = chunker.chunk_text(&body);
    let fallback = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let summary = extract_summary(&body, &fallback);
    let keywords = extract_keywords(&body);
    let file_ref_str = file_ref_string(req.file)?;
    let chunks: Vec<PreparedChunk> = chunks_raw
        .into_iter()
        .map(|c| PreparedChunk {
            ord: c.ord,
            heading_path: c.heading_path,
            line_start: c.line_start,
            line_end: c.line_end,
            text: c.text,
        })
        .collect();
    Ok(PreparedFile {
        file_ref: file_ref_str,
        corpus: req.corpus.name.clone(),
        mtime_ms: req.mtime.0,
        content_hash: hash,
        summary,
        keywords,
        indexed_at_ms,
        chunks,
        embeddings: None,
    })
}

pub(super) fn file_ref_string(file: &FileRef) -> Result<String> {
    file.as_path()
        .to_str()
        .map(|s| s.to_string())
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
}
