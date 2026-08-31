use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;
// Re-exported from `lancedb` rather than depended on directly: the two must be
// the same `arrow` build or every `RecordBatch` we hand to LanceDB is a
// different type than the one it expects.
use lancedb::arrow::arrow;
use lancedb::arrow::arrow::array::ListArray;
use lancedb::arrow::arrow::array::builder::{ListBuilder, StringBuilder};
use lancedb::arrow::arrow::array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use lancedb::arrow::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};

use crate::embedder::{EMBEDDING_DIM, EmbedBatch, EmbedRole};
use hallouminate_domain::common::{CorpusKey, HallouminateError, Result, RetiredRoot};
use hallouminate_domain::corpus::ClaimMark;
use hallouminate_domain::embeddings::canonical_model_name;
use hallouminate_domain::indexer::{
    BatchWriteStats, ChunkStore, FileSnapshot, PreparedFile, SearchHit, SignalLists,
};
use hallouminate_domain::search::ChunkRetrieval;

const TABLE_NAME: &str = "chunks";
const META_FILENAME: &str = "meta.toml";
/// Single-owner lockfile inside the ground dir — see [`acquire_store_lock`].
const STORE_LOCK_FILENAME: &str = "store.lock";
/// Separate advisory guard proving that `store.lock` metadata is current.
const STORE_LOCK_DIAGNOSTICS_FILENAME: &str = "store.lock.diagnostics";

/// Diagnostic metadata written into a successfully acquired store lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreLockOwner {
    pid: u32,
    socket: Option<PathBuf>,
    version: String,
}

impl StoreLockOwner {
    /// Metadata for a direct, non-daemon store owner.
    pub fn for_process() -> Self {
        Self {
            pid: std::process::id(),
            socket: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Metadata for a daemon-owned store.
    pub fn for_daemon(socket: PathBuf) -> Self {
        Self {
            socket: Some(socket),
            ..Self::for_process()
        }
    }
}

/// Aggregate statistics for one corpus key, returned by [`LanceStore::corpus_chunk_stats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusChunkStats {
    pub indexed_files: u64,
    pub total_chunks: u64,
    pub last_indexed_ms: Option<i64>,
}

/// Configures one LanceDB maintenance pass.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceOptions {
    /// Correlates adapter stage events with the daemon's lifecycle event.
    pub maintenance_id: u64,
    pub prune_older_than: Duration,
    /// Bounds one compaction pass to at most this many source fragments
    /// (ADR daemon-rework-001 paced mode: a `Pace::Paced` slice runs a
    /// bounded chunk of the backlog instead of the whole thing in one shot).
    /// `None` preserves today's unbounded behaviour.
    pub max_fragments_per_slice: Option<usize>,
}

/// Reports the effects of one LanceDB maintenance pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceStats {
    pub fragments_removed: Option<usize>,
    pub fragments_added: Option<usize>,
    pub old_versions_pruned: Option<u64>,
}

/// Real backlog signals read from LanceDB metadata, feeding the daemon's
/// maintenance-debt ladder (ADR daemon-rework-001). Cheap: table stats +
/// version list, no compaction/prune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanceDebt {
    pub fragments: u64,
    pub stale_versions: u64,
}

