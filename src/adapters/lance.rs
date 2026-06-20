use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow::array::ListArray;
use arrow::array::builder::{ListBuilder, StringBuilder};
use arrow::array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};

use crate::domain::common::{FileRef, HallouminateError, Result};
use crate::domain::corpus::ClaimMark;
use crate::domain::embeddings::canonical_model_name;
use crate::domain::indexer::plan::FileSnapshot;

/// Dimensionality of every stored embedding vector.
///
/// Matches the output width of the `BAAI/bge-small-en-v1.5` model the indexer
/// embeds with. The `embedding` column is a `FixedSizeList` of exactly this
/// many `f32`s, and `hybrid_search` rejects query vectors of any other length.
pub const EMBEDDING_DIM: usize = 384;
const TABLE_NAME: &str = "chunks";
const META_FILENAME: &str = "meta.toml";

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
    /// `Some(v)` (embeddings ON): one vector per chunk, length-checked
    /// against `chunks`. `None` (embeddings OFF): the indexer ran without an
    /// embedder, so every chunk row is written with a null embedding.
    pub embeddings: Option<Vec<[f32; EMBEDDING_DIM]>>,
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
}

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

/// Identity that ties a ground directory to the embedding configuration it
/// was built with. A change in any field invalidates the stored vectors (or
/// their absence), so `meta_check_or_init` treats all three as a unit and
/// refuses a mismatch with the same "delete + reindex" remedy.
///
/// `quantized` and `embeddings_enabled` default to the pre-feature shape
/// (full precision, embeddings ON) so a sidecar written before these fields
/// existed reads back as the mode it was actually built in. Changing the
/// active embedding mode — e.g. setting `enabled = false` — then trips the
/// mismatch guard on the next open, correct, since switching the mode does
/// change the store's contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Meta {
    embedding_model_name: String,
    #[serde(default = "default_quantized")]
    quantized: bool,
    #[serde(default = "default_embeddings_enabled")]
    embeddings_enabled: bool,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

/// The schema version this build reads and writes, bumped whenever the Arrow
/// `chunks` schema changes shape (v2 added the `frontmatter` column; v3 added
/// the per-chunk `claim_marks` column). Also the
/// serde default, but every LanceDB store ever written records this field, so a
/// missing value can only come from a hand-edited sidecar.
fn default_schema_version() -> u32 {
    3
}

#[doc(hidden)]
/// Public accessor for the current schema version; exposed for integration
/// tests that need to write a stale meta.toml without hard-coding the value.
pub fn default_schema_version_pub() -> u32 {
    default_schema_version()
}

fn default_quantized() -> bool {
    false
}

fn default_embeddings_enabled() -> bool {
    true
}

