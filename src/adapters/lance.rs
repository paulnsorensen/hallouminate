use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::ListArray;
use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};

use crate::domain::common::{FileRef, HallouminateError, Result};
use crate::domain::embeddings::canonical_model_name;
use crate::domain::indexer::plan::FileSnapshot;

pub const EMBEDDING_DIM: usize = 384;
const TABLE_NAME: &str = "chunks";
const META_FILENAME: &str = "meta.toml";

// ── Public types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PreparedChunk {
    pub ord: usize,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct PreparedFile {
    pub file_ref: String,
    pub corpus: String,
    pub mtime_ms: i64,
    pub content_hash: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub indexed_at_ms: i64,
    pub chunks: Vec<PreparedChunk>,
    pub embeddings: Vec<[f32; EMBEDDING_DIM]>,
}

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
}

// ── Deterministic chunk_id ──────────────────────────────────────────────

/// Stable, deterministic chunk identifier derived from (file_ref, ord).
///
/// Same (file_ref, ord) → same chunk_id; lets `merge_insert(chunk_id)` cleanly
/// overwrite the same logical chunk on re-index and orphan-drop chunks beyond
/// the new ord range via `when_not_matched_by_source_delete`.
pub fn chunk_id_for(file_ref: &str, ord: usize) -> String {
    let mut buf = String::with_capacity(file_ref.len() + 8);
    buf.push_str(file_ref);
    buf.push('#');
    buf.push_str(&ord.to_string());
    let h = blake3::hash(buf.as_bytes());
    let hex = h.to_hex();
    hex.as_str()[..32].to_string()
}

// ── Meta sidecar (TOML) ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Meta {
    embedding_model_name: String,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

fn default_schema_version() -> u32 {
    1
}

fn meta_check_or_init(meta_path: &Path, requested_model: &str) -> Result<()> {
    let requested_model = canonical_model_name(requested_model)?;
    if meta_path.exists() {
        let text = std::fs::read_to_string(meta_path)?;
        let mut meta: Meta = toml::from_str(&text)
            .map_err(|e| HallouminateError::Config(format!("parse meta.toml: {e}")))?;
        let stored_model = canonical_model_name(&meta.embedding_model_name)?;
        if stored_model != requested_model {
            return Err(HallouminateError::Embed(format!(
                "embedding model mismatch: store has {:?}, requested {:?}; \
                 delete {} and re-run `hallouminate index` to rebuild",
                stored_model,
                requested_model,
                meta_path.parent().unwrap_or(meta_path).display(),
            )));
        }
        if meta.embedding_model_name != stored_model {
            meta.embedding_model_name = stored_model.to_string();
            write_meta(meta_path, &meta)?;
        }
        return Ok(());
    }
    let meta = Meta {
        embedding_model_name: requested_model.to_string(),
        schema_version: 1,
    };
    write_meta(meta_path, &meta)?;
    Ok(())
}

fn write_meta(meta_path: &Path, meta: &Meta) -> Result<()> {
    let body = toml::to_string_pretty(&meta)
        .map_err(|e| HallouminateError::Config(format!("serialize meta: {e}")))?;
    let toml_text = format!("# auto-managed by hallouminate; do not edit\n{body}");
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(meta_path, toml_text)?;
    Ok(())
}

// ── Schema ──────────────────────────────────────────────────────────────

fn list_utf8_field(name: &str) -> Field {
    Field::new(
        name,
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        false,
    )
}

pub fn chunks_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("chunk_id", DataType::Utf8, false),
        Field::new("file_ref", DataType::Utf8, false),
        Field::new("corpus", DataType::Utf8, false),
        Field::new("mtime_ms", DataType::Int64, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("summary", DataType::Utf8, false),
        list_utf8_field("keywords"),
        Field::new("indexed_at_ms", DataType::Int64, false),
        Field::new("ord", DataType::Int64, false),
        list_utf8_field("heading_path"),
        Field::new("line_start", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                EMBEDDING_DIM as i32,
            ),
            false,
        ),
    ]))
}

// ── Record batch building ───────────────────────────────────────────────

