//! Integration tests for `bench-report`: joins `sessions.jsonl` +
//! `grades.jsonl` into `report.json` / `report.md`. All fixtures are
//! constructed directly (no fake CLI needed — `bench-report` reads files,
//! it doesn't spawn an agent).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use agent_bench::{
    Arm, BenchReport, GradeRecord, Question, QuestionSet, QuestionTag, SessionRecord, TokenUsage,
};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bench-report"))
}

fn sample_question(id: &str, tag: QuestionTag) -> Question {
    Question {
        repo: "subject-repo".to_string(),
        id: id.to_string(),
        tag,
        question: format!("What does component {id} do?"),
        gold_answer: format!("Component {id} handles the request queue."),
        rubric_notes: format!("Must name the request queue for {id}."),
    }
}

fn write_question_set(dir: &Path, questions: &[Question]) -> PathBuf {
    let path = dir.join("questions.json");
    let set = QuestionSet {
        questions: questions.to_vec(),
    };
    fs::write(&path, serde_json::to_string_pretty(&set).unwrap()).unwrap();
    path
}

fn session(question_id: &str, arm: Arm, run_index: u32, usage: TokenUsage) -> SessionRecord {
    SessionRecord {
        question_id: question_id.to_string(),
        repo: "subject-repo".to_string(),
        arm,
        run_index,
        answer_text: "some answer".to_string(),
        usage,
        transcript_path: PathBuf::from("/tmp/transcript.json"),
        wall_ms: 100,
        exit_status: 0,
    }
}

fn grade(question_id: &str, arm: Arm, run_index: u32, pass: bool) -> GradeRecord {
    GradeRecord::grade(
        question_id.to_string(),
        arm,
        run_index,
        if pass { 5 } else { 0 },
        4,
        "rationale".to_string(),
    )
    .unwrap()
}

/// Write `n` (session, grade) pairs for `question_id`/`arm`, with `passes`
/// of the first `passes` runs (by run_index) marked as passing. `usage`
/// applies to every run.
fn write_runs(
    sessions_path: &Path,
    grades_path: &Path,
    question_id: &str,
    arm: Arm,
    n: u32,
    passes: u32,
    usage: TokenUsage,
) {
    for run_index in 0..n {
        agent_bench::append_jsonl(sessions_path, &session(question_id, arm, run_index, usage))
            .unwrap();
        agent_bench::append_jsonl(
            grades_path,
            &grade(question_id, arm, run_index, run_index < passes),
        )
        .unwrap();
    }
}

struct RunReport {
    out_dir: PathBuf,
    report: BenchReport,
    report_json_bytes: Vec<u8>,
    report_md: String,
}

