//! Hybrid search facade.
//!
//! Thin domain-layer wrapper over `LanceStore::hybrid_search`. Keeps the
//! `lancedb` crate out of the domain surface so callers depend on this
//! stable name rather than on storage internals. `corpus` is required so
//! multi-corpus stores stay isolated at query time.

use crate::adapters::lance::{LanceStore, SearchHit};
use crate::domain::common::Result;

pub async fn hybrid_search(
    store: &LanceStore,
    corpus: &str,
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchHit>> {
    store.hybrid_search(corpus, query, query_vec, limit).await
}
