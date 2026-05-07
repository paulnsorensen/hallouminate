use std::fs;
use std::path::Path;

use hallouminate::app::cli::{run_index, IndexArgs};
use rusqlite::{params, Connection};

fn write_config(config_path: &Path, corpus_root: &Path, db_path: &Path, cache_dir: &Path) {
    let toml = format!(
        r#"
[[corpus]]
name = "fixtures"
paths = [{root:?}]
globs = ["**/*.md"]

[embeddings]
model     = "bge-small-en-v1.5"
cache_dir = {cache:?}

[storage]
db_path = {db:?}
"#,
        root = corpus_root.to_string_lossy().to_string(),
        cache = cache_dir.to_string_lossy().to_string(),
        db = db_path.to_string_lossy().to_string(),
    );
    fs::write(config_path, toml).expect("write config");
}

fn seed_fixtures(root: &Path) {
    fs::write(
        root.join("arrakis.md"),
        "# Arrakis\n\n## Spice melange\n\nThe spice must flow across the dunes of Arrakis.\n",
    )
    .unwrap();
    fs::write(
        root.join("caladan.md"),
        "# Caladan\n\n## House Atreides\n\nDuke Leto rules the watery world far from the desert.\n",
    )
    .unwrap();
    fs::write(
        root.join("giedi.md"),
        "# Giedi Prime\n\n## House Harkonnen\n\nA brutal Harkonnen industrial homeworld with no spice.\n",
    )
    .unwrap();
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

fn capture_file_and_chunks(conn: &Connection, file_ref_suffix: &str) -> (i64, Vec<i64>) {
    let pattern = format!("%{file_ref_suffix}");
    let file_id: i64 = conn
        .query_row(
            "SELECT file_id FROM files WHERE file_ref LIKE ?1",
            params![pattern],
            |r| r.get(0),
        )
        .expect("file row exists for fixture");
    let mut stmt = conn
        .prepare("SELECT chunk_id FROM chunks WHERE file_id = ?1 ORDER BY chunk_id")
        .expect("prepare chunks query");
    let chunk_ids: Vec<i64> = stmt
        .query_map(params![file_id], |r| r.get::<_, i64>(0))
        .expect("query chunk ids")
        .map(|r| r.expect("chunk row"))
        .collect();
    (file_id, chunk_ids)
}

fn count_in(conn: &Connection, sql: &str, ids: &[i64]) -> i64 {
    if ids.is_empty() {
        return 0;
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let full = sql.replace("(?)", &format!("({placeholders})"));
    let params_vec: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
    conn.query_row(&full, params_vec.as_slice(), |r| r.get(0))
        .expect(sql)
}

#[test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
fn deleting_fixture_purges_files_chunks_fts_and_vec_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let db_path = dir.path().join("index.db");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &db_path, &cache_dir);

    run_index(IndexArgs {
        config: Some(config_path.clone()),
        ..Default::default()
    })
    .expect("first index run");

    let conn = Connection::open(&db_path).expect("open db after first run");
    assert_eq!(count(&conn, "SELECT count(*) FROM files"), 3);
    let (giedi_file_id, giedi_chunk_ids) = capture_file_and_chunks(&conn, "giedi.md");
    assert!(
        !giedi_chunk_ids.is_empty(),
        "giedi.md must produce at least one chunk"
    );
    let total_chunks_before = count(&conn, "SELECT count(*) FROM chunks");
    drop(conn);

    fs::remove_file(corpus_root.join("giedi.md")).expect("remove giedi.md");

    let stats = run_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .expect("second index run");
    let report = stats.corpora.first().expect("one corpus report");
    assert_eq!(
        report.files_deleted, 1,
        "second run should report exactly one deletion"
    );
    assert_eq!(report.files_upserted, 0);
    assert_eq!(report.embeddings_inserted, 0);

    let conn = Connection::open(&db_path).expect("reopen db after second run");
    assert_eq!(count(&conn, "SELECT count(*) FROM files"), 2);

    let files_left: i64 = conn
        .query_row(
            "SELECT count(*) FROM files WHERE file_id = ?1",
            params![giedi_file_id],
            |r| r.get(0),
        )
        .expect("files lookup");
    assert_eq!(files_left, 0, "files row for deleted fixture must be gone");

    let chunks_left = count_in(
        &conn,
        "SELECT count(*) FROM chunks WHERE chunk_id IN (?)",
        &giedi_chunk_ids,
    );
    assert_eq!(chunks_left, 0, "chunks rows must be purged");

    let fts_left = count_in(
        &conn,
        "SELECT count(*) FROM chunks_fts WHERE rowid IN (?)",
        &giedi_chunk_ids,
    );
    assert_eq!(fts_left, 0, "chunks_fts rows must be purged");

    let vec_left = count_in(
        &conn,
        "SELECT count(*) FROM chunks_vec WHERE chunk_id IN (?)",
        &giedi_chunk_ids,
    );
    assert_eq!(vec_left, 0, "chunks_vec rows must be purged");

    let total_chunks_after = count(&conn, "SELECT count(*) FROM chunks");
    assert_eq!(
        total_chunks_after,
        total_chunks_before - giedi_chunk_ids.len() as i64,
        "exactly the deleted fixture's chunks should be gone"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM chunks_fts"),
        total_chunks_after,
        "chunks_fts must stay in sync with chunks"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM chunks_vec"),
        total_chunks_after,
        "chunks_vec must stay in sync with chunks"
    );

    let harkonnen: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'Harkonnen'",
            [],
            |r| r.get(0),
        )
        .expect("fts query");
    assert_eq!(
        harkonnen, 0,
        "Harkonnen-only fixture's terms must vanish from FTS"
    );
}