fn meta_check_or_init(
    meta_path: &Path,
    requested_model: &str,
    quantized: bool,
    enabled: bool,
) -> Result<()> {
    let requested_model = canonical_model_name(requested_model)?;
    if meta_path.exists() {
        let text = std::fs::read_to_string(meta_path)?;
        let mut meta: Meta = toml::from_str(&text)
            .map_err(|e| HallouminateError::Config(format!("parse meta.toml: {e}")))?;
        let stored_model = canonical_model_name(&meta.embedding_model_name)?;
        // Schema-version guard: an older store (v1, pre-frontmatter) carries a
        // different Arrow `chunks` schema. Catch the mismatch here, before any
        // query, and return the same "delete + reindex" remedy rather than
        // letting LanceDB surface a raw Arrow schema-mismatch crash later.
        if meta.schema_version != default_schema_version() {
            if meta.schema_version < default_schema_version() {
                return Err(HallouminateError::StoreSchemaStale {
                    found: meta.schema_version,
                    expected: default_schema_version(),
                    ground_dir: meta_path.parent().unwrap_or(meta_path).to_path_buf(),
                });
            }
            // store > expected: downgrade. Keep loud + fatal — never silently drop newer data.
            return Err(HallouminateError::Config(format!(
                "store schema version {} is NEWER than this build expects ({}); this binary is \
                 older than the one that wrote {}. Upgrade hallouminate, or delete the store to \
                 rebuild.",
                meta.schema_version,
                default_schema_version(),
                meta_path.parent().unwrap_or(meta_path).display(),
            )));
        }
        if stored_model != requested_model
            || meta.quantized != quantized
            || meta.embeddings_enabled != enabled
        {
            return Err(HallouminateError::Embed(format!(
                "embedding store mismatch: store has \
                 (model {:?}, quantized {}, embeddings_enabled {}), requested \
                 (model {:?}, quantized {}, embeddings_enabled {}); \
                 delete {} and re-run `hallouminate index` to rebuild",
                stored_model,
                meta.quantized,
                meta.embeddings_enabled,
                requested_model,
                quantized,
                enabled,
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
        quantized,
        embeddings_enabled: enabled,
        schema_version: default_schema_version(),
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
        // Nullable: null = no (or malformed) frontmatter block on the page.
        Field::new("frontmatter", DataType::Utf8, true),
        Field::new("indexed_at_ms", DataType::Int64, false),
        Field::new("ord", DataType::Int64, false),
        list_utf8_field("heading_path"),
        Field::new("line_start", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, false),
        Field::new("text", DataType::Utf8, false),
        // Nullable: null = no claim marks anchored within this chunk.
        Field::new("claim_marks", DataType::Utf8, true),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                EMBEDDING_DIM as i32,
            ),
            // Nullable: embeddings-OFF mode writes a null vector per chunk.
            // ON mode writes a real 384-dim vector. One schema, no sentinel.
            true,
        ),
    ]))
}

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
    let mut frontmatters: Vec<Option<String>> = Vec::new();
    let mut indexed_at: Vec<i64> = Vec::new();
    let mut ords: Vec<i64> = Vec::new();
    let mut heading_paths: Vec<Vec<String>> = Vec::new();
    let mut line_starts: Vec<i64> = Vec::new();
    let mut line_ends: Vec<i64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut claim_marks: Vec<Option<String>> = Vec::new();
    let mut embeddings_flat: Vec<f32> = Vec::new();
    // One validity bit per chunk row: true = real vector, false = null
    // (embeddings-OFF mode). Stays all-true on the ON path so the null
    // buffer is dropped entirely and the column is byte-identical to before.
    let mut embedding_valid: Vec<bool> = Vec::new();

    for file in batch {
        if let Some(embeddings) = &file.embeddings
            && file.chunks.len() != embeddings.len()
        {
            return Err(HallouminateError::Indexer(format!(
                "prepared file {:?}: {} chunks but {} embeddings",
                file.file_ref,
                file.chunks.len(),
                embeddings.len()
            )));
        }
        for (idx, chunk) in file.chunks.iter().enumerate() {
            chunk_ids.push(chunk_id_for(&file.file_ref, chunk.ord));
            file_refs.push(file.file_ref.clone());
            corpora.push(file.corpus.clone());
            mtimes.push(file.mtime_ms);
            hashes.push(file.content_hash.clone());
            summaries.push(file.summary.clone());
            keywords.push(file.keywords.clone());
            frontmatters.push(file.frontmatter.clone());
            indexed_at.push(file.indexed_at_ms);
            ords.push(chunk.ord as i64);
            heading_paths.push(chunk.heading_path.clone());
            line_starts.push(chunk.line_start as i64);
            line_ends.push(chunk.line_end as i64);
            texts.push(chunk.text.clone());
            claim_marks.push(chunk.claim_marks.clone());
            match &file.embeddings {
                Some(embeddings) => {
                    embeddings_flat.extend_from_slice(&embeddings[idx]);
                    embedding_valid.push(true);
                }
                None => {
                    // A null FixedSizeList entry still occupies `EMBEDDING_DIM`
                    // slots in the values buffer; they are masked by the null
                    // bit, so the placeholder zeros are never read.
                    embeddings_flat.extend_from_slice(&[0.0_f32; EMBEDDING_DIM]);
                    embedding_valid.push(false);
                }
            }
        }
    }

    let embedding_field = Arc::new(Field::new("item", DataType::Float32, true));
    let embedding_values = Float32Array::from(embeddings_flat);
    let nulls = if embedding_valid.iter().all(|&v| v) {
        None
    } else {
        Some(arrow::buffer::NullBuffer::from(embedding_valid))
    };
    let embedding_array = FixedSizeListArray::try_new(
        embedding_field,
        EMBEDDING_DIM as i32,
        Arc::new(embedding_values),
        nulls,
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
        Arc::new(StringArray::from_iter(frontmatters)),
        Arc::new(Int64Array::from(indexed_at)),
        Arc::new(Int64Array::from(ords)),
        Arc::new(build_list_utf8(&heading_paths)),
        Arc::new(Int64Array::from(line_starts)),
        Arc::new(Int64Array::from(line_ends)),
        Arc::new(StringArray::from(texts)),
        Arc::new(StringArray::from_iter(claim_marks)),
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

/// Decode a chunk's `claim_marks` JSON column value into structured marks. A
/// null cell (no marks anchored in the chunk) yields an empty `Vec`. Malformed
/// JSON is logged and treated as no marks rather than failing the whole search —
/// a corrupt stored payload must not take down a query.
fn decode_claim_marks(col: &StringArray, row: usize) -> Vec<ClaimMark> {
    if col.is_null(row) {
        return Vec::new();
    }
    let raw = col.value(row);
    match serde_json::from_str::<Vec<ClaimMark>>(raw) {
        Ok(marks) => marks,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::lance",
                error = %e,
                "failed to decode claim_marks JSON; treating chunk as having no marks"
            );
            Vec::new()
        }
    }
}