#[allow(clippy::too_many_arguments)]
fn run_bench_report(
    dir: &Path,
    out_name: &str,
    sessions_path: &Path,
    grades_path: &Path,
    questions_path: &Path,
    resamples: u32,
    seed: u64,
    confidence: f64,
) -> RunReport {
    let out_dir = dir.join(out_name);
    let output = Command::new(bin_path())
        .arg("--sessions")
        .arg(sessions_path)
        .arg("--grades")
        .arg(grades_path)
        .arg("--questions")
        .arg(questions_path)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--resamples")
        .arg(resamples.to_string())
        .arg("--seed")
        .arg(seed.to_string())
        .arg("--confidence")
        .arg(confidence.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bench-report failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report_json_bytes = fs::read(out_dir.join("report.json")).unwrap();
    let report: BenchReport = serde_json::from_slice(&report_json_bytes).unwrap();
    let report_md = fs::read_to_string(out_dir.join("report.md")).unwrap();

    RunReport {
        out_dir,
        report,
        report_json_bytes,
        report_md,
    }
}

// Hand-computed pass@k / pass^k (see python check in the dispatch notes):
// n = 5 runs, c = 2 passes.
//   fail = n - c = 3
//   pass@1 = 1 - C(3,1)/C(5,1) = 1 - 3/5   = 0.4
//   pass@2 = 1 - C(3,2)/C(5,2) = 1 - 3/10  = 0.7
//   pass@3 = 1 - C(3,3)/C(5,3) = 1 - 1/10  = 0.9
//   pass^1 = C(2,1)/C(5,1) = 2/5  = 0.4
//   pass^2 = C(2,2)/C(5,2) = 1/10 = 0.1
//   pass^3 = C(2,3)/C(5,3) = 0/10 = 0.0   (k > c)
#[test]
fn pass_at_k_and_pass_pow_k_match_hand_computed_values() {
    let dir = tempfile::tempdir().unwrap();
    let questions_path =
        write_question_set(dir.path(), &[sample_question("q1", QuestionTag::Greppable)]);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 10,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    write_runs(&sessions_path, &grades_path, "q1", Arm::Wiki, 5, 2, usage);
    // Baseline data for the same question so paired-CI computation has a
    // pairable question; irrelevant to this test's assertions.
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Baseline,
        5,
        1,
        usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        100,
        1,
        0.95,
    );

    let wiki_all = run
        .report
        .per_arm
        .iter()
        .find(|s| s.arm == Arm::Wiki && s.tag.is_none())
        .expect("Wiki/all ArmSummary present");

    assert_eq!(wiki_all.questions, 1);
    assert_eq!(wiki_all.runs, 5);

    let expected_at_k = [(1u32, 0.4), (2, 0.7), (3, 0.9)];
    for (k, expected) in expected_at_k {
        let actual = *wiki_all
            .pass_at_k
            .get(&k)
            .unwrap_or_else(|| panic!("pass_at_k missing k={k}: {:?}", wiki_all.pass_at_k));
        assert!(
            (actual - expected).abs() < 1e-9,
            "pass@{k}: expected {expected}, got {actual}"
        );
    }

    let expected_pow_k = [(1u32, 0.4), (2, 0.1), (3, 0.0)];
    for (k, expected) in expected_pow_k {
        let actual = *wiki_all
            .pass_pow_k
            .get(&k)
            .unwrap_or_else(|| panic!("pass_pow_k missing k={k}: {:?}", wiki_all.pass_pow_k));
        assert!(
            (actual - expected).abs() < 1e-9,
            "pass^{k}: expected {expected}, got {actual}"
        );
    }
}

#[test]
fn pass_at_k_range_is_restricted_to_the_shared_min_n_across_questions() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [
        sample_question("q-short", QuestionTag::WikiOnly),
        sample_question("q-long", QuestionTag::WikiOnly),
    ];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 1,
        output_tokens: 1,
        cache_read_input_tokens: 1,
        cache_creation_input_tokens: 1,
    };
    write_runs(
        &sessions_path,
        &grades_path,
        "q-short",
        Arm::Wiki,
        3,
        1,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-long",
        Arm::Wiki,
        10,
        4,
        usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        1,
        0.95,
    );

    let wiki_all = run
        .report
        .per_arm
        .iter()
        .find(|s| s.arm == Arm::Wiki && s.tag.is_none())
        .expect("Wiki/all ArmSummary present");

    assert_eq!(wiki_all.questions, 2);
    assert_eq!(
        wiki_all.pass_at_k.keys().max().copied(),
        Some(3),
        "pass@k must stop at min(n)=3 so every k covers both questions: {:?}",
        wiki_all.pass_at_k
    );
    assert_eq!(
        wiki_all.pass_pow_k.keys().max().copied(),
        Some(3),
        "pass^k must stop at min(n)=3 so every k covers both questions: {:?}",
        wiki_all.pass_pow_k
    );
}

