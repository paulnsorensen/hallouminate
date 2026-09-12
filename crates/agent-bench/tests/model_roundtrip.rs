use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_bench::{
    Arm, ArmSummary, BenchReport, GradeRecord, Manifest, ModelIds, PairedCi, PromptRef, Question,
    QuestionArmStat, QuestionRow, QuestionSet, QuestionTag, SessionRecord, SizeClass, SubjectRepo,
    TokenUsage,
};

fn sample_token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_input_tokens: 30,
        cache_creation_input_tokens: 20,
    }
}

#[test]
fn arm_round_trips_and_matches_literal_json() {
    let json = r#""wiki""#;
    let arm: Arm = serde_json::from_str(json).unwrap();
    assert_eq!(arm, Arm::Wiki);
    assert_eq!(serde_json::to_string(&arm).unwrap(), json);

    let json = r#""baseline""#;
    let arm: Arm = serde_json::from_str(json).unwrap();
    assert_eq!(arm, Arm::Baseline);
    assert_eq!(serde_json::to_string(&arm).unwrap(), json);
}

#[test]
fn question_tag_round_trips_and_matches_literal_json() {
    for (json, expected) in [
        (r#""wiki-only""#, QuestionTag::WikiOnly),
        (r#""greppable""#, QuestionTag::Greppable),
        (r#""abstention""#, QuestionTag::Abstention),
    ] {
        let tag: QuestionTag = serde_json::from_str(json).unwrap();
        assert_eq!(tag, expected);
        assert_eq!(serde_json::to_string(&tag).unwrap(), json);
    }
}

#[test]
fn question_tag_unknown_variant_error_names_the_value() {
    let err = serde_json::from_str::<QuestionTag>(r#""wiki_only""#).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("wiki_only"),
        "error message did not contain offending value: {message}"
    );
}

#[test]
fn size_class_round_trips_and_matches_literal_json() {
    let json = r#""small""#;
    let class: SizeClass = serde_json::from_str(json).unwrap();
    assert_eq!(class, SizeClass::Small);
    assert_eq!(serde_json::to_string(&class).unwrap(), json);

    let json = r#""large""#;
    let class: SizeClass = serde_json::from_str(json).unwrap();
    assert_eq!(class, SizeClass::Large);
    assert_eq!(serde_json::to_string(&class).unwrap(), json);
}

#[test]
fn token_usage_total_sums_all_four_fields() {
    let usage = sample_token_usage();
    assert_eq!(usage.total(), 200);
    assert_ne!(
        usage.total(),
        usage.input_tokens + usage.output_tokens,
        "total() must include cache fields, not just input+output"
    );
}

#[test]
fn token_usage_add_is_field_wise() {
    let a = sample_token_usage();
    let b = sample_token_usage();
    let sum = a + b;
    assert_eq!(sum.input_tokens, 200);
    assert_eq!(sum.output_tokens, 100);
    assert_eq!(sum.cache_read_input_tokens, 60);
    assert_eq!(sum.cache_creation_input_tokens, 40);

    let mut acc = TokenUsage::default();
    acc += a;
    acc += b;
    assert_eq!(acc, sum);

    let summed: TokenUsage = vec![a, b].into_iter().sum();
    assert_eq!(summed, sum);
}

#[test]
fn question_round_trips_against_literal_json() {
    let json = serde_json::json!({
        "repo": "hallouminate",
        "id": "q1",
        "tag": "wiki-only",
        "question": "What is the daemon socket path?",
        "gold_answer": "XDG_RUNTIME_DIR/hallouminate.sock",
        "rubric_notes": "Must mention XDG_RUNTIME_DIR fallback."
    });
    let question: Question = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(question.repo, "hallouminate");
    assert_eq!(question.id, "q1");
    assert_eq!(question.tag, QuestionTag::WikiOnly);
    assert_eq!(serde_json::to_value(&question).unwrap(), json);

    let set = QuestionSet {
        questions: vec![question],
    };
    let round_tripped: QuestionSet =
        serde_json::from_value(serde_json::to_value(&set).unwrap()).unwrap();
    assert_eq!(round_tripped, set);
}

#[test]
fn session_record_round_trips_against_literal_json() {
    let json = serde_json::json!({
        "question_id": "q1",
        "repo": "hallouminate",
        "arm": "wiki",
        "run_index": 0,
        "answer_text": "It lives under XDG_RUNTIME_DIR.",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 20
        },
        "transcript_path": "/tmp/transcript.jsonl",
        "wall_ms": 4200,
        "exit_status": 0
    });
    let record: SessionRecord = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(record.arm, Arm::Wiki);
    assert_eq!(
        record.transcript_path,
        PathBuf::from("/tmp/transcript.jsonl")
    );
    assert_eq!(serde_json::to_value(&record).unwrap(), json);
}

#[test]
fn grade_record_rejects_out_of_range_score() {
    let err = GradeRecord::grade(
        "q1".to_string(),
        Arm::Wiki,
        0,
        6,
        4,
        "rationale".to_string(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains('6'),
        "error did not name the offending score: {err}"
    );
}

#[test]
fn grade_record_pass_is_derived_from_threshold() {
    let at_threshold_4 = GradeRecord::grade(
        "q1".to_string(),
        Arm::Wiki,
        0,
        4,
        4,
        "rationale".to_string(),
    )
    .unwrap();
    let at_threshold_5 = GradeRecord::grade(
        "q1".to_string(),
        Arm::Wiki,
        0,
        4,
        5,
        "rationale".to_string(),
    )
    .unwrap();
    assert!(at_threshold_4.pass);
    assert!(!at_threshold_5.pass);
}

#[test]
fn grade_record_round_trips_against_literal_json() {
    let json = serde_json::json!({
        "question_id": "q1",
        "arm": "baseline",
        "run_index": 2,
        "score": 5,
        "pass": true,
        "judge_rationale": "Matches gold answer exactly."
    });
    let record: GradeRecord = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(record.arm, Arm::Baseline);
    assert_eq!(record.score, 5);
    assert!(record.pass);
    assert_eq!(serde_json::to_value(&record).unwrap(), json);
}

#[test]
fn manifest_round_trips_against_literal_json_and_toml() {
    let manifest = Manifest {
        model_ids: ModelIds {
            subject: "claude-sonnet-5".to_string(),
            judge: "claude-opus-5".to_string(),
        },
        claude_code_version: "2.1.0".to_string(),
        subject_repos: vec![SubjectRepo {
            name: "hallouminate".to_string(),
            url: "https://github.com/paulnsorensen/hallouminate".to_string(),
            commit: "deadbeef".to_string(),
            size_class: SizeClass::Small,
        }],
        prompt_hashes: vec![PromptRef {
            path: PathBuf::from("prompts/wiki.md"),
            blake3: "abc123".to_string(),
        }],
        question_set_hash: "question-hash".to_string(),
        container_image_refs: vec!["ghcr.io/example/agent:latest".to_string()],
        results_dir: PathBuf::from("/results/run-1"),
        checkout_root: PathBuf::from("checkouts"),
    };

    let json = serde_json::json!({
        "model_ids": {"subject": "claude-sonnet-5", "judge": "claude-opus-5"},
        "claude_code_version": "2.1.0",
        "subject_repos": [{
            "name": "hallouminate",
            "url": "https://github.com/paulnsorensen/hallouminate",
            "commit": "deadbeef",
            "size_class": "small"
        }],
        "prompt_hashes": [{"path": "prompts/wiki.md", "blake3": "abc123"}],
        "question_set_hash": "question-hash",
        "container_image_refs": ["ghcr.io/example/agent:latest"],
        "results_dir": "/results/run-1",
        "checkout_root": "checkouts"
    });
    assert_eq!(serde_json::to_value(&manifest).unwrap(), json);
    let from_json: Manifest = serde_json::from_value(json).unwrap();
    assert_eq!(from_json, manifest);

    let toml_str = r#"
claude_code_version = "2.1.0"
question_set_hash = "question-hash"
container_image_refs = ["ghcr.io/example/agent:latest"]
results_dir = "/results/run-1"
checkout_root = "checkouts"

[model_ids]
subject = "claude-sonnet-5"
judge = "claude-opus-5"

[[subject_repos]]
name = "hallouminate"
url = "https://github.com/paulnsorensen/hallouminate"
commit = "deadbeef"
size_class = "small"

[[prompt_hashes]]
path = "prompts/wiki.md"
blake3 = "abc123"
"#;
    let from_toml: Manifest = toml::from_str(toml_str).unwrap();
    assert_eq!(from_toml, manifest);
    let round_tripped: Manifest = toml::from_str(&toml::to_string(&manifest).unwrap()).unwrap();
    assert_eq!(round_tripped, manifest);
}

#[test]
fn bench_report_round_trips_with_deterministic_map_order() {
    let mut pass_at_k = BTreeMap::new();
    pass_at_k.insert(1, 0.5);
    pass_at_k.insert(5, 0.9);
    let mut pass_pow_k = BTreeMap::new();
    pass_pow_k.insert(1, 0.5);
    pass_pow_k.insert(5, 0.4);

    let arm_summary = ArmSummary {
        arm: Arm::Wiki,
        tag: None,
        questions: 10,
        runs: 50,
        pass_at_k,
        pass_pow_k,
        usage: sample_token_usage(),
        tokens_to_correct_answer: Some(1234.5),
    };

    let mut per_arm_stat = BTreeMap::new();
    per_arm_stat.insert(
        Arm::Baseline,
        QuestionArmStat {
            runs: 5,
            passes: 3,
            usage: sample_token_usage(),
            tokens_to_correct_answer: None,
        },
    );
    per_arm_stat.insert(
        Arm::Wiki,
        QuestionArmStat {
            runs: 5,
            passes: 4,
            usage: sample_token_usage(),
            tokens_to_correct_answer: Some(500.0),
        },
    );

    let report = BenchReport {
        per_arm: vec![arm_summary],
        per_question: vec![QuestionRow {
            question_id: "q1".to_string(),
            repo: "hallouminate".to_string(),
            tag: QuestionTag::WikiOnly,
            per_arm: per_arm_stat,
        }],
        paired_ci: PairedCi {
            metric: "pass_at_1".to_string(),
            point_estimate: 0.1,
            lower: 0.0,
            upper: 0.2,
            confidence: 0.95,
            resamples: 10000,
            seed: 42,
        },
    };

    let serialized = serde_json::to_string(&report).unwrap();
    // BTreeMap<Arm, _> keys serialize in Arm's Ord order: Wiki before Baseline.
    let baseline_pos = serialized.find("\"baseline\"").unwrap();
    let wiki_pos = serialized.rfind("\"wiki\"").unwrap();
    assert!(wiki_pos < baseline_pos);

    let round_tripped: BenchReport = serde_json::from_str(&serialized).unwrap();
    assert_eq!(round_tripped, report);
}

#[test]
fn blake3_file_hash_is_deterministic_and_changes_with_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.txt");
    std::fs::write(&path, b"hello world").unwrap();

    let hash1 = agent_bench::blake3_file_hash(&path).unwrap();
    let hash2 = agent_bench::blake3_file_hash(&path).unwrap();
    assert_eq!(hash1, hash2);

    std::fs::write(&path, b"hello worlD").unwrap();
    let hash3 = agent_bench::blake3_file_hash(&path).unwrap();
    assert_ne!(hash1, hash3);
}

#[test]
fn load_json_error_includes_path() {
    let missing = PathBuf::from("/nonexistent/path/to/file.json");
    let err = agent_bench::load_json::<Question>(&missing).unwrap_err();
    assert!(err.to_string().contains(missing.to_str().unwrap()));
}

#[test]
fn load_toml_error_includes_path() {
    let missing = PathBuf::from("/nonexistent/path/to/file.toml");
    let err = agent_bench::load_toml::<Manifest>(&missing).unwrap_err();
    assert!(err.to_string().contains(missing.to_str().unwrap()));
}
