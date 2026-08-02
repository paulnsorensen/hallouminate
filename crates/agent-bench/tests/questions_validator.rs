use std::fs;
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

fn run_validator(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bench-validate-questions"))
        .args(args)
        .output()
        .expect("running bench-validate-questions")
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// Writes a minimal TOML manifest with one `[[subject_repos]]` entry per
/// name in `repos`, and `question_set_hash` set to `hash`.
fn write_manifest(path: &Path, repos: &[&str], hash: &str) {
    let mut toml = format!(
        "claude_code_version = \"0.0.0-test\"\nprompt_hashes = []\nquestion_set_hash = \"{hash}\"\ncontainer_image_refs = []\nresults_dir = \"/tmp/results\"\ncheckout_root = \"/tmp/checkouts\"\n\n[model_ids]\nsubject = \"claude-sonnet-5\"\njudge = \"claude-opus-5\"\n\n"
    );
    for repo in repos {
        toml.push_str(&format!(
            "[[subject_repos]]\nname = \"{repo}\"\nurl = \"https://example.com/{repo}\"\ncommit = \"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\"\nsize_class = \"small\"\n\n"
        ));
    }
    fs::write(path, toml).unwrap();
}

fn question(repo: &str, seq: usize, tag: &str, gold_answer: &str) -> Value {
    json!({
        "repo": repo,
        "id": format!("{repo}-{seq:03}"),
        "tag": tag,
        "question": format!("Question {seq} about {repo}?"),
        "gold_answer": gold_answer,
        "rubric_notes": "Full credit requires the exact fact stated in gold_answer."
    })
}

/// Builds `wiki_only + greppable + abstention` questions for `repo`, ids
/// sequential from 1, following the `<repo>-NNN` id-prefix convention.
fn repo_block(repo: &str, wiki_only: usize, greppable: usize, abstention: usize) -> Vec<Value> {
    let mut seq = 1;
    let mut out = Vec::new();
    for _ in 0..wiki_only {
        out.push(question(
            repo,
            seq,
            "wiki-only",
            "A fact recorded only in the wiki, not reliably greppable.",
        ));
        seq += 1;
    }
    for _ in 0..greppable {
        out.push(question(
            repo,
            seq,
            "greppable",
            "A fact found by grepping or reading the repo directly.",
        ));
        seq += 1;
    }
    for _ in 0..abstention {
        out.push(question(
            repo,
            seq,
            "abstention",
            "Not recorded anywhere in the wiki or the repository.",
        ));
        seq += 1;
    }
    out
}

/// A `QuestionSet` satisfying every rule: two repos, 12 questions each
/// (5 wiki-only, 5 greppable, 2 abstention).
fn valid_question_set() -> Value {
    let mut questions = repo_block("repo-a", 5, 5, 2);
    questions.extend(repo_block("repo-b", 5, 5, 2));
    json!({ "questions": questions })
}

#[test]
fn committed_example_passes_all_rules() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(
        &manifest_path,
        &["example-small-repo", "example-large-repo"],
        "irrelevant-hash-not-checked-in-this-test",
    );
    let questions_path = workspace_root().join("eval/agent-bench/questions.example.json");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

#[test]
fn duplicate_id_reports_rule_and_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = valid_question_set();
    let dup_id = set["questions"][0]["id"].as_str().unwrap().to_string();
    set["questions"][1]["id"] = Value::String(dup_id.clone());

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 1"), "stderr: {stderr}");
    assert!(stderr.contains(&dup_id), "stderr: {stderr}");
}

#[test]
fn unknown_repo_reports_rule_and_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = valid_question_set();
    let extra = repo_block("repo-c", 5, 5, 2);
    set["questions"].as_array_mut().unwrap().extend(extra);

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 3"), "stderr: {stderr}");
    assert!(stderr.contains("repo-c-001"), "stderr: {stderr}");
}

#[test]
fn below_floor_total_count_reports_rule() {
    let dir = tempfile::tempdir().unwrap();
    let set = json!({ "questions": repo_block("repo-a", 5, 5, 2) });

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 4"), "stderr: {stderr}");
    assert!(stderr.contains("12"), "stderr: {stderr}");
    assert!(stderr.contains("24"), "stderr: {stderr}");
}