#[test]
fn single_arm_zero_passes_is_not_flagged_as_both_arms() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [sample_question("q-solo", QuestionTag::WikiOnly)];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 1,
        output_tokens: 1,
        cache_read_input_tokens: 1,
        cache_creation_input_tokens: 1,
    };
    write_runs(
        &sessions_path,
        &grades_path,
        "q-solo",
        Arm::Wiki,
        3,
        0,
        usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        1,
        0.95,
    );

    let section = run
        .report_md
        .split("### ")
        .find(|s| s.starts_with("q-solo"))
        .expect("report.md must have a section for q-solo");
    assert!(
        !section.contains("FLAGGED"),
        "single-arm question must not be flagged as both arms zero: {section}"
    );
}

fn unpaired_ci_width(
    wiki: &[f64],
    baseline: &[f64],
    resamples: u32,
    seed: u64,
    confidence: f64,
) -> f64 {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut diffs = Vec::with_capacity(resamples as usize);
    for _ in 0..resamples {
        let mut wsum = 0.0;
        for _ in 0..wiki.len() {
            wsum += wiki[rng.random_range(0..wiki.len())];
        }
        let mut bsum = 0.0;
        for _ in 0..baseline.len() {
            bsum += baseline[rng.random_range(0..baseline.len())];
        }
        diffs.push(wsum / wiki.len() as f64 - bsum / baseline.len() as f64);
    }
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let alpha = (1.0 - confidence) / 2.0;
    let lo_idx = (alpha * (diffs.len() - 1) as f64).round() as usize;
    let hi_idx = ((1.0 - alpha) * (diffs.len() - 1) as f64).round() as usize;
    diffs[hi_idx] - diffs[lo_idx]
}

#[test]
fn paired_bootstrap_is_deterministic_and_narrower_than_unpaired_resampling() {
    let dir = tempfile::tempdir().unwrap();

    // 6 questions, n = 10 runs per arm. Both the raw per-question rates AND
    // the per-question diff (wiki_rate - baseline_rate) genuinely vary:
    // diffs = [0.1, 0.3, -0.1, 0.4, 0.0, 0.2] (includes a negative and a
    // zero), mean 0.15. A `paired_bootstrap` stubbed to return a
    // degenerate point (e.g. always the point estimate) cannot pass this
    // test, because the resampled-mean distribution over these varying
    // diffs has real spread: the CI must have nonzero width and must
    // bracket the point estimate.
    //
    // Paired bootstrap resamples QUESTIONS as the unit, preserving each
    // question's own (wiki, baseline) pairing; an unpaired bootstrap that
    // resamples each arm's per-question rates independently additionally
    // picks up the between-question variance in the raw rates (0.1..0.8),
    // so it should still come out wider than the paired CI even though
    // both now have nonzero width.
    let baseline_c = [1u32, 2, 3, 4, 5, 6];
    let wiki_c = [2u32, 5, 2, 8, 5, 8];
    let n = 10u32;

    let questions: Vec<Question> = (1..=6)
        .map(|i| sample_question(&format!("q{i}"), QuestionTag::WikiOnly))
        .collect();
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 1,
        output_tokens: 1,
        cache_read_input_tokens: 1,
        cache_creation_input_tokens: 1,
    };
    for i in 0..6 {
        let qid = format!("q{}", i + 1);
        write_runs(
            &sessions_path,
            &grades_path,
            &qid,
            Arm::Baseline,
            n,
            baseline_c[i],
            usage,
        );
        write_runs(
            &sessions_path,
            &grades_path,
            &qid,
            Arm::Wiki,
            n,
            wiki_c[i],
            usage,
        );
    }

    let resamples = 20_000;
    let seed = 42;
    let confidence = 0.95;

    let run_a = run_bench_report(
        dir.path(),
        "out-a",
        &sessions_path,
        &grades_path,
        &questions_path,
        resamples,
        seed,
        confidence,
    );
    let run_b = run_bench_report(
        dir.path(),
        "out-b",
        &sessions_path,
        &grades_path,
        &questions_path,
        resamples,
        seed,
        confidence,
    );

    assert_eq!(
        run_a.report_json_bytes, run_b.report_json_bytes,
        "same seed must produce byte-identical report.json"
    );

    let paired = &run_a.report.paired_ci;
    assert_eq!(paired.resamples, resamples);
    assert_eq!(paired.seed, seed);
    // Hand-computed: diffs = [0.1, 0.3, -0.1, 0.4, 0.0, 0.2], sum = 0.9,
    // mean = 0.9 / 6 = 0.15.
    assert!(
        (paired.point_estimate - 0.15).abs() < 1e-9,
        "point estimate should be the mean per-question diff 0.15, got {}",
        paired.point_estimate
    );
    let paired_width = paired.upper - paired.lower;
    assert!(
        paired_width > 0.0,
        "paired CI must have nonzero width given genuinely varying per-question diffs, got width {paired_width}"
    );
    assert!(
        paired.lower <= paired.point_estimate + 1e-9
            && paired.point_estimate - 1e-9 <= paired.upper,
        "CI must bracket the point estimate: lower={}, point_estimate={}, upper={}",
        paired.lower,
        paired.point_estimate,
        paired.upper
    );

    let wiki_rates: Vec<f64> = wiki_c
        .iter()
        .map(|&c| f64::from(c) / f64::from(n))
        .collect();
    let baseline_rates: Vec<f64> = baseline_c
        .iter()
        .map(|&c| f64::from(c) / f64::from(n))
        .collect();
    let unpaired_width =
        unpaired_ci_width(&wiki_rates, &baseline_rates, resamples, seed, confidence);

    assert!(
        paired_width < unpaired_width,
        "paired resampling (question as the unit) must be narrower than unpaired resampling: \
         paired_width={paired_width}, unpaired_width={unpaired_width}"
    );
    assert!(
        unpaired_width > 0.05,
        "unpaired CI should be clearly wide given the heterogeneous per-question rates, got {unpaired_width}"
    );

    let _ = run_a.out_dir;
    let _ = run_b.out_dir;
}

