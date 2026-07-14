use crate::corpus::ClaimMark;

// TEMPORARY (Stage 2b bridge, removed in Stage 2c when PreparedFile.embeddings
// drops): sourced from the adapter's true home for the embedding dimension.

/// One chunk of a prepared file, ready to be written as a row in the `chunks`
/// table.
#[derive(Debug, Clone)]
pub struct PreparedChunk {
    pub ord: usize,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
    /// Canonical JSON of the claim marks anchored within this chunk's line
    /// range, or `None` when the chunk has no marks. Per-chunk (positional),
    /// unlike the page-level `frontmatter` denormalized identically onto every
    /// row. Stored in the nullable `claim_marks` column.
    pub claim_marks: Option<String>,
}

/// A single source file plus all of its chunks, ready for `apply_batch`.
///
/// File-level metadata (`summary`, `keywords`, `mtime_ms`, …) is denormalized
/// onto every chunk row when the batch is built.
#[derive(Debug, Clone)]
pub struct PreparedFile {
    pub file_ref: String,
    pub corpus: String,
    pub mtime_ms: i64,
    pub content_hash: String,
    pub summary: String,
    pub keywords: Vec<String>,
    /// Canonical JSON of the page's parsed frontmatter, or `None` when the file
    /// has no frontmatter block (or it was malformed). Denormalized onto every
    /// chunk row, like `summary`/`keywords`.
    pub frontmatter: Option<String>,
    pub indexed_at_ms: i64,
    pub chunks: Vec<PreparedChunk>,
}

/// One ranked result row returned by `hybrid_search` or `fts_search`.
///
/// Carries the chunk's text and location plus its parent file's `summary`,
/// `keywords`, and `mtime_ms`, with `score` set by the active reranker.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: String,
    pub file_ref: String,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub score: f32,
    pub mtime_ms: i64,
    /// Claim marks decoded from the chunk's `claim_marks` JSON column. Empty
    /// when the chunk carried no marks (a null column value).
    pub claim_marks: Vec<ClaimMark>,
    /// Per-query z-score of `score`, stamped by the orchestrator after rerank.
    /// TRANSIENT: not decoded from or persisted to a Lance column; defaults to
    /// `None` at decode and only the cross-encoder path populates it.
    pub z_score: Option<f64>,
}
