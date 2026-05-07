use std::fs;
use std::path::Path;

use hallouminate::app::cli::{cmd_index, IndexArgs};
use rusqlite::Connection;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

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
        root.join("alpha.md"),
        "# Alpha doc\n\nThe spice must flow throughout the corpus.\n",
    )
    .unwrap();
    fs::write(
        root.join("beta.md"),
        "# Beta notes\n\nWitness the indexer pipeline.\n",
    )
    .unwrap();
}

#[test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
fn cmd_index_indexes_fixture_corpus_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let db_path = dir.path().join("index.db");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &db_path, &cache_dir);

    cmd_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .expect("first index run");

    // Re-open with raw rusqlite for assertions — pure read-only inspection of the
    // file produced by cmd_index does not need sqlite-vec extension loading.
    let conn = Connection::open(&db_path).expect("reopen db");
    assert_eq!(count(&conn, "SELECT count(*) FROM files"), 2);
    let chunks = count(&conn, "SELECT count(*) FROM chunks");
    assert!(chunks >= 2, "expected at least 2 chunks, got {chunks}");
    assert_eq!(count(&conn, "SELECT count(*) FROM chunks_vec"), chunks);
    assert_eq!(count(&conn, "SELECT count(*) FROM chunks_fts"), chunks);
}