fn build_list_utf8(values: &[Vec<String>]) -> ListArray {
    let mut builder = ListBuilder::new(StringBuilder::new());
    for row in values {
        for s in row {
            builder.values().append_value(s);
        }
        builder.append(true);
    }
    builder.finish()
}

fn build_record_batch(batch: &[PreparedFile], schema: SchemaRef) -> Result<RecordBatch> {
    let mut chunk_ids: Vec<String> = Vec::new();
    let mut file_refs: Vec<String> = Vec::new();
    let mut corpora: Vec<String> = Vec::new();
    let mut mtimes: Vec<i64> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();
    let mut summaries: Vec<String> = Vec::new();
    let mut keywords: Vec<Vec<String>> = Vec::new();
    let mut indexed_at: Vec<i64> = Vec::new();
    let mut ords: Vec<i64> = Vec::new();
    let mut heading_paths: Vec<Vec<String>> = Vec::new();
    let mut line_starts: Vec<i64> = Vec::new();
    let mut line_ends: Vec<i64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut embeddings_flat: Vec<f32> = Vec::new();

    for file in batch {
        if file.chunks.len() != file.embeddings.len() {
            return Err(HallouminateError::Indexer(format!(
                "prepared file {:?}: {} chunks but {} embeddings",
                file.file_ref,
                file.chunks.len(),
                file.embeddings.len()
            )));
        }
        for (chunk, embedding) in file.chunks.iter().zip(file.embeddings.iter()) {
            chunk_ids.push(chunk_id_for(&file.file_ref, chunk.ord));
            file_refs.push(file.file_ref.clone());
            corpora.push(file.corpus.clone());
            mtimes.push(file.mtime_ms);
            hashes.push(file.content_hash.clone());
            summaries.push(file.summary.clone());
            keywords.push(file.keywords.clone());
            indexed_at.push(file.indexed_at_ms);
            ords.push(chunk.ord as i64);
            heading_paths.push(chunk.heading_path.clone());
            line_starts.push(chunk.line_start as i64);
            line_ends.push(chunk.line_end as i64);
            texts.push(chunk.text.clone());
            embeddings_flat.extend_from_slice(embedding);
        }
    }

    let embedding_field = Arc::new(Field::new("item", DataType::Float32, true));
    let embedding_values = Float32Array::from(embeddings_flat);
    let embedding_array = FixedSizeListArray::try_new(
        embedding_field,
        EMBEDDING_DIM as i32,
        Arc::new(embedding_values),
        None,
    )
    .map_err(|e| HallouminateError::Indexer(format!("build embedding column: {e}")))?;

    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(StringArray::from(chunk_ids)),
        Arc::new(StringArray::from(file_refs)),
        Arc::new(StringArray::from(corpora)),
        Arc::new(Int64Array::from(mtimes)),
        Arc::new(StringArray::from(hashes)),
        Arc::new(StringArray::from(summaries)),
        Arc::new(build_list_utf8(&keywords)),
        Arc::new(Int64Array::from(indexed_at)),
        Arc::new(Int64Array::from(ords)),
        Arc::new(build_list_utf8(&heading_paths)),
        Arc::new(Int64Array::from(line_starts)),
        Arc::new(Int64Array::from(line_ends)),
        Arc::new(StringArray::from(texts)),
        Arc::new(embedding_array),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| HallouminateError::Indexer(format!("build record batch: {e}")))
}

/// Escape a string for inclusion in a DataFusion SQL literal.
///
/// DataFusion follows standard SQL string-literal rules: only single quotes
/// need escaping (by doubling). Backslashes, newlines, and other control
/// characters are literal inside `'...'` and need no transformation. NUL
/// bytes are impossible in file paths on POSIX (kernel guarantee), so
/// `file_ref` strings never contain them.
fn escape_sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

// ── Shared RecordBatch column accessors ─────────────────────────────────

fn string_col<'a>(rb: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    rb.column_by_name(name)
        .ok_or_else(|| HallouminateError::Indexer(format!("missing column {name}")))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| HallouminateError::Indexer(format!("{name} not utf8")))
}

fn int64_col<'a>(rb: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    rb.column_by_name(name)
        .ok_or_else(|| HallouminateError::Indexer(format!("missing column {name}")))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| HallouminateError::Indexer(format!("{name} not int64")))
}

