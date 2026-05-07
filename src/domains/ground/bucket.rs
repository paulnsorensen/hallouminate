use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::domains::common::{HallouminateError, Result};
use crate::domains::corpus::make_snippet;
use crate::domains::search::FusedHit;

use super::types::{DocChunk, DocFile};

pub(super) struct ChunkRow {
    pub chunk_id: i64,
    pub file_id: i64,
    pub file_ref: String,
    pub corpus: String,
    pub mtime_ms: i64,
    pub summary: Option<String>,
    pub keywords_json: String,
    pub heading_path_json: String,
    pub line_start: i64,
    pub line_end: i64,
    pub text: String,
}

pub(super) fn build_docs(
    conn: &Connection,
    fused: &[FusedHit],
    top_files: usize,
    chunks_per_file: usize,
) -> Result<BTreeMap<String, DocFile>> {
    let mut buckets: HashMap<i64, FileBucket> = HashMap::new();
    for hit in fused {
        let row = match fetch_chunk_row(conn, hit.chunk_id.0)? {
            Some(r) => r,
            None => continue,
        };
        buckets
            .entry(row.file_id)
            .or_insert_with(|| FileBucket::new(&row))
            .push(*hit, row);
    }
    let mut files: Vec<FileBucket> = buckets.into_values().collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_ref.cmp(&b.file_ref))
    });
    files.truncate(top_files);
    let mut out = BTreeMap::new();
    for f in files {
        let (key, doc) = f.into_doc(chunks_per_file)?;
        out.insert(key, doc);
    }
    Ok(out)
}

struct FileBucket {
    file_ref: String,
    corpus: String,
    mtime_ms: i64,
    summary: Option<String>,
    keywords_json: String,
    score: f64,
    fts_rank: Option<u32>,
    vec_rank: Option<u32>,
    chunks: Vec<(FusedHit, ChunkRow)>,
}

impl FileBucket {
    fn new(row: &ChunkRow) -> Self {
        Self {
            file_ref: row.file_ref.clone(),
            corpus: row.corpus.clone(),
            mtime_ms: row.mtime_ms,
            summary: row.summary.clone(),
            keywords_json: row.keywords_json.clone(),
            score: f64::MIN,
            fts_rank: None,
            vec_rank: None,
            chunks: Vec::new(),
        }
    }

    fn push(&mut self, hit: FusedHit, row: ChunkRow) {
        if hit.score > self.score {
            self.score = hit.score;
            self.fts_rank = hit.fts_rank;
            self.vec_rank = hit.vec_rank;
        }
        self.chunks.push((hit, row));
    }

    fn into_doc(mut self, chunks_per_file: usize) -> Result<(String, DocFile)> {
        self.chunks.sort_by(|a, b| {
            b.0.score
                .partial_cmp(&a.0.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
        });
        self.chunks.truncate(chunks_per_file);
        let chunks = self
            .chunks
            .iter()
            .map(|(hit, row)| build_doc_chunk(hit, row))
            .collect::<Result<Vec<_>>>()?;
        let keywords: Vec<String> = serde_json::from_str(&self.keywords_json)
            .map_err(|e| HallouminateError::Indexer(format!("keywords json: {e}")))?;
        Ok((
            self.file_ref,
            DocFile {
                summary: self.summary,
                keywords,
                score: self.score,
                fts_rank: self.fts_rank,
                vec_rank: self.vec_rank,
                mtime: iso8601_ms(self.mtime_ms),
                corpus: self.corpus,
                chunks,
            },
        ))
    }
}

fn build_doc_chunk(hit: &FusedHit, row: &ChunkRow) -> Result<DocChunk> {
    let heading_path: Vec<String> = serde_json::from_str(&row.heading_path_json)
        .map_err(|e| HallouminateError::Indexer(format!("heading_path json: {e}")))?;
    Ok(DocChunk {
        chunk_id: row.chunk_id,
        heading_path,
        line_range: [row.line_start as u32, row.line_end as u32],
        score: hit.score,
        fts_rank: hit.fts_rank,
        vec_rank: hit.vec_rank,
        snippet: make_snippet(&row.text),
    })
}

fn fetch_chunk_row(conn: &Connection, chunk_id: i64) -> Result<Option<ChunkRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.chunk_id, c.file_id, f.file_ref, f.corpus, f.mtime_ms, \
                f.summary, f.keywords, c.heading_path, c.line_start, c.line_end, c.text \
         FROM chunks c JOIN files f ON c.file_id = f.file_id \
         WHERE c.chunk_id = ?1",
    )?;
    let row = stmt
        .query_row(params![chunk_id], |r| {
            Ok(ChunkRow {
                chunk_id: r.get(0)?,
                file_id: r.get(1)?,
                file_ref: r.get(2)?,
                corpus: r.get(3)?,
                mtime_ms: r.get(4)?,
                summary: r.get(5)?,
                keywords_json: r.get(6)?,
                heading_path_json: r.get(7)?,
                line_start: r.get(8)?,
                line_end: r.get(9)?,
                text: r.get(10)?,
            })
        })
        .optional()?;
    Ok(row)
}

fn iso8601_ms(mtime_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(mtime_ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| String::from("1970-01-01T00:00:00Z"))
}