#[test]
fn question_with_zero_passes_in_both_arms_is_kept_and_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [
        sample_question("q-zero", QuestionTag::Abstention),
        sample_question("q-normal", QuestionTag::Greppable),
    ];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 5,
        output_tokens: 5,
        cache_read_input_tokens: 5,
        cache_creation_input_tokens: 5,
    };
    write_runs(
        &sessions_path,
        &grades_path,
        "q-zero",
        Arm::Wiki,
        3,
        0,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-zero",
        Arm::Baseline,
        3,
        0,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-normal",
        Arm::Wiki,
        3,
        2,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-normal",
        Arm::Baseline,
        3,
        1,
        usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        7,
        0.95,
    );

    let zero_row = run
        .report
        .per_question
        .iter()
        .find(|r| r.question_id == "q-zero")
        .expect("q-zero must appear in per_question, not be dropped");
    assert_eq!(
        zero_row.per_arm.len(),
        2,
        "q-zero must have both arms represented"
    );
    for stat in zero_row.per_arm.values() {
        assert_eq!(stat.passes, 0);
        assert_eq!(stat.runs, 3);
        assert_eq!(
            stat.tokens_to_correct_answer, None,
            "zero correct runs must be represented as None, not dropped or defaulted to 0"
        );
    }

    let section = run
        .report_md
        .split("### ")
        .find(|s| s.starts_with("q-zero"))
        .expect("report.md must have a section for q-zero");
    assert!(
        section.contains("FLAGGED"),
        "report.md must flag q-zero (zero passes in both arms): {section}"
    );

    let normal_section = run
        .report_md
        .split("### ")
        .find(|s| s.starts_with("q-normal"))
        .expect("report.md must have a section for q-normal");
    assert!(
        !normal_section.contains("FLAGGED"),
        "report.md must not flag a question with nonzero passes: {normal_section}"
    );
}

