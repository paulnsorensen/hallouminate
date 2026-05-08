use rusqlite::params;

use crate::adapters::sqlite::pool::DbConn;
use crate::domains::common::Result;

pub const EMBEDDING_DIM: usize = 384;

fn vec_to_bytes(v: &[f32; EMBEDDING_DIM]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn insert_vec(conn: &DbConn, chunk_id: i64, embedding: &[f32; EMBEDDING_DIM]) -> Result<()> {
    let bytes = vec_to_bytes(embedding);
    conn.raw().execute(
        "INSERT INTO chunks_vec (chunk_id, embedding) VALUES (?1, ?2)",
        params![chunk_id, bytes],
    )?;
    Ok(())
}

pub fn delete_vec_for_chunk(conn: &DbConn, chunk_id: i64) -> Result<()> {
    conn.raw().execute(
        "DELETE FROM chunks_vec WHERE chunk_id = ?1",
        params![chunk_id],
    )?;
    Ok(())
}

pub fn knn_chunks(
    conn: &DbConn,
    query: &[f32; EMBEDDING_DIM],
    k: usize,
) -> Result<Vec<(i64, f64)>> {
    debug_assert!(k > 0, "knn_chunks: k must be >= 1");
    let bytes = vec_to_bytes(query);
    let mut stmt = conn.raw().prepare(
        "SELECT chunk_id, vec_distance_cosine(embedding, ?1) AS distance \
         FROM chunks_vec \
         ORDER BY distance ASC \
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![bytes, k as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::pool::open_db;
    use crate::adapters::sqlite::schema::apply_schema;

    fn fresh_conn() -> DbConn {
        let db = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&db).expect("apply schema");
        db
    }

    fn unit_basis(idx: usize) -> [f32; EMBEDDING_DIM] {
        let mut v = [0.0f32; EMBEDDING_DIM];
        v[idx] = 1.0;
        v
    }

    #[test]
    fn knn_chunks_ranks_matching_vector_first() {
        let db = fresh_conn();
        insert_vec(&db, 1, &unit_basis(0)).expect("insert e0");
        insert_vec(&db, 2, &unit_basis(1)).expect("insert e1");
        let hits = knn_chunks(&db, &unit_basis(0), 2).expect("knn");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1, "matching basis vector must rank first");
        assert_eq!(hits[1].0, 2);
        assert!(
            hits[0].1 < hits[1].1,
            "matching vector must have smaller cosine distance: {hits:?}"
        );
        assert!(
            hits[0].1 < 1e-5,
            "self-match cosine distance must be ~0, got {}",
            hits[0].1
        );
    }

    #[test]
    fn knn_chunks_respects_k_limit() {
        let db = fresh_conn();
        insert_vec(&db, 1, &unit_basis(0)).expect("a");
        insert_vec(&db, 2, &unit_basis(1)).expect("b");
        insert_vec(&db, 3, &unit_basis(2)).expect("c");
        let hits = knn_chunks(&db, &unit_basis(0), 1).expect("knn");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 1);
    }

    #[test]
    fn delete_vec_for_chunk_purges_row() {
        let db = fresh_conn();
        insert_vec(&db, 7, &unit_basis(0)).expect("insert");
        delete_vec_for_chunk(&db, 7).expect("delete");
        let count: i64 = db
            .raw()
            .query_row("SELECT count(*) FROM chunks_vec", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "k must be >= 1")]
    fn knn_chunks_rejects_zero_k_in_debug() {
        let db = fresh_conn();
        let _ = knn_chunks(&db, &unit_basis(0), 0);
    }
}
