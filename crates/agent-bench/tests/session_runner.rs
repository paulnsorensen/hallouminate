#![cfg(unix)]

//! Integration tests for the `bench-run` session runner. All sessions go
//! through a fake `claude` CLI (a small shell script controlled by env
//! vars) selected via `AGENT_BENCH_CLAUDE_BIN` — no network, no real Claude
//! Code.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use agent_bench::SessionRecord;

/// Fake `claude -p --output-format json` CLI.
///
/// Behaviour is controlled by env vars so a single script covers every
/// scenario:
/// - `--version` as the first argument: print `$FAKE_CLI_VERSION (Claude
///   Code)` (default `2.1.0`, matching `write_manifest`) and exit 0 without
///   touching the sentinel — a version probe is not a session spawn.
/// - `FAKE_CLI_SENTINEL=<path>`: touch that path on every session
///   invocation, so a test can prove the binary was (or was not) spawned.
/// - `FAKE_CLI_ARGV_LOG=<path>`: append the session's full argv to that
///   path, one invocation per line.
/// - `FAKE_CLI_MODE=invalid-json`: print non-JSON stdout and exit 0.
/// - `FAKE_CLI_MODE=fail`: exit 3 with no stdout.
/// - otherwise: print `{"result": "$FAKE_CLI_ANSWER", "usage": {...}}` with
///   all four usage fields non-zero and distinct, so a two-field sum would
///   visibly undercount.
const FAKE_CLI: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' "${FAKE_CLI_VERSION:-2.1.0} (Claude Code)"
  exit 0
fi
if [ -n "$FAKE_CLI_SENTINEL" ]; then
  touch "$FAKE_CLI_SENTINEL"
fi
if [ -n "$FAKE_CLI_ARGV_LOG" ]; then
  printf '%s\n' "$*" >> "$FAKE_CLI_ARGV_LOG"
fi
mode="${FAKE_CLI_MODE:-ok}"
answer="${FAKE_CLI_ANSWER:-default-answer}"
if [ "$mode" = "invalid-json" ]; then
  echo "not valid json output"
  exit 0
fi
if [ "$mode" = "fail" ]; then
  exit 3
fi
cat <<JSON
{"result":"$answer","usage":{"input_tokens":11,"output_tokens":22,"cache_read_input_tokens":33,"cache_creation_input_tokens":44}}
JSON
"#;

fn install_fake_cli(dir: &Path) -> PathBuf {
    let path = dir.join("fake-claude.sh");
    fs::write(&path, FAKE_CLI).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Creates a real git checkout at `root/name`, commits one file, and
/// returns its HEAD SHA -- `resolve_checkouts` in bench-run.rs requires the
/// pinned commit to match a real checkout's HEAD.
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

fn write_questions(path: &Path, ids: &[&str]) {
    let questions: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "repo": "hallouminate",
                "id": id,
                "tag": "wiki-only",
                "question": format!("What is {id}?"),
                "gold_answer": "answer",
                "rubric_notes": "notes"
            })
        })
        .collect();
    let content = serde_json::to_string(&json!({ "questions": questions })).unwrap();
    fs::write(path, content).unwrap();
}

fn write_manifest(
    path: &Path,
    question_set_hash: &str,
    results_dir: &Path,
    checkout_root: &Path,
    commit: &str,
) {
    let content = format!(
        r#"
claude_code_version = "2.1.0"
question_set_hash = "{hash}"
container_image_refs = []
prompt_hashes = []
results_dir = "{results_dir}"
checkout_root = "{checkout_root}"

[model_ids]
subject = "claude-sonnet-5"
judge = "claude-opus-5"

[[subject_repos]]
name = "hallouminate"
url = "https://github.com/paulnsorensen/hallouminate"
commit = "{commit}"
size_class = "small"
"#,
        hash = question_set_hash,
        results_dir = results_dir.display(),
        checkout_root = checkout_root.display(),
        commit = commit,
    );
    fs::write(path, content).unwrap();
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

fn read_arm_config(name: &str) -> Value {
    let path = repo_root().join("eval/agent-bench/config").join(name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()))
}

