//! Every ONNX session must stay on the CPU. ort's default device policy
//! places the graph on CoreML on macOS, and the daemon footprint then grows
//! to ~21 GB. ONNX Runtime logs the node placement at verbose level.

use std::fs;
use std::process::Command;

const CPU_PLACEMENT: &str = "All nodes placed on [CPUExecutionProvider]";

#[test]
#[ignore = "downloads the embedding and reranker models on first run; opt-in via --ignored"]
fn config_download_places_every_session_on_cpu() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_dir = std::env::temp_dir().join("hallouminate-cli-test-cache");
    let config_path = dir.path().join("config.toml");
    let toml = format!(
        r#"
[embeddings]
enabled = true
model = "snowflake/snowflake-arctic-embed-s"
cache_dir = {cache:?}

[search]
crossencoder = "jina-reranker-v1-turbo-en"
"#,
        cache = cache_dir.to_string_lossy().to_string(),
    );
    fs::write(&config_path, toml).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_hallouminate"))
        .args(["config", "download", "--config"])
        .arg(&config_path)
        .current_dir(dir.path())
        .env("ORT_LOG", "verbose")
        .output()
        .expect("spawn config download");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "config download failed: {stderr}");

    let coreml_lines = stderr
        .lines()
        .filter(|line| line.contains("CoreMLExecutionProvider"))
        .count();
    assert_eq!(coreml_lines, 0, "a session placed nodes on CoreML");
    assert_eq!(
        stderr.matches(CPU_PLACEMENT).count(),
        2,
        "the embedder and the reranker must each log `{CPU_PLACEMENT}`"
    );
}