fn list_utf8_col<'a>(rb: &'a RecordBatch, name: &str) -> Result<&'a ListArray> {
    rb.column_by_name(name)
        .ok_or_else(|| HallouminateError::Indexer(format!("missing column {name}")))?
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| HallouminateError::Indexer(format!("{name} not list")))
}

fn decode_list(list: &ListArray, row: usize) -> Vec<String> {
    let values = list.value(row);
    let strs = values.as_any().downcast_ref::<StringArray>();
    let Some(s) = strs else {
        return Vec::new();
    };
    (0..s.len()).map(|i| s.value(i).to_string()).collect()
}

fn decode_hits(rb: &RecordBatch, out: &mut Vec<SearchHit>) -> Result<()> {
    let chunk_id = string_col(rb, "chunk_id")?;
    let file_ref = string_col(rb, "file_ref")?;
    let summary = string_col(rb, "summary")?;
    let text = string_col(rb, "text")?;
    let line_start = int64_col(rb, "line_start")?;
    let line_end = int64_col(rb, "line_end")?;
    let mtime_ms = int64_col(rb, "mtime_ms")?;
    let heading_path = list_utf8_col(rb, "heading_path")?;
    let keywords = list_utf8_col(rb, "keywords")?;
    let score = rb
        .column_by_name("_relevance_score")
        .or_else(|| rb.column_by_name("_score"))
        .cloned();
    for i in 0..rb.num_rows() {
        let s = score
            .as_ref()
            .and_then(|arr| arr.as_any().downcast_ref::<Float32Array>())
            .map(|f| f.value(i))
            .unwrap_or(0.0);
        out.push(SearchHit {
            chunk_id: chunk_id.value(i).to_string(),
            file_ref: file_ref.value(i).to_string(),
            heading_path: decode_list(heading_path, i),
            line_start: line_start.value(i) as usize,
            line_end: line_end.value(i) as usize,
            text: text.value(i).to_string(),
            summary: summary.value(i).to_string(),
            keywords: decode_list(keywords, i),
            score: s,
            mtime_ms: mtime_ms.value(i),
        });
    }
    Ok(())
}

fn file_ref_in_filter(refs: &[String]) -> String {
    let quoted: Vec<String> = refs
        .iter()
        .map(|r| format!("'{}'", escape_sql_str(r)))
        .collect();
    format!("file_ref IN ({})", quoted.join(", "))
}

/// Predicate scoped to a single corpus for `merge_insert`'s orphan-drop and
/// for `delete_file`. Without the `corpus = ...` term, a multi-corpus store
/// would delete or update rows belonging to other corpora that happen to
/// share the same `file_ref`.
fn corpus_and_file_ref_filter(corpus: &str, refs: &[String]) -> String {
    format!(
        "corpus = '{}' AND {}",
        escape_sql_str(corpus),
        file_ref_in_filter(refs)
    )
}

fn map_lance_err<E: std::fmt::Display>(e: E) -> HallouminateError {
    HallouminateError::Db(Box::new(std::io::Error::other(format!("lance: {e}"))))
}

// ── LanceStore ──────────────────────────────────────────────────────────

pub struct LanceStore {
    table: lancedb::Table,
    #[allow(dead_code)]
    connection: lancedb::Connection,
    #[allow(dead_code)]
    meta_path: PathBuf,
}

impl LanceStore {
    pub async fn open_or_create(ground_dir: &Path, model_name: &str) -> Result<Self> {
        std::fs::create_dir_all(ground_dir)?;
        let meta_path = ground_dir.join(META_FILENAME);
        meta_check_or_init(&meta_path, model_name)?;
        let uri = ground_dir.to_str().ok_or_else(|| {
            HallouminateError::Config(format!("non-utf8 ground dir: {}", ground_dir.display()))
        })?;
        let connection = lancedb::connect(uri)
            .execute()
            .await
            .map_err(map_lance_err)?;
        let table = open_or_create_table(&connection).await?;
        Ok(Self {
            table,
            connection,
            meta_path,
        })
    }

    pub async fn count_rows(&self) -> Result<u64> {
        self.table
            .count_rows(None)
            .await
            .map_err(map_lance_err)
            .map(|n| n as u64)
    }

