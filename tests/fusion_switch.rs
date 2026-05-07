use std::fs;
use std::path::Path;

use hallouminate::app::cli::{cmd_index, run_ground, GroundArgs, IndexArgs};
use hallouminate::domains::ground::{DocFile, GroundResponse};

fn write_config(
    config_path: &Path,
    corpus_root: &Path,
    db_path: &Path,
    cache_dir: &Path,
    fusion: &str,
) {
    let toml = format!(
        r#"
[[corpus]]
name = "fixtures"
paths = [{root:?}]
globs = ["**/*.md"]

[search]
fusion = "{fusion}"

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

fn top_path(response: &GroundResponse) -> &str {
    let (path, _): (&String, &DocFile) = response
        .docs
        .iter()
        .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap())
        .expect("at least one hit");
    path.as_str()
}

#[test]
#[ignore = "downloads ~33MB embedding model on first run; opt-in via --ignored"]
fn rrf_and_convex_fusion_pick_same_top_file_for_targeted_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_root = dir.path().join("corpus");
    fs::create_dir_all(&corpus_root).unwrap();
    seed_fixtures(&corpus_root);

    let db_path = dir.path().join("index.db");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");

    let rrf_cfg = dir.path().join("config-rrf.toml");
    let convex_cfg = dir.path().join("config-convex.toml");
    write_config(&rrf_cfg, &corpus_root, &db_path, &cache_dir, "rrf");
    write_config(&convex_cfg, &corpus_root, &db_path, &cache_dir, "convex");

    cmd_index(IndexArgs {
        config: Some(rrf_cfg.clone()),
        ..Default::default()
    })
    .expect("index fixture corpus");

    let query = "spice melange Arrakis";

    let rrf_response = run_ground(GroundArgs {
        query: query.into(),
        config: Some(rrf_cfg),
        ..Default::default()
    })
    .expect("ground with rrf");

    let convex_response = run_ground(GroundArgs {
        query: query.into(),
        config: Some(convex_cfg),
        ..Default::default()
    })
    .expect("ground with convex");

    let rrf_top = top_path(&rrf_response);
    let convex_top = top_path(&convex_response);

    assert!(
        rrf_top.ends_with("arrakis.md"),
        "rrf top hit should be arrakis.md, got {rrf_top}"
    );
    assert_eq!(
        rrf_top, convex_top,
        "rrf and convex must agree on top-1 file for an unambiguous query"
    );
}
