#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/agent-bench has a workspace root two levels up")
        .to_path_buf()
}

/// Writes an executable `/bin/sh` fake agent CLI at `path`. `body` is the
/// shell script's logic between the shebang and EOF.
fn write_fake_cli(path: &Path, body: &str) {
    let script = format!("#!/bin/sh\n{body}\n");
    fs::write(path, script).expect("writing fake CLI script");
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("setting fake CLI script executable");
}

fn write_manifest(path: &Path, repo_name: &str) {
    let manifest = json!({
        "model_ids": {"subject": "claude-sonnet-5", "judge": "claude-opus-5"},
        "claude_code_version": "0.0.0-test",
        "subject_repos": [{
            "name": repo_name,
            "url": "https://example.com/demo",
            "commit": "deadbeef",
            "size_class": "small"
        }],
        "prompt_hashes": [],
        "question_set_hash": "question-hash",
        "container_image_refs": [],
        "results_dir": "/tmp/results"
    });
    fs::write(path, serde_json::to_string_pretty(&manifest).unwrap()).unwrap();
}

fn run_bench_author(
    manifest: &Path,
    repo: &str,
    budget_tokens: u64,
    out_dir: &Path,
    fake_cli: &Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bench-author"))
        .args([
            "--manifest",
            manifest.to_str().unwrap(),
            "--repo",
            repo,
            "--budget-tokens",
            &budget_tokens.to_string(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ])
        .env("AGENT_BENCH_CLAUDE_BIN", fake_cli)
        .output()
        .expect("running bench-author")
}

fn read_log_lines(out_dir: &Path) -> Vec<Value> {
    let path = out_dir.join("authoring-log.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn authoring_log_records_all_four_fields_and_cumulative_totals() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    write_manifest(&manifest_path, "demo-repo");
    let out_dir = dir.path().join("out");
    let counter_path = dir.path().join("counter");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        &format!(
            r#"
COUNTER="{counter}"
N=$(cat "$COUNTER" 2>/dev/null || echo 0)
N=$((N + 1))
echo $N > "$COUNTER"
if [ "$N" -eq 1 ]; then
  echo '{{"is_error":false,"subtype":"turn","usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":37,"cache_creation_input_tokens":21}}}}'
else
  echo '{{"is_error":false,"subtype":"success","usage":{{"input_tokens":8,"output_tokens":4,"cache_read_input_tokens":19,"cache_creation_input_tokens":11}}}}'
fi
"#,
            counter = counter_path.display()
        ),
    );

    let output = run_bench_author(&manifest_path, "demo-repo", 100_000, &out_dir, &fake_cli);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = read_log_lines(&out_dir);
    assert_eq!(lines.len(), 2);

    // Non-zero cache fields on every line: a two-field (input+output) sum
    // would silently under-count and this assertion would fail to catch it.
    for line in &lines {
        let usage = &line["usage"];
        assert!(usage["cache_read_input_tokens"].as_u64().unwrap() > 0);
        assert!(usage["cache_creation_input_tokens"].as_u64().unwrap() > 0);
    }

    let turn0_total = 10 + 5 + 37 + 21;
    let turn1_total = 8 + 4 + 19 + 11;
    let cumulative0 = &lines[0]["cumulative"];
    let cumulative1 = &lines[1]["cumulative"];
    let sum0 = cumulative0["input_tokens"].as_u64().unwrap()
        + cumulative0["output_tokens"].as_u64().unwrap()
        + cumulative0["cache_read_input_tokens"].as_u64().unwrap()
        + cumulative0["cache_creation_input_tokens"].as_u64().unwrap();
    let sum1 = cumulative1["input_tokens"].as_u64().unwrap()
        + cumulative1["output_tokens"].as_u64().unwrap()
        + cumulative1["cache_read_input_tokens"].as_u64().unwrap()
        + cumulative1["cache_creation_input_tokens"].as_u64().unwrap();
    assert_eq!(sum0, turn0_total);
    assert_eq!(sum1, turn0_total + turn1_total);
}

#[test]
fn budget_exhaustion_stops_before_exceeding_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    write_manifest(&manifest_path, "demo-repo");
    let out_dir = dir.path().join("out");

    let fake_cli = dir.path().join("fake-claude.sh");
    // Never signals completion; each turn costs 60 tokens total.
    write_fake_cli(
        &fake_cli,
        r#"echo '{"is_error":false,"subtype":"turn","usage":{"input_tokens":15,"output_tokens":15,"cache_read_input_tokens":15,"cache_creation_input_tokens":15}}'"#,
    );

    let output = run_bench_author(&manifest_path, "demo-repo", 100, &out_dir, &fake_cli);
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("100"),
        "stderr did not name the budget: {stderr}"
    );
    assert!(
        stderr.contains("120"),
        "stderr did not name the consumed total: {stderr}"
    );

    // Both turns still land in the log; budget-exceeded is a hard stop, not
    // silent truncation of the record.
    let lines = read_log_lines(&out_dir);
    assert_eq!(lines.len(), 2);
}

