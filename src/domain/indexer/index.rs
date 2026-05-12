use std::collections::HashMap;
use std::path::PathBuf;

use crate::adapters::sqlite::{all_files_for_corpus, DbConn};
use crate::domain::common::{CorpusConfig, FileRef, Result};
use crate::domain::corpus::scan;
use crate::domain::embeddings::EmbedBatch;

pub use super::apply::{apply, ApplyStats};
pub use super::plan::{plan, IndexPlan, MtimeCandidate, Upsert};

pub type IndexStats = ApplyStats;

pub fn index_corpus(
    corpus: &CorpusConfig,
    conn: &DbConn,
    embedder: &mut dyn EmbedBatch,
) -> Result<IndexStats> {
    let disk = scan(corpus)?;
    let db_rows = all_files_for_corpus(conn, &corpus.name)?;
    let db = db_rows
        .into_iter()
        .map(|row| (FileRef::new(PathBuf::from(&row.file_ref)), row))
        .collect::<HashMap<_, _>>();
    let p = plan(disk, db);
    apply(p, conn, embedder, corpus)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::adapters::sqlite::{apply_schema, open_db};
    use crate::domain::embeddings::EMBEDDING_DIM;

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

    fn seed_three_files(root: &Path) {
        fs::write(root.join("a.md"), "# Alpha\n\nfirst content\n").unwrap();
        fs::write(root.join("b.md"), "# Beta\n\nsecond content\n").unwrap();
        fs::write(root.join("c.md"), "# Charlie\n\nthird content\n").unwrap();
    }

    fn corpus_at(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
        }
    }

    fn count(conn: &DbConn, sql: &str) -> i64 {
        conn.raw().query_row(sql, [], |r| r.get(0)).expect(sql)
    }

    fn fts_match_count(conn: &DbConn, term: &str) -> i64 {
        conn.raw()
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                rusqlite::params![term],
                |r| r.get(0),
            )
            .expect("fts match count")
    }

    #[test]
    fn index_corpus_first_run_upserts_all_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        seed_three_files(tmp.path());
        let conn = fresh_conn();
        let mut emb = ZeroEmbedder;

        let stats = index_corpus(&corpus_at(tmp.path()), &conn, &mut emb).unwrap();

        assert_eq!(stats.files_upserted, 3);
        assert_eq!(stats.files_touched, 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(stats.embeddings_inserted >= 3);
        assert_eq!(count(&conn, "SELECT count(*) FROM files"), 3);
    }

    #[test]
    fn index_corpus_second_run_inserts_zero_embeddings() {
        let tmp = tempfile::tempdir().unwrap();
        seed_three_files(tmp.path());
        let corpus = corpus_at(tmp.path());
        let conn = fresh_conn();
        let mut emb = ZeroEmbedder;
        index_corpus(&corpus, &conn, &mut emb).unwrap();

        let stats = index_corpus(&corpus, &conn, &mut emb).unwrap();

        assert_eq!(stats.files_upserted, 0);
        assert_eq!(stats.files_touched, 0);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.embeddings_inserted, 0);
    }

    #[test]
    fn index_corpus_prunes_all_tables_when_file_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        seed_three_files(tmp.path());
        let corpus = corpus_at(tmp.path());
        let conn = fresh_conn();
        let mut emb = ZeroEmbedder;
        index_corpus(&corpus, &conn, &mut emb).unwrap();
        assert_eq!(fts_match_count(&conn, "third"), 1);

        fs::remove_file(tmp.path().join("c.md")).unwrap();
        let stats = index_corpus(&corpus, &conn, &mut emb).unwrap();

        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_upserted, 0);
        assert_eq!(stats.embeddings_inserted, 0);
        let chunks_total = count(&conn, "SELECT count(*) FROM chunks");
        assert_eq!(count(&conn, "SELECT count(*) FROM files"), 2);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM chunks_fts"),
            chunks_total
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM chunks_vec"),
            chunks_total
        );
        assert_eq!(fts_match_count(&conn, "third"), 0);
    }
}
