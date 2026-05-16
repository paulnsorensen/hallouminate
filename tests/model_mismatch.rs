use std::fs;
use std::path::Path;

use hallouminate::adapters::lance::LanceStore;
use hallouminate::app::cli::{IndexArgs, cmd_index};

const MODEL_A: &str = "BAAI/bge-small-en-v1.5";
const MODEL_B: &str = "sentence-transformers/all-MiniLM-L6-v2";

fn write_config(
    config_path: &Path,
    corpus_root: &Path,
    ground_dir: &Path,
    cache_dir: &Path,
    model: &str,
) {
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
ground_dir = {dir:?}
"#,
        root = corpus_root.to_string_lossy().to_string(),
        cache = cache_dir.to_string_lossy().to_string(),
        dir = ground_dir.to_string_lossy().to_string(),
        model = model,
    );
    fs::write(config_path, toml).expect("write config");
}

/// Opening a LanceDB ground directory with one embedding model and then
/// reopening it with a different model must refuse, name both models in the
/// error chain, point at a real remediation (`hallouminate index` against an
/// emptied ground directory), and leave the original `meta.toml` byte-identical.
///
/// Ported from the SQLite-era preserved test
/// `.context/preserved/model_mismatch.rs`. The new path keys off the
/// LanceStore sidecar `meta.toml` (see `meta_check_or_init` in
/// `src/adapters/lance.rs`) instead of a `meta` SQL row.
#[tokio::test]
async fn switching_embedding_model_refuses_with_reset_hint_and_no_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    fs::write(
        corpus_root.join("arrakis.md"),
        "# Arrakis\n\nThe spice must flow.\n",
    )
    .unwrap();

    let ground_dir = dir.path().join("ground");

    // 1. Establish meta.toml under model A. We do not need to index any rows
    //    — open_or_create alone writes the sidecar.
    {
        let _store = LanceStore::open_or_create(&ground_dir, MODEL_A)
            .await
            .expect("open store with model A");
    }
    let meta_path = ground_dir.join("meta.toml");
    let meta_before = fs::read_to_string(&meta_path).expect("read meta.toml after first open");
    assert!(
        meta_before.contains(MODEL_A),
        "meta.toml must name original model: {meta_before}"
    );

    // 2. Write a config that points the same ground_dir at MODEL_B.
    let config_path = dir.path().join("config.toml");
    let cache_dir = std::env::temp_dir().join("hallouminate-mismatch-test-cache");
    write_config(&config_path, &corpus_root, &ground_dir, &cache_dir, MODEL_B);

    // 3. cmd_index must refuse before any indexing or model download.
    let err = cmd_index(IndexArgs {
        config: Some(config_path),
        ..Default::default()
    })
    .await
    .expect_err("model switch must refuse");

    let chain = format!("{err:#}");
    assert!(
        chain.contains("delete") && chain.contains("hallouminate index"),
        "error must point at a real remediation (delete + re-run), got: {chain}"
    );
    assert!(
        chain.contains(MODEL_A) && chain.contains(MODEL_B),
        "error must name both models, got: {chain}"
    );

    // 4. No rows written: reopen under MODEL_A (allowed by the meta check)
    //    and confirm count_rows == 0.
    let reopened = LanceStore::open_or_create(&ground_dir, MODEL_A)
        .await
        .expect("reopen store with original model");
    assert_eq!(
        reopened.count_rows().await.expect("count rows"),
        0,
        "refused run must not write any rows"
    );

    // 5. meta.toml is byte-identical to the snapshot taken before the
    //    refused index call — refusal must not rewrite schema version,
    //    auto-managed banner, or any other sidecar content.
    let meta_after = fs::read_to_string(&meta_path).expect("read meta.toml after refusal");
    assert_eq!(
        meta_after, meta_before,
        "meta.toml must be untouched on refusal\nbefore:\n{meta_before}\nafter:\n{meta_after}"
    );
}