#[test]
fn arm_configs_differ_by_exactly_one_hallouminate_server_entry() {
    let wiki = read_arm_config("wiki-arm.mcp.json");
    let baseline = read_arm_config("baseline-arm.mcp.json");

    let wiki_obj = wiki
        .as_object()
        .expect("wiki-arm.mcp.json is a JSON object");
    let baseline_obj = baseline
        .as_object()
        .expect("baseline-arm.mcp.json is a JSON object");
    assert_eq!(
        wiki_obj.keys().collect::<Vec<_>>(),
        baseline_obj.keys().collect::<Vec<_>>(),
        "top-level keys must match between arm configs"
    );

    let wiki_servers = wiki_obj["mcpServers"]
        .as_object()
        .expect("wiki-arm.mcp.json has an mcpServers object");
    let baseline_servers = baseline_obj["mcpServers"]
        .as_object()
        .expect("baseline-arm.mcp.json has an mcpServers object");

    let removed: Vec<&String> = baseline_servers
        .keys()
        .filter(|k| !wiki_servers.contains_key(*k))
        .collect();
    assert!(
        removed.is_empty(),
        "baseline-arm.mcp.json has server keys missing from wiki-arm.mcp.json: {removed:?}"
    );

    let added: Vec<&String> = wiki_servers
        .keys()
        .filter(|k| !baseline_servers.contains_key(*k))
        .collect();
    assert_eq!(
        added,
        vec!["hallouminate"],
        "wiki-arm.mcp.json must add exactly one server key, 'hallouminate'; got {added:?}"
    );

    for key in baseline_servers.keys() {
        assert_eq!(
            wiki_servers[key], baseline_servers[key],
            "server entry {key:?} differs between arm configs, but only 'hallouminate' may differ"
        );
    }
}

#[test]
fn wiki_arm_config_carries_no_env_based_corpus_scope() {
    let wiki = read_arm_config("wiki-arm.mcp.json");

    let mut strings = Vec::new();
    collect_strings(&wiki, &mut strings);

    assert!(
        !strings.iter().any(|s| s.contains("CORPUS_SCOPE")),
        "wiki-arm.mcp.json must not carry an env-based corpus-scope string \
         (dead: nothing in the codebase reads it) \u{2014} corpus scoping is \
         enforced by bench-run's check_no_source_corpus_leak precondition \
         instead; found: {strings:?}"
    );
    assert!(
        wiki.get("mcpServers")
            .and_then(|servers| servers.get("hallouminate"))
            .and_then(|server| server.get("env"))
            .is_none(),
        "wiki-arm.mcp.json's hallouminate server must carry no env block at all"
    );
}

/// Proves `bench-run`'s `check_no_source_corpus_leak` precondition is real
/// enforcement, not documentation: a checkout whose `.hallouminate/config.toml`
/// declares `corpus_paths` for the subject repo must abort the wiki arm
/// before any session is spawned, since a non-empty `corpus_paths` derives a
/// `repo:{name}:corpus` source corpus (repository.rs:104-125) that would leak
/// semantic source search into what is supposed to be a wiki-only measurement.
#[test]
fn wiki_arm_source_corpus_leak_aborts_before_any_session_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let hallouminate_dir = checkout_root.join("hallouminate").join(".hallouminate");
    fs::create_dir_all(&hallouminate_dir).unwrap();
    fs::write(
        hallouminate_dir.join("config.toml"),
        "[[repository]]\nname = \"hallouminate\"\npath = \".\"\ncorpus_paths = [\"docs\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a subject repo with non-empty corpus_paths must abort the wiki arm"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI ran despite a source-corpus leak in the checkout config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hallouminate"),
        "stderr missing the offending repo name: {stderr}"
    );
    assert!(
        stderr.contains("corpus_paths"),
        "stderr missing the offending field name: {stderr}"
    );
    assert!(
        stderr.contains("docs"),
        "stderr missing the offending corpus_paths value: {stderr}"
    );
}

#[test]
fn sessions_jsonl_has_one_record_per_question_arm_run_with_transcripts_and_full_usage() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1", "q2"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let status = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("both")
        .arg("--runs")
        .arg("2")
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .unwrap();
    assert!(status.success());

    let sessions_path = out_dir.join("sessions.jsonl");
    let records: Vec<SessionRecord> = agent_bench::read_jsonl(&sessions_path).unwrap();
    // 2 questions × 2 arms × 2 runs.
    assert_eq!(records.len(), 8);

    for record in &records {
        assert!(
            record.transcript_path.exists(),
            "transcript missing: {}",
            record.transcript_path.display()
        );
        assert_eq!(record.exit_status, 0);
        assert!(record.usage.input_tokens > 0);
        assert!(record.usage.output_tokens > 0);
        assert!(record.usage.cache_read_input_tokens > 0);
        assert!(record.usage.cache_creation_input_tokens > 0);
        assert_eq!(
            record.usage.total(),
            record.usage.input_tokens
                + record.usage.output_tokens
                + record.usage.cache_read_input_tokens
                + record.usage.cache_creation_input_tokens,
            "total() must decompose across all four usage fields, not just two"
        );
    }
}

