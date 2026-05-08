use rusqlite::params;

use crate::adapters::sqlite::pool::DbConn;
use crate::domains::common::Result;

#[derive(Debug, Clone)]
pub struct NewChunk<'a> {
    pub file_id: i64,
    pub ord: i64,
    pub heading_path_json: &'a str,
    pub line_start: i64,
    pub line_end: i64,
    pub text: &'a str,
}

pub fn insert_chunk(conn: &DbConn, chunk: &NewChunk<'_>) -> Result<i64> {
    let raw = conn.raw();
    raw.execute(
        "INSERT INTO chunks \
            (file_id, ord, heading_path, line_start, line_end, text) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            chunk.file_id,
            chunk.ord,
            chunk.heading_path_json,
            chunk.line_start,
            chunk.line_end,
            chunk.text,
        ],
    )?;
    Ok(raw.last_insert_rowid())
}

pub fn delete_chunks_for_file(conn: &DbConn, file_id: i64) -> Result<()> {
    conn.raw()
        .execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::pool::open_db;
    use crate::adapters::sqlite::queries::vec::{insert_vec, EMBEDDING_DIM};
    use crate::adapters::sqlite::queries::{delete_file_cascade, upsert_file, NewFile};
    use crate::adapters::sqlite::schema::apply_schema;

    fn fresh_conn() -> DbConn {
        let db = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&db).expect("apply schema");
        db
    }

    fn sample_file<'a>(file_ref: &'a str, corpus: &'a str) -> NewFile<'a> {
        NewFile {
            file_ref,
            corpus,
            mtime_ms: 1_700_000_000_000,
            content_hash: "deadbeef",
            summary: None,
            keywords_json: "[]",
            indexed_at_ms: 1_700_000_001_000,
        }
    }

    fn sample_chunk(file_id: i64, text: &str) -> NewChunk<'_> {
        NewChunk {
            file_id,
            ord: 0,
            heading_path_json: "[\"Arrakis\"]",
            line_start: 1,
            line_end: 4,
            text,
        }
    }

    fn fts_match_count(db: &DbConn, term: &str) -> i64 {
        db.raw()
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                params![term],
                |row| row.get(0),
            )
            .expect("fts query")
    }

    fn vec_count(db: &DbConn) -> i64 {
        db.raw()
            .query_row("SELECT count(*) FROM chunks_vec", [], |row| row.get(0))
            .expect("count chunks_vec")
    }

    #[test]
    fn insert_chunk_makes_text_searchable_via_fts() {
        let db = fresh_conn();
        let file_id = upsert_file(&db, &sample_file("/tmp/melange.md", "docs")).expect("upsert");
        let chunk_id = insert_chunk(&db, &sample_chunk(file_id, "the spice melange flows"))
            .expect("insert chunk");
        assert!(chunk_id > 0);
        assert_eq!(fts_match_count(&db, "melange"), 1);
        assert_eq!(fts_match_count(&db, "sandworm"), 0);
    }

    #[test]
    fn delete_chunks_for_file_removes_fts_rows() {
        let db = fresh_conn();
        let file_id = upsert_file(&db, &sample_file("/tmp/spice.md", "docs")).expect("upsert");
        insert_chunk(&db, &sample_chunk(file_id, "fremen ride sandworms")).expect("chunk");
        assert_eq!(fts_match_count(&db, "fremen"), 1);
        delete_chunks_for_file(&db, file_id).expect("delete chunks");
        assert_eq!(fts_match_count(&db, "fremen"), 0);
    }

    #[test]
    fn delete_file_cascade_purges_chunks_fts_and_vec() {
        let db = fresh_conn();
        let file_id = upsert_file(&db, &sample_file("/tmp/citadel.md", "docs")).expect("upsert");
        let chunk_id =
            insert_chunk(&db, &sample_chunk(file_id, "witness me chrome warriors")).expect("chunk");
        let mut embedding = [0.0f32; EMBEDDING_DIM];
        embedding[0] = 1.0;
        insert_vec(&db, chunk_id, &embedding).expect("insert vec");
        assert_eq!(fts_match_count(&db, "chrome"), 1);
        assert_eq!(vec_count(&db), 1);

        delete_file_cascade(&db, file_id).expect("cascade");

        let chunks_left: i64 = db
            .raw()
            .query_row(
                "SELECT count(*) FROM chunks WHERE file_id = ?1",
                params![file_id],
                |row| row.get(0),
            )
            .expect("count chunks");
        assert_eq!(chunks_left, 0);
        assert_eq!(fts_match_count(&db, "chrome"), 0);
        assert_eq!(
            vec_count(&db),
            0,
            "chunks_vec must be drained by chunks_ad_vec trigger"
        );
    }
}
