//! Shared test helpers for LanceDB integration suites.
//!
//! The fake embedder produces deterministic vectors keyed off blake3 of the
//! input text — same input always yields the same vector — so search results
//! are stable without needing a real fastembed model download.

#![allow(dead_code)]

pub mod daemon;

use hallouminate::adapters::lance::{EMBEDDING_DIM, PreparedChunk, PreparedFile};
use hallouminate::domain::common::Result;
use hallouminate::domain::embeddings::{EmbedBatch, EmbedRole};

pub struct StubEmbedder;

impl EmbedBatch for StubEmbedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        _role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = [0.0_f32; EMBEDDING_DIM];
                let h = blake3::hash(t.as_bytes());
                for (i, byte) in h.as_bytes().iter().enumerate() {
                    if i < EMBEDDING_DIM {
                        v[i] = (*byte as f32) / 255.0;
                    }
                }
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 1e-9 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
                v
            })
            .collect())
    }
}

pub struct ZeroEmbedder;

impl EmbedBatch for ZeroEmbedder {
    fn embed_batch(
        &mut self,
        texts: &[String],
        _role: EmbedRole,
    ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
        Ok(texts.iter().map(|_| [0.0_f32; EMBEDDING_DIM]).collect())
    }
}

/// Build a `PreparedFile` with N chunks, each carrying the given body text.
/// Embeddings are stub-generated from each chunk's text via `StubEmbedder`.
pub fn prepared_file_with_chunks(
    file_ref: &str,
    corpus: &str,
    mtime_ms: i64,
    content_hash: &str,
    chunk_texts: Vec<&str>,
) -> PreparedFile {
    let mut emb = StubEmbedder;
    let chunks: Vec<PreparedChunk> = chunk_texts
        .iter()
        .enumerate()
        .map(|(i, t)| PreparedChunk {
            ord: i,
            heading_path: vec!["section".into()],
            line_start: i + 1,
            line_end: i + 1,
            text: t.to_string(),
        })
        .collect();
    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let embeddings = emb
        .embed_batch(&texts, EmbedRole::Passage)
        .expect("stub embed");
    PreparedFile {
        file_ref: file_ref.to_string(),
        corpus: corpus.to_string(),
        mtime_ms,
        content_hash: content_hash.to_string(),
        summary: format!("summary of {file_ref}"),
        keywords: vec!["docs".into(), "test".into()],
        frontmatter: None,
        indexed_at_ms: 1_700_000_000_000,
        chunks,
        embeddings: Some(embeddings),
    }
}

/// Build a `PreparedFile` whose chunks are placeholder lorem text. Useful when
/// the test only cares about chunk count, not search behavior.
pub fn placeholder_prepared_file(file_ref: &str, n_chunks: usize) -> PreparedFile {
    let texts: Vec<&str> = (0..n_chunks)
        .map(|i| match i % 4 {
            0 => "alpha bravo charlie",
            1 => "delta echo foxtrot",
            2 => "golf hotel india",
            _ => "juliet kilo lima",
        })
        .collect();
    prepared_file_with_chunks(file_ref, "docs", 100, "deadbeef", texts)
}