#[test]
fn drifted_question_set_hash_aborts_before_any_session_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");
    let stale_hash = "0000deadbeef0000";
    let checkout_root = dir.path().join("checkouts");
    write_manifest(
        &manifest_path,
        stale_hash,
        &out_dir,
        &checkout_root,
        "deadbeef",
    );

    let actual_hash = agent_bench::blake3_file_hash(&questions_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "drifted hash must abort with non-zero exit"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI ran despite question-set hash drift"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(stale_hash),
        "stderr missing manifest's frozen hash: {stderr}"
    );
    assert!(
        stderr.contains(&actual_hash),
        "stderr missing the questions file's actual hash: {stderr}"
    );
}

#[test]
fn rerun_without_force_skips_and_with_force_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let run = |force: bool, answer: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bench-run"));
        cmd.env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
            .env("FAKE_CLI_ANSWER", answer)
            .arg("--manifest")
            .arg(&manifest_path)
            .arg("--questions")
            .arg(&questions_path)
            .arg("--arm")
            .arg("wiki")
            .arg("--runs")
            .arg("1")
            .arg("--out-dir")
            .arg(&out_dir);
        if force {
            cmd.arg("--force");
        }
        assert!(cmd.status().unwrap().success());
    };

    run(false, "first-answer");
    let records: Vec<SessionRecord> =
        agent_bench::read_jsonl(&out_dir.join("sessions.jsonl")).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].answer_text, "first-answer");

    run(false, "second-answer-should-be-ignored");
    let records: Vec<SessionRecord> =
        agent_bench::read_jsonl(&out_dir.join("sessions.jsonl")).unwrap();
    assert_eq!(
        records.len(),
        1,
        "re-run without --force must not duplicate"
    );
    assert_eq!(
        records[0].answer_text, "first-answer",
        "re-run without --force must not overwrite the existing record"
    );

    run(true, "forced-answer");
    let records: Vec<SessionRecord> =
        agent_bench::read_jsonl(&out_dir.join("sessions.jsonl")).unwrap();
    assert_eq!(
        records.len(),
        1,
        "--force must overwrite the prior record in place, not duplicate it"
    );
    assert_eq!(records[0].answer_text, "forced-answer");
}

#[test]
fn invalid_json_response_is_recorded_as_an_error_with_nonzero_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let status = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_MODE", "invalid-json")
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--runs")
        .arg("1")
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the harness itself must not crash on one bad agent response"
    );

    let records: Vec<SessionRecord> =
        agent_bench::read_jsonl(&out_dir.join("sessions.jsonl")).unwrap();
    assert_eq!(records.len(), 1);
    assert_ne!(
        records[0].exit_status, 0,
        "invalid JSON from the agent must be recorded as a session error"
    );
    assert_eq!(records[0].usage.total(), 0);
    assert!(
        !records[0].answer_text.is_empty(),
        "an error record must carry a diagnostic, not a silent empty answer"
    );
}

/// The manifest pins `model_ids.subject` and the README promises every run
/// is reproducible from its manifest. That only holds if the pin actually
/// reaches the agent: without `--model`, the CLI resolves whatever default
/// model is current, so two sweeps a week apart can silently use different
/// models while the manifest certifies they did not.
#[test]
fn pinned_subject_model_reaches_the_agent_argv() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let argv_log = dir.path().join("argv.log");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_ARGV_LOG", &argv_log)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--runs")
        .arg("1")
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(lines.len(), 1, "expected exactly one session spawn");
    // `write_manifest` pins model_ids.subject = "claude-sonnet-5".
    assert!(
        lines[0].contains("--model claude-sonnet-5"),
        "session argv must carry the manifest's pinned subject model: {:?}",
        lines[0]
    );
}

/// `manifest.claude_code_version` is recorded provenance. Recording it
/// without verifying it certifies a fact the harness never checked, so a
/// sweep run under a different agent CLI is indistinguishable from a
/// compliant one.
#[test]
fn agent_cli_version_mismatch_aborts_before_any_session_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    // `write_manifest` pins claude_code_version = "2.1.0".
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .env("FAKE_CLI_VERSION", "9.9.9")
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--runs")
        .arg("1")
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "an agent CLI whose version differs from the manifest's pin must abort"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI ran a session despite agent CLI version drift"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("2.1.0"),
        "stderr missing the manifest's pinned version: {stderr}"
    );
    assert!(
        stderr.contains("9.9.9"),
        "stderr missing the CLI's actual version: {stderr}"
    );
}