    /// Upsert a batch of prepared files. All files in a single call MUST
    /// belong to the same corpus — the orphan-drop predicate is scoped to
    /// that corpus, so mixing corpora here would risk deleting unrelated
    /// rows. The merge join key is `(corpus, chunk_id)` so two corpora that
    /// happen to share a `file_ref` keep independent rows.
    pub async fn apply_batch(&self, batch: Vec<PreparedFile>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let corpus = batch[0].corpus.clone();
        if batch.iter().any(|f| f.corpus != corpus) {
            return Err(HallouminateError::Indexer(
                "apply_batch: all PreparedFiles in a batch must share the same corpus".into(),
            ));
        }
        let schema = chunks_schema();
        let record_batch = build_record_batch(&batch, schema.clone())?;
        let file_refs: Vec<String> = batch.iter().map(|f| f.file_ref.clone()).collect();
        let scope = corpus_and_file_ref_filter(&corpus, &file_refs);
        let reader = RecordBatchIterator::new(std::iter::once(Ok(record_batch)), schema);
        let reader: Box<dyn arrow_array::RecordBatchReader + Send> = Box::new(reader);
        let mut builder = self.table.merge_insert(&["corpus", "chunk_id"]);
        builder
            .when_matched_update_all(None)
            .when_not_matched_insert_all()
            .when_not_matched_by_source_delete(Some(scope));
        builder.execute(reader).await.map_err(map_lance_err)?;
        self.ensure_search_indexes().await?;
        Ok(())
    }