#[test]
fn report_md_usage_breakdown_reconciles_exactly_with_report_json() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [sample_question("q1", QuestionTag::Greppable)];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");

    // Distinct, unmistakable per-run usage for each arm so a mixed-up
    // arm/field association in report.md would be caught.
    let wiki_usage = TokenUsage {
        input_tokens: 11,
        output_tokens: 22,
        cache_read_input_tokens: 33,
        cache_creation_input_tokens: 44,
    };
    let baseline_usage = TokenUsage {
        input_tokens: 101,
        output_tokens: 202,
        cache_read_input_tokens: 303,
        cache_creation_input_tokens: 404,
    };
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Wiki,
        3,
        3,
        wiki_usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Baseline,
        3,
        3,
        baseline_usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        3,
        0.95,
    );

    for (arm, per_run) in [(Arm::Wiki, wiki_usage), (Arm::Baseline, baseline_usage)] {
        let summary = run
            .report
            .per_arm
            .iter()
            .find(|s| s.arm == arm && s.tag.is_none())
            .unwrap_or_else(|| panic!("{arm:?}/all ArmSummary present"));
        assert_eq!(summary.usage.input_tokens, per_run.input_tokens * 3);
        assert_eq!(summary.usage.output_tokens, per_run.output_tokens * 3);
        assert_eq!(
            summary.usage.cache_read_input_tokens,
            per_run.cache_read_input_tokens * 3
        );
        assert_eq!(
            summary.usage.cache_creation_input_tokens,
            per_run.cache_creation_input_tokens * 3
        );

        // report.md must carry the same four numbers, associated with the
        // right arm's section (found via a heading containing the arm's
        // debug name before the next heading).
        let heading = format!("{arm:?}");
        let arm_section = run
            .report_md
            .split("### ")
            .find(|s| s.starts_with(&heading))
            .unwrap_or_else(|| panic!("report.md has a section for {heading}"));
        assert!(
            arm_section.contains(&summary.usage.input_tokens.to_string()),
            "{arm:?} section missing input_tokens {}: {arm_section}",
            summary.usage.input_tokens
        );
        assert!(
            arm_section.contains(&summary.usage.output_tokens.to_string()),
            "{arm:?} section missing output_tokens {}: {arm_section}",
            summary.usage.output_tokens
        );
        assert!(
            arm_section.contains(&summary.usage.cache_read_input_tokens.to_string()),
            "{arm:?} section missing cache_read_input_tokens {}: {arm_section}",
            summary.usage.cache_read_input_tokens
        );
        assert!(
            arm_section.contains(&summary.usage.cache_creation_input_tokens.to_string()),
            "{arm:?} section missing cache_creation_input_tokens {}: {arm_section}",
            summary.usage.cache_creation_input_tokens
        );
    }
}

fn failed_session(
    question_id: &str,
    arm: Arm,
    run_index: u32,
    usage: TokenUsage,
    exit_status: i32,
) -> SessionRecord {
    SessionRecord {
        question_id: question_id.to_string(),
        repo: "subject-repo".to_string(),
        arm,
        run_index,
        answer_text: String::new(),
        usage,
        transcript_path: PathBuf::from("/tmp/transcript.json"),
        wall_ms: 100,
        exit_status,
    }
}

