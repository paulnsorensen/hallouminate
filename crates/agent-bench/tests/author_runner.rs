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

/// Creates a real git checkout at `root/name`, commits one file, and
/// returns its HEAD SHA -- `bench-author` requires the pinned commit to
/// match a real checkout's HEAD.
fn init_git_checkout(root: &Path, name: &str) -> String {
    let checkout = root.join(name);
    fs::create_dir_all(&checkout).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    fs::write(checkout.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    let out = Command::new("git")
        .args(["-C", checkout.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Writes an executable `/bin/sh` fake agent CLI at `path`. `body` is the
/// shell script's logic between the shebang and EOF.
///
/// Every fake answers `--version` first with `$FAKE_CLI_VERSION` (default
/// `0.0.0-test`, matching `write_manifest`), since `bench-author` probes the
/// agent CLI's version against the manifest's pin before authoring.
fn write_fake_cli(path: &Path, body: &str) {
    let preamble = r#"if [ "$1" = "--version" ]; then
  printf '%s\n' "${FAKE_CLI_VERSION:-0.0.0-test} (Claude Code)"
  exit 0
fi"#;
    let script = format!("#!/bin/sh\n{preamble}\n{body}\n");
    fs::write(path, script).expect("writing fake CLI script");
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("setting fake CLI script executable");
}

fn write_manifest(path: &Path, repo_name: &str, checkout_root: &Path, commit: &str) {
    let manifest = json!({
        "model_ids": {"subject": "claude-sonnet-5", "judge": "claude-opus-5"},
        "claude_code_version": "0.0.0-test",
        "subject_repos": [{
            "name": repo_name,
            "url": "https://example.com/demo",
            "commit": commit,
            "size_class": "small"
        }],
        "prompt_hashes": [],
        "question_set_hash": "question-hash",
        "container_image_refs": [],
        "results_dir": "/tmp/results",
        "checkout_root": checkout_root
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

/// Same as `run_bench_author`, but bounded: the child runs on a worker
/// thread and the test fails rather than hanging the suite if it does not
/// terminate within `timeout`.
fn run_bench_author_bounded(
    manifest: &Path,
    repo: &str,
    budget_tokens: u64,
    out_dir: &Path,
    fake_cli: &Path,
    timeout: std::time::Duration,
) -> std::process::Output {
    let (tx, rx) = std::sync::mpsc::channel();
    let (manifest, repo, out_dir, fake_cli) = (
        manifest.to_path_buf(),
        repo.to_string(),
        out_dir.to_path_buf(),
        fake_cli.to_path_buf(),
    );
    std::thread::spawn(move || {
        let output = run_bench_author(&manifest, &repo, budget_tokens, &out_dir, &fake_cli);
        let _ = tx.send(output);
    });
    rx.recv_timeout(timeout)
        .expect("bench-author did not terminate within the bound — it is looping forever")
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
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
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
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
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
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
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
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
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
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
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

/// Authoring is the run whose output the whole wiki arm is measured against.
/// If the manifest's pinned subject model never reaches the CLI, the wiki was
/// authored by whatever model happened to be the current default.
#[test]
fn pinned_subject_model_and_isolation_flags_reach_the_agent_argv() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
    let out_dir = dir.path().join("out");
    let argv_log = dir.path().join("argv.log");
    let counter_path = dir.path().join("counter");

    // Two turns, so the resumed (`--continue`) turn is covered too.
    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        &format!(
            r#"
{{ printf '%s' "$*" | tr '\n' ' '; printf '\n'; }} >> "{argv_log}"
COUNTER="{counter}"
N=$(cat "$COUNTER" 2>/dev/null || echo 0)
N=$((N + 1))
printf '%s\n' "$N" > "$COUNTER"
if [ "$N" -eq 1 ]; then
  printf '%s\n' '{{"is_error":false,"subtype":"turn","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":1}}}}'
else
  printf '%s\n' '{{"is_error":false,"subtype":"success","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":1}}}}'
fi
"#,
            argv_log = argv_log.display(),
            counter = counter_path.display()
        ),
    );

    let output = run_bench_author(&manifest_path, "demo-repo", 100_000, &out_dir, &fake_cli);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(lines.len(), 2, "expected two authoring turns: {logged}");
    // `write_manifest` pins model_ids.subject = "claude-sonnet-5".
    for line in &lines {
        assert!(
            line.contains("--model claude-sonnet-5"),
            "every authoring turn must carry the manifest's pinned subject model: {line:?}"
        );
        // Authoring passes no --mcp-config at all, so --strict-mcp-config
        // means "native tools only"; without it an operator's ambient MCP
        // config authors the wiki with tools the protocol never granted.
        assert!(
            line.contains("--strict-mcp-config"),
            "every authoring turn must spawn with --strict-mcp-config: {line:?}"
        );
        // The fake logs `$*`, so the empty `--setting-sources` argument shows
        // up as the trailing space after the flag.
        assert!(
            line.ends_with("--setting-sources "),
            "every authoring turn must spawn with an empty --setting-sources: {line:?}"
        );
    }
}

/// Same defect class as the unapplied model pin: `claude_code_version` is
/// recorded provenance nothing verified, so a wiki authored under a
/// different agent CLI is indistinguishable from a compliant one.
#[test]
fn agent_cli_version_mismatch_aborts_before_authoring() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    // `write_manifest` pins claude_code_version = "0.0.0-test".
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        &format!(
            r#"
touch "{sentinel}"
printf '%s\n' '{{"is_error":false,"subtype":"success","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":1}}}}'
"#,
            sentinel = sentinel.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bench-author"))
        .args([
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--repo",
            "demo-repo",
            "--budget-tokens",
            "100000",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ])
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_VERSION", "9.9.9")
        .output()
        .expect("running bench-author");

    assert!(
        !output.status.success(),
        "an agent CLI whose version differs from the manifest's pin must abort"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI authored a turn despite agent CLI version drift"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0.0.0-test"),
        "stderr missing the manifest's pinned version: {stderr}"
    );
    assert!(
        stderr.contains("9.9.9"),
        "stderr missing the CLI's actual version: {stderr}"
    );
    assert!(read_log_lines(&out_dir).is_empty());
}

/// A CLI returning well-formed JSON with `is_error: true` and all-zero usage
/// (auth failure, MCP startup failure) satisfies neither loop exit: it never
/// completes and never spends budget. Before the fix this ran forever —
/// measured at ~6000 turns in 15 seconds with an unbounded log.
#[test]
fn zero_usage_turn_is_an_error_rather_than_an_endless_respawn() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
    let out_dir = dir.path().join("out");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        r#"printf '%s\n' '{"is_error":true,"subtype":"error_during_execution","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'"#,
    );

    let output = run_bench_author_bounded(
        &manifest_path,
        "demo-repo",
        100_000,
        &out_dir,
        &fake_cli,
        std::time::Duration::from_secs(60),
    );
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("turn 0") && stderr.contains("zero"),
        "stderr must name the turn and its zero usage: {stderr}"
    );
}

/// A non-completing turn that DOES spend tokens still loops until the budget
/// runs out — with a generous budget that is effectively forever. The turn
/// cap bounds it independently of the budget.
#[test]
fn non_completing_turns_stop_at_the_turn_cap() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "demo-repo");
    write_manifest(&manifest_path, "demo-repo", &checkout_root, &commit);
    let out_dir = dir.path().join("out");

    let fake_cli = dir.path().join("fake-claude.sh");
    write_fake_cli(
        &fake_cli,
        r#"printf '%s\n' '{"is_error":false,"subtype":"turn","usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'"#,
    );

    let output = run_bench_author_bounded(
        &manifest_path,
        "demo-repo",
        u64::from(u32::MAX),
        &out_dir,
        &fake_cli,
        std::time::Duration::from_secs(120),
    );
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("200"),
        "stderr must name the turn cap it hit: {stderr}"
    );

    let lines = read_log_lines(&out_dir);
    assert_eq!(
        lines.len(),
        200,
        "the cap must stop the loop at exactly MAX_TURNS turns"
    );
}