/// The resumable ledger keys records on (question_id, arm, run_index) only.
/// After a question set is edited and re-frozen, every one of those triples
/// still resolves, so a resume would skip every session and grade the OLD
/// answers against the NEW gold answers. The re-freeze must refuse to resume.
#[test]
fn refrozen_question_set_refuses_to_resume_a_stale_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let first_hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(
        &manifest_path,
        &first_hash,
        &out_dir,
        &checkout_root,
        &commit,
    );

    let status = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--runs")
        .arg("1")
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .unwrap();
    assert!(status.success());

    // Re-freeze: same question id, edited content, new hash in the manifest.
    let mut set: Value =
        serde_json::from_str(&fs::read_to_string(&questions_path).unwrap()).unwrap();
    set["questions"][0]["gold_answer"] = json!("a materially different gold answer");
    fs::write(&questions_path, serde_json::to_string(&set).unwrap()).unwrap();
    let second_hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    assert_ne!(first_hash, second_hash);
    write_manifest(
        &manifest_path,
        &second_hash,
        &out_dir,
        &checkout_root,
        &commit,
    );

    let sentinel = dir.path().join("sentinel");
    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--runs")
        .arg("1")
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "resuming a ledger produced under a different question set must abort"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI ran despite a stale ledger — the abort must precede any session"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&first_hash),
        "stderr missing the ledger's question-set hash: {stderr}"
    );
    assert!(
        stderr.contains(&second_hash),
        "stderr missing the manifest's question-set hash: {stderr}"
    );
    assert!(
        stderr.contains("--force"),
        "stderr must point at the escape hatch: {stderr}"
    );
}

/// `check_no_source_corpus_leak` used to match only the `[[repository]]`
/// entry whose `name` equalled the BENCHMARK manifest's repo name. A subject
/// repo's own config names itself whatever its author chose, so on the most
/// likely real configuration the check found no entry and returned Ok —
/// failing open on the sole enforcement point for the wiki-only invariant.
#[test]
fn source_corpus_leak_under_a_differently_named_repository_entry_still_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");
    let checkout_root = dir.path().join("checkouts");
    let commit = init_git_checkout(&checkout_root, "hallouminate");
    write_manifest(&manifest_path, &hash, &out_dir, &checkout_root, &commit);

    let hallouminate_dir = checkout_root.join("hallouminate").join(".hallouminate");
    fs::create_dir_all(&hallouminate_dir).unwrap();
    // The checkout's own config names itself "upstream-name", not the
    // benchmark manifest's "hallouminate" label.
    fs::write(
        hallouminate_dir.join("config.toml"),
        "[[repository]]\nname = \"upstream-name\"\npath = \".\"\ncorpus_paths = [\"src\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--arm")
        .arg("wiki")
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "corpus_paths under any [[repository]] entry must abort the wiki arm, \
         whatever that entry calls itself"
    );
    assert!(
        !sentinel.exists(),
        "fake CLI ran despite a source-corpus leak under a differently named entry"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("upstream-name"),
        "stderr missing the offending entry name: {stderr}"
    );
    assert!(
        stderr.contains("src"),
        "stderr missing the offending corpus_paths value: {stderr}"
    );
}

#[test]
fn checkout_drift_aborts_before_any_session_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = install_fake_cli(dir.path());
    let questions_path = dir.path().join("questions.json");
    write_questions(&questions_path, &["q1"]);
    let hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    let out_dir = dir.path().join("out");
    let sentinel = dir.path().join("sentinel");
    let checkout_root = dir.path().join("checkouts");
    let actual_commit = init_git_checkout(&checkout_root, "hallouminate");
    let pinned_commit = "0".repeat(40);
    write_manifest(
        &manifest_path,
        &hash,
        &out_dir,
        &checkout_root,
        &pinned_commit,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bench-run"))
        .env("AGENT_BENCH_CLAUDE_BIN", &fake_cli)
        .env("FAKE_CLI_SENTINEL", &sentinel)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "drifted checkout must abort with non-zero exit"
    );
    assert!(!sentinel.exists(), "fake CLI ran despite checkout drift");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&pinned_commit),
        "stderr missing manifest's pinned commit: {stderr}"
    );
    assert!(
        stderr.contains(&actual_commit),
        "stderr missing the checkout's actual HEAD: {stderr}"
    );
}
