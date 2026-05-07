use std::fs;
use std::path::Path;

use hallouminate::app::cli::{run_index, IndexArgs};

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
        "# Giedi Prime\n\n## House Harkonnen\n\nA brutal industrial homeworld with no spice.\n",
    )
    .unwrap();
}

#[test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
fn second_index_run_inserts_zero_embeddings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let db_path = dir.path().join("index.db");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &db_path, &cache_dir);

    let first = run_index(IndexArgs {
        config: Some(config_path.clone()),
        ..Default::default()
    })
    .expect("first index run");

    let first_corpus = first
        .corpora
        .first()
        .expect("first run produced one corpus report");
    assert_eq!(first_corpus.files_upserted, 3, "first run upserts all 3");
    assert!(
        first_corpus.embeddings_inserted >= 3,
        "first run inserts at least one embedding per file, got {}",
        first_corpus.embeddings_inserted,
    );

    let second = run_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .expect("second index run");

    let second_corpus = second
        .corpora
        .first()
        .expect("second run produced one corpus report");
    assert_eq!(
        second_corpus.embeddings_inserted, 0,
        "idempotent re-run must insert zero embeddings"
    );
    assert_eq!(
        second_corpus.files_upserted, 0,
        "idempotent re-run must upsert zero files"
    );
    assert_eq!(
        second_corpus.chunks_inserted, 0,
        "idempotent re-run must insert zero chunks"
    );
    assert_eq!(
        second_corpus.files_deleted, 0,
        "idempotent re-run must delete zero files"
    );
}