/// Rounds a prune-retention `Duration` to whole seconds for LanceDB's
/// `Duration::seconds` (i64), saturating instead of wrapping on overflow.
/// A non-zero sub-second remainder rounds up so a sub-second grace window
/// never truncates to zero-retention pruning.
fn prune_retention_secs(d: Duration) -> i64 {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Stable, deterministic chunk identifier derived from (file_ref, ord).
///
/// Same (file_ref, ord) → same chunk_id; as part of the merge key
/// `(corpus, root, chunk_id)`, this overwrites the same logical chunk on re-index
/// and orphan-drops chunks beyond the new ord range.
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
/// `chunks` schema changes shape (v2 added `frontmatter`; v3 added
/// `claim_marks`; v4 added canonical `root` and derived `search_text`).
/// Also the serde default, though every managed store records this field.
fn default_schema_version() -> u32 {
    4
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
    // Write-then-rename: readers never see a partial meta.toml (rename is
    // atomic within the same directory/filesystem), and the pid-unique tmp
    // name keeps concurrent first-inits from interleaving writes into a
    // shared tmp file.
    let tmp_path = meta_path.with_file_name(format!(
        "{META_FILENAME}.{pid}.tmp",
        pid = std::process::id()
    ));
    std::fs::write(&tmp_path, toml_text)?;
    std::fs::rename(&tmp_path, meta_path)?;
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
        Field::new("root", DataType::Utf8, false),
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
        Field::new("search_text", DataType::Utf8, false),
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

/// Pairs a prepared file with its (optional) per-chunk embeddings for
/// [`build_record_batch`]. `PreparedFile` no longer carries embeddings
/// itself (US-002: embeddings are adapter-owned).
struct FileWithEmbeddings<'a> {
    file: &'a PreparedFile,
    embeddings: Option<&'a [[f32; EMBEDDING_DIM]]>,
}

fn build_record_batch(batch: &[FileWithEmbeddings], schema: SchemaRef) -> Result<RecordBatch> {
    let mut chunk_ids: Vec<String> = Vec::new();
    let mut file_refs: Vec<String> = Vec::new();
    let mut corpora: Vec<String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
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
    let mut search_texts: Vec<String> = Vec::new();
    let mut claim_marks: Vec<Option<String>> = Vec::new();
    let mut embeddings_flat: Vec<f32> = Vec::new();
    // One validity bit per chunk row: true = real vector, false = null
    // (embeddings-OFF mode). Stays all-true on the ON path so the null
    // buffer is dropped entirely and the column is byte-identical to before.
    let mut embedding_valid: Vec<bool> = Vec::new();

    for fwe in batch {
        if let Some(embeddings) = &fwe.embeddings
            && fwe.file.chunks.len() != embeddings.len()
        {
            return Err(HallouminateError::Indexer(format!(
                "prepared file {:?}: {} chunks but {} embeddings",
                fwe.file.file_ref,
                fwe.file.chunks.len(),
                embeddings.len()
            )));
        }
        let Some(root) = fwe.file.corpus_key.canonical_root.to_str() else {
            return Err(HallouminateError::Indexer(format!(
                "canonical corpus root is not valid UTF-8: {}",
                fwe.file.corpus_key.canonical_root.display()
            )));
        };
        for (idx, chunk) in fwe.file.chunks.iter().enumerate() {
            chunk_ids.push(chunk_id_for(&fwe.file.file_ref, chunk.ord));
            file_refs.push(fwe.file.file_ref.clone());
            corpora.push(fwe.file.corpus_key.name.clone());
            roots.push(root.to_string());
            mtimes.push(fwe.file.mtime_ms);
            hashes.push(fwe.file.content_hash.clone());
            summaries.push(fwe.file.summary.clone());
            keywords.push(fwe.file.keywords.clone());
            frontmatters.push(fwe.file.frontmatter.clone());
            indexed_at.push(fwe.file.indexed_at_ms);
            ords.push(chunk.ord as i64);
            heading_paths.push(chunk.heading_path.clone());
            line_starts.push(chunk.line_start as i64);
            line_ends.push(chunk.line_end as i64);
            texts.push(chunk.text.clone());
            search_texts.push(chunk.search_text.clone());
            claim_marks.push(chunk.claim_marks.clone());
            match &fwe.embeddings {
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
        Arc::new(StringArray::from(roots)),
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
        Arc::new(StringArray::from(search_texts)),
        Arc::new(StringArray::from_iter(claim_marks)),
        Arc::new(embedding_array),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| HallouminateError::Indexer(format!("build record batch: {e}")))
}

struct DonorRow {
    ord: i64,
    vector: Option<[f32; EMBEDDING_DIM]>,
}

/// (corpus, root, file_ref) identity of one donor row-group.
type DonorGroupKey = (String, String, String);
/// Donor rows grouped by content_hash, then by row-group identity.
type DonorGroups = HashMap<String, HashMap<DonorGroupKey, Vec<DonorRow>>>;

fn decode_donor_rows(batches: &[RecordBatch]) -> Result<DonorGroups> {
    let mut by_hash: DonorGroups = HashMap::new();
    for rb in batches {
        if rb.num_rows() == 0 {
            continue;
        }
        let content_hashes = string_col(rb, "content_hash")?;
        let corpora = string_col(rb, "corpus")?;
        let roots = string_col(rb, "root")?;
        let file_refs = string_col(rb, "file_ref")?;
        let ords = int64_col(rb, "ord")?;
        let Some(embedding_column) = rb.column_by_name("embedding") else {
            return Err(HallouminateError::Indexer(
                "missing column embedding".into(),
            ));
        };
        let Some(embedding_column) = embedding_column
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
        else {
            return Err(HallouminateError::Indexer(
                "embedding column not a fixed-size list".into(),
            ));
        };
        for row in 0..rb.num_rows() {
            let key = (
                corpora.value(row).to_string(),
                roots.value(row).to_string(),
                file_refs.value(row).to_string(),
            );
            let vector = if embedding_column.is_null(row) {
                None
            } else {
                let values = embedding_column.value(row);
                let Some(floats) = values.as_any().downcast_ref::<Float32Array>() else {
                    return Err(HallouminateError::Indexer(
                        "embedding item column not float32".into(),
                    ));
                };
                let mut vector = [0.0_f32; EMBEDDING_DIM];
                for (slot, target) in vector.iter_mut().enumerate() {
                    *target = floats.value(slot);
                }
                Some(vector)
            };
            by_hash
                .entry(content_hashes.value(row).to_string())
                .or_default()
                .entry(key)
                .or_default()
                .push(DonorRow {
                    ord: ords.value(row),
                    vector,
                });
        }
    }
    Ok(by_hash)
}

fn pick_donor_vectors(
    groups: &HashMap<DonorGroupKey, Vec<DonorRow>>,
    chunk_count: usize,
) -> Option<Vec<[f32; EMBEDDING_DIM]>> {
    let mut keys: Vec<&DonorGroupKey> = Vec::new();
    for key in groups.keys() {
        keys.push(key);
    }
    keys.sort();

    for key in keys {
        let rows = &groups[key];
        if rows.len() != chunk_count {
            continue;
        }
        let mut sorted: Vec<&DonorRow> = Vec::new();
        for row in rows {
            sorted.push(row);
        }
        sorted.sort_by_key(|row| row.ord);
        debug_assert!(
            sorted
                .iter()
                .enumerate()
                .all(|(i, row)| row.ord == i as i64),
            "donor row-group ords must be exactly 0..chunk_count",
        );

        let mut vectors: Vec<[f32; EMBEDDING_DIM]> = Vec::with_capacity(chunk_count);
        for row in sorted {
            match row.vector {
                Some(vector) => vectors.push(vector),
                None => break,
            }
        }
        if vectors.len() == chunk_count {
            return Some(vectors);
        }
    }
    None
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

fn decode_hits(rb: &RecordBatch, corpus_key: &CorpusKey, out: &mut Vec<SearchHit>) -> Result<()> {
    // A zero-row batch contributes no hits and may carry a projected-away
    // schema (LanceDB can return an empty result whose columns are absent when
    // a corpus-key filter matches nothing in a populated store). Demanding
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
    let search_text = string_col(rb, "search_text")?;
    let line_start = int64_col(rb, "line_start")?;
    let line_end = int64_col(rb, "line_end")?;
    let mtime_ms = int64_col(rb, "mtime_ms")?;
    let heading_path = list_utf8_col(rb, "heading_path")?;
    let keywords = list_utf8_col(rb, "keywords")?;
    let claim_marks = string_col(rb, "claim_marks")?;
    for i in 0..rb.num_rows() {
        out.push(SearchHit {
            chunk_id: chunk_id.value(i).to_string(),
            corpus_key: corpus_key.clone(),
            file_ref: file_ref.value(i).to_string(),
            heading_path: decode_list(heading_path, i),
            line_start: line_start.value(i) as usize,
            line_end: line_end.value(i) as usize,
            text: text.value(i).to_string(),
            search_text: search_text.value(i).to_string(),
            summary: summary.value(i).to_string(),
            keywords: decode_list(keywords, i),
            // Per-signal scores (FTS relevance, higher-better; vector
            // distance, lower-better) are mutually incomparable and never
            // read pre-fusion — `domain::search` overwrites every hit's
            // `score` with the fused RRF value before a caller sees it. Zero
            // here rather than let either meaning cross the `ChunkStore`
            // port under one field.
            score: 0.0,
            mtime_ms: mtime_ms.value(i),
            claim_marks: decode_claim_marks(claim_marks, i),
            z_score: None,
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

fn corpus_key_filter(corpus_key: &CorpusKey) -> Result<String> {
    let Some(root) = corpus_key.canonical_root.to_str() else {
        return Err(HallouminateError::Indexer(format!(
            "canonical corpus root is not valid UTF-8: {}",
            corpus_key.canonical_root.display()
        )));
    };
    Ok(format!(
        "corpus = '{}' AND root = '{}'",
        escape_sql_str(&corpus_key.name),
        escape_sql_str(root)
    ))
}

fn corpus_and_file_ref_filter(corpus_key: &CorpusKey, refs: &[String]) -> Result<String> {
    Ok(format!(
        "{} AND {}",
        corpus_key_filter(corpus_key)?,
        file_ref_in_filter(refs)
    ))
}

fn map_lance_err<E: std::fmt::Display>(e: E) -> HallouminateError {
    HallouminateError::Db(Box::new(std::io::Error::other(format!("lance: {e}"))))
}

/// Runs one lance scan on its own supervised task so a death inside lance's
/// scan machinery is observed as a `JoinError` and mapped to a failed result
/// (a failed RPC at the daemon boundary) instead of crashing the daemon.
/// lance 8.0.0's `filtered_read.rs:426` unwraps an internal `JoinError` and
/// panics when its scan task is cancelled -- observed at shutdown when
/// `abort_all` fires (#223, ADR daemon-rework-006).
/// A supervised scan runs to completion even when the caller's future is
/// dropped -- deliberate: caller-side cancellation reaching lance's scan
/// task is exactly what trips the upstream unwrap.
async fn supervise_scan<T: Send + 'static>(
    op: &'static str,
    scan: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    match tokio::spawn(scan).await {
        Ok(result) => result,
        Err(join_error) => Err(scan_join_error(op, join_error)),
    }
}

/// Maps a scan task's `JoinError` to the adapter error type, logging a panic
/// payload at error level with the scan `op` so the trace carries enough
/// context to file upstream (ADR daemon-rework-006).
fn scan_join_error(op: &str, join_error: tokio::task::JoinError) -> HallouminateError {
    if join_error.is_panic() {
        let payload = join_error.into_panic();
        let panic_msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        tracing::error!(
            target: "hallouminate::lance",
            op,
            panic_payload = %panic_msg,
            "lance scan task panicked (#223: lance filtered_read unwraps JoinError); surfacing as failed RPC"
        );
        map_lance_err(format!("scan '{op}' panicked: {panic_msg}"))
    } else {
        tracing::error!(
            target: "hallouminate::lance",
            op,
            "lance scan task cancelled before completion; surfacing as failed RPC"
        );
        map_lance_err(format!("scan '{op}' cancelled"))
    }
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
    /// Mirrors the store's `embeddings_enabled` identity. When false, the
    /// `embedding` column is all nulls, so `ensure_search_indexes` skips the
    /// ANN index entirely (there is nothing to vector-search).
    embeddings_enabled: bool,
    /// Latches true once `ensure_search_indexes` has confirmed the search
    /// indexes exist, letting later `apply_batch` calls skip the
    /// `list_indices()` round-trip. The table is created once per instance,
    /// so a fresh `LanceStore` always starts unlatched.
    indexes_ensured: AtomicBool,
    /// Latches true once `has_text_index` has observed the `text` FTS index
    /// present, letting later `hybrid_search`/`fts_search` calls skip the
    /// `list_indices()` round-trip on every query. Set unconditionally by
    /// `ensure_search_indexes` too, since that call guarantees the FTS index
    /// exists (unlike the row-gated ANN index) once it returns `Ok`.
    text_index_present: AtomicBool,
    /// Exclusive store lock plus its diagnostic freshness guard, held for this
    /// store's lifetime. Dropping the guard before the store lock means a
    /// contender either observes current metadata or omits it.
    _dir_lock: StoreLocks,
    /// Owns query/passage embedding when embeddings are enabled. The shared
    /// synchronous mutex is acquired only on Tokio's blocking pool, keeping
    /// model access serial without blocking either runtime flavor's workers.
    embedder: Arc<std::sync::Mutex<Option<Box<dyn EmbedBatch>>>>,
    embedder_available: AtomicBool,
}

/// How long [`acquire_store_lock`] retries a contended `flock` before giving
/// up. Sized to absorb the deferred-`fput` release window (sub-second, see the
/// fn doc) while staying far below any genuine second-daemon lifetime, so real
/// single-ownership violations still fail closed.
const STORE_LOCK_RETRY_BUDGET: Duration = Duration::from_secs(2);
const STORE_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Store ownership and its metadata-freshness guard. Field declaration order
/// drops the diagnostics guard first, so no contender can pair stale metadata
/// with a newly acquired store lock.
struct StoreLocks {
    _diagnostics_lock: std::fs::File,
    _store_lock: std::fs::File,
}

/// One non-blocking `flock` attempt on `store.lock`. `Ok(None)` means the lock
/// is currently held (`WOULDBLOCK`); any other errno is a distinct failure.
fn try_acquire_store_lock(ground_dir: &Path, owner: &StoreLockOwner) -> Result<Option<StoreLocks>> {
    use std::io::{Seek, Write};
    use std::os::unix::fs::OpenOptionsExt;

    use rustix::fs::{FlockOperation, flock};

    let lock_path = ground_dir.join(STORE_LOCK_FILENAME);
    let mut store_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    match flock(&store_lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let diagnostics_lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(ground_dir.join(STORE_LOCK_DIAGNOSTICS_FILENAME))?;
            match flock(&diagnostics_lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(errno) if errno == rustix::io::Errno::WOULDBLOCK => return Ok(None),
                Err(errno) => {
                    return Err(HallouminateError::Config(format!(
                        "failed to guard lock diagnostics for {}: {}",
                        ground_dir.display(),
                        std::io::Error::from(errno),
                    )));
                }
            }
            let metadata = serde_json::to_vec(owner).map_err(|error| {
                HallouminateError::Config(format!("serialize store lock owner: {error}"))
            })?;
            store_lock.set_len(0)?;
            store_lock.rewind()?;
            store_lock.write_all(&metadata)?;
            store_lock.sync_data()?;
            Ok(Some(StoreLocks {
                _diagnostics_lock: diagnostics_lock,
                _store_lock: store_lock,
            }))
        }
        Err(errno) if errno == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(errno) => Err(HallouminateError::Config(format!(
            "failed to lock {}: {}",
            ground_dir.display(),
            std::io::Error::from(errno),
        ))),
    }
}

/// Return metadata only while the current store holder also holds its
/// diagnostics guard. A raw holder or a release/acquisition transition leaves
/// the guard available, so stale bytes never become a false owner report.
fn contended_store_lock_owner(ground_dir: &Path) -> Option<StoreLockOwner> {
    use std::os::unix::fs::OpenOptionsExt;

    use rustix::fs::{FlockOperation, flock};

    let diagnostics_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(ground_dir.join(STORE_LOCK_DIAGNOSTICS_FILENAME))
        .ok()?;
    match flock(&diagnostics_lock, FlockOperation::NonBlockingLockExclusive) {
        Err(errno) if errno == rustix::io::Errno::WOULDBLOCK => {
            let lock_path = ground_dir.join(STORE_LOCK_FILENAME);
            std::fs::read(lock_path)
                .ok()
                .and_then(|contents| serde_json::from_slice(&contents).ok())
        }
        Ok(()) | Err(_) => None,
    }
}

/// Take a non-blocking exclusive `flock` on `store.lock` inside the ground
/// dir, binding single ownership to the store rather than to any daemon
/// socket path.
///
/// The daemon's per-socket lock is not enough (#204): socket resolution is
/// environment-dependent (`XDG_RUNTIME_DIR` present vs the `~/.cache`
/// fallback), so two daemons spawned from different environments each held
/// their own socket lock and co-owned this directory — and their interleaved
/// maintenance/write commits deleted data files the other's manifest still
/// referenced. Locking here covers every open path (daemon boot baseline,
/// per-request `resources_for` builds, and any future direct open).
///
/// A single non-blocking attempt is not enough: when *this* process closes a
/// store and immediately reopens the same ground dir, the kernel releases the
/// advisory lock during deferred `fput` (`task_work`/`delayed_fput`), so the
/// reopen can momentarily race the release and see `WOULDBLOCK` even though no
/// other owner exists — reproduced on macOS CI (os error 35). We retry for
/// [`STORE_LOCK_RETRY_BUDGET`] to absorb that window; a genuinely concurrent
/// second daemon holds the lock for its whole lifetime, far past the budget,
/// so the #204 single-owner guarantee still fails closed.
async fn acquire_store_lock(ground_dir: &Path, owner: &StoreLockOwner) -> Result<StoreLocks> {
    let deadline = tokio::time::Instant::now() + STORE_LOCK_RETRY_BUDGET;
    loop {
        match try_acquire_store_lock(ground_dir, owner)? {
            Some(locks) => return Ok(locks),
            None if tokio::time::Instant::now() >= deadline => {
                let owner = contended_store_lock_owner(ground_dir)
                    .map(|owner| {
                        format!(
                            " lock owner pid={} socket={} version={}",
                            owner.pid,
                            owner
                                .socket
                                .as_deref()
                                .map(|socket| socket.display().to_string())
                                .unwrap_or_else(|| "<direct>".to_string()),
                            owner.version,
                        )
                    })
                    .unwrap_or_default();
                return Err(HallouminateError::Config(format!(
                    "ground store {} is locked by another hallouminate process; every ground dir has exactly one owner — stop the other daemon (hallouminate daemon stop) or point [storage].ground_dir elsewhere before retrying.{owner}",
                    ground_dir.display(),
                )));
            }
            None => tokio::time::sleep(STORE_LOCK_RETRY_INTERVAL).await,
        }
    }
}

async fn run_embedding_blocking(
    embedder: Arc<std::sync::Mutex<Option<Box<dyn EmbedBatch>>>>,
    texts: Vec<String>,
    role: EmbedRole,
) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
    match tokio::task::spawn_blocking(move || {
        let mut guard = embedder
            .lock()
            .map_err(|_| HallouminateError::Embed("embedding model lock poisoned".into()))?;
        let Some(embedder) = guard.as_mut() else {
            return Err(HallouminateError::Embed(
                "embeddings are enabled but the embedding model is unavailable; retry the request"
                    .into(),
            ));
        };
        embedder.embed_batch(&texts, role)
    })
    .await
    {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(HallouminateError::Embed(format!(
            "embedding task failed: {error}"
        ))),
    }
}
impl LanceStore {
    /// Validates an existing store's metadata without opening LanceDB or
    /// taking ownership of an embedder. A missing store has nothing to
    /// validate and is accepted.
    ///
    /// # Errors
    ///
    /// Delegates to `meta_check_or_init`, so an existing store can fail for any
    /// of these reasons:
    /// - `StoreSchemaStale` — the stored schema version is older than this
    ///   build expects (remedy: delete + reindex).
    /// - `Config` — the stored schema version is newer than this build expects
    ///   (fatal downgrade), or `meta.toml` cannot be parsed.
    /// - `Embed` — the requested embedding configuration (model, quantization,
    ///   or enabled flag) mismatches the stored sidecar.
    /// - the requested model is unsupported, or the stored model name is
    ///   corrupt (via `canonical_model_name`).
    pub fn validate_existing_metadata(
        ground_dir: &Path,
        model_name: &str,
        quantized: bool,
        embeddings_enabled: bool,
    ) -> Result<()> {
        let meta_path = ground_dir.join(META_FILENAME);
        if meta_path.exists() {
            meta_check_or_init(&meta_path, model_name, quantized, embeddings_enabled)?;
        }
        Ok(())
    }

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
        embedder: Option<Box<dyn EmbedBatch>>,
    ) -> Result<Self> {
        Self::open_or_create_with_owner(
            ground_dir,
            model_name,
            quantized,
            embeddings_enabled,
            embedder,
            StoreLockOwner::for_process(),
        )
        .await
    }

    /// Open a store while recording the supplied diagnostic lock owner.
    pub async fn open_or_create_with_owner(
        ground_dir: &Path,
        model_name: &str,
        quantized: bool,
        embeddings_enabled: bool,
        embedder: Option<Box<dyn EmbedBatch>>,
        owner: StoreLockOwner,
    ) -> Result<Self> {
        std::fs::create_dir_all(ground_dir)?;
        let meta_path = ground_dir.join(META_FILENAME);
        meta_check_or_init(&meta_path, model_name, quantized, embeddings_enabled)?;
        let dir_lock = acquire_store_lock(ground_dir, &owner).await?;
        let uri = ground_dir.to_str().ok_or_else(|| {
            HallouminateError::Config(format!("non-utf8 ground dir: {}", ground_dir.display()))
        })?;
        let connection = lancedb::connect(uri)
            .execute()
            .await
            .map_err(map_lance_err)?;
        let table = open_or_create_table(&connection).await?;
        let embedder_available = embedder.is_some();
        Ok(Self {
            table,
            embeddings_enabled,
            indexes_ensured: AtomicBool::new(false),
            text_index_present: AtomicBool::new(false),
            _dir_lock: dir_lock,
            embedder: Arc::new(std::sync::Mutex::new(embedder)),
            embedder_available: AtomicBool::new(embedder_available),
        })
    }

    /// Returns whether this enabled store currently owns a usable embedder.
    pub fn embedder_available(&self) -> bool {
        self.embedder_available.load(Ordering::Acquire)
    }

    /// Installs an embedder after a transient initialization failure.
    ///
    /// # Errors
    ///
    /// Returns an error if the model lock was poisoned.
    pub fn install_embedder(&self, embedder: Box<dyn EmbedBatch>) -> Result<()> {
        let mut guard = self
            .embedder
            .lock()
            .map_err(|_| HallouminateError::Embed("embedding model lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some(embedder);
            self.embedder_available.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Returns the total number of chunk rows in the table.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB count query fails.
    pub async fn count_rows(&self) -> Result<u64> {
        let table = self.table.clone();
        supervise_scan("count_rows", async move {
            table.count_rows(None).await.map_err(map_lance_err)
        })
        .await
        .map(|n| n as u64)
    }

    /// Compacts small fragments into larger ones and prunes superseded
    /// dataset versions, keeping on-disk growth bounded across the
    /// long-running daemon's lifetime. Every `merge_insert` (`apply_batch`),
    /// `update` (`touch_mtime`), and `delete` (`delete_file`) call retains a
    /// fragment + a version, so an unmaintained store accumulates both
    /// without limit.
    ///
    /// `options.prune_older_than` bounds how far back pruning reaches rather than
    /// always reclaiming every superseded version: this `LanceStore` is
    /// always the single writer for its table (see the daemon's write-lane
    /// invariant), so no concurrent *process* can have an older version
    /// checked out for time travel -- but an in-process query stream that
    /// snapshotted a version just before this call runs does not hold the
    /// write lane, so a zero-duration prune could delete files out from
    /// under it mid-scan. Passing a grace window (e.g. the daemon's
    /// `MAINTENANCE_PRUNE_GRACE_SECS`) leaves recently-superseded versions
    /// in place long enough for such in-flight queries to drain.
    ///
    /// LanceDB's optimize result reports fragment and version counts but no
    /// reliable bytes-read or bytes-written counters, so maintenance events
    /// intentionally omit byte fields rather than report estimates.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB compaction or version-cleanup fails.
    pub async fn maintain(&self, options: MaintenanceOptions) -> Result<MaintenanceStats> {
        tracing::debug!(
            target: "hallouminate::lance",
            maintenance_event = "compaction_started",
            maintenance_id = options.maintenance_id,
            "LanceDB maintenance compaction started",
        );
        let compact = self
            .table
            .optimize(lancedb::table::OptimizeAction::Compact {
                options: lancedb::table::CompactionOptions {
                    max_source_fragments: options.max_fragments_per_slice,
                    ..Default::default()
                },
                remap_options: None,
            })
            .await;
        let mut stats = match compact {
            Ok(stats) => {
                tracing::debug!(
                    target: "hallouminate::lance",
                    maintenance_event = "compaction_finished",
                    maintenance_id = options.maintenance_id,
                    outcome = "success",
                    fragments_removed = stats
                        .compaction
                        .as_ref()
                        .map(|stats| stats.fragments_removed),
                    fragments_added = stats
                        .compaction
                        .as_ref()
                        .map(|stats| stats.fragments_added),
                    "LanceDB maintenance compaction completed",
                );
                stats
            }
            Err(error) => {
                let error = map_lance_err(error);
                tracing::warn!(
                    target: "hallouminate::lance",
                    maintenance_event = "compaction_finished",
                    maintenance_id = options.maintenance_id,
                    outcome = "failure",
                    error = %error,
                    "LanceDB maintenance compaction failed",
                );
                return Err(error);
            }
        };

        tracing::debug!(
            target: "hallouminate::lance",
            maintenance_event = "prune_started",
            maintenance_id = options.maintenance_id,
            "LanceDB maintenance prune started",
        );
        let prune_secs = prune_retention_secs(options.prune_older_than);
        let prune = self
            .table
            .optimize(lancedb::table::OptimizeAction::Prune {
                older_than: Some(lancedb::table::Duration::seconds(prune_secs)),
                delete_unverified: Some(true),
                error_if_tagged_old_versions: None,
            })
            .await;
        let prune = match prune {
            Ok(prune) => {
                tracing::debug!(
                    target: "hallouminate::lance",
                    maintenance_event = "prune_finished",
                    maintenance_id = options.maintenance_id,
                    outcome = "success",
                    old_versions_pruned = prune.prune.as_ref().map(|stats| stats.old_versions),
                    "LanceDB maintenance prune completed",
                );
                prune
            }
            Err(error) => {
                let error = map_lance_err(error);
                tracing::warn!(
                    target: "hallouminate::lance",
                    maintenance_event = "prune_finished",
                    maintenance_id = options.maintenance_id,
                    outcome = "failure",
                    error = %error,
                    "LanceDB maintenance prune failed",
                );
                return Err(error);
            }
        };
        stats.prune = prune.prune;
        Ok(MaintenanceStats {
            fragments_removed: stats
                .compaction
                .as_ref()
                .map(|stats| stats.fragments_removed),
            fragments_added: stats.compaction.as_ref().map(|stats| stats.fragments_added),
            old_versions_pruned: stats.prune.as_ref().map(|stats| stats.old_versions),
        })
    }

    /// Reads real backlog signals used by the daemon's maintenance-debt ladder
    /// (ADR daemon-rework-001): fragment count from table statistics, and stale
    /// (superseded) dataset version count from version history. Cheap relative
    /// to a full maintenance pass -- no compaction or pruning runs.
    ///
    /// # Errors
    /// Returns an error if the LanceDB stats or version-listing call fails.
    pub async fn debt(&self) -> Result<LanceDebt> {
        let stats = self.table.stats().await.map_err(map_lance_err)?;
        let versions = self.table.list_versions().await.map_err(map_lance_err)?;
        Ok(LanceDebt {
            fragments: stats.fragment_stats.num_fragments as u64,
            stale_versions: versions.len().saturating_sub(1) as u64,
        })
    }

    pub async fn corpus_chunk_stats(&self, corpus_key: &CorpusKey) -> Result<CorpusChunkStats> {
        let key_filter = corpus_key_filter(corpus_key)?;
        let table = self.table.clone();
        let count_predicate = key_filter.clone();
        let total_chunks = supervise_scan("corpus_chunk_stats_count", async move {
            table
                .count_rows(Some(count_predicate))
                .await
                .map_err(map_lance_err)
        })
        .await? as u64;
        let predicate = format!("{key_filter} AND ord = 0");
        let table = self.table.clone();
        let batches = supervise_scan("corpus_chunk_stats", async move {
            let stream = table
                .query()
                .only_if(predicate)
                .select(lancedb::query::Select::columns(&["indexed_at_ms"]))
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await?;
        let mut indexed_files: u64 = 0;
        let mut last_indexed_ms: Option<i64> = None;
        for rb in &batches {
            indexed_files += rb.num_rows() as u64;
            if rb.num_rows() > 0 {
                let col = int64_col(rb, "indexed_at_ms")?;
                for i in 0..rb.num_rows() {
                    let v = col.value(i);
                    last_indexed_ms = Some(match last_indexed_ms {
                        None => v,
                        Some(cur) => cur.max(v),
                    });
                }
            }
        }
        Ok(CorpusChunkStats {
            indexed_files,
            total_chunks,
            last_indexed_ms,
        })
    }

    /// Upserts a batch of prepared files, embedding their chunk `search_text`
    /// when this store owns an [`Embedder`]. All files in a single call MUST
    /// share one [`CorpusKey`]; the orphan-drop predicate is scoped to that
    /// exact name-and-root identity. The merge join key is
    /// `(corpus, root, chunk_id)`, so sibling roots retain independent rows.
    ///
    /// Before embedding, each file's `content_hash` is checked against any
    /// existing rows in the store (any corpus/root — one store carries one
    /// embedding model). A donor row-group with an equal chunk count and
    /// non-null vectors is copied ord-aligned instead of re-embedding.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch mixes corpus keys, when the embedder
    /// returns a mismatched vector count, when building the Arrow record
    /// batch fails, or when LanceDB write or index maintenance fails.
    async fn apply_batch(&self, batch: Vec<PreparedFile>) -> Result<BatchWriteStats> {
        if batch.is_empty() {
            return Ok(BatchWriteStats::default());
        }
        let corpus_key = batch[0].corpus_key.clone();
        if batch.iter().any(|file| file.corpus_key != corpus_key) {
            return Err(HallouminateError::Indexer(
                "apply_batch: all PreparedFiles in a batch must share the same corpus key".into(),
            ));
        }

        let mut content_hashes: Vec<String> = Vec::new();
        if self.embeddings_enabled {
            for file in &batch {
                content_hashes.push(file.content_hash.clone());
            }
        }
        let donor_groups = self.donor_vectors_batch(&content_hashes).await?;

        let mut donor_vectors: Vec<Option<Vec<[f32; EMBEDDING_DIM]>>> =
            Vec::with_capacity(batch.len());
        for file in &batch {
            let donor = match donor_groups.get(&file.content_hash) {
                Some(groups) => pick_donor_vectors(groups, file.chunks.len()),
                None => None,
            };
            donor_vectors.push(donor);
        }

        let mut all_texts: Vec<String> = Vec::new();
        let mut splits: Vec<usize> = Vec::with_capacity(batch.len());
        for i in 0..batch.len() {
            if donor_vectors[i].is_some() {
                splits.push(0);
                continue;
            }
            let file = &batch[i];
            splits.push(file.chunks.len());
            for c in &file.chunks {
                all_texts.push(c.search_text.clone());
            }
        }

        let mut stats = BatchWriteStats::default();
        let mut file_embeddings: Vec<Option<Vec<[f32; EMBEDDING_DIM]>>> =
            Vec::with_capacity(batch.len());
        if self.embeddings_enabled {
            let expected = all_texts.len();
            let mut vectors = if all_texts.is_empty() {
                Vec::new()
            } else {
                run_embedding_blocking(Arc::clone(&self.embedder), all_texts, EmbedRole::Passage)
                    .await?
            };
            if vectors.len() != expected {
                return Err(HallouminateError::Indexer(format!(
                    "embedder returned {} vectors for {} chunks",
                    vectors.len(),
                    expected
                )));
            }
            let mut iter = vectors.drain(..);
            for i in 0..batch.len() {
                if let Some(donor) = donor_vectors[i].take() {
                    stats.chunks_written += donor.len();
                    file_embeddings.push(Some(donor));
                    continue;
                }
                let count = splits[i];
                let mut buf: Vec<[f32; EMBEDDING_DIM]> = Vec::with_capacity(count);
                for _ in 0..count {
                    let v = iter.next().ok_or_else(|| {
                        HallouminateError::Indexer("embedding count drained early".into())
                    })?;
                    buf.push(v);
                }
                stats.chunks_written += count;
                stats.embeddings_written += count;
                file_embeddings.push(Some(buf));
            }
        } else {
            for count in splits.iter().copied() {
                stats.chunks_written += count;
                file_embeddings.push(None);
            }
        }

        let paired: Vec<FileWithEmbeddings> = batch
            .iter()
            .zip(file_embeddings.iter())
            .map(|(file, embeddings)| FileWithEmbeddings {
                file,
                embeddings: embeddings.as_deref(),
            })
            .collect();
        let schema = chunks_schema();
        let record_batch = build_record_batch(&paired, schema.clone())?;
        let mut file_refs: Vec<String> = Vec::with_capacity(batch.len());
        for file in &batch {
            file_refs.push(file.file_ref.clone());
        }
        let scope = corpus_and_file_ref_filter(&corpus_key, &file_refs)?;
        let existing_indices = self.table.list_indices().await.map_err(map_lance_err)?;
        let had_text_index = existing_indices.iter().any(|index| {
            index.index_type == lancedb::index::IndexType::FTS
                && index.columns.iter().any(|column| column == "search_text")
        });
        let reader = RecordBatchIterator::new(std::iter::once(Ok(record_batch)), schema);
        let reader: Box<dyn arrow::array::RecordBatchReader + Send> = Box::new(reader);
        let mut builder = self.table.merge_insert(&["corpus", "root", "chunk_id"]);
        builder
            .when_matched_update_all(None)
            .when_not_matched_insert_all()
            .when_not_matched_by_source_delete(Some(scope));
        if let Err(e) = builder.execute(reader).await {
            tracing::error!(
                target: "hallouminate::lance",
                corpus = %corpus_key.name,
                root = %corpus_key.canonical_root.display(),
                files = batch.len(),
                error = %e,
                "LanceDB merge_insert failed; batch not written"
            );
            return Err(map_lance_err(e));
        }
        if had_text_index
            && let Err(e) = self
                .table
                .optimize(lancedb::table::OptimizeAction::Index(Default::default()))
                .await
        {
            tracing::error!(
                target: "hallouminate::lance",
                corpus = %corpus_key.name,
                root = %corpus_key.canonical_root.display(),
                error = %e,
                "LanceDB search-index refresh failed after merge_insert"
            );
            return Err(map_lance_err(e));
        }
        if let Err(e) = self.ensure_search_indexes().await {
            tracing::error!(
                target: "hallouminate::lance",
                corpus = %corpus_key.name,
                root = %corpus_key.canonical_root.display(),
                error = %e,
                "LanceDB search-index build failed after merge_insert"
            );
            return Err(e);
        }
        Ok(stats)
    }

    /// Looks up donor row-groups for every distinct `content_hash` in one
    /// batch with a single filtered table scan (`content_hash IN (...)`),
    /// across any corpus/root in the store. Returns each hash's candidate
    /// groups, keyed by `(corpus, root, file_ref)`, for the caller to pick
    /// from via [`pick_donor_vectors`]: ord-aligned vectors when exactly one
    /// qualifying group exists (chunk count equal to the file's chunk count
    /// and every row carrying a non-null vector), first-qualifying-group-wins
    /// in `(corpus, root, file_ref)` order, groups never blended.
    async fn donor_vectors_batch(&self, content_hashes: &[String]) -> Result<DonorGroups> {
        if content_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let mut distinct: HashSet<&str> = HashSet::new();
        for hash in content_hashes {
            distinct.insert(hash.as_str());
        }
        let mut escaped: Vec<String> = Vec::with_capacity(distinct.len());
        for hash in distinct {
            escaped.push(format!("'{}'", escape_sql_str(hash)));
        }
        let predicate = format!("content_hash IN ({})", escaped.join(", "));
        let table = self.table.clone();
        let batches = supervise_scan("apply_batch_donor_lookup", async move {
            let stream = table
                .query()
                .only_if(predicate)
                .select(lancedb::query::Select::columns(&[
                    "content_hash",
                    "corpus",
                    "root",
                    "file_ref",
                    "ord",
                    "embedding",
                ]))
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await?;
        decode_donor_rows(&batches)
    }

    /// Build the FTS index on `search_text` (and the ANN index on `embedding`) if
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
        let has_text_index = existing.iter().any(|i| {
            i.index_type == lancedb::index::IndexType::FTS
                && i.columns.iter().any(|c| c == "search_text")
        });
        if !has_text_index {
            self.table
                .create_index(
                    &["search_text"],
                    lancedb::index::Index::FTS(Default::default()),
                )
                .execute()
                .await
                .map_err(map_lance_err)?;
        }
        // FM-Index is built unconditionally on `search_text` too (mirrors FTS,
        // not row-gated like the ANN index below) — it backs the `contains`
        // signal, which joins the other retrieval signals as a peer ranked
        // list in `domain::search`'s fusion pass. Its `index_type` must be
        // checked (not just the column), since an FTS index also lives on
        // `search_text` and `columns.iter().any(...)` alone can't tell them
        // apart.
        let has_fm_index = existing.iter().any(|i| {
            i.index_type == lancedb::index::IndexType::Fm
                && i.columns.iter().any(|c| c == "search_text")
        });
        if !has_fm_index {
            self.table
                .create_index(
                    &["search_text"],
                    lancedb::index::Index::Fm(Default::default()),
                )
                .name("search_text_fm_idx".to_string())
                .execute()
                .await
                .map_err(map_lance_err)?;
        }
        // FTS is unconditionally ensured above (unlike the row-gated ANN
        // index below), so it is guaranteed present at this point — latch it
        // so `has_text_index` callers skip the `list_indices()` round-trip.
        self.text_index_present.store(true, Ordering::Release);
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

    /// True once the FTS (inverted) index on `search_text` exists. A full-text
    /// query against a table that has rows but no inverted index hard-errors
    /// in LanceDB, and `apply_batch` commits the rows (`merge_insert`) before
    /// it commits the index (`ensure_search_indexes`) — so a concurrent query
    /// can observe that in-between version. Callers guard on this and treat
    /// "index not built yet" as "no results" (a transient state during
    /// indexing) rather than surfacing the error.
    /// Latched: once `true`, stays `true` for the life of this `LanceStore`
    /// -- the FTS index is never dropped, so a cached hit skips the
    /// `list_indices` round-trip on every later query.
    async fn has_text_index(&self) -> Result<bool> {
        if self.text_index_present.load(Ordering::Acquire) {
            return Ok(true);
        }
        let existing = self.table.list_indices().await.map_err(map_lance_err)?;
        let present = existing.iter().any(|i| {
            i.index_type == lancedb::index::IndexType::FTS
                && i.columns.iter().any(|c| c == "search_text")
        });
        if present {
            self.text_index_present.store(true, Ordering::Release);
        }
        Ok(present)
    }

    /// Updates the stored `mtime_ms` for every row of `(corpus, root, file_ref)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB update fails.
    pub async fn touch_mtime(
        &self,
        corpus_key: &CorpusKey,
        file_ref: &str,
        new_mtime_ms: i64,
    ) -> Result<()> {
        let predicate = format!(
            "{} AND file_ref = '{}'",
            corpus_key_filter(corpus_key)?,
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

    pub async fn delete_file(&self, corpus_key: &CorpusKey, file_ref: &str) -> Result<()> {
        let predicate = format!(
            "{} AND file_ref = '{}'",
            corpus_key_filter(corpus_key)?,
            escape_sql_str(file_ref)
        );
        self.table.delete(&predicate).await.map_err(map_lance_err)?;
        Ok(())
    }

    /// Distinct `root` values present in the chunks table, as stored
    /// (canonical form). No caller-supplied scope: enumerates every root
    /// machine-wide, independent of any `CorpusKey`.
    ///
    /// # Errors
    /// Returns an error if the LanceDB scan fails.
    pub async fn distinct_roots(&self) -> Result<Vec<PathBuf>> {
        let table = self.table.clone();
        let batches = supervise_scan("distinct_roots", async move {
            let stream = table
                .query()
                // Every indexed file has an `ord = 0` row (enforced in the
                // indexer's writer, see `corpus_chunk_stats` above), so
                // filtering to it cuts row volume by the chunks-per-file
                // factor without losing any distinct root.
                .only_if("ord = 0")
                .select(lancedb::query::Select::columns(&["root"]))
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await?;
        let mut roots: HashSet<String> = HashSet::new();
        for rb in &batches {
            let col = string_col(rb, "root")?;
            for i in 0..rb.num_rows() {
                let v = col.value(i);
                if !roots.contains(v) {
                    roots.insert(v.to_string());
                }
            }
        }
        Ok(roots.into_iter().map(PathBuf::from).collect())
    }

    /// Deletes every row at `root`, across all corpora sharing that root.
    /// Predicate is `root = '<escaped>'` alone -- deliberately not scoped by
    /// `corpus_key_filter`, since a retired root orphans every corpus at it.
    /// Takes `&RetiredRoot` -- the caller must have run `root` through
    /// `retired_roots` first, which is the entire safety proof for this
    /// irreversible, machine-wide delete.
    ///
    /// # Errors
    /// Returns an error if `root` is not valid UTF-8 or the LanceDB delete call fails.
    pub async fn delete_root(&self, root: &RetiredRoot) -> Result<u64> {
        let Some(root_str) = root.as_path().to_str() else {
            return Err(HallouminateError::Indexer(format!(
                "root is not valid UTF-8: {}",
                root.as_path().display()
            )));
        };
        let predicate = format!("root = '{}'", escape_sql_str(root_str));
        let table = self.table.clone();
        supervise_scan("delete_root", async move {
            table.delete(&predicate).await.map_err(map_lance_err)
        })
        .await
        .map(|result| result.num_deleted_rows)
    }

    /// Looks up the indexer snapshot for a single `(corpus, root, file_ref)`.
    /// Used by the MCP `add_markdown` handler so a re-write of an unchanged
    /// file can short-circuit re-embedding (route the file through the planner's
    /// `mtime_touches` path instead of `upserts`). Returns `None` when the file
    /// has never been indexed under this corpus key.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB query fails or a returned column has an
    /// unexpected type.
    pub async fn get_file_snapshot(
        &self,
        corpus_key: &CorpusKey,
        file_ref: &str,
    ) -> Result<Option<FileSnapshot>> {
        let predicate = format!(
            "{} AND file_ref = '{}' AND ord = 0",
            corpus_key_filter(corpus_key)?,
            escape_sql_str(file_ref)
        );
        let table = self.table.clone();
        let batches = supervise_scan("get_file_snapshot", async move {
            let stream = table
                .query()
                .only_if(predicate)
                .select(lancedb::query::Select::columns(&[
                    "file_ref",
                    "mtime_ms",
                    "content_hash",
                ]))
                .limit(1)
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await?;
        for rb in batches {
            if rb.num_rows() == 0 {
                continue;
            }
            let file_ref_col = string_col(&rb, "file_ref")?;
            let mtime_col = int64_col(&rb, "mtime_ms")?;
            let hash_col = string_col(&rb, "content_hash")?;
            return Ok(Some(FileSnapshot {
                file_ref: file_ref_col.value(0).to_string(),
                corpus_key: corpus_key.clone(),
                mtime_ms: mtime_col.value(0),
                content_hash: hash_col.value(0).to_string(),
            }));
        }
        Ok(None)
    }

    /// Returns one `FileSnapshot` per indexed file in `corpus_key`. We rely on
    /// the invariant that every prepared file emits at least one chunk with
    /// `ord = 0` (enforced in the indexer's writer), which lets us push
    /// dedup into the store as an `ord = 0` filter instead of materializing
    /// one row per chunk and folding through a HashMap.
    ///
    /// # Errors
    ///
    /// Returns an error if the LanceDB query fails or a returned column has an
    /// unexpected type.
    async fn list_files(&self, corpus_key: &CorpusKey) -> Result<Vec<FileSnapshot>> {
        let predicate = format!("{} AND ord = 0", corpus_key_filter(corpus_key)?);
        let table = self.table.clone();
        let batches = supervise_scan("list_files", async move {
            let stream = table
                .query()
                .only_if(predicate)
                .select(lancedb::query::Select::columns(&[
                    "file_ref",
                    "mtime_ms",
                    "content_hash",
                ]))
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await?;
        let mut out: Vec<FileSnapshot> = Vec::new();
        for rb in batches {
            let file_ref_col = string_col(&rb, "file_ref")?;
            let mtime_col = int64_col(&rb, "mtime_ms")?;
            let hash_col = string_col(&rb, "content_hash")?;
            for i in 0..rb.num_rows() {
                out.push(FileSnapshot {
                    file_ref: file_ref_col.value(i).to_string(),
                    corpus_key: corpus_key.clone(),
                    mtime_ms: mtime_col.value(i),
                    content_hash: hash_col.value(i).to_string(),
                });
            }
        }
        Ok(out)
    }

    /// Retrieve the FTS and vector ranked lists separately, unfused. Runs
    /// both signals concurrently; the vector list is empty when this store
    /// has no embedder. Scoped to one exact corpus key. Returns empty lists
    /// for an empty key or when no rows match its name-and-root filter.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding the query fails, or if either LanceDB
    /// scan or row decode fails.
    async fn retrieve_signals(
        &self,
        corpus_key: &CorpusKey,
        query: &str,
        limit: usize,
    ) -> Result<SignalLists> {
        if !self.has_text_index().await? {
            return Ok(SignalLists::default());
        }
        let corpus_filter = corpus_key_filter(corpus_key)?;

        let (fts_batches, vector_batches) = tokio::try_join!(
            self.fts_scan(corpus_filter.clone(), query.to_string(), limit),
            self.vector_scan(corpus_filter, query.to_string(), limit),
        )?;

        let mut fts_hits: Vec<SearchHit> = Vec::new();
        for rb in &fts_batches {
            decode_hits(rb, corpus_key, &mut fts_hits)?;
        }
        let mut vector_hits: Vec<SearchHit> = Vec::new();
        for rb in &vector_batches {
            decode_hits(rb, corpus_key, &mut vector_hits)?;
        }

        let mut hits: HashMap<String, SearchHit> = HashMap::new();
        let fts = index_signal(fts_hits, &mut hits);
        let vector = index_signal(vector_hits, &mut hits);

        Ok(SignalLists { fts, vector, hits })
    }

    /// Full-text scan of `search_text` under `filter`, best `limit` hits
    /// first. Runs concurrently with `vector_scan` under `try_join!` in
    /// `retrieve_signals`.
    async fn fts_scan(
        &self,
        filter: String,
        query: String,
        limit: usize,
    ) -> Result<Vec<RecordBatch>> {
        let table = self.table.clone();
        supervise_scan("fts_search", async move {
            let stream = table
                .query()
                .only_if(filter)
                .full_text_search(lancedb::index::scalar::FullTextSearchQuery::new(query))
                .limit(limit)
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await
    }

    /// Nearest-neighbour scan under `filter`, best `limit` hits first. Empty
    /// when this store has no embedder. Runs concurrently with `fts_scan`
    /// under `try_join!` in `retrieve_signals`.
    async fn vector_scan(
        &self,
        filter: String,
        query: String,
        limit: usize,
    ) -> Result<Vec<RecordBatch>> {
        if !self.embeddings_enabled {
            return Ok(Vec::new());
        }
        let embedder = Arc::clone(&self.embedder);
        let vectors = run_embedding_blocking(embedder, vec![query], EmbedRole::Query).await?;
        let query_vec = vectors.into_iter().next().ok_or_else(|| {
            HallouminateError::Embed("embed_batch returned no vector for query".into())
        })?;
        let table = self.table.clone();
        supervise_scan("vector_search", async move {
            let stream = table
                .query()
                .only_if(filter)
                .nearest_to(&query_vec[..])
                .map_err(map_lance_err)?
                .limit(limit)
                .execute()
                .await
                .map_err(map_lance_err)?;
            let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(map_lance_err)?;
            Ok(batches)
        })
        .await
    }
}

/// Index one signal's hits into the shared pool, keyed by `chunk_id`,
/// keeping the first (best-ranked) occurrence on a duplicate. Returns the
/// chunk_ids in rank order, deduplicated.
fn index_signal(hits: Vec<SearchHit>, pool: &mut HashMap<String, SearchHit>) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for hit in hits {
        if seen.insert(hit.chunk_id.clone()) {
            order.push(hit.chunk_id.clone());
        }
        pool.entry(hit.chunk_id.clone()).or_insert(hit);
    }
    order
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

#[async_trait]
impl ChunkStore for LanceStore {
    async fn list_files(&self, corpus_key: &CorpusKey) -> Result<Vec<FileSnapshot>> {
        LanceStore::list_files(self, corpus_key).await
    }

    async fn touch_mtime(
        &self,
        corpus_key: &CorpusKey,
        file_ref: &str,
        mtime_ms: i64,
    ) -> Result<()> {
        LanceStore::touch_mtime(self, corpus_key, file_ref, mtime_ms).await
    }

    async fn delete_file(&self, corpus_key: &CorpusKey, file_ref: &str) -> Result<()> {
        LanceStore::delete_file(self, corpus_key, file_ref).await
    }

    async fn distinct_roots(&self) -> Result<Vec<PathBuf>> {
        LanceStore::distinct_roots(self).await
    }

    async fn delete_root(&self, root: &RetiredRoot) -> Result<u64> {
        LanceStore::delete_root(self, root).await
    }

    async fn apply_batch(&self, files: Vec<PreparedFile>) -> Result<BatchWriteStats> {
        LanceStore::apply_batch(self, files).await
    }
}

#[async_trait]
impl ChunkRetrieval for LanceStore {
    async fn retrieve_signals(
        &self,
        corpus_key: &CorpusKey,
        query: &str,
        limit: usize,
    ) -> Result<SignalLists> {
        LanceStore::retrieve_signals(self, corpus_key, query, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hallouminate_domain::indexer::PreparedChunk;

    fn corpus_key(name: &str, root: &str) -> CorpusKey {
        CorpusKey::from_configured_root(name, root)
    }

    fn docs_key() -> CorpusKey {
        corpus_key("docs", "/tmp")
    }

    #[test]
    fn store_lock_owner_serializes_daemon_diagnostics() {
        let owner = StoreLockOwner::for_daemon(PathBuf::from("/tmp/daemon.sock"));
        let decoded: StoreLockOwner =
            serde_json::from_slice(&serde_json::to_vec(&owner).expect("serialize")).expect("parse");
        assert_eq!(decoded.socket, Some(PathBuf::from("/tmp/daemon.sock")));
        assert_eq!(decoded.version, env!("CARGO_PKG_VERSION"));
        assert!(decoded.pid > 0);
    }

    #[test]
    fn acquired_store_lock_publishes_its_owner_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let owner = StoreLockOwner::for_daemon(temp.path().join("daemon.sock"));

        let _locks = try_acquire_store_lock(temp.path(), &owner)
            .expect("acquire")
            .expect("uncontended lock");
        assert_eq!(contended_store_lock_owner(temp.path()), Some(owner));
    }

    #[test]
    fn contended_store_lock_reports_the_current_owner() {
        let temp = tempfile::tempdir().expect("tempdir");
        let owner = StoreLockOwner::for_daemon(temp.path().join("daemon.sock"));

        let _locks = try_acquire_store_lock(temp.path(), &owner)
            .expect("acquire")
            .expect("uncontended lock");
        assert!(
            try_acquire_store_lock(temp.path(), &StoreLockOwner::for_process())
                .expect("contend")
                .is_none()
        );
        assert_eq!(contended_store_lock_owner(temp.path()), Some(owner));
    }

    #[test]
    fn diagnostics_reader_during_store_lock_transition_retries() {
        use rustix::fs::{FlockOperation, flock};
        use std::os::unix::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let diagnostics_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(temp.path().join(STORE_LOCK_DIAGNOSTICS_FILENAME))
            .expect("open diagnostics lock");
        flock(&diagnostics_lock, FlockOperation::NonBlockingLockExclusive)
            .expect("hold diagnostics lock");

        assert!(
            try_acquire_store_lock(temp.path(), &StoreLockOwner::for_process())
                .expect("diagnostics reader only delays acquisition")
                .is_none()
        );
        drop(diagnostics_lock);

        assert!(
            try_acquire_store_lock(temp.path(), &StoreLockOwner::for_process())
                .expect("retry after diagnostics reader")
                .is_some()
        );
    }

    #[test]
    fn malformed_contended_owner_metadata_is_omitted() {
        use std::io::{Seek, Write};

        let temp = tempfile::tempdir().expect("tempdir");
        let owner = StoreLockOwner::for_process();
        let _locks = try_acquire_store_lock(temp.path(), &owner)
            .expect("acquire")
            .expect("uncontended lock");
        let mut metadata = std::fs::OpenOptions::new()
            .write(true)
            .open(temp.path().join(STORE_LOCK_FILENAME))
            .expect("open metadata");
        metadata.set_len(0).expect("clear metadata");
        metadata.rewind().expect("rewind metadata");
        metadata
            .write_all(b"not json")
            .expect("write malformed metadata");

        assert_eq!(contended_store_lock_owner(temp.path()), None);
    }

    #[test]
    fn stale_owner_metadata_is_not_reported_for_an_uninstrumented_holder() {
        use rustix::fs::{FlockOperation, flock};
        use std::os::unix::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join(STORE_LOCK_FILENAME);
        let stale = StoreLockOwner::for_daemon(temp.path().join("stale.sock"));
        std::fs::write(
            &lock_path,
            serde_json::to_vec(&stale).expect("serialize stale"),
        )
        .expect("write stale metadata");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .expect("open lock");
        flock(&lock, FlockOperation::NonBlockingLockExclusive).expect("hold raw store lock");

        assert_eq!(contended_store_lock_owner(temp.path()), None);
        drop(lock);
    }

    #[test]
    fn prune_retention_secs_rounds_up_sub_second_remainder() {
        assert_eq!(prune_retention_secs(Duration::ZERO), 0);
        assert_eq!(prune_retention_secs(Duration::from_secs(5)), 5);
        assert_eq!(prune_retention_secs(Duration::from_millis(500)), 1);
        assert_eq!(prune_retention_secs(Duration::from_millis(1500)), 2);
        assert_eq!(prune_retention_secs(Duration::new(u64::MAX, 0)), i64::MAX);
    }

    #[tokio::test]
    async fn supervise_scan_returns_inner_ok_and_err_unchanged() {
        let ok: Result<u32> = supervise_scan("ok_scan", async { Ok(7) }).await;
        assert_eq!(ok.unwrap(), 7);

        let err: Result<u32> =
            supervise_scan("err_scan", async { Err(map_lance_err("scan failed")) }).await;
        assert_eq!(err.unwrap_err().to_string(), "db: lance: scan failed");
    }

    #[tokio::test]
    async fn supervise_scan_contains_panicked_scan_as_error_not_crash() {
        let result: Result<u32> = supervise_scan("panicking_scan", async {
            panic!("lance filtered_read unwrap")
        })
        .await;
        let err = result.unwrap_err();
        match err {
            HallouminateError::Db(_) => {}
            other => panic!("wrong variant: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("panicking_scan"), "missing op name: {msg}");
        assert!(msg.contains("panicked"), "missing panic marker: {msg}");
        assert!(
            msg.contains("lance filtered_read unwrap"),
            "missing payload: {msg}"
        );
    }

    #[tokio::test]
    async fn supervise_scan_extracts_owned_string_panic_payload() {
        let detail = String::from("owned payload 42");
        let result: Result<u32> =
            supervise_scan("string_panic_scan", async move { panic!("{detail}") }).await;
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("owned payload 42"), "missing payload: {msg}");
    }

    #[tokio::test]
    async fn scan_join_error_maps_deliberately_cancelled_scan_to_error() {
        let handle = tokio::spawn(std::future::pending::<Result<u32>>());
        handle.abort();
        let join_error = handle
            .await
            .expect_err("aborted pending task must fail to join");
        assert!(join_error.is_cancelled());
        let err = scan_join_error("cancelled_scan", join_error);
        match err {
            HallouminateError::Db(_) => {}
            other => panic!("wrong variant: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("cancelled_scan"), "missing op name: {msg}");
        assert!(msg.contains("cancelled"), "missing cancel marker: {msg}");
    }

    #[tokio::test]
    async fn supervise_scan_reports_non_string_panic_payload_without_crashing() {
        let result: Result<u32> =
            supervise_scan("weird_panic_scan", async { std::panic::panic_any(42_u64) }).await;
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("weird_panic_scan"), "missing op name: {msg}");
        assert!(
            msg.contains("<non-string panic payload>"),
            "missing placeholder: {msg}"
        );
    }

    type RecordedEmbedCalls = Arc<std::sync::Mutex<Vec<(Vec<String>, EmbedRole)>>>;

    struct InputRecordingEmbedder {
        calls: RecordedEmbedCalls,
    }

    impl EmbedBatch for InputRecordingEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.calls
                .lock()
                .expect("recording lock")
                .push((texts.to_vec(), role));
            Ok(vec![[0.0; EMBEDDING_DIM]; texts.len()])
        }
    }

    struct ThreadRecordingEmbedder {
        threads: Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>,
    }

    impl EmbedBatch for ThreadRecordingEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.threads
                .lock()
                .expect("recording lock")
                .push(std::thread::current().id());
            Ok(vec![[0.0; EMBEDDING_DIM]; texts.len()])
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn passage_and_query_embedding_use_blocking_pool_on_current_thread_runtime() {
        let runtime_thread = std::thread::current().id();
        let threads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let embedder: Arc<std::sync::Mutex<Option<Box<dyn EmbedBatch>>>> = Arc::new(
            std::sync::Mutex::new(Some(Box::new(ThreadRecordingEmbedder {
                threads: Arc::clone(&threads),
            }))),
        );

        run_embedding_blocking(
            Arc::clone(&embedder),
            vec!["passage".to_string()],
            EmbedRole::Passage,
        )
        .await
        .expect("passage embedding");
        run_embedding_blocking(embedder, vec!["query".to_string()], EmbedRole::Query)
            .await
            .expect("query embedding");

        let threads = threads.lock().expect("recording lock");
        assert_eq!(threads.len(), 2);
        for embedding_thread in threads.iter() {
            assert_ne!(
                *embedding_thread, runtime_thread,
                "embedding must run on Tokio's blocking pool"
            );
        }
    }

    #[tokio::test]
    async fn apply_batch_embeds_search_text_instead_of_display_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(InputRecordingEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");
        let mut file = synthetic_prepared("/tmp/embed.md", 1);
        file.chunks[0].text = "display evidence text".into();
        file.chunks[0].search_text = "derived retrieval text".into();

        store.apply_batch(vec![file]).await.expect("apply batch");

        let calls = calls.lock().expect("recording lock");
        assert_eq!(
            calls.as_slice(),
            &[(
                vec!["derived retrieval text".to_string()],
                EmbedRole::Passage
            )]
        );
    }

    type DistinctVectorCalls = Arc<std::sync::Mutex<Vec<Vec<String>>>>;

    struct DistinctVectorEmbedder {
        calls: DistinctVectorCalls,
    }

    impl EmbedBatch for DistinctVectorEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.calls
                .lock()
                .expect("recording lock")
                .push(texts.to_vec());
            let mut vectors = Vec::with_capacity(texts.len());
            for text in texts {
                let mut vector = [0.0_f32; EMBEDDING_DIM];
                for (i, byte) in text.bytes().enumerate() {
                    vector[i % EMBEDDING_DIM] += byte as f32;
                }
                vectors.push(vector);
            }
            Ok(vectors)
        }
    }

    async fn raw_insert(store: &LanceStore, file: &PreparedFile, with_embeddings: bool) {
        let embeddings: Option<Vec<[f32; EMBEDDING_DIM]>> = if with_embeddings {
            Some(synth_embeddings(file.chunks.len()))
        } else {
            None
        };
        let fwe = FileWithEmbeddings {
            file,
            embeddings: embeddings.as_deref(),
        };
        let schema = chunks_schema();
        let record_batch = build_record_batch(&[fwe], schema.clone()).expect("build raw batch");
        let reader = RecordBatchIterator::new(std::iter::once(Ok(record_batch)), schema);
        let reader: Box<dyn arrow::array::RecordBatchReader + Send> = Box::new(reader);
        let mut builder = store.table.merge_insert(&["corpus", "root", "chunk_id"]);
        builder
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        builder.execute(reader).await.expect("write raw batch");
    }

    #[tokio::test]
    async fn apply_batch_reuses_vectors_for_byte_identical_file_across_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(DistinctVectorEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");

        let root_a = corpus_key("docs", "/root-a");
        let file_a = synthetic_prepared_for(&root_a, "same.md", 2, "shared text", 1, 1);
        store.apply_batch(vec![file_a]).await.expect("apply root a");
        assert_eq!(calls.lock().expect("recording lock").len(), 1);

        let root_b = corpus_key("docs", "/root-b");
        let file_b = synthetic_prepared_for(&root_b, "same.md", 2, "shared text", 2, 2);
        store.apply_batch(vec![file_b]).await.expect("apply root b");

        assert_eq!(
            calls.lock().expect("recording lock").len(),
            1,
            "donor reuse must skip the embedder for the identical file"
        );

        let mut expected_vector = [0.0_f32; EMBEDDING_DIM];
        for (i, byte) in "shared text".bytes().enumerate() {
            expected_vector[i % EMBEDDING_DIM] += byte as f32;
        }
        let hits = store
            .retrieve_signals(&root_b, "shared", 10)
            .await
            .expect("retrieve");
        assert_eq!(hits.hits.len(), 2, "expected both chunks from root b");
        for hit in hits.hits.values() {
            assert_eq!(hit.file_ref, "same.md");
        }
    }

    #[tokio::test]
    async fn apply_batch_embeds_when_content_hash_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(DistinctVectorEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");

        let root_a = corpus_key("docs", "/root-a");
        let mut file_a = synthetic_prepared_for(&root_a, "same.md", 1, "original text", 1, 1);
        file_a.content_hash = "hash-one".into();
        store.apply_batch(vec![file_a]).await.expect("apply root a");

        let root_b = corpus_key("docs", "/root-b");
        let mut file_b = synthetic_prepared_for(&root_b, "same.md", 1, "changed text", 2, 2);
        file_b.content_hash = "hash-two".into();
        store.apply_batch(vec![file_b]).await.expect("apply root b");

        assert_eq!(
            calls.lock().expect("recording lock").len(),
            2,
            "a changed content_hash must never reuse a donor and must embed as today"
        );
    }

    #[tokio::test]
    async fn apply_batch_embeds_when_donor_chunk_count_differs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(DistinctVectorEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");

        let root_a = corpus_key("docs", "/root-a");
        let mut file_a = synthetic_prepared_for(&root_a, "same.md", 3, "shared text", 1, 1);
        file_a.content_hash = "same-hash".into();
        store.apply_batch(vec![file_a]).await.expect("apply root a");

        let root_b = corpus_key("docs", "/root-b");
        let mut file_b = synthetic_prepared_for(&root_b, "same.md", 2, "shared text", 2, 2);
        file_b.content_hash = "same-hash".into();
        store.apply_batch(vec![file_b]).await.expect("apply root b");

        assert_eq!(
            calls.lock().expect("recording lock").len(),
            2,
            "a donor chunk-count mismatch must never copy stale vectors"
        );
    }

    #[tokio::test]
    async fn apply_batch_embeds_when_only_donor_has_null_embeddings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(DistinctVectorEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");

        let root_a = corpus_key("docs", "/root-a");
        let mut donor = synthetic_prepared_for(&root_a, "donor.md", 1, "shared text", 1, 1);
        donor.content_hash = "null-donor-hash".into();
        raw_insert(&store, &donor, false).await;

        let root_b = corpus_key("docs", "/root-b");
        let mut file_b = synthetic_prepared_for(&root_b, "same.md", 1, "shared text", 2, 2);
        file_b.content_hash = "null-donor-hash".into();
        store.apply_batch(vec![file_b]).await.expect("apply root b");

        assert_eq!(
            calls.lock().expect("recording lock").len(),
            1,
            "a null-vector donor must not suppress embedding"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_store_without_embedder_rejects_embedding_dependent_operations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let threads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(ThreadRecordingEmbedder { threads })),
        )
        .await
        .expect("open enabled store");
        store
            .apply_batch(vec![synthetic_prepared("/tmp/seed.md", 1)])
            .await
            .expect("seed store");
        *store.embedder.lock().expect("embedder lock") = None;
        store.embedder_available.store(false, Ordering::Release);

        let mut unavailable_file = synthetic_prepared("/tmp/unavailable.md", 1);
        unavailable_file.content_hash = "cafef00d".into();
        let write_error = store
            .apply_batch(vec![unavailable_file])
            .await
            .expect_err("enabled writes must not degrade to null vectors");
        assert!(
            write_error
                .to_string()
                .contains("embedding model is unavailable")
        );

        let query_error = store
            .retrieve_signals(&docs_key(), "seed", 10)
            .await
            .expect_err("enabled queries must not degrade to lexical-only search");
        assert!(
            query_error
                .to_string()
                .contains("embedding model is unavailable")
        );
    }

    /// Blocks its first `embed_batch` call until released, so a test can
    /// observe the embedder mutex mid-call; every later call returns
    /// immediately and is counted.
    struct GatedFirstCallEmbedder {
        first_call: AtomicBool,
        entered_tx: std::sync::mpsc::SyncSender<()>,
        release_rx: std::sync::mpsc::Receiver<()>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl EmbedBatch for GatedFirstCallEmbedder {
        fn embed_batch(
            &mut self,
            texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.first_call.swap(false, Ordering::SeqCst) {
                self.entered_tx.send(()).expect("signal entered");
                self.release_rx.recv().expect("wait for release");
            }
            Ok(vec![[0.0; EMBEDDING_DIM]; texts.len()])
        }
    }

    /// Regression guard for #219: the embedder mutex must be acquired fresh
    /// per batch inside `run_embedding_blocking`, not held by the caller
    /// across a whole bulk index. Proves two properties deterministically
    /// (no wall-clock timing): (1) the mutex IS contended while one batch's
    /// embed call is in flight, and (2) as soon as that call returns, the
    /// mutex is free again for the *next* batch — i.e. `apply_batch` never
    /// wraps the mutex around more than a single `run_embedding_blocking`
    /// call. `GatedFirstCallEmbedder`'s call count also confirms a second
    /// batch triggers its own fresh acquisition rather than reusing a
    /// lock the first batch never released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embedder_lock_releases_between_batches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(GatedFirstCallEmbedder {
                first_call: AtomicBool::new(true),
                entered_tx,
                release_rx,
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");
        let store = Arc::new(store);

        let batch_one = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            batch_one
                .apply_batch(vec![synthetic_prepared("/tmp/batch-one.md", 1)])
                .await
                .expect("apply batch one")
        });

        // Wait until batch one's embed call has entered and is blocked —
        // it is holding the mutex right now.
        entered_rx.recv().expect("embed_batch entered");
        assert!(
            store.embedder.try_lock().is_err(),
            "embedder mutex must be held while a batch's embed call is in flight"
        );

        // Let batch one's embed call return.
        release_tx.send(()).expect("release embed call");
        handle.await.expect("batch one task");

        // The mutex must be free again immediately — nothing in apply_batch
        // holds it past the embed call, so a second batch (or a concurrent
        // `ground` query-embed) is never queued behind the first.
        assert!(
            store.embedder.try_lock().is_ok(),
            "embedder mutex must be released once batch one's embed call returns, \
             not held across the rest of the index (#219 regression)"
        );

        let mut batch_two_file = synthetic_prepared("/tmp/batch-two.md", 1);
        batch_two_file.content_hash = "b16b00b5".into();
        store
            .apply_batch(vec![batch_two_file])
            .await
            .expect("apply batch two");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each batch must trigger its own fresh embed call, not reuse a held lock"
        );
    }

    #[tokio::test]
    async fn corpus_chunk_stats_scoped_to_requested_corpus() {
        // Seed a two-corpus table and verify that corpus_chunk_stats returns
        // counts for only the requested corpus with no cross-corpus bleed.
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");

        let mut alpha_a = synthetic_prepared("/alpha/a.md", 3);
        alpha_a.corpus_key = corpus_key("alpha", "/alpha");
        alpha_a.indexed_at_ms = 1000;
        let mut alpha_b = synthetic_prepared("/alpha/b.md", 2);
        alpha_b.corpus_key = corpus_key("alpha", "/alpha");
        alpha_b.indexed_at_ms = 2000;
        let mut beta_c = synthetic_prepared("/beta/c.md", 5);
        beta_c.corpus_key = corpus_key("beta", "/beta");
        beta_c.indexed_at_ms = 500;

        store
            .apply_batch(vec![alpha_a, alpha_b])
            .await
            .expect("apply alpha");
        store.apply_batch(vec![beta_c]).await.expect("apply beta");

        let alpha = store
            .corpus_chunk_stats(&corpus_key("alpha", "/alpha"))
            .await
            .expect("alpha stats");
        assert_eq!(alpha.indexed_files, 2, "alpha: two files");
        assert_eq!(alpha.total_chunks, 5, "alpha: 3 + 2 chunks");
        assert_eq!(
            alpha.last_indexed_ms,
            Some(2000),
            "alpha: max indexed_at_ms"
        );

        let beta = store
            .corpus_chunk_stats(&corpus_key("beta", "/beta"))
            .await
            .expect("beta stats");
        assert_eq!(beta.indexed_files, 1, "beta: one file");
        assert_eq!(beta.total_chunks, 5, "beta: five chunks");
        assert_eq!(
            beta.last_indexed_ms,
            Some(500),
            "beta: single indexed_at_ms"
        );

        let empty = store
            .corpus_chunk_stats(&corpus_key("nonexistent", "/none"))
            .await
            .expect("empty stats");
        assert_eq!(empty.indexed_files, 0);
        assert_eq!(empty.total_chunks, 0);
        assert_eq!(empty.last_indexed_ms, None);
    }

    #[tokio::test]
    async fn maintain_prunes_versions_and_preserves_query_correctness() {
        // Every apply_batch (merge_insert) and delete_file (delete) commits a
        // new dataset version, so many small upserts + deletes build up a
        // version history that maintain() (compact + prune) should shrink.
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");

        for i in 0..20 {
            let pf = synthetic_prepared(&format!("/tmp/f{i}.md"), 1);
            store.apply_batch(vec![pf]).await.expect("apply");
        }
        for i in 0..10 {
            store
                .delete_file(&docs_key(), &format!("/tmp/f{i}.md"))
                .await
                .expect("delete");
        }

        let versions_before = store
            .table
            .list_versions()
            .await
            .expect("list_versions")
            .len();
        assert!(
            versions_before > 10,
            "expected many retained versions before maintain: {versions_before}"
        );

        // Zero retention is safe here: the test has no concurrent queries,
        // so there is no in-process read window for a zero-duration prune
        // to race (see `maintain`'s doc comment).
        let stats = store
            .maintain(MaintenanceOptions {
                maintenance_id: 1,
                prune_older_than: Duration::ZERO,
                max_fragments_per_slice: None,
            })
            .await
            .expect("maintain");
        assert!(
            stats.old_versions_pruned.is_some_and(|n| n > 0),
            "maintain must report a positive pruned-version count: {:?}",
            stats.old_versions_pruned
        );

        let versions_after = store
            .table
            .list_versions()
            .await
            .expect("list_versions")
            .len();
        assert!(
            versions_after < versions_before,
            "maintain must prune old versions: before {versions_before}, after {versions_after}"
        );

        // Queries must still return correct rows after compaction + prune.
        assert_eq!(
            store.count_rows().await.unwrap(),
            10,
            "10 of 20 files remain after deleting the first 10"
        );
        let snaps = store.list_files(&docs_key()).await.expect("list_files");
        assert!(
            snaps.iter().any(|s| s.file_ref == "/tmp/f10.md"),
            "surviving file must still be queryable after maintain"
        );
        assert!(
            !snaps.iter().any(|s| s.file_ref == "/tmp/f0.md"),
            "deleted file must stay gone after maintain"
        );
    }

    #[tokio::test]
    async fn debt_reports_fragment_and_stale_version_counts() {
        // Mirrors maintain_prunes_versions_and_preserves_query_correctness's
        // setup: many small upserts + deletes build up fragments and a
        // version history that debt() should report without running any
        // compaction or pruning itself.
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");

        for i in 0..20 {
            let pf = synthetic_prepared(&format!("/tmp/f{i}.md"), 1);
            store.apply_batch(vec![pf]).await.expect("apply");
        }
        for i in 0..10 {
            store
                .delete_file(&docs_key(), &format!("/tmp/f{i}.md"))
                .await
                .expect("delete");
        }

        let versions_before = store
            .table
            .list_versions()
            .await
            .expect("list_versions")
            .len() as u64;
        let debt_before = store.debt().await.expect("debt");
        assert!(
            debt_before.fragments >= 1,
            "expected at least one fragment: {:?}",
            debt_before
        );
        assert_eq!(
            debt_before.stale_versions,
            versions_before - 1,
            "stale_versions must equal the retained-version count minus the current version"
        );

        store
            .maintain(MaintenanceOptions {
                maintenance_id: 1,
                prune_older_than: Duration::ZERO,
                max_fragments_per_slice: None,
            })
            .await
            .expect("maintain");

        let debt_after = store.debt().await.expect("debt");
        assert!(
            debt_after.stale_versions < debt_before.stale_versions,
            "maintain must reduce debt's stale_versions count: before {}, after {}",
            debt_before.stale_versions,
            debt_after.stale_versions
        );
    }

    #[tokio::test]
    async fn maintain_max_fragments_per_slice_bounds_one_compaction_pass() {
        // Two identically-seeded stores: one maintained with a tight
        // fragment-per-slice bound, one unbounded. The bound must make the
        // single compaction pass touch strictly fewer fragments than the
        // unbounded pass on the same backlog (ADR daemon-rework-001 paced
        // mode -- a bounded slice does a chunk of the backlog, not all of it).
        async fn seed_store(dir: &std::path::Path) -> LanceStore {
            let store =
                LanceStore::open_or_create(dir, "BAAI/bge-small-en-v1.5", false, false, None)
                    .await
                    .expect("open store");
            for i in 0..20 {
                let pf = synthetic_prepared(&format!("/tmp/f{i}.md"), 1);
                store.apply_batch(vec![pf]).await.expect("apply");
            }
            store
        }

        let bounded_dir = tempfile::tempdir().unwrap();
        let bounded_store = seed_store(bounded_dir.path()).await;
        let bounded_stats = bounded_store
            .maintain(MaintenanceOptions {
                maintenance_id: 1,
                prune_older_than: Duration::ZERO,
                max_fragments_per_slice: Some(2),
            })
            .await
            .expect("bounded maintain");

        let unbounded_dir = tempfile::tempdir().unwrap();
        let unbounded_store = seed_store(unbounded_dir.path()).await;
        let unbounded_stats = unbounded_store
            .maintain(MaintenanceOptions {
                maintenance_id: 2,
                prune_older_than: Duration::ZERO,
                max_fragments_per_slice: None,
            })
            .await
            .expect("unbounded maintain");

        let bounded_removed = bounded_stats
            .fragments_removed
            .expect("bounded pass must report a fragments_removed count");
        let unbounded_removed = unbounded_stats
            .fragments_removed
            .expect("unbounded pass must report a fragments_removed count");
        assert!(
            bounded_removed < unbounded_removed,
            "max_fragments_per_slice must constrain one pass to fewer removed fragments \
             than the unbounded pass on the same backlog: bounded {bounded_removed}, \
             unbounded {unbounded_removed}"
        );

        // The bounded run must still leave correct, queryable data behind.
        assert_eq!(bounded_store.count_rows().await.unwrap(), 20);
    }

    #[tokio::test]
    async fn has_text_index_latches_and_skips_list_indices_after_first_true() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        assert!(
            !store.text_index_present.load(Ordering::Relaxed),
            "fresh store starts unlatched"
        );

        let pf = synthetic_prepared("/tmp/a.md", 1);
        store.apply_batch(vec![pf]).await.expect("apply");
        assert!(
            store.text_index_present.load(Ordering::Relaxed),
            "latch must be set once the FTS index is confirmed present"
        );

        let indices = store.table.list_indices().await.expect("list_indices");
        let search_text_index = indices
            .iter()
            .find(|index| {
                index.index_type == lancedb::index::IndexType::FTS
                    && index.columns.iter().any(|column| column == "search_text")
            })
            .expect("FTS index on search_text present before drop");
        store
            .table
            .drop_index(&search_text_index.name)
            .await
            .expect("drop FTS index");

        assert!(
            store.has_text_index().await.expect("has_text_index"),
            "latched has_text_index must not re-query list_indices after the index was dropped"
        );
    }

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
    fn corpus_key_filters_escape_name_root_and_file_ref() {
        let key = corpus_key("repo:o'brien:wiki", "/tmp/root'one");
        let key_filter = corpus_key_filter(&key).expect("key filter");
        assert_eq!(
            key_filter,
            "corpus = 'repo:o''brien:wiki' AND root = '/tmp/root''one'"
        );

        let refs = vec!["/tmp/file's.md".into()];
        let file_filter = corpus_and_file_ref_filter(&key, &refs).expect("key and file filter");
        assert_eq!(
            file_filter,
            "corpus = 'repo:o''brien:wiki' AND root = '/tmp/root''one' AND file_ref IN ('/tmp/file''s.md')"
        );
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
    fn meta_check_or_init_defaults_missing_embedding_fields_on_v4() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            r#"# auto-managed by hallouminate; do not edit
embedding_model_name = "BAAI/bge-small-en-v1.5"
schema_version = 4
"#,
        )
        .unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect("missing embedding fields must retain their serde defaults");
    }

    #[test]
    fn meta_check_or_init_stale_on_schema_version_below_expected() {
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
            .expect_err("a v1 store must be rejected by the v4 binary");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale {
                    found: 1,
                    expected: 4,
                    ..
                }
            ),
            "expected StoreSchemaStale, got: {err}"
        );
    }

    #[test]
    fn meta_check_or_init_stale_on_v2_store() {
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
            .expect_err("a v2 store must be rejected by the v4 binary");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale {
                    found: 2,
                    expected: 4,
                    ..
                }
            ),
            "expected StoreSchemaStale, got: {err}"
        );
    }

    #[test]
    fn meta_check_or_init_roundtrips_current_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true).unwrap();
        let text = std::fs::read_to_string(&meta_path).unwrap();
        let meta: Meta = toml::from_str(&text).unwrap();
        assert_eq!(default_schema_version(), 4);
        assert_eq!(meta.schema_version, 4);
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, true)
            .expect("v4 store must re-open");
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
        let names: Vec<&str> = schema
            .fields()
            .iter()
            .map(|field| field.name().as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "chunk_id",
                "file_ref",
                "corpus",
                "root",
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
                "search_text",
                "claim_marks",
                "embedding",
            ]
        );
        for name in ["root", "search_text"] {
            let field = schema.field_with_name(name).expect("v4 field");
            assert_eq!(field.data_type(), &DataType::Utf8);
            assert!(!field.is_nullable(), "{name} must be non-null");
        }
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
        let mut pf = PreparedFile {
            file_ref: file_ref.to_string(),
            corpus_key: CorpusKey::from_configured_root("docs", "/tmp"),
            mtime_ms: 7,
            content_hash: "deadbeef".into(),
            summary: "summary".into(),
            keywords: vec!["k1".into(), "k2".into()],
            frontmatter: None,
            indexed_at_ms: 11,
            chunks: Vec::new(),
        };
        for i in 0..chunks {
            pf.chunks.push(PreparedChunk {
                ord: i,
                heading_path: vec!["H".into()],
                line_start: 1,
                line_end: 2,
                text: format!("chunk-{i}"),
                search_text: format!("chunk-{i}"),
                claim_marks: None,
            });
        }
        pf
    }

    fn synthetic_prepared_for(
        corpus_key: &CorpusKey,
        file_ref: &str,
        chunks: usize,
        search_text: &str,
        mtime_ms: i64,
        indexed_at_ms: i64,
    ) -> PreparedFile {
        let mut file = synthetic_prepared(file_ref, chunks);
        file.corpus_key = corpus_key.clone();
        file.mtime_ms = mtime_ms;
        file.indexed_at_ms = indexed_at_ms;
        for chunk in &mut file.chunks {
            chunk.text = search_text.to_string();
            chunk.search_text = search_text.to_string();
        }
        file
    }

    fn synth_embeddings(n: usize) -> Vec<[f32; EMBEDDING_DIM]> {
        vec![[0.0_f32; EMBEDDING_DIM]; n]
    }

    #[test]
    fn build_record_batch_row_count_matches_total_chunks_across_files() {
        let a = synthetic_prepared("/tmp/a.md", 3);
        let a_emb = synth_embeddings(a.chunks.len());
        let b = synthetic_prepared("/tmp/b.md", 2);
        let b_emb = synth_embeddings(b.chunks.len());
        let batch = vec![
            FileWithEmbeddings {
                file: &a,
                embeddings: Some(&a_emb),
            },
            FileWithEmbeddings {
                file: &b,
                embeddings: Some(&b_emb),
            },
        ];
        let schema = chunks_schema();
        let rb = build_record_batch(&batch, schema).expect("build batch");
        assert_eq!(rb.num_rows(), 5);
        assert_eq!(rb.num_columns(), 18);
    }

    #[test]
    fn build_record_batch_writes_exact_root_display_and_search_text_values() {
        let mut file = synthetic_prepared("/tmp/values.md", 1);
        file.chunks[0].text = "display text [^1]\n\n[^1]: evidence".into();
        file.chunks[0].search_text = "Heading\nSummary\ndisplay text".into();
        let embeddings = synth_embeddings(1);
        let batch = [FileWithEmbeddings {
            file: &file,
            embeddings: Some(&embeddings),
        }];
        let record_batch = build_record_batch(&batch, chunks_schema()).expect("build record batch");

        let root = string_col(&record_batch, "root").expect("root");
        let text = string_col(&record_batch, "text").expect("text");
        let search_text = string_col(&record_batch, "search_text").expect("search_text");

        assert_eq!(
            root.value(0),
            file.corpus_key
                .canonical_root
                .to_str()
                .expect("canonical root is utf8")
        );
        assert_eq!(text.value(0), "display text [^1]\n\n[^1]: evidence");
        assert_eq!(search_text.value(0), "Heading\nSummary\ndisplay text");
    }

    #[cfg(unix)]
    #[test]
    fn build_record_batch_rejects_non_utf8_canonical_root() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut file = synthetic_prepared("/tmp/non-utf8-root.md", 1);
        file.corpus_key.canonical_root = PathBuf::from(OsString::from_vec(b"/tmp/\xff".to_vec()));
        let batch = [FileWithEmbeddings {
            file: &file,
            embeddings: None,
        }];

        let error = build_record_batch(&batch, chunks_schema())
            .expect_err("non-UTF-8 canonical root must be rejected");
        let HallouminateError::Indexer(message) = error else {
            panic!("expected indexer error, got {error}");
        };
        assert!(
            message.contains("canonical corpus root is not valid UTF-8"),
            "unexpected indexer error: {message}"
        );
    }

    #[test]
    fn build_record_batch_denormalizes_frontmatter_with_null_for_absent() {
        let mut with_fm = synthetic_prepared("/tmp/fm.md", 2);
        with_fm.frontmatter = Some(r#"{"status":"draft"}"#.to_string());
        let with_fm_emb = synth_embeddings(with_fm.chunks.len());
        let without_fm = synthetic_prepared("/tmp/plain.md", 1); // frontmatter: None
        let without_fm_emb = synth_embeddings(without_fm.chunks.len());
        let schema = chunks_schema();
        let rb = build_record_batch(
            &[
                FileWithEmbeddings {
                    file: &with_fm,
                    embeddings: Some(&with_fm_emb),
                },
                FileWithEmbeddings {
                    file: &without_fm,
                    embeddings: Some(&without_fm_emb),
                },
            ],
            schema,
        )
        .expect("build batch");
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
        let emb = synth_embeddings(pf.chunks.len());
        let schema = chunks_schema();
        let rb = build_record_batch(
            &[FileWithEmbeddings {
                file: &pf,
                embeddings: Some(&emb),
            }],
            schema,
        )
        .expect("build batch");
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
        let pf = synthetic_prepared("/tmp/bad.md", 2);
        let mut emb = synth_embeddings(pf.chunks.len());
        emb.pop(); // 2 chunks, 1 embedding
        let schema = chunks_schema();
        let err = build_record_batch(
            &[FileWithEmbeddings {
                file: &pf,
                embeddings: Some(&emb),
            }],
            schema,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("chunks but 1 embeddings"),
            "got: {err}"
        );
    }

    #[test]
    fn build_record_batch_off_mode_writes_null_embeddings_for_every_chunk() {
        let pf = synthetic_prepared("/tmp/off.md", 3);
        let batch = vec![FileWithEmbeddings {
            file: &pf,
            embeddings: None,
        }];
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
        let pf = synthetic_prepared("/tmp/on.md", 3);
        let emb = synth_embeddings(pf.chunks.len());
        let batch = vec![FileWithEmbeddings {
            file: &pf,
            embeddings: Some(&emb),
        }];
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
        let pf = synthetic_prepared("/tmp/det.md", 2);
        let emb = synth_embeddings(pf.chunks.len());
        let batch = vec![FileWithEmbeddings {
            file: &pf,
            embeddings: Some(&emb),
        }];
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
    /// change what ends up in the table. In embeddings-OFF mode the latch
    /// closes after the FTS index is built, and both files' rows must remain
    /// present, durable, and searchable.
    #[tokio::test]
    async fn re_indexing_across_batches_keeps_rows_durable_with_indexes_built() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
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
            store.indexes_ensured.load(Ordering::Acquire),
            "OFF mode must latch after building its only search index (FTS)"
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
            store.indexes_ensured.load(Ordering::Acquire),
            "the OFF-mode latch must remain closed across later batches"
        );

        let signals = store
            .retrieve_signals(&docs_key(), "chunk-0", 10)
            .await
            .expect("fts search after re-index");
        assert!(
            !signals.fts.is_empty(),
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
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open OFF-mode store");

        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 3)])
            .await
            .expect("first OFF batch");
        assert!(
            store.indexes_ensured.load(Ordering::Acquire),
            "OFF mode must latch after the first batch builds FTS — nothing else can build"
        );

        store
            .apply_batch(vec![synthetic_prepared("/tmp/b.md", 2)])
            .await
            .expect("second OFF batch on the cached path");
        assert_eq!(
            store.count_rows().await.unwrap(),
            5,
            "cached-path batch must still write its rows"
        );

        let signals = store
            .retrieve_signals(&docs_key(), "chunk-1", 10)
            .await
            .expect("fts search after cached re-index");
        assert!(
            !signals.fts.is_empty(),
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
    /// every other repo. Only the lexical (embedder-less) decode path is
    /// exercised here — ON-mode would require a real embedder, which would
    /// make this a network/model-dependent unit test.
    #[tokio::test]
    async fn searching_an_empty_corpus_in_a_populated_store_returns_no_hits_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
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

        // Decode path must tolerate a zero-row corpus rather than erroring
        // (an unindexed corpus surfaces empty result columns). ON-mode
        // exercise via a real embedder would make this a network/model-
        // dependent unit test, so only the lexical (embedder-less) decode
        // path is covered here; ON-mode zero-row decoding is covered by
        // `build_record_batch_off_mode_writes_null_embeddings_for_every_chunk`
        // and friends at the row-encode layer.
        let signals = store
            .retrieve_signals(&corpus_key("repo:empty:wiki", "/tmp/empty"), "chunk", 10)
            .await
            .expect("retrieve_signals on an empty corpus must not error");
        assert!(
            signals.fts.is_empty(),
            "a zero-row corpus must yield no hits, got {}",
            signals.fts.len()
        );
    }

    // ─── T7: unit guard classifies schema-version direction ──────────────────

    #[test]
    fn guard_stale_when_stored_version_is_v3() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            "# auto-managed by hallouminate; do not edit\n\
             embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
             quantized = false\n\
             embeddings_enabled = false\n\
             schema_version = 3\n",
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect_err("v3 store must be stale and rebuildable");
        assert!(
            matches!(
                err,
                HallouminateError::StoreSchemaStale {
                    found: 3,
                    expected: 4,
                    ..
                }
            ),
            "expected v3 StoreSchemaStale, got: {err}"
        );
    }

    #[test]
    fn guard_ok_when_stored_version_is_v4() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            "# auto-managed by hallouminate; do not edit\n\
             embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
             quantized = false\n\
             embeddings_enabled = false\n\
             schema_version = 4\n",
        )
        .unwrap();
        meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect("v4 store must open");
    }

    #[test]
    fn guard_fatal_config_when_stored_version_is_v5() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.toml");
        std::fs::write(
            &meta_path,
            "# auto-managed by hallouminate; do not edit\n\
             embedding_model_name = \"BAAI/bge-small-en-v1.5\"\n\
             quantized = false\n\
             embeddings_enabled = false\n\
             schema_version = 5\n",
        )
        .unwrap();
        let err = meta_check_or_init(&meta_path, "BAAI/bge-small-en-v1.5", false, false)
            .expect_err("v5 store must fail fatally");
        assert!(
            matches!(err, HallouminateError::Config(_)),
            "expected Config (downgrade fatal), got: {err}"
        );
        let message = err.to_string();
        assert!(message.contains("NEWER"), "must say NEWER: {message}");
        assert!(
            message.to_lowercase().contains("upgrade"),
            "must advise upgrade: {message}"
        );
    }

    #[tokio::test]
    async fn fm_index_and_fts_coexist_only_on_search_text_column() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 3)])
            .await
            .expect("seed docs corpus");

        let indices = store.table.list_indices().await.expect("list indices");
        let search_text_indices: Vec<_> = indices
            .iter()
            .filter(|index| index.columns.iter().any(|column| column == "search_text"))
            .collect();
        assert_eq!(search_text_indices.len(), 2);
        assert!(
            search_text_indices
                .iter()
                .any(|index| index.index_type == lancedb::index::IndexType::Fm)
        );
        assert!(
            search_text_indices
                .iter()
                .any(|index| index.index_type == lancedb::index::IndexType::FTS)
        );
        assert!(
            indices
                .iter()
                .all(|index| !index.columns.iter().any(|column| column == "text")),
            "display text must not be indexed: {indices:?}"
        );
    }

    #[tokio::test]
    async fn fts_searches_search_text_and_decodes_display_text() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let mut file = synthetic_prepared("/tmp/fields.md", 1);
        file.chunks[0].text = "displaycontracttoken".into();
        file.chunks[0].search_text = "retrievalcontracttoken".into();
        store.apply_batch(vec![file]).await.expect("apply batch");

        let signals = store
            .retrieve_signals(&docs_key(), "retrievalcontracttoken", 10)
            .await
            .expect("search indexed text");
        assert_eq!(signals.fts.len(), 1);
        let hit = signals
            .hits
            .get(&signals.fts[0])
            .expect("hit for ranked chunk_id");
        assert_eq!(hit.text, "displaycontracttoken");
        assert_eq!(hit.search_text, "retrievalcontracttoken");

        let display_signals = store
            .retrieve_signals(&docs_key(), "displaycontracttoken", 10)
            .await
            .expect("search display-only token");
        assert!(display_signals.fts.is_empty());
    }

    #[tokio::test]
    async fn later_batch_disjoint_token_is_searchable_after_fts_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let mut first = synthetic_prepared("/tmp/first.md", 1);
        first.chunks[0].search_text = "firstbatchtoken".into();
        store.apply_batch(vec![first]).await.expect("seed FTS");

        let mut later = synthetic_prepared("/tmp/later.md", 1);
        later.chunks[0].search_text = "laterbatchdisjointtoken".into();
        store
            .apply_batch(vec![later])
            .await
            .expect("append later batch");

        let signals = store
            .retrieve_signals(&docs_key(), "laterbatchdisjointtoken", 10)
            .await
            .expect("search later batch token");
        assert_eq!(signals.fts.len(), 1);
        let hit = signals
            .hits
            .get(&signals.fts[0])
            .expect("hit for ranked chunk_id");
        assert_eq!(hit.file_ref, "/tmp/later.md");
        assert_eq!(hit.chunk_id, chunk_id_for("/tmp/later.md", 0));
    }

    #[tokio::test]
    async fn reopened_store_refreshes_persisted_fts_for_same_name_sibling_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_a = corpus_key("repo:shared:wiki", "/tmp/reopen-root-a");
        let key_b = corpus_key("repo:shared:wiki", "/tmp/reopen-root-b");
        {
            let store = LanceStore::open_or_create(
                dir.path(),
                "BAAI/bge-small-en-v1.5",
                false,
                false,
                None,
            )
            .await
            .expect("open initial store");
            store
                .apply_batch(vec![synthetic_prepared_for(
                    &key_a,
                    "/tmp/reopen-a.md",
                    1,
                    "persistedrootatoken",
                    10,
                    100,
                )])
                .await
                .expect("seed root A");
            let indices = store
                .table
                .list_indices()
                .await
                .expect("list persisted indices");
            assert!(indices.iter().any(|index| {
                index.index_type == lancedb::index::IndexType::FTS
                    && index.columns.iter().any(|column| column == "search_text")
            }));
        }

        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("reopen store");
        assert!(!store.indexes_ensured.load(Ordering::Acquire));
        assert!(!store.text_index_present.load(Ordering::Acquire));
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b,
                "/tmp/reopen-b.md",
                1,
                "reopenedrootbuniquetoken",
                20,
                200,
            )])
            .await
            .expect("append root B after reopen");

        let signals_b = store
            .retrieve_signals(&key_b, "reopenedrootbuniquetoken", 10)
            .await
            .expect("search root B token in root B");
        assert_eq!(signals_b.fts.len(), 1);
        let hit_b = signals_b
            .hits
            .get(&signals_b.fts[0])
            .expect("hit for ranked chunk_id");
        assert_eq!(hit_b.corpus_key, key_b);
        assert_eq!(hit_b.file_ref, "/tmp/reopen-b.md");
        assert_eq!(hit_b.chunk_id, chunk_id_for("/tmp/reopen-b.md", 0));
        assert!(
            store
                .retrieve_signals(&key_a, "reopenedrootbuniquetoken", 10)
                .await
                .expect("search root B token in root A")
                .fts
                .is_empty()
        );

        let signals_a = store
            .retrieve_signals(&key_a, "persistedrootatoken", 10)
            .await
            .expect("search persisted root A token in root A");
        assert_eq!(signals_a.fts.len(), 1);
        let hit_a = signals_a
            .hits
            .get(&signals_a.fts[0])
            .expect("hit for ranked chunk_id");
        assert_eq!(hit_a.corpus_key, key_a);
        assert_eq!(hit_a.file_ref, "/tmp/reopen-a.md");
        assert!(
            store
                .retrieve_signals(&key_b, "persistedrootatoken", 10)
                .await
                .expect("search root A token in root B")
                .fts
                .is_empty()
        );
    }

    #[tokio::test]
    async fn same_name_roots_isolate_lexical_search_list_touch_stats_replacement_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let key_a = corpus_key("repo:shared:wiki", "/tmp/root-a");
        let key_b = corpus_key("repo:shared:wiki", "/tmp/root-b");
        let file_ref = "/tmp/shared.md";

        let mixed_error = store
            .apply_batch(vec![
                synthetic_prepared_for(&key_a, file_ref, 1, "mixed-a", 1, 1),
                synthetic_prepared_for(&key_b, file_ref, 1, "mixed-b", 2, 2),
            ])
            .await
            .expect_err("mixed corpus-key batch must be rejected");
        assert!(
            mixed_error
                .to_string()
                .contains("must share the same corpus key")
        );
        assert_eq!(store.count_rows().await.expect("count empty store"), 0);

        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_a,
                file_ref,
                2,
                "lexicalrootatokenunique",
                10,
                100,
            )])
            .await
            .expect("seed root A");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b,
                file_ref,
                2,
                "lexicalrootbtokenunique",
                20,
                200,
            )])
            .await
            .expect("seed root B");
        assert_eq!(store.count_rows().await.expect("count both roots"), 4);

        let signals_a = store
            .retrieve_signals(&key_a, "lexicalrootatokenunique", 10)
            .await
            .expect("lexical search root A");
        assert_eq!(signals_a.fts.len(), 2);
        for chunk_id in &signals_a.fts {
            let hit = signals_a.hits.get(chunk_id).expect("hit for ranked chunk");
            assert_eq!(hit.corpus_key, key_a);
        }
        let cross_root = store
            .retrieve_signals(&key_b, "lexicalrootatokenunique", 10)
            .await
            .expect("lexical search root B for root A token");
        assert!(cross_root.fts.is_empty());
        assert!(cross_root.hits.is_empty());

        let snapshots_a = store.list_files(&key_a).await.expect("list root A");
        let snapshots_b = store.list_files(&key_b).await.expect("list root B");
        assert_eq!(snapshots_a.len(), 1);
        assert_eq!(snapshots_a[0].corpus_key, key_a);
        assert_eq!(snapshots_a[0].mtime_ms, 10);
        assert_eq!(snapshots_b.len(), 1);
        assert_eq!(snapshots_b[0].corpus_key, key_b);
        assert_eq!(snapshots_b[0].mtime_ms, 20);

        let stats_a = store
            .corpus_chunk_stats(&key_a)
            .await
            .expect("stats root A");
        let stats_b = store
            .corpus_chunk_stats(&key_b)
            .await
            .expect("stats root B");
        assert_eq!(stats_a.indexed_files, 1);
        assert_eq!(stats_a.total_chunks, 2);
        assert_eq!(stats_a.last_indexed_ms, Some(100));
        assert_eq!(stats_b.indexed_files, 1);
        assert_eq!(stats_b.total_chunks, 2);
        assert_eq!(stats_b.last_indexed_ms, Some(200));

        store
            .touch_mtime(&key_a, file_ref, 30)
            .await
            .expect("touch root A");
        let snapshot_a = store
            .get_file_snapshot(&key_a, file_ref)
            .await
            .expect("snapshot root A")
            .expect("root A present");
        let snapshot_b = store
            .get_file_snapshot(&key_b, file_ref)
            .await
            .expect("snapshot root B")
            .expect("root B present");
        assert_eq!(snapshot_a.mtime_ms, 30);
        assert_eq!(snapshot_b.mtime_ms, 20);

        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_a,
                file_ref,
                1,
                "lexicalrootareplacementunique",
                40,
                300,
            )])
            .await
            .expect("replace root A");
        let stats_a = store
            .corpus_chunk_stats(&key_a)
            .await
            .expect("stats root A");
        let stats_b = store
            .corpus_chunk_stats(&key_b)
            .await
            .expect("stats root B");
        assert_eq!(stats_a.total_chunks, 1);
        assert_eq!(stats_a.last_indexed_ms, Some(300));
        assert_eq!(stats_b.total_chunks, 2);
        assert_eq!(stats_b.last_indexed_ms, Some(200));
        let signals_b = store
            .retrieve_signals(&key_b, "lexicalrootbtokenunique", 10)
            .await
            .expect("root B remains searchable after root A replacement");
        assert_eq!(signals_b.fts.len(), 2);
        assert_eq!(
            store.count_rows().await.expect("count after replacement"),
            3
        );

        store
            .delete_file(&key_a, file_ref)
            .await
            .expect("delete root A");
        assert!(
            store
                .list_files(&key_a)
                .await
                .expect("list root A")
                .is_empty()
        );
        assert_eq!(
            store.list_files(&key_b).await.expect("list root B").len(),
            1
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&key_a)
                .await
                .expect("stats deleted root A")
                .total_chunks,
            0
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&key_b)
                .await
                .expect("stats retained root B")
                .total_chunks,
            2
        );
        let signals_b = store
            .retrieve_signals(&key_b, "lexicalrootbtokenunique", 10)
            .await
            .expect("root B remains searchable after root A delete");
        assert_eq!(signals_b.fts.len(), 2);
    }

    #[tokio::test]
    async fn same_name_roots_isolate_retrieve_signals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = LanceStore::open_or_create(
            dir.path(),
            "BAAI/bge-small-en-v1.5",
            false,
            true,
            Some(Box::new(InputRecordingEmbedder {
                calls: Arc::clone(&calls),
            })),
        )
        .await
        .expect("open enabled store");
        let key_a = corpus_key("repo:shared:wiki", "/tmp/hybrid-root-a");
        let key_b = corpus_key("repo:shared:wiki", "/tmp/hybrid-root-b");

        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_a,
                "/tmp/hybrid-a.md",
                1,
                "shared-hybrid-token root-a",
                10,
                100,
            )])
            .await
            .expect("seed hybrid root A");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b,
                "/tmp/hybrid-b.md",
                1,
                "shared-hybrid-token root-b",
                20,
                200,
            )])
            .await
            .expect("seed hybrid root B");

        let signals_a = store
            .retrieve_signals(&key_a, "shared-hybrid-token", 10)
            .await
            .expect("retrieve signals root A");
        assert_eq!(signals_a.hits.len(), 1);
        let hit_a = signals_a
            .hits
            .values()
            .next()
            .expect("root A hit")
            .to_owned();
        assert_eq!(hit_a.corpus_key, key_a);
        assert_eq!(hit_a.file_ref, "/tmp/hybrid-a.md");

        let signals_b = store
            .retrieve_signals(&key_b, "shared-hybrid-token", 10)
            .await
            .expect("retrieve signals root B");
        assert_eq!(signals_b.hits.len(), 1);
        let hit_b = signals_b
            .hits
            .values()
            .next()
            .expect("root B hit")
            .to_owned();
        assert_eq!(hit_b.corpus_key, key_b);
        assert_eq!(hit_b.file_ref, "/tmp/hybrid-b.md");

        let calls = calls.lock().expect("recording lock");
        let mut query_calls = 0;
        for (_, role) in calls.iter() {
            if *role == EmbedRole::Query {
                query_calls += 1;
            }
        }
        assert_eq!(query_calls, 2);
    }

    #[tokio::test]
    async fn fm_index_builds_below_ann_row_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", true, false, None)
                .await
                .expect("open store");
        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 1)])
            .await
            .expect("seed single-chunk corpus");

        let indices = store.table.list_indices().await.expect("list indices");
        assert!(
            indices.iter().any(|index| {
                index.index_type == lancedb::index::IndexType::Fm
                    && index.columns.iter().any(|column| column == "search_text")
            }),
            "FM-Index must be built below the ANN threshold: {indices:?}"
        );
        assert!(
            indices
                .iter()
                .all(|index| !index.columns.iter().any(|column| column == "embedding")),
            "ANN index must not exist below the threshold: {indices:?}"
        );
    }

    #[tokio::test]
    async fn fm_index_build_does_not_rename_or_collide_with_fts_index_name() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 3)])
            .await
            .expect("seed docs corpus");

        let indices = store.table.list_indices().await.expect("list indices");
        let fts_index = indices
            .iter()
            .find(|index| index.index_type == lancedb::index::IndexType::FTS)
            .expect("FTS index must exist on search_text");
        let fm_index = indices
            .iter()
            .find(|index| index.index_type == lancedb::index::IndexType::Fm)
            .expect("FM-Index must exist on search_text");
        assert_ne!(fts_index.name, fm_index.name);
        assert_eq!(fm_index.name, "search_text_fm_idx");
    }

    /// decode_hits must never let a raw per-signal score (FTS relevance or
    /// vector distance) cross the `ChunkStore` port: both are mutually
    /// incomparable and `domain::search` always overwrites `score` with the
    /// fused RRF value before a caller sees it, so decode_hits should hand
    /// back `0.0` rather than either raw meaning.
    #[tokio::test]
    async fn retrieve_signals_hits_carry_zero_score_before_fusion() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        store
            .apply_batch(vec![synthetic_prepared("/tmp/a.md", 2)])
            .await
            .expect("seed docs corpus");

        let signals = store
            .retrieve_signals(&docs_key(), "chunk-0", 10)
            .await
            .expect("fts search");
        assert!(!signals.hits.is_empty(), "expected at least one hit");
        for hit in signals.hits.values() {
            assert_eq!(
                hit.score, 0.0,
                "decode_hits must not leak a raw per-signal score pre-fusion"
            );
        }
    }

    #[tokio::test]
    async fn distinct_roots_returns_every_root_independent_of_corpus_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let root_a = tempfile::tempdir().expect("root a");
        let root_b = tempfile::tempdir().expect("root b");
        let key_a = corpus_key("repo:a:corpus", root_a.path().to_str().expect("utf8"));
        let key_b_corpus = corpus_key("repo:b:corpus", root_b.path().to_str().expect("utf8"));
        let key_b_wiki = corpus_key("repo:b:wiki", root_b.path().to_str().expect("utf8"));
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_a,
                "/tmp/a.md",
                1,
                "roota",
                10,
                100,
            )])
            .await
            .expect("seed root a");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b_corpus,
                "/tmp/b-corpus.md",
                1,
                "rootbcorpus",
                10,
                100,
            )])
            .await
            .expect("seed root b corpus");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b_wiki,
                "/tmp/b-wiki.md",
                1,
                "rootbwiki",
                10,
                100,
            )])
            .await
            .expect("seed root b wiki");

        let mut roots = store.distinct_roots().await.expect("distinct roots");
        roots.sort();
        let mut expected = vec![
            key_a.canonical_root.clone(),
            key_b_corpus.canonical_root.clone(),
        ];
        expected.sort();
        assert_eq!(roots, expected);
    }

    #[tokio::test]
    async fn delete_root_removes_all_rows_at_root_and_leaves_others_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let root_a = tempfile::tempdir().expect("root a");
        let root_b = tempfile::tempdir().expect("root b");
        let key_a = corpus_key("repo:a:corpus", root_a.path().to_str().expect("utf8"));
        let key_b = corpus_key("repo:b:corpus", root_b.path().to_str().expect("utf8"));
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_a,
                "/tmp/a.md",
                2,
                "roota",
                10,
                100,
            )])
            .await
            .expect("seed root a");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_b,
                "/tmp/b.md",
                3,
                "rootb",
                10,
                100,
            )])
            .await
            .expect("seed root b");

        drop(root_a);
        let retired =
            hallouminate_domain::common::retired_roots(std::slice::from_ref(&key_a.canonical_root));
        let retired_root_a = retired.into_iter().next().expect("root a retired");

        let removed = store
            .delete_root(&retired_root_a)
            .await
            .expect("delete root a");
        assert_eq!(removed, 2);

        let stats_a = store.corpus_chunk_stats(&key_a).await.expect("stats a");
        assert_eq!(stats_a.total_chunks, 0);
        let stats_b = store.corpus_chunk_stats(&key_b).await.expect("stats b");
        assert_eq!(stats_b.total_chunks, 3);
    }

    /// Two-root regression test (spec's named acceptance criterion): with a
    /// real store holding rows for two sibling roots, deleting one root's
    /// directory on disk and running the GC flow (distinct_roots ->
    /// retired_roots -> delete_root) leaves the surviving root's rows fully
    /// intact and the deleted root's rows at zero.
    #[tokio::test]
    async fn two_root_regression_gc_flow_deletes_retired_root_leaves_survivor_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let root_gone = tempfile::tempdir().expect("root gone");
        let root_survivor = tempfile::tempdir().expect("root survivor");
        // Same corpus name at both roots -- this is what actually
        // discriminates a delete correctly keyed on `root` alone from one
        // mistakenly keyed on `corpus` (or `(corpus, root)`): with two
        // different corpus names, a corpus-scoped delete would also leave
        // the survivor untouched, and this regression test would not catch
        // the #215 sibling-wipe shape it exists to prevent.
        let key_gone = corpus_key(
            "repo:shared:corpus",
            root_gone.path().to_str().expect("utf8"),
        );
        let key_survivor = corpus_key(
            "repo:shared:corpus",
            root_survivor.path().to_str().expect("utf8"),
        );
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_gone,
                "/tmp/gone.md",
                2,
                "gone",
                10,
                100,
            )])
            .await
            .expect("seed gone root");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_survivor,
                "/tmp/survivor.md",
                2,
                "survivor",
                10,
                100,
            )])
            .await
            .expect("seed survivor root");

        // Retire root_gone by deleting its directory on disk.
        let gone_path = root_gone.path().to_path_buf();
        drop(root_gone);
        assert!(!gone_path.exists());

        let known = store.distinct_roots().await.expect("distinct roots");
        let retired = hallouminate_domain::common::retired_roots(&known);
        assert_eq!(
            retired.iter().map(|r| r.as_path()).collect::<Vec<_>>(),
            vec![key_gone.canonical_root.as_path()]
        );
        for root in &retired {
            store.delete_root(root).await.expect("delete retired root");
        }

        let stats_gone = store
            .corpus_chunk_stats(&key_gone)
            .await
            .expect("stats gone");
        assert_eq!(stats_gone.total_chunks, 0);
        let stats_survivor = store
            .corpus_chunk_stats(&key_survivor)
            .await
            .expect("stats survivor");
        assert_eq!(stats_survivor.total_chunks, 2);
    }

    #[tokio::test]
    async fn delete_root_removes_both_corpora_sharing_one_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let root = tempfile::tempdir().expect("root");
        let key_wiki = corpus_key("repo:x:wiki", root.path().to_str().expect("utf8"));
        let key_corpus = corpus_key("repo:x:corpus", root.path().to_str().expect("utf8"));
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_wiki,
                "/tmp/wiki.md",
                2,
                "wiki",
                10,
                100,
            )])
            .await
            .expect("seed wiki corpus");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_corpus,
                "/tmp/corpus.md",
                3,
                "corpus",
                10,
                100,
            )])
            .await
            .expect("seed corpus corpus");

        drop(root);
        let retired = hallouminate_domain::common::retired_roots(std::slice::from_ref(
            &key_wiki.canonical_root,
        ));
        let retired_root = retired.into_iter().next().expect("root retired");

        let removed = store
            .delete_root(&retired_root)
            .await
            .expect("delete shared root");
        assert_eq!(removed, 5);

        let stats_wiki = store
            .corpus_chunk_stats(&key_wiki)
            .await
            .expect("stats wiki");
        assert_eq!(stats_wiki.total_chunks, 0);
        let stats_corpus = store
            .corpus_chunk_stats(&key_corpus)
            .await
            .expect("stats corpus");
        assert_eq!(stats_corpus.total_chunks, 0);
    }

    #[tokio::test]
    async fn delete_root_escapes_a_single_quote_in_the_root_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            LanceStore::open_or_create(dir.path(), "BAAI/bge-small-en-v1.5", false, false, None)
                .await
                .expect("open store");
        let parent = tempfile::tempdir().expect("parent");
        let quoted_root = parent.path().join("o'brien");
        std::fs::create_dir(&quoted_root).expect("create quoted root");
        let adversarial_root = parent.path().join("x' OR '1'='1");
        std::fs::create_dir(&adversarial_root).expect("create adversarial root");
        let survivor_root = tempfile::tempdir().expect("survivor root");

        let key_quoted = corpus_key("repo:quoted:corpus", quoted_root.to_str().expect("utf8"));
        let key_adversarial = corpus_key(
            "repo:adversarial:corpus",
            adversarial_root.to_str().expect("utf8"),
        );
        let key_survivor = corpus_key(
            "repo:survivor:corpus",
            survivor_root.path().to_str().expect("utf8"),
        );
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_quoted,
                "/tmp/quoted.md",
                2,
                "quoted",
                10,
                100,
            )])
            .await
            .expect("seed quoted root");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_adversarial,
                "/tmp/adversarial.md",
                4,
                "adversarial",
                10,
                100,
            )])
            .await
            .expect("seed adversarial root");
        store
            .apply_batch(vec![synthetic_prepared_for(
                &key_survivor,
                "/tmp/survivor.md",
                3,
                "survivor",
                10,
                100,
            )])
            .await
            .expect("seed survivor root");

        std::fs::remove_dir_all(&quoted_root).ok();
        let retired_quoted = hallouminate_domain::common::retired_roots(std::slice::from_ref(
            &key_quoted.canonical_root,
        ))
        .into_iter()
        .next()
        .expect("quoted root retired");
        let removed = store
            .delete_root(&retired_quoted)
            .await
            .expect("delete quoted root");
        assert_eq!(removed, 2);

        std::fs::remove_dir_all(&adversarial_root).ok();
        let retired_adversarial = hallouminate_domain::common::retired_roots(std::slice::from_ref(
            &key_adversarial.canonical_root,
        ))
        .into_iter()
        .next()
        .expect("adversarial root retired");
        let removed_adversarial = store
            .delete_root(&retired_adversarial)
            .await
            .expect("delete adversarial root");
        assert_eq!(removed_adversarial, 4);

        let stats_quoted = store
            .corpus_chunk_stats(&key_quoted)
            .await
            .expect("stats quoted");
        assert_eq!(stats_quoted.total_chunks, 0);
        let stats_adversarial = store
            .corpus_chunk_stats(&key_adversarial)
            .await
            .expect("stats adversarial");
        assert_eq!(stats_adversarial.total_chunks, 0);
        let stats_survivor = store
            .corpus_chunk_stats(&key_survivor)
            .await
            .expect("stats survivor");
        assert_eq!(stats_survivor.total_chunks, 3);
    }
}
