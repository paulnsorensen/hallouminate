use std::fs;
use std::path::Path;

use hallouminate::adapters::lance::LanceStore;
use hallouminate::app::cli::{cmd_index, IndexArgs};

const MODEL: &str = "BAAI/bge-small-en-v1.5";

fn write_config(config_path: &Path, corpus_root: &Path, ground_dir: &Path, cache_dir: &Path) {
    let toml = format!(
        r#"
[[corpus]]
name = "fixtures"
paths = [{root:?}]
globs = ["**/*.md"]

[embeddings]
model     = "BAAI/bge-small-en-v1.5"
cache_dir = {cache:?}

[storage]
ground_dir = {dir:?}
"#,
        root = corpus_root.to_string_lossy().to_string(),
        cache = cache_dir.to_string_lossy().to_string(),
        dir = ground_dir.to_string_lossy().to_string(),
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

#[tokio::test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
async fn cmd_index_indexes_fixture_corpus_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let ground_dir = dir.path().join("ground");
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir);

    cmd_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .await
    .expect("first index run");

    // Re-open the LanceStore and assert chunks landed.
    let store = LanceStore::open_or_create(&ground_dir, MODEL)
        .await
        .expect("reopen ground dir");
    let rows = store.count_rows().await.expect("count rows");
    assert!(
        rows >= 2,
        "expected at least 2 chunks (one per fixture file), got {rows}"
    );
}