    /// Build the FTS index on `text` (and the ANN index on `embedding`) if
    /// they don't already exist. LanceDB requires data to be present before
    /// some indexes can be created, so this runs after `merge_insert` —
    /// idempotent via `list_indices()`.
    async fn ensure_search_indexes(&self) -> Result<()> {
        let existing = self.table.list_indices().await.map_err(map_lance_err)?;
        let has_text_index = existing
            .iter()
            .any(|i| i.columns.iter().any(|c| c == "text"));
        if !has_text_index {
            self.table
                .create_index(&["text"], lancedb::index::Index::FTS(Default::default()))
                .execute()
                .await
                .map_err(map_lance_err)?;
        }
        // Vector index is optional — small corpora work fine without it via
        // brute-force scan, and IVF-PQ needs enough rows for meaningful
        // training. Skip if row count is below a small threshold.
        let has_vec_index = existing
            .iter()
            .any(|i| i.columns.iter().any(|c| c == "embedding"));
        if !has_vec_index {
            let rows = self.table.count_rows(None).await.map_err(map_lance_err)? as u64;
            if rows >= 256 {
                if let Err(e) = self
                    .table
                    .create_index(&["embedding"], lancedb::index::Index::Auto)
                    .execute()
                    .await
                {
                    // ANN index is an optimization, not a correctness
                    // requirement — brute-force scan still works. Log so
                    // operators can diagnose why ANN never kicked in.
                    tracing::warn!(
                        target: "hallouminate::lance",
                        err = %e,
                        "failed to create ANN index on `embedding`; queries will brute-force scan"
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn touch_mtime(&self, corpus: &str, file_ref: &str, new_mtime_ms: i64) -> Result<()> {
        let predicate = format!(
            "corpus = '{}' AND file_ref = '{}'",
            escape_sql_str(corpus),
            escape_sql_str(file_ref)
        );
        self.table
            .update()
            .only_if(predicate)
            .column("mtime_ms", new_mtime_ms.to_string())
            .execute()
            .await
            .map_err(map_lance_err)?;
        Ok(())
    }

    pub async fn delete_file(&self, corpus: &str, file_ref: &str) -> Result<()> {
        let predicate = format!(
            "corpus = '{}' AND file_ref = '{}'",
            escape_sql_str(corpus),
            escape_sql_str(file_ref)
        );
        self.table.delete(&predicate).await.map_err(map_lance_err)?;
        Ok(())
    }

    /// Returns one `FileSnapshot` per indexed file in `corpus`. We rely on
    /// the invariant that every prepared file emits at least one chunk with
    /// `ord = 0` (enforced in the indexer's writer), which lets us push
    /// dedup into the store as an `ord = 0` filter instead of materializing
    /// one row per chunk and folding through a HashMap.
    pub async fn list_files(&self, corpus: &str) -> Result<HashMap<FileRef, FileSnapshot>> {
        let predicate = format!("corpus = '{}' AND ord = 0", escape_sql_str(corpus));
        let stream = self
            .table
            .query()
            .only_if(predicate)
            .select(lancedb::query::Select::columns(&[
                "file_ref",
                "corpus",
                "mtime_ms",
                "content_hash",
            ]))
            .execute()
            .await
            .map_err(map_lance_err)?;
        let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
        let mut out: HashMap<FileRef, FileSnapshot> = HashMap::new();
        for rb in batches {
            let file_ref_col = string_col(&rb, "file_ref")?;
            let corpus_col = string_col(&rb, "corpus")?;
            let mtime_col = int64_col(&rb, "mtime_ms")?;
            let hash_col = string_col(&rb, "content_hash")?;
            for i in 0..rb.num_rows() {
                let file_ref = file_ref_col.value(i).to_string();
                let snap = FileSnapshot {
                    file_ref: file_ref.clone(),
                    corpus: corpus_col.value(i).to_string(),
                    mtime_ms: mtime_col.value(i),
                    content_hash: hash_col.value(i).to_string(),
                };
                out.insert(FileRef::new(std::path::PathBuf::from(&file_ref)), snap);
            }
        }
        Ok(out)
    }

    /// Hybrid BM25 + vector search reranked with LanceDB's built-in
    /// `RRFReranker`, scoped to a single `corpus`. Returns an empty `Vec`
    /// for an empty corpus or when no rows match the corpus filter.
    pub async fn hybrid_search(
        &self,
        corpus: &str,
        query: &str,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if query_vec.len() != EMBEDDING_DIM {
            return Err(HallouminateError::Embed(format!(
                "query vector dim mismatch: got {}, expected {}",
                query_vec.len(),
                EMBEDDING_DIM
            )));
        }
        if self.count_rows().await? == 0 {
            return Ok(Vec::new());
        }
        let reranker = std::sync::Arc::new(lancedb::rerankers::rrf::RRFReranker::default());
        let corpus_filter = format!("corpus = '{}'", escape_sql_str(corpus));
        let stream = self
            .table
            .query()
            .only_if(corpus_filter)
            .full_text_search(lancedb::index::scalar::FullTextSearchQuery::new(
                query.to_string(),
            ))
            .nearest_to(query_vec)
            .map_err(map_lance_err)?
            .limit(limit)
            .rerank(reranker)
            .execute()
            .await
            .map_err(map_lance_err)?;
        let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
        let mut out: Vec<SearchHit> = Vec::new();
        for rb in batches {
            decode_hits(&rb, &mut out)?;
        }
        Ok(out)
    }
}

async fn open_or_create_table(connection: &lancedb::Connection) -> Result<lancedb::Table> {
    let names = connection
        .table_names()
        .execute()
        .await
        .map_err(map_lance_err)?;
    if names.iter().any(|n| n == TABLE_NAME) {
        return connection
            .open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(map_lance_err);
    }
    let schema = chunks_schema();
    let empty: Vec<std::result::Result<RecordBatch, arrow_schema::ArrowError>> = Vec::new();
    let reader = RecordBatchIterator::new(empty.into_iter(), schema);
    let reader: Box<dyn arrow_array::RecordBatchReader + Send> = Box::new(reader);
    connection
        .create_table(TABLE_NAME, reader)
        .execute()
        .await
        .map_err(map_lance_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_is_deterministic_for_same_file_ref_and_ord() {
        let a = chunk_id_for("/tmp/foo.md", 3);
        let b = chunk_id_for("/tmp/foo.md", 3);
        assert_eq!(a, b);
    }

    #[test]
    fn chunk_id_differs_for_different_ord() {
        let a = chunk_id_for("/tmp/foo.md", 3);
        let b = chunk_id_for("/tmp/foo.md", 4);
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_differs_for_different_file_ref() {
        let a = chunk_id_for("/tmp/foo.md", 0);
        let b = chunk_id_for("/tmp/bar.md", 0);
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_is_32_lowercase_hex_chars() {
        let id = chunk_id_for("/tmp/whatever.md", 7);
        assert_eq!(id.len(), 32);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_lowercase()))
        );
    }

    #[test]
    fn file_ref_in_filter_quotes_each_ref() {
        let refs = vec!["/tmp/a.md".into(), "/tmp/b.md".into()];
        let f = file_ref_in_filter(&refs);
        assert_eq!(f, "file_ref IN ('/tmp/a.md', '/tmp/b.md')");
    }

    #[test]
    fn file_ref_in_filter_escapes_single_quotes() {
        let refs = vec!["/tmp/o'brien.md".into()];
        let f = file_ref_in_filter(&refs);
        assert_eq!(f, "file_ref IN ('/tmp/o''brien.md')");
    }

    #[test]
    fn escape_sql_str_leaves_backslash_literal() {
        // DataFusion follows standard SQL: backslash is NOT an escape char
        // inside '...' literals. The string "a\b" stays "a\b".
        assert_eq!(escape_sql_str(r"a\b"), r"a\b");
    }

    #[test]
    fn escape_sql_str_leaves_newline_and_tab_literal() {
        assert_eq!(escape_sql_str("line\nfeed\there"), "line\nfeed\there");
    }

    #[test]
    fn escape_sql_str_doubles_every_single_quote_not_just_first() {
        assert_eq!(escape_sql_str("a'b'c'd"), "a''b''c''d");
    }

    #[test]
    fn escape_sql_str_handles_already_doubled_quote_safely() {
        // Defense: input contains a literal '' (two quotes side-by-side).
        // Each is escaped independently → 4 quotes in output.
        assert_eq!(escape_sql_str("a''b"), "a''''b");
    }

    #[test]
    fn meta_check_or_init_writes_meta_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "bge-small-en-v1.5").unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        let meta: Meta = toml::from_str(&text).unwrap();
        assert_eq!(meta.embedding_model_name, "BAAI/bge-small-en-v1.5");
        assert!(text.contains("schema_version"));
        assert!(text.contains("auto-managed"));
    }

    #[test]
    fn meta_check_or_init_passes_when_existing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5").unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5").expect("second call must succeed");
    }

    #[test]
    fn meta_check_or_init_errors_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5").unwrap();
        let err =
            meta_check_or_init(&meta_path, "sentence-transformers/all-MiniLM-L6-v2").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("BAAI/bge-small-en-v1.5"), "{msg}");
        assert!(
            msg.contains("sentence-transformers/all-MiniLM-L6-v2"),
            "{msg}"
        );
        assert!(msg.contains("delete"), "{msg}");
        assert!(msg.contains("hallouminate index"), "{msg}");
    }

    #[test]
    fn meta_check_or_init_with_legacy_alias_writes_canonical_to_fresh_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "bge-small-en-v1.5").unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        let meta: Meta = toml::from_str(&text).unwrap();
        assert_eq!(meta.embedding_model_name, "BAAI/bge-small-en-v1.5");
    }

    #[test]
    fn meta_check_or_init_rejects_unsupported_requested_model() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        let err = meta_check_or_init(&meta_path, "clip-vit-b32")
            .expect_err("unsupported request must error before any write");
        assert!(
            err.to_string().contains("unsupported embedding model"),
            "{err}"
        );
        assert!(
            !meta_path.exists(),
            "must not write sidecar on rejected request"
        );
    }

    #[test]
    fn meta_check_or_init_rejects_corrupt_stored_model() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "hand-edited-garbage"
