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
/// - `FAKE_CLI_SENTINEL=<path>`: touch that path on every invocation, so a
///   test can prove the binary was (or was not) spawned.
/// - `FAKE_CLI_MODE=invalid-json`: print non-JSON stdout and exit 0.
/// - `FAKE_CLI_MODE=fail`: exit 3 with no stdout.
/// - otherwise: print `{"result": "$FAKE_CLI_ANSWER", "usage": {...}}` with
///   all four usage fields non-zero and distinct, so a two-field sum would
///   visibly undercount.
const FAKE_CLI: &str = r#"#!/bin/sh
if [ -n "$FAKE_CLI_SENTINEL" ]; then
  touch "$FAKE_CLI_SENTINEL"
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

fn write_manifest(path: &Path, question_set_hash: &str, results_dir: &Path) {
    let content = format!(
        r#"
claude_code_version = "2.1.0"
question_set_hash = "{hash}"
container_image_refs = []
prompt_hashes = []
results_dir = "{results_dir}"

[model_ids]
subject = "claude-sonnet-5"
judge = "claude-opus-5"

[[subject_repos]]
name = "hallouminate"
url = "https://github.com/paulnsorensen/hallouminate"
commit = "deadbeef"
size_class = "small"
"#,
        hash = question_set_hash,
        results_dir = results_dir.display(),
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
fn wiki_arm_references_wiki_corpus_only_never_source_corpus() {
    let wiki = read_arm_config("wiki-arm.mcp.json");

    let mut strings = Vec::new();
    collect_strings(&wiki, &mut strings);

    assert!(
        strings.iter().any(|s| s.ends_with(":wiki")),
        "wiki-arm.mcp.json must reference a repo:<name>:wiki corpus; found strings: {strings:?}"
    );
    let source_corpus_refs: Vec<&String> =
        strings.iter().filter(|s| s.ends_with(":corpus")).collect();
    assert!(
        source_corpus_refs.is_empty(),
        "wiki-arm.mcp.json must never reference a source corpus (repo:<name>:corpus) \
         — the measured effect is the wiki, not semantic source search; found: {source_corpus_refs:?}"
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
    write_manifest(&manifest_path, &hash, &out_dir);

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
    write_manifest(&manifest_path, stale_hash, &out_dir);

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
    write_manifest(&manifest_path, &hash, &out_dir);

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
    write_manifest(&manifest_path, &hash, &out_dir);

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
