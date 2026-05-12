use crate::adapters::sqlite::{delete_file_cascade, get_file_by_ref, touch_mtime, DbConn, FileRow};
use crate::domain::common::{CorpusConfig, Result};
use crate::domain::corpus::blake3_file;
use crate::domain::embeddings::EmbedBatch;

use super::plan::{IndexPlan, MtimeCandidate, Upsert};
use super::writer::{file_ref_string, write_file_chunks, WriteRequest};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub files_upserted: usize,
    pub files_touched: usize,
    pub files_deleted: usize,
    pub chunks_inserted: usize,
    pub embeddings_inserted: usize,
}

pub fn apply(
    plan: IndexPlan,
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
    corpus: &CorpusConfig,
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    for upsert in plan.upserts {
        run_upsert(conn, embedder, corpus, upsert, &mut stats)?;
    }
    for cand in plan.mtime_touches {
        run_touch_or_upsert(conn, embedder, corpus, cand, &mut stats)?;
    }
    for row in plan.deletes {
        run_delete(conn, row, &mut stats)?;
    }
    tracing::debug!(
        target: "hallouminate::indexer",
        embeddings_inserted_total = stats.embeddings_inserted,
        "apply finished"
    );
    Ok(stats)
}

fn run_upsert(
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
    corpus: &CorpusConfig,
    upsert: Upsert,
    stats: &mut ApplyStats,
) -> Result<()> {
    let tx = conn.raw().unchecked_transaction()?;
    let prior = get_file_by_ref(conn, &file_ref_string(&upsert.file)?)?;
    write_file_chunks(
        conn,
        embedder,
        WriteRequest {
            corpus,
            file: &upsert.file,
            mtime: upsert.mtime,
            prior,
        },
        stats,
    )?;
    tx.commit()?;
    stats.files_upserted += 1;
    Ok(())
}

fn run_touch_or_upsert(
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
    corpus: &CorpusConfig,
    cand: MtimeCandidate,
    stats: &mut ApplyStats,
) -> Result<()> {
    let new_hash = blake3_file(cand.file.as_path())?;
    if new_hash == cand.row.content_hash {
        let tx = conn.raw().unchecked_transaction()?;
        touch_mtime(conn, cand.row.file_id, cand.new_mtime.0)?;
        tx.commit()?;
        stats.files_touched += 1;
        return Ok(());
    }
    let tx = conn.raw().unchecked_transaction()?;
    write_file_chunks(
        conn,
        embedder,
        WriteRequest {
            corpus,
            file: &cand.file,
            mtime: cand.new_mtime,
            prior: Some(cand.row),
        },
        stats,
    )?;
    tx.commit()?;
    stats.files_upserted += 1;
    Ok(())
}

