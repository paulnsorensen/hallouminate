use rusqlite::Connection;

use crate::domains::common::Result;

const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
    file_id       INTEGER PRIMARY KEY,
    file_ref      TEXT NOT NULL UNIQUE,
    corpus        TEXT NOT NULL,
    mtime_ms      INTEGER NOT NULL,
    content_hash  TEXT NOT NULL,
    summary       TEXT,
    keywords      TEXT NOT NULL DEFAULT '[]',
    indexed_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_corpus ON files(corpus);

CREATE TABLE IF NOT EXISTS chunks (
    chunk_id     INTEGER PRIMARY KEY,
    file_id      INTEGER NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
    ord          INTEGER NOT NULL,
    heading_path TEXT NOT NULL DEFAULT '[]',
    line_start   INTEGER NOT NULL,
    line_end     INTEGER NOT NULL,
    text         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text, heading_path,
    content='chunks', content_rowid='chunk_id',
    tokenize='porter unicode61'
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    chunk_id  INTEGER PRIMARY KEY,
    embedding FLOAT[384]
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text, heading_path)
    VALUES (new.chunk_id, new.text, new.heading_path);
END;

CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text, heading_path)
    VALUES ('delete', old.chunk_id, old.text, old.heading_path);
END;

CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text, heading_path)
    VALUES ('delete', old.chunk_id, old.text, old.heading_path);
    INSERT INTO chunks_fts(rowid, text, heading_path)
    VALUES (new.chunk_id, new.text, new.heading_path);
END;
"#;

pub fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::pool::open_db;

    fn fresh_conn() -> Connection {
        open_db(Path::new(":memory:")).expect("open :memory:")
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("prepare sqlite_master query");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query_map")
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn apply_schema_is_idempotent() {
        let conn = fresh_conn();
        apply_schema(&conn).expect("first apply");
        apply_schema(&conn).expect("second apply must not error");
    }

    #[test]
    fn apply_schema_creates_required_tables() {
        let conn = fresh_conn();
        apply_schema(&conn).expect("apply");
        let names = table_names(&conn);
        for required in ["files", "chunks", "chunks_fts", "chunks_vec", "meta"] {
            assert!(
                names.iter().any(|n| n == required),
                "missing {required}: have {names:?}"
            );
        }
    }

    #[test]
    fn fts_insert_trigger_indexes_chunk_text() {
        let conn = fresh_conn();
        apply_schema(&conn).expect("apply");
        conn.execute(
            "INSERT INTO files \
             (file_id, file_ref, corpus, mtime_ms, content_hash, indexed_at_ms) \
             VALUES (1, '/tmp/a.md', 'docs', 0, 'deadbeef', 0)",
            [],
        )
        .expect("insert file");
        conn.execute(
            "INSERT INTO chunks \
             (chunk_id, file_id, ord, line_start, line_end, text) \
             VALUES (1, 1, 0, 1, 1, 'sandworm rides on Arrakis')",
            [],
        )
        .expect("insert chunk");
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'sandworm'",
                [],
                |row| row.get(0),
            )
            .expect("fts query");
        assert_eq!(hits, 1, "fts trigger must index inserted chunk text");
    }
}