#[test]
fn repo_missing_abstention_reports_rule_and_repo() {
    let dir = tempfile::tempdir().unwrap();
    let mut questions = repo_block("repo-a", 5, 7, 0);
    questions.extend(repo_block("repo-b", 5, 5, 2));
    let set = json!({ "questions": questions });

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 5"), "stderr: {stderr}");
    assert!(stderr.contains("repo-a"), "stderr: {stderr}");
    assert!(stderr.contains("abstention"), "stderr: {stderr}");
}

#[test]
fn repo_below_wiki_only_floor_reports_rule_and_repo() {
    let dir = tempfile::tempdir().unwrap();
    let mut questions = repo_block("repo-a", 1, 9, 2);
    questions.extend(repo_block("repo-b", 5, 5, 2));
    let set = json!({ "questions": questions });

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 5"), "stderr: {stderr}");
    assert!(stderr.contains("repo-a"), "stderr: {stderr}");
    assert!(stderr.contains("wiki-only"), "stderr: {stderr}");
}

#[test]
fn empty_gold_answer_reports_rule_and_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = valid_question_set();
    let offending_id = set["questions"][0]["id"].as_str().unwrap().to_string();
    set["questions"][0]["gold_answer"] = Value::String(String::new());

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rule 7"), "stderr: {stderr}");
    assert!(stderr.contains(&offending_id), "stderr: {stderr}");
}

#[test]
fn multiple_violations_are_all_reported_in_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = valid_question_set();
    let questions = set["questions"].as_array_mut().unwrap();

    // Duplicate id.
    let dup_id = questions[0]["id"].as_str().unwrap().to_string();
    questions[1]["id"] = Value::String(dup_id.clone());

    // Empty gold_answer on a distinct question.
    let empty_gold_id = questions[2]["id"].as_str().unwrap().to_string();
    questions[2]["gold_answer"] = Value::String(String::new());

    // Unknown repo.
    let extra = repo_block("repo-c", 5, 5, 2);
    questions.extend(extra);

    let questions_path = dir.path().join("questions.json");
    write_json(&questions_path, &set);
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(&manifest_path, &["repo-a", "repo-b"], "hash");

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rule 1") && stderr.contains(&dup_id),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("rule 7") && stderr.contains(&empty_gold_id),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("rule 3") && stderr.contains("repo-c-001"),
        "stderr: {stderr}"
    );
}

#[test]
fn check_freeze_fails_on_byte_diff_and_shows_both_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let questions_path = dir.path().join("questions.json");
    let example_path = workspace_root().join("eval/agent-bench/questions.example.json");
    fs::copy(&example_path, &questions_path).unwrap();

    let correct_hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    let manifest_path = dir.path().join("manifest.toml");
    write_manifest(
        &manifest_path,
        &["example-small-repo", "example-large-repo"],
        &correct_hash,
    );

    let unmodified = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--check-freeze",
    ]);
    assert!(
        unmodified.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&unmodified.stderr)
    );

    // Flip one byte inside a string value, preserving valid JSON and length.
    let text = fs::read_to_string(&questions_path).unwrap();
    let modified = text.replacen("MIT License", "MIT Licenze", 1);
    assert_ne!(
        text, modified,
        "fixture no longer contains the target substring"
    );
    fs::write(&questions_path, &modified).unwrap();
    let modified_hash = agent_bench::blake3_file_hash(&questions_path).unwrap();
    assert_ne!(correct_hash, modified_hash);

    let output = run_validator(&[
        "--questions",
        questions_path.to_str().unwrap(),
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--check-freeze",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&correct_hash), "stderr: {stderr}");
    assert!(stderr.contains(&modified_hash), "stderr: {stderr}");
}

#[test]
fn print_hash_matches_check_freeze_comparison_hash() {
    let example_path = workspace_root().join("eval/agent-bench/questions.example.json");
    let output = run_validator(&[
        "--questions",
        example_path.to_str().unwrap(),
        "--print-hash",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_hash = agent_bench::blake3_file_hash(&example_path).unwrap();
    assert_eq!(stdout, format!("{expected_hash}\n"));
}