// Hand-computed pass@k for the 5 successful runs (n=5, c=2) -- the same
// values verified in `pass_at_k_and_pass_pow_k_match_hand_computed_values`:
//   fail = 3
//   pass@1 = 1 - C(3,1)/C(5,1) = 0.4
//   pass@2 = 1 - C(3,2)/C(5,2) = 0.7
//   pass@3 = 1 - C(3,3)/C(5,3) = 0.9
#[test]
fn failed_sessions_are_excluded_from_every_figure() {
    let dir = tempfile::tempdir().unwrap();
    let questions_path =
        write_question_set(dir.path(), &[sample_question("q1", QuestionTag::Greppable)]);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 10,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    write_runs(&sessions_path, &grades_path, "q1", Arm::Wiki, 5, 2, usage);
    // Baseline so the question is pairable; irrelevant to this test's assertions.
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Baseline,
        5,
        1,
        usage,
    );

    // A 6th Wiki run that failed to exit cleanly, carrying a large, distinct
    // usage and a passing grade. If `exit_status` were ignored, it would
    // inflate `runs` to 6, `passes` to 3, and `usage` by `failed_usage`.
    let failed_usage = TokenUsage {
        input_tokens: 9_999,
        output_tokens: 9_999,
        cache_read_input_tokens: 9_999,
        cache_creation_input_tokens: 9_999,
    };
    agent_bench::append_jsonl(
        &sessions_path,
        &failed_session("q1", Arm::Wiki, 5, failed_usage, 17),
    )
    .unwrap();
    agent_bench::append_jsonl(&grades_path, &grade("q1", Arm::Wiki, 5, true)).unwrap();

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        1,
        0.95,
    );

    let wiki_all = run
        .report
        .per_arm
        .iter()
        .find(|s| s.arm == Arm::Wiki && s.tag.is_none())
        .expect("Wiki/all ArmSummary present");

    assert_eq!(wiki_all.runs, 5, "failed run must not be counted in n");
    assert_eq!(wiki_all.usage.input_tokens, usage.input_tokens * 5);
    assert_eq!(wiki_all.usage.output_tokens, usage.output_tokens * 5);
    assert_eq!(
        wiki_all.usage.cache_read_input_tokens,
        usage.cache_read_input_tokens * 5
    );
    assert_eq!(
        wiki_all.usage.cache_creation_input_tokens,
        usage.cache_creation_input_tokens * 5
    );

    let expected_at_k = [(1u32, 0.4), (2, 0.7), (3, 0.9)];
    for (k, expected) in expected_at_k {
        let actual = *wiki_all
            .pass_at_k
            .get(&k)
            .unwrap_or_else(|| panic!("pass_at_k missing k={k}: {:?}", wiki_all.pass_at_k));
        assert!(
            (actual - expected).abs() < 1e-9,
            "pass@{k}: expected {expected}, got {actual}"
        );
    }

    assert!(
        run.report_md
            .contains("## Failed sessions (excluded from every figure below)"),
        "report.md missing failed-sessions section: {}",
        run.report_md
    );
    let section = run
        .report_md
        .split("## Failed sessions (excluded from every figure below)")
        .nth(1)
        .and_then(|s| s.split("##").next())
        .unwrap();
    assert!(
        section.contains("Wiki: 1"),
        "expected exactly 1 failed Wiki session reported: {section}"
    );
}

#[test]
fn dropped_questions_are_named_and_paired_count_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [
        sample_question("q-both", QuestionTag::Greppable),
        sample_question("q-wiki-only", QuestionTag::Greppable),
    ];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    write_runs(
        &sessions_path,
        &grades_path,
        "q-both",
        Arm::Wiki,
        3,
        2,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-both",
        Arm::Baseline,
        3,
        1,
        usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q-wiki-only",
        Arm::Wiki,
        2,
        1,
        usage,
    );

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        50,
        1,
        0.95,
    );

    assert!(
        run.report_md
            .contains("q-wiki-only (missing runs in an arm)"),
        "dropped section must name q-wiki-only: {}",
        run.report_md
    );

    let paired_count_lines: Vec<&str> = run
        .report_md
        .lines()
        .filter(|l| l.trim_start() == "- paired questions: 1")
        .collect();
    assert_eq!(
        paired_count_lines.len(),
        2,
        "expected both the pass-rate and token-cost CIs to report 1 paired question: {}",
        run.report_md
    );

    let dropped_row = run
        .report
        .per_question
        .iter()
        .find(|row| row.question_id == "q-wiki-only")
        .expect("q-wiki-only must still appear in report.json per_question");
    assert!(
        dropped_row.per_arm.contains_key(&Arm::Wiki),
        "q-wiki-only must still carry its Wiki stats"
    );
}

