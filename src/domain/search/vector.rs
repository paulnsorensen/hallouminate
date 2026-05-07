use std::num::NonZeroUsize;

use crate::adapters::sqlite::{knn_chunks, DbConn, EMBEDDING_DIM};
use crate::domain::common::{ChunkId, HallouminateError, Result};

pub fn vec_search(
    conn: &DbConn,
    query_embedding: &[f32; EMBEDDING_DIM],
    limit: usize,
) -> Result<Vec<(ChunkId, f64)>> {
    let k = NonZeroUsize::new(limit)
        .ok_or_else(|| HallouminateError::Config("vec_search limit must be > 0".into()))?;
    let raw = knn_chunks(conn, query_embedding, k)?;
    Ok(raw
        .into_iter()
        .map(|(id, dist)| (ChunkId(id), dist))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::{apply_schema, insert_vec, open_db};

    fn fresh_conn() -> DbConn {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    fn normalize(mut v: [f32; EMBEDDING_DIM]) -> [f32; EMBEDDING_DIM] {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }

    fn vec_with(values: &[(usize, f32)]) -> [f32; EMBEDDING_DIM] {
        let mut v = [0.0f32; EMBEDDING_DIM];
        for (idx, val) in values {
            v[*idx] = *val;
        }
        normalize(v)
    }

    #[test]
    fn vec_search_orders_by_dot_product_for_normalized_inputs() {
        let conn = fresh_conn();
        let near = vec_with(&[(0, 1.0), (1, 0.1)]);
        let far = vec_with(&[(0, 0.1), (1, 1.0)]);
        insert_vec(&conn, 11, &near).expect("insert near");
        insert_vec(&conn, 22, &far).expect("insert far");

        let query = vec_with(&[(0, 1.0)]);
        let hits = vec_search(&conn, &query, 2).expect("vec_search");

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, ChunkId(11), "higher dot-product must come first");
        assert_eq!(hits[1].0, ChunkId(22));
        assert!(
            hits[0].1 < hits[1].1,
            "closer vector must have smaller cosine distance: {hits:?}"
        );
    }

    #[test]
    fn vec_search_respects_limit() {
        let conn = fresh_conn();
        insert_vec(&conn, 1, &vec_with(&[(0, 1.0)])).expect("a");
        insert_vec(&conn, 2, &vec_with(&[(1, 1.0)])).expect("b");
        insert_vec(&conn, 3, &vec_with(&[(2, 1.0)])).expect("c");

        let hits = vec_search(&conn, &vec_with(&[(0, 1.0)]), 1).expect("vec_search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, ChunkId(1));
    }

    #[test]
    fn vec_search_returns_empty_when_no_rows() {
        let conn = fresh_conn();
        let hits = vec_search(&conn, &vec_with(&[(0, 1.0)]), 5).expect("vec_search");
        assert!(hits.is_empty());
    }
}