fn run_delete(conn: &DbConn, row: FileRow, stats: &mut ApplyStats) -> Result<()> {
    let tx = conn.raw().unchecked_transaction()?;
    delete_file_cascade(conn, row.file_id)?;
    tx.commit()?;
    stats.files_deleted += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::apply;
    use crate::adapters::sqlite::{
        apply_schema, get_file_by_ref, insert_chunk, insert_vec, open_db, upsert_file, DbConn,
        FileRow, NewChunk, NewFile,
    };
    use crate::domain::common::{CorpusConfig, FileRef, Mtime, Result};
    use crate::domain::corpus::blake3_file;
    use crate::domain::embeddings::{EmbedBatch, EMBEDDING_DIM};
    use crate::domain::indexer::plan::{IndexPlan, MtimeCandidate, Upsert};

    struct ZeroEmbedder;

    impl EmbedBatch for ZeroEmbedder {
        fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<[f32; EMBEDDING_DIM]>> {
            Ok(texts.iter().map(|_| [0.0f32; EMBEDDING_DIM]).collect())
        }
    }

    fn fresh_conn() -> DbConn {
        let conn = open_db(Path::new(":memory:")).expect("open :memory:");
        apply_schema(&conn).expect("apply schema");
        conn
    }

    fn docs_corpus() -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            ..Default::default()
        }
    }

    fn count(conn: &DbConn, sql: &str) -> i64 {
        conn.raw().query_row(sql, [], |row| row.get(0)).expect(sql)
    }

    fn fts_count(conn: &DbConn, term: &str) -> i64 {
        let sql = "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1";
        conn.raw()
            .query_row(sql, rusqlite::params![term], |r| r.get(0))
            .expect("fts count")
    }

    fn fetch(conn: &DbConn, file_ref: &str) -> FileRow {
        get_file_by_ref(conn, file_ref).unwrap().unwrap()
    }

    fn seed_file(conn: &DbConn, file_ref: &str, hash: &str) -> i64 {
        upsert_file(
            conn,
            &NewFile {
                file_ref,
                corpus: "docs",
                mtime_ms: 1,
                content_hash: hash,
                summary: None,
                keywords_json: "[]",
                indexed_at_ms: 1,
            },
        )
        .expect("seed file")
    }

    fn seed_chunk_with_vec(conn: &DbConn, file_id: i64, text: &str) -> i64 {
        let chunk_id = insert_chunk(
            conn,
            &NewChunk {
                file_id,
                ord: 0,
                heading_path_json: "[]",
                line_start: 1,
                line_end: 1,
                text,
            },
        )
        .expect("chunk");
        insert_vec(conn, chunk_id, &[0.0f32; EMBEDDING_DIM]).expect("vec");
        chunk_id
    }

    #[test]
    fn run_delete_purges_files_chunks_fts_and_vec_rows() {
        let conn = fresh_conn();
        let file_id = seed_file(&conn, "/tmp/doomed.md", "abc");
        seed_chunk_with_vec(&conn, file_id, "spice melange");
        let row = fetch(&conn, "/tmp/doomed.md");
        let plan = IndexPlan {
            deletes: vec![row],
            ..Default::default()
        };
        let stats = apply(plan, &conn, &mut ZeroEmbedder, &docs_corpus()).expect("apply");
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM files"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM chunks"), 0);
        assert_eq!(fts_count(&conn, "melange"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM chunks_vec"), 0);
    }

    #[test]
    fn run_touch_calls_touch_mtime_when_hash_unchanged() {
        let conn = fresh_conn();
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("stable.md");
        fs::write(&path, "# Title\nbody\n").expect("write");
        let hash = blake3_file(&path).expect("hash");
        seed_file(&conn, path.to_str().unwrap(), &hash);
        let row = fetch(&conn, path.to_str().unwrap());
        let plan = IndexPlan {
            mtime_touches: vec![MtimeCandidate {
                file: FileRef::new(path.clone()),
                row,
                new_mtime: Mtime(500),
            }],
            ..Default::default()
        };
        let stats = apply(plan, &conn, &mut ZeroEmbedder, &docs_corpus()).expect("apply");
        assert_eq!(stats.files_touched, 1);
        assert_eq!(stats.embeddings_inserted, 0);
        let after = fetch(&conn, path.to_str().unwrap());
        assert_eq!(after.mtime_ms, 500);
        assert_eq!(after.content_hash, hash);
    }

    #[test]
    fn run_touch_falls_through_to_upsert_when_hash_changed() {
        let conn = fresh_conn();
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("changed.md");
        fs::write(&path, "old body\n").expect("write old");
        let old_hash = blake3_file(&path).expect("hash");
        let file_id = seed_file(&conn, path.to_str().unwrap(), &old_hash);
        seed_chunk_with_vec(&conn, file_id, "stale text");
        fs::write(&path, "# New\n\nbrand new body\n").expect("rewrite");
        let row = fetch(&conn, path.to_str().unwrap());
        let plan = IndexPlan {
            mtime_touches: vec![MtimeCandidate {
                file: FileRef::new(path.clone()),
                row,
                new_mtime: Mtime(999),
            }],
            ..Default::default()
        };
        let stats = apply(plan, &conn, &mut ZeroEmbedder, &docs_corpus()).expect("apply");
        assert_eq!(stats.files_upserted, 1);
        assert!(stats.embeddings_inserted >= 1);
        let after = fetch(&conn, path.to_str().unwrap());
        assert_ne!(after.content_hash, old_hash);
        assert_eq!(after.mtime_ms, 999);
        assert_eq!(fts_count(&conn, "stale"), 0);
    }

    #[test]
    fn run_upsert_creates_file_chunks_and_vectors() {
        let conn = fresh_conn();
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("new.md");
        fs::write(&path, "# Hello\n\nfirst\n## Second\n\nbody two\n").expect("write");
        let plan = IndexPlan {
            upserts: vec![Upsert {
                file: FileRef::new(path.clone()),
                mtime: Mtime(42),
            }],
            ..Default::default()
        };
        let stats = apply(plan, &conn, &mut ZeroEmbedder, &docs_corpus()).expect("apply");
        assert_eq!(stats.files_upserted, 1);
        assert_eq!(stats.chunks_inserted, 2);
        assert_eq!(stats.embeddings_inserted, 2);
        assert_eq!(count(&conn, "SELECT count(*) FROM chunks_vec"), 2);
        let row = fetch(&conn, path.to_str().unwrap());
        assert_eq!(row.corpus, "docs");
        assert!(row.summary.unwrap().contains("Hello"));
    }
}