schema_version = 1
"#,
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5")
            .expect_err("corrupt sidecar must error");
        assert!(
            err.to_string().contains("unsupported embedding model"),
            "{err}"
        );
    }

    #[test]
    fn meta_check_or_init_normalizes_existing_legacy_alias() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "bge-small-en-v1.5"
schema_version = 1
"#,
        )
        .unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5").unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        assert!(text.contains("BAAI/bge-small-en-v1.5"), "{text}");
        assert!(!text.contains("\"bge-small-en-v1.5\""), "{text}");
    }

    #[test]
    fn chunks_schema_has_all_documented_columns_in_order() {
        let schema = chunks_schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec![
                "chunk_id",
                "file_ref",
                "corpus",
                "mtime_ms",
                "content_hash",
                "summary",
                "keywords",
                "indexed_at_ms",
                "ord",
                "heading_path",
                "line_start",
                "line_end",
                "text",
                "embedding",
            ]
        );
    }

    #[test]
    fn chunks_schema_embedding_column_is_fixed_size_384_f32() {
        let schema = chunks_schema();
        let embedding = schema.field_with_name("embedding").unwrap();
        match embedding.data_type() {
            arrow_schema::DataType::FixedSizeList(child, dim) => {
                assert_eq!(*dim, EMBEDDING_DIM as i32, "expected 384, got {dim}");
                match child.data_type() {
                    arrow_schema::DataType::Float32 => {}
                    other => panic!("expected Float32 child, got {other:?}"),
                }
            }
            other => panic!("expected FixedSizeList, got {other:?}"),
        }
    }

    fn synthetic_prepared(file_ref: &str, chunks: usize) -> PreparedFile {
        let mut pf = PreparedFile {
            file_ref: file_ref.to_string(),
            corpus: "docs".into(),
            mtime_ms: 7,
            content_hash: "deadbeef".into(),
            summary: "summary".into(),
            keywords: vec!["k1".into(), "k2".into()],
            indexed_at_ms: 11,
            chunks: Vec::new(),
            embeddings: Vec::new(),
        };
        for i in 0..chunks {
            pf.chunks.push(PreparedChunk {
                ord: i,
                heading_path: vec!["H".into()],
                line_start: 1,
                line_end: 2,
                text: format!("chunk-{i}"),
            });
            pf.embeddings.push([0.0_f32; EMBEDDING_DIM]);
        }
        pf
    }

    #[test]
    fn build_record_batch_row_count_matches_total_chunks_across_files() {
        let batch = vec![
            synthetic_prepared("/tmp/a.md", 3),
            synthetic_prepared("/tmp/b.md", 2),
        ];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build batch");
        assert_eq!(rb.num_rows(), 5);
        assert_eq!(rb.num_columns(), 14);
    }

    #[test]
    fn build_record_batch_denormalizes_file_metadata_onto_every_chunk_row() {
        let batch = vec![synthetic_prepared("/tmp/dup.md", 3)];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build batch");
        let file_refs = rb
            .column_by_name("file_ref")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let summaries = rb
            .column_by_name("summary")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..rb.num_rows() {
            assert_eq!(file_refs.value(i), "/tmp/dup.md");
            assert_eq!(summaries.value(i), "summary");
        }
    }

    #[test]
    fn build_record_batch_rejects_chunk_embedding_length_mismatch() {
        let mut pf = synthetic_prepared("/tmp/bad.md", 2);
        pf.embeddings.pop(); // 2 chunks, 1 embedding
        let schema = chunks_schema();
        let err = build_record_batch(&[pf], schema).unwrap_err();
        assert!(
            err.to_string().contains("chunks but 1 embeddings"),
            "got: {err}"
        );
    }

    #[test]
    fn build_record_batch_assigns_deterministic_chunk_ids_via_chunk_id_for() {
        let batch = vec![synthetic_prepared("/tmp/det.md", 2)];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build batch");
        let chunk_ids = rb
            .column_by_name("chunk_id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(chunk_ids.value(0), chunk_id_for("/tmp/det.md", 0));
        assert_eq!(chunk_ids.value(1), chunk_id_for("/tmp/det.md", 1));
    }

    #[test]
    fn build_record_batch_with_empty_batch_returns_zero_row_batch() {
        let schema = chunks_schema();
        let rb = build_record_batch(&[], schema).expect("build empty");
        assert_eq!(rb.num_rows(), 0);
    }

    #[test]
    fn file_ref_in_filter_handles_empty_input() {
        // Boundary: empty input still produces well-formed SQL — caller must
        // avoid feeding an empty list, but we shouldn't crash.
        let refs: Vec<String> = Vec::new();
        let f = file_ref_in_filter(&refs);
        assert_eq!(f, "file_ref IN ()");
    }

    #[test]
    fn meta_check_or_init_creates_parent_directory_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("nested/dir/meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5").unwrap();
        assert!(meta_path.exists());
    }
}
