use rusqlite::{OptionalExtension, Row, params};

use crate::adapters::sqlite::pool::DbConn;
use crate::domain::common::Result;

pub mod chunks;
pub mod vec;

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

pub fn upsert_file(conn: &DbConn, file: &NewFile<'_>) -> Result<i64> {
    let file_id: i64 = conn.raw().query_row(
        "INSERT INTO files \
            (file_ref, corpus, mtime_ms, content_hash, summary, keywords, indexed_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(file_ref) DO UPDATE SET \
             corpus = excluded.corpus, \
             mtime_ms = excluded.mtime_ms, \
             content_hash = excluded.content_hash, \
             summary = excluded.summary, \
             keywords = excluded.keywords, \
             indexed_at_ms = excluded.indexed_at_ms \
         RETURNING file_id",
        params![
            file.file_ref,
            file.corpus,
            file.mtime_ms,
            file.content_hash,
            file.summary,
            file.keywords_json,
            file.indexed_at_ms,
        ],
        |row| row.get(0),
    )?;
    Ok(file_id)
}

pub fn get_file_by_ref(conn: &DbConn, file_ref: &str) -> Result<Option<FileRow>> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE file_ref = ?1");
    let mut stmt = conn.raw().prepare(&sql)?;
    let row = stmt.query_row(params![file_ref], row_to_file).optional()?;
    Ok(row)
}

pub fn touch_mtime(conn: &DbConn, file_id: i64, mtime_ms: i64) -> Result<()> {
    conn.raw().execute(
        "UPDATE files SET mtime_ms = ?1 WHERE file_id = ?2",
        params![mtime_ms, file_id],
    )?;
    Ok(())
}

pub fn delete_file_cascade(conn: &DbConn, file_id: i64) -> Result<()> {
    conn.raw()
        .execute("DELETE FROM files WHERE file_id = ?1", params![file_id])?;
    Ok(())
}

pub fn all_files_for_corpus(conn: &DbConn, corpus: &str) -> Result<Vec<FileRow>> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE corpus = ?1 ORDER BY file_ref");
    let mut stmt = conn.raw().prepare(&sql)?;
    let rows = stmt
        .query_map(params![corpus], row_to_file)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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

    fn fresh_conn() -> DbConn {
        let db = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&db).expect("apply schema");
        db
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
        let db = fresh_conn();
        let id = upsert_file(&db, &sample("/tmp/dune.md", "docs")).expect("upsert");
        assert!(id > 0);
        let fetched = get_file_by_ref(&db, "/tmp/dune.md")
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
        let db = fresh_conn();
        let id_first = upsert_file(&db, &sample("/tmp/a.md", "docs")).expect("first");
        let mut second = sample("/tmp/a.md", "docs");
        second.content_hash = "cafebabe";
        second.mtime_ms = 1_700_000_500_000;
        let id_second = upsert_file(&db, &second).expect("second");
        assert_eq!(id_first, id_second, "upsert must keep the same file_id");
        let fetched = get_file_by_ref(&db, "/tmp/a.md").expect("get").unwrap();
        assert_eq!(fetched.content_hash, "cafebabe");
        assert_eq!(fetched.mtime_ms, 1_700_000_500_000);
    }

    #[test]
    fn get_file_by_ref_returns_none_for_missing() {
        let db = fresh_conn();
        let missing = get_file_by_ref(&db, "/tmp/ghost.md").expect("query");
        assert!(missing.is_none());
    }

    #[test]
    fn touch_mtime_updates_only_mtime_field() {
        let db = fresh_conn();
        let id = upsert_file(&db, &sample("/tmp/b.md", "docs")).expect("upsert");
        touch_mtime(&db, id, 9_999_999_999).expect("touch");
        let fetched = get_file_by_ref(&db, "/tmp/b.md").expect("get").unwrap();
        assert_eq!(fetched.mtime_ms, 9_999_999_999);
        assert_eq!(fetched.content_hash, "deadbeef");
    }

    #[test]
    fn delete_file_cascade_removes_row() {
        let db = fresh_conn();
        let id = upsert_file(&db, &sample("/tmp/c.md", "docs")).expect("upsert");
        delete_file_cascade(&db, id).expect("delete");
        assert!(get_file_by_ref(&db, "/tmp/c.md").expect("query").is_none());
    }

    #[test]
    fn all_files_for_corpus_filters_and_orders() {
        let db = fresh_conn();
        upsert_file(&db, &sample("/tmp/z.md", "docs")).expect("z");
        upsert_file(&db, &sample("/tmp/a.md", "docs")).expect("a");
        upsert_file(&db, &sample("/tmp/x.md", "code")).expect("x");
        let docs = all_files_for_corpus(&db, "docs").expect("docs");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].file_ref, "/tmp/a.md");
        assert_eq!(docs[1].file_ref, "/tmp/z.md");
        let code = all_files_for_corpus(&db, "code").expect("code");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].file_ref, "/tmp/x.md");
    }
}