#[test]
fn malformed_response_fails_loudly_and_never_logs_zero_usage() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    write_manifest(&manifest_path, "demo-repo");
    let out_dir = dir.path().join("out");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(&fake_cli, "echo 'not json at all'");

    let output = run_bench_author(&manifest_path, "demo-repo", 100_000, &out_dir, &fake_cli);
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("turn 0") && stderr.contains("non-JSON"),
        "stderr missing turn/diagnostic context: {stderr}"
    );

    let lines = read_log_lines(&out_dir);
    assert!(
        lines.is_empty(),
        "malformed turn must not append a log line"
    );
}

#[test]
fn prompt_is_loaded_from_disk_and_hash_matches_independent_computation() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    write_manifest(&manifest_path, "demo-repo");
    let out_dir = dir.path().join("out");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        r#"echo '{"is_error":false,"subtype":"success","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":1}}'"#,
    );

    let output = run_bench_author(&manifest_path, "demo-repo", 100_000, &out_dir, &fake_cli);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let summary_path = out_dir.join("authoring-summary.json");
    let summary: Value = serde_json::from_str(&fs::read_to_string(&summary_path).unwrap()).unwrap();

    let prompt_path = workspace_root().join("eval/agent-bench/prompts/wiki-authoring.md");
    let expected_hash = agent_bench::blake3_file_hash(&prompt_path).unwrap();
    assert_eq!(summary["prompt_blake3"].as_str().unwrap(), expected_hash);
}

#[test]
fn authoring_summary_reconciles_with_log() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    write_manifest(&manifest_path, "demo-repo");
    let out_dir = dir.path().join("out");
    let counter_path = dir.path().join("counter");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        &format!(
            r#"
COUNTER="{counter}"
N=$(cat "$COUNTER" 2>/dev/null || echo 0)
N=$((N + 1))
echo $N > "$COUNTER"
if [ "$N" -eq 1 ]; then
  echo '{{"is_error":false,"subtype":"turn","usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":37,"cache_creation_input_tokens":21}}}}'
else
  echo '{{"is_error":false,"subtype":"success","usage":{{"input_tokens":8,"output_tokens":4,"cache_read_input_tokens":19,"cache_creation_input_tokens":11}}}}'
fi
"#,
            counter = counter_path.display()
        ),
    );

    let output = run_bench_author(&manifest_path, "demo-repo", 100_000, &out_dir, &fake_cli);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = read_log_lines(&out_dir);
    assert_eq!(lines.len(), 2);

    let sum_field = |field: &str| -> u64 {
        lines
            .iter()
            .map(|l| l["usage"][field].as_u64().unwrap())
            .sum()
    };

    let summary_path = out_dir.join("authoring-summary.json");
    let summary: Value = serde_json::from_str(&fs::read_to_string(&summary_path).unwrap()).unwrap();

    assert_eq!(
        summary["input_tokens"].as_u64().unwrap(),
        sum_field("input_tokens")
    );
    assert_eq!(
        summary["output_tokens"].as_u64().unwrap(),
        sum_field("output_tokens")
    );
    assert_eq!(
        summary["cache_read_input_tokens"].as_u64().unwrap(),
        sum_field("cache_read_input_tokens")
    );
    assert_eq!(
        summary["cache_creation_input_tokens"].as_u64().unwrap(),
        sum_field("cache_creation_input_tokens")
    );
    let expected_total = sum_field("input_tokens")
        + sum_field("output_tokens")
        + sum_field("cache_read_input_tokens")
        + sum_field("cache_creation_input_tokens");
    assert_eq!(summary["total_tokens"].as_u64().unwrap(), expected_total);
    assert_eq!(summary["turns"].as_u64().unwrap(), 2);
    assert_eq!(summary["budget_tokens"].as_u64().unwrap(), 100_000);
}
