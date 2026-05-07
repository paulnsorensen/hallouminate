use rusqlite::params;

use crate::adapters::sqlite::DbConn;
use crate::domain::common::{ChunkId, Result};

pub fn fts_search(conn: &DbConn, query: &str, limit: usize) -> Result<Vec<(ChunkId, f64)>> {
    let mut stmt = conn.raw().prepare(
        "SELECT rowid, rank FROM chunks_fts \
         WHERE chunks_fts MATCH ?1 \
         ORDER BY rank \
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![query, limit as i64], |row| {
            Ok((ChunkId(row.get::<_, i64>(0)?), row.get::<_, f64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::{
        apply_schema, insert_chunk, open_db, upsert_file, NewChunk, NewFile,
    };

    fn fresh_conn() -> DbConn {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    fn seed_file(conn: &DbConn, file_ref: &str) -> i64 {
        upsert_file(
            conn,
            &NewFile {
                file_ref,
                corpus: "docs",
                mtime_ms: 0,
                content_hash: "h",
                summary: None,
                keywords_json: "[]",
                indexed_at_ms: 0,
            },
        )
        .expect("upsert file")
    }

    fn seed_chunk(conn: &DbConn, file_id: i64, ord: i64, text: &str) -> i64 {
        insert_chunk(
            conn,
            &NewChunk {
                file_id,
                ord,
                heading_path_json: "[]",
                line_start: 1,
                line_end: 4,
                text,
            },
        )
        .expect("insert chunk")
    }

    #[test]
    fn fts_search_returns_chunk_matching_keyword() {
        let conn = fresh_conn();
        let file_id = seed_file(&conn, "/tmp/a.md");
        let target = seed_chunk(&conn, file_id, 0, "the spice melange flows on Arrakis");
        seed_chunk(&conn, file_id, 1, "fremen ride sandworms across dunes");

        let hits = fts_search(&conn, "melange", 5).expect("fts");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, ChunkId(target));
    }

    #[test]
    fn fts_search_orders_more_relevant_first() {
        let conn = fresh_conn();
        let file_id = seed_file(&conn, "/tmp/b.md");
        let weak = seed_chunk(&conn, file_id, 0, "spice appears once here");
        let strong = seed_chunk(
            &conn,
            file_id,
            1,
            "spice spice spice spice melange spice flow",
        );

        let hits = fts_search(&conn, "spice", 5).expect("fts");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, ChunkId(strong));
        assert_eq!(hits[1].0, ChunkId(weak));
        assert!(
            hits[0].1 < hits[1].1,
            "stronger match must have lower (more negative) bm25 rank: {hits:?}"
        );
    }

    #[test]
    fn fts_search_respects_limit() {
        let conn = fresh_conn();
        let file_id = seed_file(&conn, "/tmp/c.md");
        seed_chunk(&conn, file_id, 0, "alpha bravo");
        seed_chunk(&conn, file_id, 1, "alpha charlie");
        seed_chunk(&conn, file_id, 2, "alpha delta");

        let hits = fts_search(&conn, "alpha", 2).expect("fts");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fts_search_returns_empty_when_no_match() {
        let conn = fresh_conn();
        let file_id = seed_file(&conn, "/tmp/d.md");
        seed_chunk(&conn, file_id, 0, "only common words live here");

        let hits = fts_search(&conn, "kwisatzhaderach", 5).expect("fts");
        assert!(hits.is_empty(), "no rows must match: {hits:?}");
    }
}
