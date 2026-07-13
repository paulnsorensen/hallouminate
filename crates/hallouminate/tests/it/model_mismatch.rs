use std::fs;
use std::path::Path;

use hallouminate::config::Config;
use hallouminate::daemon::DaemonState;
use hallouminate_adapters::lance::LanceStore;

const MODEL_A: &str = "BAAI/bge-small-en-v1.5";
const MODEL_B: &str = "intfloat/multilingual-e5-small";

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
/// Now exercised at `DaemonState::open` rather than `cmd_index`, because the
/// daemon is the canonical owner of the ground directory in the rewired
/// CLI/MCP path. A user who edits config to swap models and starts a fresh
/// `hallouminate daemon` should see the model-mismatch error at boot, with
/// the same shape as the pre-rewire CLI surface.
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
        let _store = LanceStore::open_or_create(&ground_dir, MODEL_A, false, true, None)
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
    let cfg_text = fs::read_to_string(&config_path).expect("read config");
    let cfg: Config = toml::from_str(&cfg_text).expect("parse config");

    // 3. Opening the daemon's shared LanceStore (the canonical owner of the
    //    ground dir under the spec) with the new model must refuse — same
    //    contract as the pre-rewire `cmd_index` path, just enforced one
    //    layer earlier so every CLI/MCP transport benefits from a single
    //    point of failure rather than each duplicating the check.
    let err = DaemonState::open(cfg, None)
        .await
        .expect_err("daemon open with mismatched model must refuse");

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
    let reopened = LanceStore::open_or_create(&ground_dir, MODEL_A, false, true, None)
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
