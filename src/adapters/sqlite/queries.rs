use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::domains::common::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub file_id: i64,
    pub file_ref: String,
    pub corpus: String,
    pub mtime_ms: i64,
    pub content_hash: String,
    pub summary: Option<String>,
    pub keywords_json: String,
    pub indexed_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct NewFile<'a> {
    pub file_ref: &'a str,
    pub corpus: &'a str,
    pub mtime_ms: i64,
    pub content_hash: &'a str,
    pub summary: Option<&'a str>,
    pub keywords_json: &'a str,
    pub indexed_at_ms: i64,
}

const FILE_COLUMNS: &str =
    "file_id, file_ref, corpus, mtime_ms, content_hash, summary, keywords, indexed_at_ms";

pub fn upsert_file(conn: &Connection, file: &NewFile<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO files \
            (file_ref, corpus, mtime_ms, content_hash, summary, keywords, indexed_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(file_ref) DO UPDATE SET \
             corpus = excluded.corpus, \
             mtime_ms = excluded.mtime_ms, \
             content_hash = excluded.content_hash, \
             summary = excluded.summary, \
             keywords = excluded.keywords, \
             indexed_at_ms = excluded.indexed_at_ms",
        params![
            file.file_ref,
            file.corpus,
            file.mtime_ms,
            file.content_hash,
            file.summary,
            file.keywords_json,
            file.indexed_at_ms,
        ],
    )?;
    let file_id: i64 = conn.query_row(
        "SELECT file_id FROM files WHERE file_ref = ?1",
        params![file.file_ref],
        |row| row.get(0),
    )?;
    Ok(file_id)
}

pub fn get_file_by_ref(conn: &Connection, file_ref: &str) -> Result<Option<FileRow>> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE file_ref = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![file_ref], row_to_file).optional()?;
    Ok(row)
}

pub fn touch_mtime(conn: &Connection, file_id: i64, mtime_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET mtime_ms = ?1 WHERE file_id = ?2",
        params![mtime_ms, file_id],
    )?;
    Ok(())
}

pub fn delete_file_cascade(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute("DELETE FROM files WHERE file_id = ?1", params![file_id])?;
    Ok(())
}

pub fn all_files_for_corpus(conn: &Connection, corpus: &str) -> Result<Vec<FileRow>> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE corpus = ?1 ORDER BY file_ref");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![corpus], row_to_file)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct NewChunk<'a> {
    pub file_id: i64,
    pub ord: i64,
    pub heading_path_json: &'a str,
    pub line_start: i64,
    pub line_end: i64,
    pub text: &'a str,
}

