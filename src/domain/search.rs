//! Hybrid search facade.
//!
//! Wraps `LanceStore`'s hybrid query (BM25 full-text + vector ANN reranked
//! with the built-in `RRFReranker`). Domain layer stays decoupled from the
//! storage crate via this single re-export.

use std::sync::Arc;

use arrow_array::{Array, Int64Array, ListArray, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::rerankers::rrf::RRFReranker;

use crate::adapters::lance::{LanceStore, SearchHit};
use crate::domain::common::{HallouminateError, Result};

pub async fn hybrid_search(
    store: &LanceStore,
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let reranker = Arc::new(RRFReranker::default());
    let stream = store
        .table()
        .query()
        .full_text_search(FullTextSearchQuery::new(query.to_string()))
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

fn decode_hits(rb: &RecordBatch, out: &mut Vec<SearchHit>) -> Result<()> {
    let chunk_id = string_col(rb, "chunk_id")?;
    let file_ref = string_col(rb, "file_ref")?;
    let summary = string_col(rb, "summary")?;
    let text = string_col(rb, "text")?;
    let line_start = int64_col(rb, "line_start")?;
    let line_end = int64_col(rb, "line_end")?;
    let heading_path = list_utf8_col(rb, "heading_path")?;
    let keywords = list_utf8_col(rb, "keywords")?;
    let score = score_col(rb);
    for i in 0..rb.num_rows() {
        let s = score
            .as_ref()
            .map(|arr| {
                arr.as_any()
                    .downcast_ref::<arrow_array::Float32Array>()
                    .map(|f| f.value(i))
                    .unwrap_or(0.0)
            })
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
        });
    }
    Ok(())
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

fn score_col(rb: &RecordBatch) -> Option<arrow_array::ArrayRef> {
    rb.column_by_name("_relevance_score")
        .or_else(|| rb.column_by_name("_score"))
        .cloned()
}

fn decode_list(list: &ListArray, row: usize) -> Vec<String> {
    let values = list.value(row);
    let strs = values.as_any().downcast_ref::<StringArray>();
    let Some(s) = strs else {
        return Vec::new();
    };
    (0..s.len()).map(|i| s.value(i).to_string()).collect()
}

fn map_lance_err<E: std::fmt::Display>(e: E) -> HallouminateError {
    HallouminateError::Db(Box::new(std::io::Error::other(format!("lance: {e}"))))
}
