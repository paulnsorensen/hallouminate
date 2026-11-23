#![cfg(unix)]

//! Process-level checks for measurement without a model call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use agent_bench::{
    Arm, Manifest, ModelIds, PromptRef, Question, QuestionSet, QuestionTag, SessionRecord,
    SizeClass, SubjectRepo,
};

struct Fixture {
    dir: tempfile::TempDir,
    manifest: Manifest,
    manifest_path: PathBuf,
    questions: PathBuf,
    agent: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let checkout_root = dir.path().join("checkouts");
        let checkout = checkout_root.join("subject");
        let hidden = checkout.join(".hallouminate");
        fs::create_dir_all(hidden.join("wiki")).unwrap();
        fs::write(checkout.join("README.md"), "pinned public source\n").unwrap();
        fs::write(
            hidden.join("config.toml"),
            "[[corpus]]\nname = \"must-not-leak\"\npaths = [\".\"]\n",
        )
        .unwrap();
        fs::write(
            hidden.join("wiki/index.md"),
            "# Frozen page\n\nprivate_knowledge_marker\n",
        )
        .unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&checkout)
                .args(args)
                .output()
                .unwrap();
            assert_ok(&output);
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "subject",
        ]);
        let commit = git(&["rev-parse", "HEAD"]);
        fs::write(checkout.join("README.md"), "unpinned working-tree edit\n").unwrap();
        fs::write(checkout.join("untracked.txt"), "untracked\n").unwrap();
        let questions = dir.path().join("questions.json");
        let rubric = PathBuf::from("eval/agent-bench/prompts/judge-rubric.md");
        fs::write(
            &questions,
            serde_json::to_vec(&QuestionSet {
                questions: vec![Question {
                    repo: "subject".into(),
                    id: "q1".into(),
                    tag: QuestionTag::WikiOnly,
                    question: "Question q1".into(),
                    gold_answer: "answer".into(),
                    rubric_notes: "notes".into(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let agent = dir.path().join("offline-agent");
        fs::write(&agent, include_bytes!("fixtures/offline_agent.py")).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = Manifest {
            model_ids: ModelIds {
                subject: "pinned-subject".into(),
                judge: "pinned-judge".into(),
            },
            claude_code_version: "2.1.0".into(),
            subject_repos: vec![SubjectRepo {
                name: "subject".into(),
                url: "https://example.invalid/subject.git".into(),
                commit,
                size_class: SizeClass::Small,
            }],
            prompt_hashes: vec![PromptRef {
                blake3: agent_bench::blake3_file_hash(&agent_bench::repo_root().join(&rubric))
                    .unwrap(),
                path: rubric,
            }],
            question_set_hash: agent_bench::blake3_file_hash(&questions).unwrap(),
            container_image_refs: Vec::new(),
            results_dir: dir.path().join("custom-results"),
            checkout_root,
        };
        let manifest_path = dir.path().join("manifest.toml");
        let fixture = Self {
            dir,
            manifest,
            manifest_path,
            questions,
            agent,
        };
        fixture.write_manifest();
        fixture
    }

    fn write_manifest(&self) {
        fs::write(
            &self.manifest_path,
            toml::to_string(&self.manifest).unwrap(),
        )
        .unwrap();
    }

    fn sessions(&self) -> PathBuf {
        self.manifest.results_dir.join("sessions.jsonl")
    }

    fn grades(&self) -> PathBuf {
        self.manifest.results_dir.join("grades.jsonl")
    }

    fn command(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env("AGENT_BENCH_CLAUDE_BIN", &self.agent)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .arg("--manifest")
            .arg(&self.manifest_path)
            .arg("--questions")
            .arg(&self.questions);
        command
    }

    fn run(&self) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_bench-run"));
        command
            .args(["--arm", "both", "--runs", "1"])
            .arg("--out-dir")
            .arg(&self.manifest.results_dir);
        command
    }

    fn judge(&self) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_bench-judge"));
        command
            .arg("--sessions")
            .arg(self.sessions())
            .arg("--out")
            .arg(self.grades());
        command
    }

    fn report(&self, label: &str) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_bench-report"));
        command
            .arg("--sessions")
            .arg(self.sessions())
            .arg("--grades")
            .arg(self.grades())
            .arg("--out-dir")
            .arg(self.dir.path().join(label))
            .args(["--seed", "42", "--resamples", "8"]);
        command
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_error(output: &Output, text: &str) {
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && error.contains(text),
        "expected {text:?}: {error}"
    );
}

#[test]
fn drifted_question_hash_aborts_before_writing_results() {
    let mut fixture = Fixture::new();
    fixture.manifest.question_set_hash = "bad-pin".into();
    fixture.write_manifest();
    let output = fixture.run().output().unwrap();
    assert_error(&output, "question-set hash drift");
    assert!(String::from_utf8_lossy(&output.stderr).contains("bad-pin"));
    assert!(!fixture.manifest.results_dir.exists());
}

#[test]
fn agent_cli_version_mismatch_aborts_before_writing_results() {
    let mut fixture = Fixture::new();
    fixture.manifest.claude_code_version = "9.9.9".into();
    fixture.write_manifest();
    let output = fixture.run().output().unwrap();
    assert_error(&output, "agent CLI version drift");
    assert!(String::from_utf8_lossy(&output.stderr).contains("2.1.0"));
    assert!(!fixture.manifest.results_dir.exists());
}

#[test]
#[ignore = "requires Linux Bubblewrap and a standalone Hallouminate binary"]
fn paired_run_uses_pinned_source_and_the_real_private_read_only_wiki() {
    let mut fixture = Fixture::new();
    let output = fixture
        .run()
        .env("HALLOUMINATE_SOCKET", "/run/host-wiki.sock")
        .output()
        .unwrap();
    assert_ok(&output);
    let records: Vec<SessionRecord> = agent_bench::read_jsonl(&fixture.sessions()).unwrap();
    assert_eq!(records.len(), 2);
    for record in &records {
        assert!(record.transcript_path.is_file());
        assert_eq!(record.exit_status, 0, "{}", record.answer_text);
        let expected = match record.arm {
            Arm::Wiki => "pinned public source; private_knowledge_marker",
            Arm::Baseline => "pinned public source; no wiki",
        };
        assert_eq!(record.answer_text, expected);
        assert_eq!(record.usage.total(), 110);
    }
    let ledger = fs::read(fixture.sessions()).unwrap();
    let meta_path = fixture.sessions().with_file_name("run-meta.json");
    let meta = fs::read(&meta_path).unwrap();
    assert_ok(&fixture.run().output().unwrap());
    assert_eq!(fs::read(fixture.sessions()).unwrap(), ledger);

    let wiki = fixture
        .manifest
        .checkout_root
        .join("subject/.hallouminate/wiki/index.md");
    let frozen = fs::read(&wiki).unwrap();
    fs::write(&wiki, "# Re-frozen wiki").unwrap();
    assert_error(
        &fixture.run().arg("--force").output().unwrap(),
        "measurement fingerprint drift",
    );
    assert_eq!(fs::read(&meta_path).unwrap(), meta);
    assert_eq!(fs::read(fixture.sessions()).unwrap(), ledger);
    fs::write(wiki, frozen).unwrap();

    fixture.manifest.model_ids.subject = "another-subject".into();
    fixture.write_manifest();
    assert_error(
        &fixture.run().arg("--force").output().unwrap(),
        "measurement fingerprint drift",
    );
    assert_eq!(fs::read(meta_path).unwrap(), meta);
    assert_eq!(fs::read(fixture.sessions()).unwrap(), ledger);
    fixture.manifest.model_ids.subject = "pinned-subject".into();
    fixture.write_manifest();

    assert_ok(&fixture.judge().output().unwrap());
    assert_ok(&fixture.report("report-before").output().unwrap());
    let grades = fs::read(fixture.grades()).unwrap();
    assert_error(
        &fixture
            .judge()
            .args(["--pass-threshold", "5"])
            .output()
            .unwrap(),
        "stale or invalid grade",
    );
    assert_error(
        &fixture
            .report("report-wrong-threshold")
            .args(["--pass-threshold", "5"])
            .output()
            .unwrap(),
        "stale or invalid grade",
    );
    assert!(!fixture.dir.path().join("report-wrong-threshold").exists());
    assert_eq!(fs::read(fixture.grades()).unwrap(), grades);

    assert_ok(&fixture.run().arg("--force").output().unwrap());
    let rerun: Vec<SessionRecord> = agent_bench::read_jsonl(&fixture.sessions()).unwrap();
    assert_eq!(rerun.len(), records.len());
    for (old, new) in records.iter().zip(&rerun) {
        assert_eq!(old.arm, new.arm);
        assert_eq!(old.answer_text, new.answer_text);
        assert_ne!(old.transcript_path, new.transcript_path);
        assert!(old.transcript_path.is_file());
    }
    assert_error(&fixture.judge().output().unwrap(), "stale or invalid grade");
    assert_error(
        &fixture.report("report-stale").output().unwrap(),
        "stale or invalid grade",
    );
    assert!(!fixture.dir.path().join("report-stale").exists());
    assert_eq!(fs::read(fixture.grades()).unwrap(), grades);
    assert_ok(&fixture.judge().arg("--force").output().unwrap());
    assert_ok(&fixture.report("report-after").output().unwrap());
}

#[test]
#[ignore = "requires Linux Bubblewrap and a standalone Hallouminate binary"]
fn wrong_wiki_endpoint_fails_before_creating_results() {
    let fixture = Fixture::new();
    let output = fixture
        .run()
        .env("AGENT_BENCH_HALLOUMINATE_BIN", "/usr/bin/false")
        .output()
        .unwrap();
    assert_error(&output, "private wiki daemon did not start");
    assert!(!fixture.manifest.results_dir.exists());
}

#[test]
fn checkout_drift_aborts_before_writing_results() {
    let mut fixture = Fixture::new();
    let actual = fixture.manifest.subject_repos[0].commit.clone();
    fixture.manifest.subject_repos[0].commit = "0".repeat(40);
    fixture.write_manifest();
    let output = fixture.run().output().unwrap();
    assert_error(
        &output,
        "refusing to start any measured run against a drifted checkout",
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(&actual) && error.contains(&"0".repeat(40)));
    assert!(!fixture.manifest.results_dir.exists());
}

#[test]
#[ignore = "requires Linux Bubblewrap and a standalone Hallouminate binary"]
fn malformed_and_failed_agent_outputs_preserve_error_records() {
    let mut fixture = Fixture::new();
    let mut set: QuestionSet = agent_bench::load_json(&fixture.questions).unwrap();
    let mut failed = set.questions[0].clone();
    failed.id = "failed".into();
    failed.question = "emit a process failure".into();
    set.questions[0].question = "emit invalid JSON".into();
    set.questions.push(failed);
    fs::write(&fixture.questions, serde_json::to_vec(&set).unwrap()).unwrap();
    fixture.manifest.question_set_hash = agent_bench::blake3_file_hash(&fixture.questions).unwrap();
    fixture.write_manifest();
    assert_ok(&fixture.run().output().unwrap());
    let records: Vec<SessionRecord> = agent_bench::read_jsonl(&fixture.sessions()).unwrap();
    assert_eq!(records.len(), 4);
    for record in records {
        assert_eq!(record.usage.total(), 0);
        match record.question_id.as_str() {
            "failed" => {
                assert_eq!(record.exit_status, 3);
                assert!(record.answer_text.contains("fixture MCP startup failed"));
            }
            "q1" => {
                assert_ne!(record.exit_status, 0);
                assert!(record.answer_text.contains("not valid JSON"));
            }
            id => panic!("unexpected question: {id}"),
        }
    }
}