#[test]
fn token_cost_paired_ci_matches_hand_computed_mean() {
    let dir = tempfile::tempdir().unwrap();
    let questions = [
        sample_question("q1", QuestionTag::Greppable),
        sample_question("q2", QuestionTag::Greppable),
    ];
    let questions_path = write_question_set(dir.path(), &questions);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");

    let wiki_usage = TokenUsage {
        input_tokens: 100,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    let baseline_usage_q1 = TokenUsage {
        input_tokens: 60,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    // q1: wiki 100/run - baseline 60/run = diff 40.
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Wiki,
        2,
        1,
        wiki_usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Baseline,
        2,
        1,
        baseline_usage_q1,
    );
    // q2: wiki 100/run - baseline 100/run = diff 0.
    write_runs(
        &sessions_path,
        &grades_path,
        "q2",
        Arm::Wiki,
        2,
        1,
        wiki_usage,
    );
    write_runs(
        &sessions_path,
        &grades_path,
        "q2",
        Arm::Baseline,
        2,
        1,
        wiki_usage,
    );

    // Hand-computed: mean((100-60), (100-100)) = mean(40, 0) = 20.0.
    let expected_point_estimate = 20.0;

    let run = run_bench_report(
        dir.path(),
        "out",
        &sessions_path,
        &grades_path,
        &questions_path,
        200,
        7,
        0.95,
    );

    assert!(
        run.report_md
            .contains("## Paired bootstrap CI (token cost)"),
        "report.md missing token-cost CI section: {}",
        run.report_md
    );
    let section = run
        .report_md
        .split("## Paired bootstrap CI (token cost)")
        .nth(1)
        .and_then(|s| s.split("##").next())
        .unwrap();
    assert!(
        section.contains("- metric: mean_tokens_per_run_diff_wiki_minus_baseline"),
        "wrong metric name: {section}"
    );
    assert!(
        section.contains(&format!("- point_estimate: {expected_point_estimate:.4}")),
        "point_estimate does not match hand-computed mean {expected_point_estimate}: {section}"
    );
    assert!(section.contains("% CI: ["), "missing CI bounds: {section}");

    // Deliberately not in report.json: `BenchReport` carries a single
    // `paired_ci` field for the pass-rate CI only.
    assert_eq!(
        run.report.paired_ci.metric,
        "pass_rate_diff_wiki_minus_baseline"
    );
    assert!(
        !String::from_utf8(run.report_json_bytes.clone())
            .unwrap()
            .contains("mean_tokens_per_run_diff_wiki_minus_baseline"),
        "token-cost metric must not leak into report.json"
    );
}

#[test]
fn resamples_zero_is_rejected_at_parse_time() {
    let dir = tempfile::tempdir().unwrap();
    let questions_path =
        write_question_set(dir.path(), &[sample_question("q1", QuestionTag::Greppable)]);
    let sessions_path = dir.path().join("sessions.jsonl");
    let grades_path = dir.path().join("grades.jsonl");
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    write_runs(&sessions_path, &grades_path, "q1", Arm::Wiki, 2, 1, usage);
    write_runs(
        &sessions_path,
        &grades_path,
        "q1",
        Arm::Baseline,
        2,
        1,
        usage,
    );

    let out_dir = dir.path().join("out");
    let output = Command::new(bin_path())
        .arg("--sessions")
        .arg(&sessions_path)
        .arg("--grades")
        .arg(&grades_path)
        .arg("--questions")
        .arg(&questions_path)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--resamples")
        .arg("0")
        .arg("--seed")
        .arg("1")
        .arg("--confidence")
        .arg("0.95")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "bench-report must reject --resamples 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resamples") && stderr.contains('0'),
        "stderr must name the invalid --resamples value: {stderr}"
    );
}
