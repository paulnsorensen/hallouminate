use std::fs;

use rusqlite::params;

use crate::adapters::sqlite::{
    delete_chunks_for_file, delete_vec_for_chunk, insert_chunk, insert_vec, upsert_file, DbConn,
    FileRow, NewChunk, NewFile,
};
use crate::domain::common::{CorpusConfig, FileRef, HallouminateError, Mtime, Result};
use crate::domain::corpus::{
    blake3_file, chunk_markdown, extract_keywords, extract_summary, Chunk,
};
use crate::domain::embeddings::EmbedBatch;

use super::apply::ApplyStats;

pub(super) struct WriteRequest<'a> {
    pub corpus: &'a CorpusConfig,
    pub file: &'a FileRef,
    pub mtime: Mtime,
    pub prior: Option<FileRow>,
}

pub(super) fn write_file_chunks(
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
    req: WriteRequest<'_>,
    stats: &mut ApplyStats,
) -> Result<()> {
    let path = req.file.as_path();
    let body = fs::read_to_string(path)?;
    let hash = blake3_file(path)?;
    let chunks = chunk_markdown(&body);
    let fallback = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let summary = extract_summary(&body, &fallback);
    let keywords_json = serde_json::to_string(&extract_keywords(&body))
        .map_err(|e| HallouminateError::Indexer(format!("keywords json: {e}")))?;
    let file_ref_str = file_ref_string(req.file)?;
    let file_id = upsert_file(
        conn,
        &NewFile {
            file_ref: &file_ref_str,
            corpus: &req.corpus.name,
            mtime_ms: req.mtime.0,
            content_hash: &hash,
            summary: Some(&summary),
            keywords_json: &keywords_json,
            indexed_at_ms: chrono::Utc::now().timestamp_millis(),
        },
    )?;
    if req.prior.is_some() {
        purge_vecs_for_file(conn, file_id)?;
        delete_chunks_for_file(conn, file_id)?;
    }
    insert_all_chunks(conn, embedder, file_id, &chunks, stats)
}

fn insert_all_chunks(
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
    file_id: i64,
    chunks: &[Chunk],
    stats: &mut ApplyStats,
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let vectors = embedder.embed_batch(&texts)?;
    if vectors.len() != chunks.len() {
        return Err(HallouminateError::Indexer(format!(
            "embedder returned {} vectors for {} chunks",
            vectors.len(),
            chunks.len()
        )));
    }
    for (chunk, vector) in chunks.iter().zip(vectors.iter()) {
        let heading_path_json = serde_json::to_string(&chunk.heading_path)
            .map_err(|e| HallouminateError::Indexer(format!("heading_path json: {e}")))?;
        let chunk_id = insert_chunk(
            conn,
            &NewChunk {
                file_id,
                ord: chunk.ord as i64,
                heading_path_json: &heading_path_json,
                line_start: chunk.line_start as i64,
                line_end: chunk.line_end as i64,
                text: &chunk.text,
            },
        )?;
        insert_vec(conn, chunk_id, vector)?;
        stats.chunks_inserted += 1;
        stats.embeddings_inserted += 1;
    }
    Ok(())
}

pub(super) fn purge_vecs_for_file(conn: &DbConn, file_id: i64) -> Result<()> {
    let raw = conn.raw();
    let mut stmt = raw.prepare("SELECT chunk_id FROM chunks WHERE file_id = ?1")?;
    let ids: Vec<i64> = stmt
        .query_map(params![file_id], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        delete_vec_for_chunk(conn, id)?;
    }
    Ok(())
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