fn decode_hits(rb: &RecordBatch, out: &mut Vec<SearchHit>) -> Result<()> {
    // A zero-row batch contributes no hits and may carry a projected-away
    // schema (LanceDB can return an empty result whose columns are absent when
    // a `corpus = '...'` filter matches nothing in a populated store). Demanding
    // every column then would error `missing column chunk_id`; return early so
    // an empty corpus in a union ground yields no hits rather than failing the
    // whole call.
    if rb.num_rows() == 0 {
        return Ok(());
    }
    let chunk_id = string_col(rb, "chunk_id")?;
    let file_ref = string_col(rb, "file_ref")?;
    let summary = string_col(rb, "summary")?;
    let text = string_col(rb, "text")?;
    let line_start = int64_col(rb, "line_start")?;
    let line_end = int64_col(rb, "line_end")?;
    let mtime_ms = int64_col(rb, "mtime_ms")?;
    let heading_path = list_utf8_col(rb, "heading_path")?;
    let keywords = list_utf8_col(rb, "keywords")?;
    let claim_marks = string_col(rb, "claim_marks")?;
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
            claim_marks: decode_claim_marks(claim_marks, i),
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

/// Handle to a single LanceDB `chunks` table and its `meta.toml` sidecar.
///
/// One instance binds to one table for its whole lifetime: the table is
/// opened (or created) once in [`open_or_create`], and the search-index
/// state is cached against that table via `indexes_ensured`.
///
/// [`open_or_create`]: LanceStore::open_or_create
pub struct LanceStore {
    table: lancedb::Table,
    #[allow(dead_code)]
    connection: lancedb::Connection,
    #[allow(dead_code)]
    meta_path: PathBuf,
    /// Mirrors the store's `embeddings_enabled` identity. When false, the
    /// `embedding` column is all nulls, so `ensure_search_indexes` skips the
    /// ANN index entirely (there is nothing to vector-search).
    embeddings_enabled: bool,
    /// Latches true once `ensure_search_indexes` has confirmed the search
    /// indexes exist, letting later `apply_batch` calls skip the
    /// `list_indices()` round-trip. The table is created once per instance,
    /// so a fresh `LanceStore` always starts unlatched.
    indexes_ensured: AtomicBool,
}

impl LanceStore {
    /// Opens the `chunks` table under `ground_dir`, creating it (and the
    /// `meta.toml` sidecar) when absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested embedding configuration mismatches
    /// the stored sidecar (model, quantization, or embeddings-enabled flag),
    /// when `ground_dir` is not valid UTF-8, or when the LanceDB connection or
    /// table open/create fails.
    pub async fn open_or_create(
        ground_dir: &Path,
        model_name: &str,
        quantized: bool,
        embeddings_enabled: bool,
    ) -> Result<Self> {
        std::fs::create_dir_all(ground_dir)?;
        let meta_path = ground_dir.join(META_FILENAME);
        meta_check_or_init(&meta_path, model_name, quantized, embeddings_enabled)?;
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
            embeddings_enabled,
            indexes_ensured: AtomicBool::new(false),
        })
    }

    /// Returns the total number of chunk rows in the table.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB count query fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the batch mixes corpora, when building the Arrow
    /// record batch fails (e.g. a chunk/embedding length mismatch), or when
    /// the LanceDB `merge_insert` or index build fails.
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
        let mut file_refs: Vec<String> = Vec::with_capacity(batch.len());
        for file in &batch {
            file_refs.push(file.file_ref.clone());
        }
        let scope = corpus_and_file_ref_filter(&corpus, &file_refs);
        let reader = RecordBatchIterator::new(std::iter::once(Ok(record_batch)), schema);
        let reader: Box<dyn arrow::array::RecordBatchReader + Send> = Box::new(reader);
        let mut builder = self.table.merge_insert(&["corpus", "chunk_id"]);
        builder
            .when_matched_update_all(None)
            .when_not_matched_insert_all()
            .when_not_matched_by_source_delete(Some(scope));
        if let Err(e) = builder.execute(reader).await {
            tracing::error!(
                target: "hallouminate::lance",
                corpus = %corpus,
                files = batch.len(),
                error = %e,
                "LanceDB merge_insert failed; batch not written"
            );
            return Err(map_lance_err(e));
        }
        if let Err(e) = self.ensure_search_indexes().await {
            tracing::error!(
                target: "hallouminate::lance",
                corpus = %corpus,
                error = %e,
                "LanceDB search-index build failed after merge_insert"
            );
            return Err(e);
        }
        Ok(())
    }

    /// Build the FTS index on `text` (and the ANN index on `embedding`) if
    /// they don't already exist. LanceDB requires data to be present before
    /// some indexes can be created, so this runs after `merge_insert` —
    /// idempotent via `list_indices()`.
    ///
    /// `indexes_ensured` latches true only once every index this store will
    /// ever build is in place, after which the `list_indices()` round-trip is
    /// skipped on subsequent calls. In embeddings-ON mode the ANN index is
    /// only built once the corpus reaches the row threshold, so the latch
    /// stays open across early batches until that index materializes.
    async fn ensure_search_indexes(&self) -> Result<()> {
        if self.indexes_ensured.load(Ordering::Acquire) {
            return Ok(());
        }
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
        // Embeddings-OFF: the `embedding` column is all nulls, so there is
        // nothing to ANN-index. Skip entirely (spec: OFF mode builds no
        // vector index). The FTS index above is still built — it is the only
        // dense-independent signal the OFF path ranks on. Nothing else will
        // ever build, so latch.
        if !self.embeddings_enabled {
            self.indexes_ensured.store(true, Ordering::Release);
            return Ok(());
        }
        // Vector index is optional — small corpora work fine without it via
        // brute-force scan, and IVF-PQ needs enough rows for meaningful
        // training. Skip if row count is below a small threshold.
        let has_vec_index = existing
            .iter()
            .any(|i| i.columns.iter().any(|c| c == "embedding"));
        if has_vec_index {
            // Both FTS and ANN are present; no further index work remains.
            self.indexes_ensured.store(true, Ordering::Release);
            return Ok(());
        }
        let rows = self.table.count_rows(None).await.map_err(map_lance_err)? as u64;
        if rows >= 256 {
            match self
                .table
                .create_index(&["embedding"], lancedb::index::Index::Auto)
                .execute()
                .await
            {
                Ok(()) => self.indexes_ensured.store(true, Ordering::Release),
                Err(e) => {
                    // ANN index is an optimization, not a correctness
                    // requirement — brute-force scan still works. Log so
                    // operators can diagnose why ANN never kicked in. Leave
                    // the latch open so a later batch retries the build.
                    tracing::warn!(
                        target: "hallouminate::lance",
                        error = %e,
                        "failed to create ANN index on `embedding`; queries will brute-force scan"
                    );
                }
            }
        }
        Ok(())
    }

    /// True once the FTS (inverted) index on `text` exists. A full-text query
    /// against a table that has rows but no inverted index hard-errors in
    /// LanceDB, and `apply_batch` commits the rows (`merge_insert`) before it
    /// commits the index (`ensure_search_indexes`) — so a concurrent query can
    /// observe that in-between version. Callers guard on this and treat "index
    /// not built yet" as "no results" (a transient state during indexing)
    /// rather than surfacing the error.
    async fn has_text_index(&self) -> Result<bool> {
        let existing = self.table.list_indices().await.map_err(map_lance_err)?;
        Ok(existing
            .iter()
            .any(|i| i.columns.iter().any(|c| c == "text")))
    }

    /// Updates the stored `mtime_ms` for every row of `(corpus, file_ref)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB update fails.
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

    /// Look up the indexer snapshot for a single `(corpus, file_ref)`. Used
    /// by the MCP `add_markdown` handler so a re-write of an unchanged file
    /// can short-circuit re-embedding (route the file through the planner's
    /// `mtime_touches` path instead of `upserts`). Returns `None` when the
    /// file has never been indexed under this corpus.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB query fails or a returned column has an
    /// unexpected type.
    pub async fn get_file_snapshot(
        &self,
        corpus: &str,
        file_ref: &str,
    ) -> Result<Option<FileSnapshot>> {
        let predicate = format!(
            "corpus = '{}' AND file_ref = '{}' AND ord = 0",
            escape_sql_str(corpus),
            escape_sql_str(file_ref)
        );
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
            .limit(1)
            .execute()
            .await
            .map_err(map_lance_err)?;
        let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
        for rb in batches {
            if rb.num_rows() == 0 {
                continue;
            }
            let file_ref_col = string_col(&rb, "file_ref")?;
            let corpus_col = string_col(&rb, "corpus")?;
            let mtime_col = int64_col(&rb, "mtime_ms")?;
            let hash_col = string_col(&rb, "content_hash")?;
            return Ok(Some(FileSnapshot {
                file_ref: file_ref_col.value(0).to_string(),
                corpus: corpus_col.value(0).to_string(),
                mtime_ms: mtime_col.value(0),
                content_hash: hash_col.value(0).to_string(),
            }));
        }
        Ok(None)
    }

    /// Returns one `FileSnapshot` per indexed file in `corpus`. We rely on
    /// the invariant that every prepared file emits at least one chunk with
    /// `ord = 0` (enforced in the indexer's writer), which lets us push
    /// dedup into the store as an `ord = 0` filter instead of materializing
    /// one row per chunk and folding through a HashMap.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB query fails or a returned column has an
    /// unexpected type.
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

    /// Hybrid BM25 + vector search reranked with a `WeightedRRFReranker`
    /// biased toward FTS (see `weighted_rrf::FTS_WEIGHT` /
    /// `VECTOR_WEIGHT`), scoped to a single `corpus`. Returns an empty
    /// `Vec` for an empty corpus or when no rows match the corpus filter.
    ///
    /// # Errors
    ///
    /// Returns an error if `query_vec` is not [`EMBEDDING_DIM`] long, or if the
    /// LanceDB search, rerank, or row decode fails.
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
        if !self.has_text_index().await? {
            return Ok(Vec::new());
        }
        let reranker = std::sync::Arc::new(weighted_rrf::WeightedRRFReranker::default());
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

    /// BM25-only search, scoped to a single `corpus`. The embeddings-OFF
    /// sibling of `hybrid_search`: same FTS path, but no `.nearest_to()`
    /// vector leg, no reranker (there is only one signal to rank), and no
    /// query-vector dim guard. Returns an empty `Vec` for an empty corpus or
    /// when no rows match the corpus filter.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB full-text search or row decode fails.
    pub async fn fts_search(
        &self,
        corpus: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if self.count_rows().await? == 0 {
            return Ok(Vec::new());
        }
        if !self.has_text_index().await? {
            return Ok(Vec::new());
        }
        let corpus_filter = format!("corpus = '{}'", escape_sql_str(corpus));
        let stream = self
            .table
            .query()
            .only_if(corpus_filter)
            .full_text_search(lancedb::index::scalar::FullTextSearchQuery::new(
                query.to_string(),
            ))
            .limit(limit)
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
    let empty: Vec<std::result::Result<RecordBatch, arrow::error::ArrowError>> = Vec::new();
    let reader = RecordBatchIterator::new(empty.into_iter(), schema);
    let reader: Box<dyn arrow::array::RecordBatchReader + Send> = Box::new(reader);
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
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        let meta: Meta = toml::from_str(&text).unwrap();
        assert_eq!(meta.embedding_model_name, "BAAI/bge-small-en-v1.5");
        assert!(!meta.quantized);
        assert!(meta.embeddings_enabled);
        assert!(text.contains("schema_version"));
        assert!(text.contains("quantized"));
        assert!(text.contains("embeddings_enabled"));
        assert!(text.contains("auto-managed"));
    }

    #[test]
    fn meta_check_or_init_passes_when_existing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect("second call must succeed");
    }

    #[test]
    fn meta_check_or_init_errors_on_model_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let err = meta_check_or_init(&meta_path, "intfloat/multilingual-e5-small", false, true)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("BAAI/bge-small-en-v1.5"), "{msg}");
        assert!(msg.contains("intfloat/multilingual-e5-small"), "{msg}");
        assert!(msg.contains("delete"), "{msg}");
        assert!(msg.contains("hallouminate index"), "{msg}");
    }

    #[test]
    fn meta_check_or_init_errors_on_quantized_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", true, true)
            .expect_err("flipping quantized must invalidate the store");
        let msg = err.to_string();
        assert!(msg.contains("quantized"), "{msg}");
        assert!(msg.contains("delete"), "{msg}");
    }

    #[test]
    fn meta_check_or_init_errors_on_enabled_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect_err("flipping embeddings_enabled must invalidate the store");
        let msg = err.to_string();
        assert!(msg.contains("embeddings_enabled"), "{msg}");
        assert!(msg.contains("delete"), "{msg}");
    }

    #[test]
    fn meta_check_or_init_reads_pre_feature_sidecar_as_enabled_full_precision() {
        // A sidecar written before this feature has neither `quantized` nor
        // `embeddings_enabled`; it must read back as the mode it was built in
        // (ON, full precision) so a re-open under the same model + ON config
        // does NOT trip the mismatch guard.
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "BAAI/bge-small-en-v1.5"
schema_version = 3
"#,
        )
        .unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect("pre-feature sidecar must match ON + full-precision config");
    }

    #[test]
    fn meta_check_or_init_stale_on_schema_version_below_expected() {
        // A v1 store predates the frontmatter column. Opening it with this (v3)
        // build must return StoreSchemaStale (recoverable), not a fatal Config
        // error, so the daemon-open path can auto-rebuild instead of crashing.
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "BAAI/bge-small-en-v1.5"
quantized = false
embeddings_enabled = true
schema_version = 1
"#,
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect_err("a v1 store must be rejected by the v3 binary");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale {
                    found: 1,
                    expected: 3,
                    ..
                }
            ),
            "expected StoreSchemaStale, got: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("stale"), "{msg}");
        assert!(msg.contains('1'), "must name found version: {msg}");
        assert!(msg.contains('3'), "must name expected version: {msg}");
    }

    #[test]
    fn meta_check_or_init_stale_on_v2_store() {
        // A v2 store predates the per-chunk `claim_marks` column. Must return
        // StoreSchemaStale so the daemon-open path rebuilds rather than crashing.
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "BAAI/bge-small-en-v1.5"
quantized = false
embeddings_enabled = true
schema_version = 2
"#,
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect_err("a v2 store must be rejected by the v3 binary");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale {
                    found: 2,
                    expected: 3,
                    ..
                }
            ),
            "expected StoreSchemaStale, got: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains('2'), "must name the stored version: {msg}");
        assert!(msg.contains('3'), "must name the expected version: {msg}");
    }

    #[test]
    fn meta_check_or_init_roundtrips_current_schema_version() {
        // A store written by this build records the current version and
        // re-opens cleanly without tripping the schema guard.
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        let meta: Meta = toml::from_str(&text).unwrap();
        assert_eq!(meta.schema_version, default_schema_version());
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect("current-version store must re-open");
    }

    #[test]
    fn meta_check_or_init_rejects_unsupported_requested_model() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        let err = meta_check_or_init(&meta_path, "clip-vit-b32", false, true)
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
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect_err("corrupt sidecar must error");
        assert!(
            err.to_string().contains("unsupported embedding model"),
            "{err}"
        );
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
                "frontmatter",
                "indexed_at_ms",
                "ord",
                "heading_path",
                "line_start",
                "line_end",
                "text",
                "claim_marks",
                "embedding",
            ]
        );
    }

    #[test]
    fn chunks_schema_embedding_column_is_fixed_size_384_f32() {
        let schema = chunks_schema();
        let embedding = schema.field_with_name("embedding").unwrap();
        match embedding.data_type() {
            arrow::datatypes::DataType::FixedSizeList(child, dim) => {
                assert_eq!(*dim, EMBEDDING_DIM as i32, "expected 384, got {dim}");
                match child.data_type() {
                    arrow::datatypes::DataType::Float32 => {}
                    other => panic!("expected Float32 child, got {other:?}"),
                }
            }
            other => panic!("expected FixedSizeList, got {other:?}"),
        }
    }

    fn synthetic_prepared(file_ref: &str, chunks: usize) -> PreparedFile {
        let mut embeddings = Vec::new();
        let mut pf = PreparedFile {
            file_ref: file_ref.to_string(),
            corpus: "docs".into(),
            mtime_ms: 7,
            content_hash: "deadbeef".into(),
            summary: "summary".into(),
            keywords: vec!["k1".into(), "k2".into()],
            frontmatter: None,
            indexed_at_ms: 11,
            chunks: Vec::new(),
            embeddings: None,
        };
        for i in 0..chunks {
            pf.chunks.push(PreparedChunk {
                ord: i,
                heading_path: vec!["H".into()],
                line_start: 1,
                line_end: 2,
                text: format!("chunk-{i}"),
                claim_marks: None,
            });
            embeddings.push([0.0_f32; EMBEDDING_DIM]);
        }
        pf.embeddings = Some(embeddings);
        pf
    }

    /// Embeddings-OFF variant: same chunks, but `embeddings: None` so every
    /// row is written with a null vector.
    fn synthetic_prepared_no_embeddings(file_ref: &str, chunks: usize) -> PreparedFile {
        let mut pf = synthetic_prepared(file_ref, chunks);
        pf.embeddings = None;
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
        assert_eq!(rb.num_columns(), 16);
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
    fn build_record_batch_denormalizes_frontmatter_with_null_for_absent() {
        let mut with_fm = synthetic_prepared("/tmp/fm.md", 2);
        with_fm.frontmatter = Some(r#"{"status":"draft"}"#.to_string());
        let without_fm = synthetic_prepared("/tmp/plain.md", 1); // frontmatter: None
        let schema = chunks_schema();
        let rb = build_record_batch(&[with_fm, without_fm], schema).expect("build batch");
        let fm = rb
            .column_by_name("frontmatter")
            .expect("frontmatter column")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("frontmatter is utf8");
        // First two rows (fm.md chunks) carry the JSON; the third (plain.md) is null.
        assert!(!fm.is_null(0));
        assert_eq!(fm.value(0), r#"{"status":"draft"}"#);
        assert!(!fm.is_null(1));
        assert_eq!(fm.value(1), r#"{"status":"draft"}"#);
        assert!(
            fm.is_null(2),
            "absent frontmatter must be a null column value"
        );
        assert_eq!(fm.null_count(), 1);
    }

    #[test]
    fn build_record_batch_denormalizes_claim_marks_with_null_for_absent() {
        // Per-chunk (not per-file): only the chunk carrying marks gets the JSON;
        // the rest are null. Mirrors the frontmatter null-handling test but at
        // chunk granularity.
        let mut pf = synthetic_prepared("/tmp/marks.md", 3);
        pf.chunks[1].claim_marks =
            Some(r#"[{"status":"confirmed","line":2,"reference":null,"note":null}]"#.to_string());
        let schema = chunks_schema();
        let rb = build_record_batch(&[pf], schema).expect("build batch");
        let cm = rb
            .column_by_name("claim_marks")
            .expect("claim_marks column")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("claim_marks is utf8");
        assert!(cm.is_null(0), "chunk 0 has no marks → null");
        assert!(!cm.is_null(1), "chunk 1 carries the marks JSON");
        assert_eq!(
            cm.value(1),
            r#"[{"status":"confirmed","line":2,"reference":null,"note":null}]"#
        );
        assert!(cm.is_null(2), "chunk 2 has no marks → null");
        assert_eq!(cm.null_count(), 2);
    }

    #[test]
    fn build_record_batch_rejects_chunk_embedding_length_mismatch() {
        let mut pf = synthetic_prepared("/tmp/bad.md", 2);
        if let Some(v) = pf.embeddings.as_mut() {
            v.pop(); // 2 chunks, 1 embedding
        }
        let schema = chunks_schema();
        let err = build_record_batch(&[pf], schema).unwrap_err();
        assert!(
            err.to_string().contains("chunks but 1 embeddings"),
            "got: {err}"
        );
    }

    #[test]
    fn build_record_batch_off_mode_writes_null_embeddings_for_every_chunk() {
        let batch = vec![synthetic_prepared_no_embeddings("/tmp/off.md", 3)];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build OFF batch");
        assert_eq!(rb.num_rows(), 3);
        let embedding = rb.column_by_name("embedding").expect("embedding column");
        assert_eq!(
            embedding.null_count(),
            3,
            "every chunk row must carry a null embedding in OFF mode"
        );
        for i in 0..rb.num_rows() {
            assert!(embedding.is_null(i), "row {i} embedding must be null");
        }
    }

    #[test]
    fn build_record_batch_on_mode_has_no_null_embeddings() {
        let batch = vec![synthetic_prepared("/tmp/on.md", 3)];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build ON batch");
        let embedding = rb.column_by_name("embedding").expect("embedding column");
        assert_eq!(
            embedding.null_count(),
            0,
            "ON mode must write a real vector for every chunk"
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
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        assert!(meta_path.exists());
    }

    /// Re-indexing across more than one batch must produce the same row state
    /// whether or not the `indexes_ensured` latch short-circuits the
    /// `list_indices()` round-trip. The latch is an optimization: it must not
    /// change what ends up in the table. In embeddings-ON mode below the ANN
    /// row threshold the latch deliberately stays open (a later batch could
    /// cross the threshold and still need the vector index built), yet both
    /// files' rows must be present, durable, and searchable.
    #[tokio::test]
    async fn re_indexing_across_batches_keeps_rows_durable_with_indexes_built() {
        let dir = tempfile::tempdir().unwrap();
        let store = LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, true)
            .await
            .expect("open store");

        assert!(
            !store.indexes_ensured.load(Ordering::Acquire),
            "a fresh store must start with the index latch open"
        );

        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 3)])
            .await
            .expect("first batch");
        assert_eq!(store.count_rows().await.unwrap(), 3);
        assert!(
            !store.indexes_ensured.load(Ordering::Acquire),
            "ON mode below the ANN row threshold must NOT latch — a later, \
             larger batch still needs the chance to build the vector index"
        );

        store
            .apply_batch(vec![synthetic_prepared("/tmp/b.md", 2)])
            .await
            .expect("second batch");
        assert_eq!(
            store.count_rows().await.unwrap(),
            5,
            "second batch must still write its rows"
        );
        assert!(
            !store.indexes_ensured.load(Ordering::Acquire),
            "latch must stay open while the corpus is still below the ANN threshold"
        );

        let hits = store
            .fts_search("docs", "chunk-0", 10)
            .await
            .expect("fts search after re-index");
        assert!(
            !hits.is_empty(),
            "FTS must still return results after a multi-batch re-index"
        );
    }

    /// In embeddings-OFF mode the only index that will ever exist is FTS, so
    /// the very first successful batch latches `indexes_ensured`. The next
    /// batch then takes the cached path (no `list_indices()` round-trip) and
    /// must still write its rows — proving the latch short-circuits the check
    /// without altering what lands in the table.
    #[tokio::test]
    async fn off_mode_latches_after_first_batch_then_skips_list_indices() {
        let dir = tempfile::tempdir().unwrap();
        let store = LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false)
            .await
            .expect("open OFF-mode store");

        store
            .apply_batch(vec![synthetic_prepared_no_embeddings("/tmp/a.md", 3)])
            .await
            .expect("first OFF batch");
        assert!(
            store.indexes_ensured.load(Ordering::Acquire),
            "OFF mode must latch after the first batch builds FTS — nothing else can build"
        );

        store
            .apply_batch(vec![synthetic_prepared_no_embeddings("/tmp/b.md", 2)])
            .await
            .expect("second OFF batch on the cached path");
        assert_eq!(
            store.count_rows().await.unwrap(),
            5,
            "cached-path batch must still write its rows"
        );

        let hits = store
            .fts_search("docs", "chunk-1", 10)
            .await
            .expect("fts search after cached re-index");
        assert!(
            !hits.is_empty(),
            "FTS must still return results after the cached-path batch"
        );
    }

    /// Regression (#106 /press): searching a corpus that has ZERO rows in a
    /// POPULATED store must return an empty hit list, not error. LanceDB can
    /// return an empty result whose schema columns are projected away; before
    /// the `decode_hits` zero-row guard this surfaced as
    /// `Indexer("missing column chunk_id")` and crashed the whole call. The
    /// cross-repo union ground fans across every effective corpus, so a single
    /// empty / unindexed sub-repo wiki would otherwise take down results from
    /// every other repo. Both the ON-mode (`hybrid_search`) and OFF-mode
    /// (`fts_search`) decode paths must tolerate it.
    #[tokio::test]
    async fn searching_an_empty_corpus_in_a_populated_store_returns_no_hits_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, true)
            .await
            .expect("open store");
        // Populate the store under corpus "docs" so count_rows() > 0 and the
        // FTS index exists — the empty-corpus query then hits the real search
        // path rather than the early count_rows()==0 / no-index short-circuits.
        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 3)])
            .await
            .expect("seed docs corpus");
        assert!(
            store.count_rows().await.unwrap() > 0,
            "store must be populated"
        );

        // OFF-mode decode path.
        let fts = store
            .fts_search("repo:empty:wiki", "chunk", 10)
            .await
            .expect("fts on an empty corpus must not error");
        assert!(
            fts.is_empty(),
            "a zero-row corpus must yield no FTS hits, got {}",
            fts.len()
        );

        // ON-mode decode path: a well-formed (zero) query vector exercises the
        // hybrid query + decode without needing a real embedding model.
        let query_vec = [0.0_f32; EMBEDDING_DIM];
        let hybrid = store
            .hybrid_search("repo:empty:wiki", "chunk", &query_vec, 10)
            .await
            .expect("hybrid on an empty corpus must not error");
        assert!(
            hybrid.is_empty(),
            "a zero-row corpus must yield no hybrid hits, got {}",
            hybrid.len()
        );
    }

    // ─── T7: unit guard classifies schema-version direction ──────────────────

    #[test]
    fn guard_stale_when_stored_version_below_expected() {
        // A store written at schema_version < default_schema_version() must
        // return StoreSchemaStale so the daemon-open path can auto-rebuild.
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        let stale_version = default_schema_version() - 1;
        std::fs::write(
            &meta_path,
            format!(
                "# auto-managed by hallouminate; do not edit\n\
                 embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
                 quantized = false\n\
                 embeddings_enabled = false\n\
                 schema_version = {stale_version}\n"
            ),
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect_err("stale store must be rejected");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale { found, expected, .. }
                    if found == stale_version && expected == default_schema_version()
            ),
            "expected StoreSchemaStale, got: {err}"
        );
    }

    #[test]
    fn guard_ok_when_stored_version_equals_expected() {
        // Exact version match: guard must pass (== branch falls through).
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            format!(
                "# auto-managed by hallouminate; do not edit\n\
                 embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
                 quantized = false\n\
                 embeddings_enabled = false\n\
                 schema_version = {}\n",
                default_schema_version()
            ),
        )
        .unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect("matching version must succeed");
    }

    #[test]
    fn guard_fatal_config_when_stored_version_above_expected() {
        // A store from a NEWER binary (downgrade scenario) must fail loud + fatal
        // with a Config error. The original store dir must be untouched (no .bak).
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        let newer_version = default_schema_version() + 1;
        std::fs::write(
            &meta_path,
            format!(
                "# auto-managed by hallouminate; do not edit\n\
                 embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
                 quantized = false\n\
                 embeddings_enabled = false\n\
                 schema_version = {newer_version}\n"
            ),
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect_err("newer store must be rejected with a fatal error");
        assert!(
            matches!(err, HallouminateError::Config(_)),
            "expected Config (downgrade fatal), got: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("NEWER"), "must say NEWER: {msg}");
        assert!(
            msg.to_lowercase().contains("upgrade"),
            "must advise upgrade: {msg}"
        );
    }
}

/// Weighted Reciprocal Rank Fusion reranker for hybrid (BM25 + vector)
/// search. Identical to LanceDB's stock `RRFReranker` except each ranked
/// list contributes a per-source multiplier on top of the standard
/// `1 / (k + rank)` term, letting us bias fusion toward FTS when the
/// embedding model is generic and the corpus is keyword-heavy.
mod weighted_rrf {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use arrow::array::downcast_array;
    use arrow::array::{Float32Array, RecordBatch, UInt64Array};
    use arrow::compute::{SortOptions, sort_to_indices, take};
    use arrow::datatypes::{DataType, Field, Schema};
    use async_trait::async_trait;
    use lancedb::rerankers::Reranker;

    /// Matches `lance::dataset::ROW_ID` — hardcoded so we don't pull
    /// `lance-core` into our dep graph just for one `&str` constant.
    const ROW_ID: &str = "_rowid";
    /// Column name LanceDB's hybrid pipeline requires the reranker to
    /// emit. Mirrors `lancedb::rerankers::RELEVANCE_SCORE`, which is
    /// private; if LanceDB ever renames it, `check_reranker_result`
    /// raises a `Schema` error and tests fail loudly.
    const RELEVANCE_SCORE: &str = "_relevance_score";

    /// FTS rank gets twice the weight of vector rank in the fused score.
    /// Picked because BM25 over our short markdown chunks beats the
    /// generic `bge-small-en-v1.5` embeddings on distinctive-token
    /// queries (the e2e oracles in `tests/fixture_e2e.rs` rely entirely
    /// on the FTS path under the stub embedder).
    pub const FTS_WEIGHT: f32 = 2.0;
    /// Baseline weight for the vector (ANN) ranked list; FTS is biased above
    /// it via [`FTS_WEIGHT`].
    pub const VECTOR_WEIGHT: f32 = 1.0;
    /// RRF dampening constant; 60 matches Cormack et al. and LanceDB's
    /// stock default.
    pub const K: f32 = 60.0;

    #[derive(Debug)]
    pub struct WeightedRRFReranker {
        k: f32,
        fts_weight: f32,
        vector_weight: f32,
    }

    impl Default for WeightedRRFReranker {
        fn default() -> Self {
            Self {
                k: K,
                fts_weight: FTS_WEIGHT,
                vector_weight: VECTOR_WEIGHT,
            }
        }
    }

    #[async_trait]
    impl Reranker for WeightedRRFReranker {
        async fn rerank_hybrid(
            &self,
            _query: &str,
            vector_results: RecordBatch,
            fts_results: RecordBatch,
        ) -> lancedb::Result<RecordBatch> {
            let vector_ids: UInt64Array = downcast_array(
                vector_results
                    .column_by_name(ROW_ID)
                    .ok_or_else(|| missing_row_id("vector_results", &vector_results))?,
            );
            let fts_ids: UInt64Array = downcast_array(
                fts_results
                    .column_by_name(ROW_ID)
                    .ok_or_else(|| missing_row_id("fts_results", &fts_results))?,
            );

            let mut scores: BTreeMap<u64, f32> = BTreeMap::new();
            accumulate(&mut scores, &vector_ids, self.vector_weight, self.k);
            accumulate(&mut scores, &fts_ids, self.fts_weight, self.k);

            let combined = self.merge_results(vector_results, fts_results)?;
            let combined_row_ids: UInt64Array = downcast_array(
                combined
                    .column_by_name(ROW_ID)
                    .ok_or_else(|| missing_row_id("merged results", &combined))?,
            );

            let relevance_scores = Float32Array::from_iter_values(
                combined_row_ids
                    .values()
                    .iter()
                    .map(|row_id| *scores.get(row_id).unwrap_or(&0.0)),
            );

            let sort_indices = sort_to_indices(
                &relevance_scores,
                Some(SortOptions {
                    descending: true,
                    ..Default::default()
                }),
                None,
            )?;

            let mut columns: Vec<Arc<dyn arrow::array::Array>> = combined.columns().to_vec();
            columns.push(Arc::new(relevance_scores));
            let columns: Vec<Arc<dyn arrow::array::Array>> = columns
                .iter()
                .map(|c| take(c, &sort_indices, None))
                .collect::<arrow::error::Result<_>>()?;

            let mut fields = combined.schema().fields().to_vec();
            fields.push(Arc::new(Field::new(
                RELEVANCE_SCORE,
                DataType::Float32,
                false,
            )));
            let schema = Schema::new(fields);

            Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
        }
    }

    fn accumulate(scores: &mut BTreeMap<u64, f32>, ids: &UInt64Array, weight: f32, k: f32) {
        for (rank, row_id) in ids.values().iter().enumerate() {
            let contribution = weight / (rank as f32 + k);
            scores
                .entry(*row_id)
                .and_modify(|s| *s += contribution)
                .or_insert(contribution);
        }
    }

    fn missing_row_id(which: &str, batch: &RecordBatch) -> lancedb::Error {
        let schema = batch.schema();
        let cols: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        lancedb::Error::InvalidInput {
            message: format!("expected column {ROW_ID} not found in {which}; found {cols:?}"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use arrow::array::StringArray;

        fn batch(ids: &[u64], names: &[&str]) -> RecordBatch {
            let schema = Arc::new(Schema::new(vec![
                Field::new("name", DataType::Utf8, false),
                Field::new(ROW_ID, DataType::UInt64, false),
            ]));
            RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(StringArray::from(names.to_vec())),
                    Arc::new(UInt64Array::from(ids.to_vec())),
                ],
            )
            .unwrap()
        }

        /// FTS-only hit at rank 0 must beat a vector-only hit at rank 0
        /// once the weight bias is applied. With equal weights the two
        /// would tie; with FTS_WEIGHT > VECTOR_WEIGHT the FTS row wins.
        #[tokio::test]
        async fn fts_only_hit_outranks_vector_only_hit_at_same_rank() {
            let vec_results = batch(&[1], &["vec_only"]);
            let fts_results = batch(&[2], &["fts_only"]);
            let reranker = WeightedRRFReranker::default();
            let out = reranker
                .rerank_hybrid("q", vec_results, fts_results)
                .await
                .unwrap();
            let names: StringArray = downcast_array(out.column(0));
            let names: Vec<&str> = names.iter().map(|n| n.unwrap()).collect();
            assert_eq!(
                names[0], "fts_only",
                "FTS-weighted RRF must rank fts_only first"
            );
            assert_eq!(names[1], "vec_only");
        }

        /// Row in both lists must beat either list's solo top hit (the
        /// weight bias must not invert RRF's core property of rewarding
        /// agreement).
        #[tokio::test]
        async fn row_in_both_lists_beats_solo_hits() {
            let vec_results = batch(&[1, 2], &["solo_vec", "shared"]);
            let fts_results = batch(&[3, 2], &["solo_fts", "shared"]);
            let reranker = WeightedRRFReranker::default();
            let out = reranker
                .rerank_hybrid("q", vec_results, fts_results)
                .await
                .unwrap();
            let names: StringArray = downcast_array(out.column(0));
            let names: Vec<&str> = names.iter().map(|n| n.unwrap()).collect();
            assert_eq!(names[0], "shared");
        }

        /// Output must carry the `_relevance_score` column LanceDB's
        /// hybrid pipeline validates via `check_reranker_result`.
        #[tokio::test]
        async fn output_includes_relevance_score_column() {
            let vec_results = batch(&[1], &["a"]);
            let fts_results = batch(&[1], &["a"]);
            let reranker = WeightedRRFReranker::default();
            let out = reranker
                .rerank_hybrid("q", vec_results, fts_results)
                .await
                .unwrap();
            assert!(
                out.schema().column_with_name(RELEVANCE_SCORE).is_some(),
                "schema must expose {RELEVANCE_SCORE}"
            );
        }
    }
}