pub fn insert_chunk(conn: &Connection, chunk: &NewChunk<'_>) -> Result<i64> {
    conn.execute(
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
    Ok(conn.last_insert_rowid())
}

pub fn delete_chunks_for_file(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
    Ok(())
}

fn row_to_file(row: &Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        file_id: row.get(0)?,
        file_ref: row.get(1)?,
        corpus: row.get(2)?,
        mtime_ms: row.get(3)?,
        content_hash: row.get(4)?,
        summary: row.get(5)?,
        keywords_json: row.get(6)?,
        indexed_at_ms: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::pool::open_db;
    use crate::adapters::sqlite::schema::apply_schema;

    fn fresh_conn() -> Connection {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    fn sample<'a>(file_ref: &'a str, corpus: &'a str) -> NewFile<'a> {
        NewFile {
            file_ref,
            corpus,
            mtime_ms: 1_700_000_000_000,
            content_hash: "deadbeef",
            summary: Some("the spice must flow"),
            keywords_json: "[\"spice\",\"sandworm\"]",
            indexed_at_ms: 1_700_000_001_000,
        }
    }

    #[test]
    fn upsert_then_fetch_round_trips_all_columns() {
        let conn = fresh_conn();
        let id = upsert_file(&conn, &sample("/tmp/dune.md", "docs")).expect("upsert");
        assert!(id > 0);
        let fetched = get_file_by_ref(&conn, "/tmp/dune.md")
            .expect("get")
            .expect("row exists");
        assert_eq!(fetched.file_id, id);
        assert_eq!(fetched.corpus, "docs");
        assert_eq!(fetched.mtime_ms, 1_700_000_000_000);
        assert_eq!(fetched.content_hash, "deadbeef");
        assert_eq!(fetched.summary.as_deref(), Some("the spice must flow"));
        assert_eq!(fetched.keywords_json, "[\"spice\",\"sandworm\"]");
        assert_eq!(fetched.indexed_at_ms, 1_700_000_001_000);
    }

    #[test]
    fn upsert_updates_existing_row_and_preserves_id() {
        let conn = fresh_conn();
        let id_first = upsert_file(&conn, &sample("/tmp/a.md", "docs")).expect("first");
        let mut second = sample("/tmp/a.md", "docs");
        second.content_hash = "cafebabe";
        second.mtime_ms = 1_700_000_500_000;
        let id_second = upsert_file(&conn, &second).expect("second");
        assert_eq!(id_first, id_second, "upsert must keep the same file_id");
        let fetched = get_file_by_ref(&conn, "/tmp/a.md").expect("get").unwrap();
        assert_eq!(fetched.content_hash, "cafebabe");
        assert_eq!(fetched.mtime_ms, 1_700_000_500_000);
    }

    #[test]
    fn get_file_by_ref_returns_none_for_missing() {
        let conn = fresh_conn();
        let missing = get_file_by_ref(&conn, "/tmp/ghost.md").expect("query");
        assert!(missing.is_none());
    }

    #[test]
    fn touch_mtime_updates_only_mtime_field() {
        let conn = fresh_conn();
        let id = upsert_file(&conn, &sample("/tmp/b.md", "docs")).expect("upsert");
        touch_mtime(&conn, id, 9_999_999_999).expect("touch");
        let fetched = get_file_by_ref(&conn, "/tmp/b.md").expect("get").unwrap();
        assert_eq!(fetched.mtime_ms, 9_999_999_999);
        assert_eq!(fetched.content_hash, "deadbeef");
    }

    #[test]
    fn delete_file_cascade_removes_row() {
        let conn = fresh_conn();
        let id = upsert_file(&conn, &sample("/tmp/c.md", "docs")).expect("upsert");
        delete_file_cascade(&conn, id).expect("delete");
        assert!(get_file_by_ref(&conn, "/tmp/c.md")
            .expect("query")
            .is_none());
    }

    fn fts_match_count(conn: &Connection, term: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
            params![term],
            |row| row.get(0),
        )
        .expect("fts query")
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

    #[test]
    fn insert_chunk_makes_text_searchable_via_fts() {
        let conn = fresh_conn();
        let file_id = upsert_file(&conn, &sample("/tmp/melange.md", "docs")).expect("upsert");
        let chunk_id = insert_chunk(&conn, &sample_chunk(file_id, "the spice melange flows"))
            .expect("insert chunk");
        assert!(chunk_id > 0);
        assert_eq!(fts_match_count(&conn, "melange"), 1);
        assert_eq!(fts_match_count(&conn, "sandworm"), 0);
    }

    #[test]
    fn delete_chunks_for_file_removes_fts_rows() {
        let conn = fresh_conn();
        let file_id = upsert_file(&conn, &sample("/tmp/spice.md", "docs")).expect("upsert");
        insert_chunk(&conn, &sample_chunk(file_id, "fremen ride sandworms")).expect("chunk");
        assert_eq!(fts_match_count(&conn, "fremen"), 1);
        delete_chunks_for_file(&conn, file_id).expect("delete chunks");
        assert_eq!(fts_match_count(&conn, "fremen"), 0);
    }

    #[test]
    fn delete_file_cascade_purges_chunks_and_fts_rows() {
        let conn = fresh_conn();
        let file_id = upsert_file(&conn, &sample("/tmp/citadel.md", "docs")).expect("upsert");
        insert_chunk(&conn, &sample_chunk(file_id, "witness me chrome warriors")).expect("chunk");
        assert_eq!(fts_match_count(&conn, "chrome"), 1);
        delete_file_cascade(&conn, file_id).expect("cascade");
        let chunks_left: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunks WHERE file_id = ?1",
                params![file_id],
                |row| row.get(0),
            )
            .expect("count chunks");
        assert_eq!(chunks_left, 0);
        assert_eq!(fts_match_count(&conn, "chrome"), 0);
    }

    #[test]
    fn all_files_for_corpus_filters_and_orders() {
        let conn = fresh_conn();
        upsert_file(&conn, &sample("/tmp/z.md", "docs")).expect("z");
        upsert_file(&conn, &sample("/tmp/a.md", "docs")).expect("a");
        upsert_file(&conn, &sample("/tmp/x.md", "code")).expect("x");
        let docs = all_files_for_corpus(&conn, "docs").expect("docs");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].file_ref, "/tmp/a.md");
        assert_eq!(docs[1].file_ref, "/tmp/z.md");
        let code = all_files_for_corpus(&conn, "code").expect("code");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].file_ref, "/tmp/x.md");
    }
}
