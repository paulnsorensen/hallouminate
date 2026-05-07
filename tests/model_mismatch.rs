use std::fs;
use std::path::Path;

use hallouminate::adapters::sqlite::pool::open_db;
use hallouminate::adapters::sqlite::schema::apply_schema;
use hallouminate::app::cli::{run_index, IndexArgs};

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

fn write_config(config_path: &Path, corpus_root: &Path, db_path: &Path, model: &str) {
    let cache_dir = std::env::temp_dir().join("hallouminate-mismatch-test-cache");
    let toml = format!(
        r#"
[[corpus]]
name = "fixtures"
paths = [{root:?}]
globs = ["**/*.md"]

[embeddings]
model     = {model:?}
cache_dir = {cache:?}

[storage]
db_path = {db:?}
"#,
        root = corpus_root.to_string_lossy().to_string(),
        cache = cache_dir.to_string_lossy().to_string(),
        db = db_path.to_string_lossy().to_string(),
        model = model,
    );
    fs::write(config_path, toml).expect("write config");
}

#[test]
fn switching_embedding_model_refuses_with_reset_hint_and_no_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    fs::write(
        corpus_root.join("arrakis.md"),
        "# Arrakis\n\nThe spice must flow.\n",
    )
    .unwrap();

    let db_path = dir.path().join("index.db");
    {
        let conn = open_db(&db_path).expect("open db");
        apply_schema(&conn).expect("apply schema");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params!["embeddings.model", "bge-small-en-v1.5"],
        )
        .expect("seed meta row");
    }

    let config_path = dir.path().join("config.toml");
    write_config(&config_path, &corpus_root, &db_path, "all-minilm-l6-v2");

    let err = run_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .expect_err("model switch must refuse");

    let chain = format!("{err:#}");
    assert!(
        chain.contains("--reset"),
        "error must point at --reset, got: {chain}"
    );
    assert!(
        chain.contains("bge-small-en-v1.5") && chain.contains("all-minilm-l6-v2"),
        "error must name both models, got: {chain}"
    );

    let conn = open_db(&db_path).expect("reopen db");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM files"),
        0,
        "refused run must not write any files rows"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM chunks"),
        0,
        "refused run must not write any chunks rows"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM chunks_fts"),
        0,
        "refused run must not write any FTS rows"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM chunks_vec"),
        0,
        "refused run must not write any vec rows"
    );

    let stored: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params!["embeddings.model"],
            |r| r.get(0),
        )
        .expect("meta row remains");
    assert_eq!(
        stored, "bge-small-en-v1.5",
        "stored model must remain unchanged after refused run"
    );
}
