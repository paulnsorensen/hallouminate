use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/agent-bench has a workspace root two levels up")
        .to_path_buf()
}

fn run_validator(manifest_path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bench-validate-manifest"))
        .args(["--manifest", manifest_path.to_str().unwrap()])
        .output()
        .expect("running bench-validate-manifest")
}

/// The pinned container image line the base manifest uses, built from the
/// same repeated-digit digest as `base_manifest` so tests can find-and-
/// replace it without risking a hand-typed hex-length mismatch.
fn container_image_line() -> String {
    format!(
        "container_image_refs = [\"ghcr.io/example/runner@sha256:{}\"]",
        "f".repeat(64)
    )
}

/// Writes a manifest TOML referencing `prompt_path` (relative to the repo
/// root) with hash `prompt_hash`, two size-distinct subject repos with full
/// 40-hex commits, a digest-pinned container image, and `results_dir` under
/// `eval/agent-bench`. Fields can be overridden per-test by mutating the
/// returned string before writing.
fn base_manifest(prompt_path: &str, prompt_hash: &str, question_set_hash: &str) -> String {
    format!(
        r#"claude_code_version = "0.0.0-test"
question_set_hash = "{question_set_hash}"
{image_line}
results_dir = "eval/agent-bench/results"
checkout_root = "eval/agent-bench/checkouts"

[model_ids]
subject = "claude-sonnet-5"
judge = "claude-opus-5"

[[subject_repos]]
name = "repo-small"
url = "https://example.com/repo-small"
commit = "{full_sha_a}"
size_class = "small"

[[subject_repos]]
name = "repo-large"
url = "https://example.com/repo-large"
commit = "{full_sha_b}"
size_class = "large"

[[prompt_hashes]]
path = "{prompt_path}"
blake3 = "{prompt_hash}"
"#,
        image_line = container_image_line(),
        full_sha_a = "a".repeat(40),
        full_sha_b = "b".repeat(40),
    )
}

/// The committed example manifest validates clean through the same code
/// path the binary uses. This is the whole-repo tripwire: if a prompt file
/// (`wiki-authoring.md`, `judge-rubric.md`) is edited without rotating its
/// recorded blake3 in `manifest.example.toml`, this test goes red.
#[test]
fn committed_example_manifest_validates_clean() {
    let manifest_path = workspace_root().join("eval/agent-bench/manifest.example.toml");
    let output = run_validator(&manifest_path);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

#[test]
fn short_sha_reports_rule_and_repo() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(&"a".repeat(40), "deadbeef");

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 2"), "stderr: {stderr}");
    assert!(stderr.contains("repo-small"), "stderr: {stderr}");
}

#[test]
fn latest_tag_image_reports_rule_and_image() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(
        &container_image_line(),
        "container_image_refs = [\"ghcr.io/example/runner:latest\"]",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 3"), "stderr: {stderr}");
    assert!(
        stderr.contains("ghcr.io/example/runner:latest"),
        "stderr: {stderr}"
    );
}

#[test]
fn prompt_hash_drift_reports_rule_and_path() {
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("prompt.md");
    fs::write(&prompt_path, "original prompt content\n").unwrap();
    let recorded_hash = agent_bench::blake3_file_hash(&prompt_path).unwrap();

    // Mutate one byte after recording the hash, simulating an edit that
    // forgot to rotate the pin.
    fs::write(&prompt_path, "originbl prompt content\n").unwrap();

    let question_hash = "c".repeat(64);
    let manifest = base_manifest(
        prompt_path.to_str().unwrap(),
        &recorded_hash,
        &question_hash,
    );
    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 4"), "stderr: {stderr}");
    assert!(
        stderr.contains(prompt_path.to_str().unwrap()),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_prompt_file_reports_rule_and_path() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let manifest = base_manifest(
        "eval/agent-bench/prompts/does-not-exist.md",
        &"d".repeat(64),
        &question_hash,
    );
    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 4"), "stderr: {stderr}");
    assert!(
        stderr.contains("eval/agent-bench/prompts/does-not-exist.md"),
        "stderr: {stderr}"
    );
}

#[test]
fn single_size_class_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace("size_class = \"large\"", "size_class = \"small\"");

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 1"), "stderr: {stderr}");
}

#[test]
fn empty_question_set_hash_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        "",
    );
    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 5"), "stderr: {stderr}");
}

#[test]
fn absolute_results_dir_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(
        "results_dir = \"eval/agent-bench/results\"",
        "results_dir = \"/tmp/results\"",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 6"), "stderr: {stderr}");
}

#[test]
fn escaping_results_dir_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(
        "results_dir = \"eval/agent-bench/results\"",
        "results_dir = \"eval/agent-bench/../../etc\"",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 6"), "stderr: {stderr}");
}

#[test]
fn absolute_checkout_root_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(
        "checkout_root = \"eval/agent-bench/checkouts\"",
        "checkout_root = \"/tmp/checkouts\"",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 7"), "stderr: {stderr}");
}

#[test]
fn escaping_checkout_root_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let question_hash = "c".repeat(64);
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/wiki-authoring.md",
        &agent_bench::blake3_file_hash(
            &workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md"),
        )
        .unwrap(),
        &question_hash,
    );
    manifest = manifest.replace(
        "checkout_root = \"eval/agent-bench/checkouts\"",
        "checkout_root = \"eval/agent-bench/../../etc\"",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 7"), "stderr: {stderr}");
}

#[test]
fn multiple_violations_are_all_reported_in_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = base_manifest(
        "eval/agent-bench/prompts/does-not-exist.md",
        &"d".repeat(64),
        "",
    );
    manifest = manifest.replace(&"a".repeat(40), "deadbeef");
    manifest = manifest.replace(
        &container_image_line(),
        "container_image_refs = [\"ghcr.io/example/runner:latest\"]",
    );

    let manifest_path = dir.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();

    let output = run_validator(&manifest_path);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 2"), "stderr: {stderr}");
    assert!(stderr.contains("rule 3"), "stderr: {stderr}");
    assert!(stderr.contains("rule 4"), "stderr: {stderr}");
    assert!(stderr.contains("rule 5"), "stderr: {stderr}");
}
