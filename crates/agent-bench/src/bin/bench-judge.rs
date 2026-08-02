//! Arm-blind judge harness: grades recorded agent sessions against gold
//! answers using the rubric in `eval/agent-bench/prompts/judge-rubric.md`,
//! and optionally reports agreement against a human-labelled subset.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use agent_bench::{Arm, GradeRecord, Question, QuestionSet, SessionRecord};
use anyhow::Context;
use clap::Parser;
use serde::Deserialize;

const RUBRIC: &str = include_str!("../../../../eval/agent-bench/prompts/judge-rubric.md");

#[derive(Debug, Parser)]
struct Args {
    /// Path to sessions.jsonl (SessionRecord per line).
    #[arg(long)]
    sessions: PathBuf,
    /// Path to the QuestionSet JSON file.
    #[arg(long)]
    questions: PathBuf,
    /// Path to write grades.jsonl (GradeRecord per line).
    #[arg(long)]
    out: PathBuf,
    /// Minimum score (0..=5) that counts as a pass.
    #[arg(long, default_value_t = 4)]
    pass_threshold: u8,
    /// Optional path to a human-labelled grades.jsonl; enables calibration
    /// reporting and the `--min-agreement` gate.
    #[arg(long)]
    calibrate: Option<PathBuf>,
    /// Minimum pass-threshold agreement with the human labels required to
    /// exit zero in calibration mode.
    #[arg(long, default_value_t = 0.80)]
    min_agreement: f64,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> anyhow::Result<()> {
    let question_set: QuestionSet = agent_bench::load_json(&args.questions)?;
    let questions_by_id: HashMap<&str, &Question> = question_set
        .questions
        .iter()
        .map(|q| (q.id.as_str(), q))
        .collect();
    let sessions: Vec<SessionRecord> = agent_bench::read_jsonl(&args.sessions)?;
    let claude_bin =
        std::env::var("AGENT_BENCH_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

    let mut judge_grades = Vec::with_capacity(sessions.len());
    for session in &sessions {
        let question = questions_by_id
            .get(session.question_id.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no question with id {:?} for session (arm {:?}, run {})",
                    session.question_id,
                    session.arm,
                    session.run_index
                )
            })?;
        let prompt = render_judge_prompt(RUBRIC, question, &session.answer_text);
        let reply = invoke_judge(&claude_bin, &prompt).with_context(|| {
            format!(
                "invoking judge model for question {:?}",
                session.question_id
            )
        })?;
        let (score, rationale) = parse_judge_reply(&reply).with_context(|| {
            format!("parsing judge reply for question {:?}", session.question_id)
        })?;
        let grade = GradeRecord::grade(
            session.question_id.clone(),
            session.arm,
            session.run_index,
            score,
            args.pass_threshold,
            rationale,
        )
        .with_context(|| format!("grading question {:?}", session.question_id))?;
        agent_bench::append_jsonl(&args.out, &grade)?;
        judge_grades.push(grade);
    }

    if let Some(human_path) = &args.calibrate {
        let human_grades: Vec<GradeRecord> = agent_bench::read_jsonl(human_path)?;
        let report = calibrate(&judge_grades, &human_grades, args.pass_threshold)?;
        println!("exact_agreement: {:.4}", report.exact_agreement);
        println!(
            "pass_threshold_agreement: {:.4}",
            report.pass_threshold_agreement
        );
        println!("kappa: {:.4}", report.kappa);
        if report.pass_threshold_agreement < args.min_agreement {
            anyhow::bail!(
                "pass-threshold agreement {:.4} is below --min-agreement {:.4}",
                report.pass_threshold_agreement,
                args.min_agreement
            );
        }
    }

    Ok(())
}

/// Render the arm-blind judge prompt. Takes only the rubric text, a
/// `Question`, and the candidate's answer text — no `Arm`, no
/// `SessionRecord`, no MCP/hallouminate metadata, so leaking the arm into
/// the judge is structurally impossible rather than merely avoided.
fn render_judge_prompt(rubric: &str, question: &Question, candidate_answer: &str) -> String {
    format!(
        "{rubric}\n\n\
         ## Question\n{question_text}\n\n\
         ## Gold answer\n{gold_answer}\n\n\
         ## Rubric notes\n{rubric_notes}\n\n\
         ## Candidate answer\n{candidate_answer}\n",
        rubric = rubric,
        question_text = question.question,
        gold_answer = question.gold_answer,
        rubric_notes = question.rubric_notes,
        candidate_answer = candidate_answer,
    )
}

#[derive(Debug, Deserialize)]
struct JudgeCliOutput {
    result: String,
}

/// Run the judge model binary (`claude -p --output-format json`, or its
/// `AGENT_BENCH_CLAUDE_BIN` override) with `prompt` on stdin, and return the
/// `result` field of its JSON reply.
fn invoke_judge(bin: &str, prompt: &str) -> anyhow::Result<String> {
    let mut child = Command::new(bin)
        .args(["-p", "--output-format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning judge model binary {bin:?}"))?;
    child
        .stdin
        .take()
        .expect("child stdin was piped")
        .write_all(prompt.as_bytes())
        .context("writing prompt to judge model stdin")?;
    let output = child
        .wait_with_output()
        .context("waiting for judge model process")?;
    if !output.status.success() {
        anyhow::bail!(
            "judge model process exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout =
        String::from_utf8(output.stdout).context("judge model stdout was not valid UTF-8")?;
    let parsed: JudgeCliOutput = serde_json::from_str(&stdout)
        .with_context(|| format!("parsing judge model JSON output: {stdout:?}"))?;
    Ok(parsed.result)
}

/// Parse a judge reply of the form `SCORE: <n>\nRATIONALE: <text>` per the
/// contract in `judge-rubric.md`. Never defaults a missing or unparseable
/// score to 0 or to a pass — always a hard error naming the offending text.
fn parse_judge_reply(text: &str) -> anyhow::Result<(u8, String)> {
    let score_line = text
        .lines()
        .find(|line| line.trim_start().starts_with("SCORE:"))
        .ok_or_else(|| anyhow::anyhow!("judge reply had no SCORE: line: {text:?}"))?;
    let score_str = score_line.trim_start().trim_start_matches("SCORE:").trim();
    let score: u8 = score_str
        .parse()
        .map_err(|_| anyhow::anyhow!("judge reply had an unparseable score {score_str:?}"))?;

    let rationale_marker = "RATIONALE:";
    let rationale_start = text
        .find(rationale_marker)
        .ok_or_else(|| anyhow::anyhow!("judge reply had no RATIONALE: line: {text:?}"))?;
    let rationale = text[rationale_start + rationale_marker.len()..]
        .trim()
        .to_string();

    Ok((score, rationale))
}

#[derive(Debug)]
struct CalibrationReport {
    exact_agreement: f64,
    pass_threshold_agreement: f64,
    kappa: f64,
}

/// Compare judge grades to human grades on their labelled overlap (matched
/// by question id, arm, run index). Both sides' pass/fail is recomputed at
/// `threshold` so the comparison is apples-to-apples even if the human
/// grades were labelled under a different threshold.
fn calibrate(
    judge_grades: &[GradeRecord],
    human_grades: &[GradeRecord],
    threshold: u8,
) -> anyhow::Result<CalibrationReport> {
    let judge_by_key: HashMap<(String, Arm, u32), &GradeRecord> = judge_grades
        .iter()
        .map(|g| ((g.question_id.clone(), g.arm, g.run_index), g))
        .collect();
    let pairs: Vec<(&GradeRecord, &GradeRecord)> = human_grades
        .iter()
        .filter_map(|human| {
            judge_by_key
                .get(&(human.question_id.clone(), human.arm, human.run_index))
                .map(|judge| (*judge, human))
        })
        .collect();
    if pairs.is_empty() {
        anyhow::bail!(
            "calibration labelled subset is empty: none of {} human grades matched a judge grade",
            human_grades.len()
        );
    }

    let n = pairs.len() as f64;
    let exact_matches = pairs
        .iter()
        .filter(|(judge, human)| judge.score == human.score)
        .count();

    let mut judge_pass_count = 0usize;
    let mut human_pass_count = 0usize;
    let mut pass_agree_count = 0usize;
    for (judge, human) in &pairs {
        let judge_pass = GradeRecord::passes(judge.score, threshold);
        let human_pass = GradeRecord::passes(human.score, threshold);
        if judge_pass {
            judge_pass_count += 1;
        }
        if human_pass {
            human_pass_count += 1;
        }
        if judge_pass == human_pass {
            pass_agree_count += 1;
        }
    }

    let po = pass_agree_count as f64 / n;
    let p_judge_yes = judge_pass_count as f64 / n;
    let p_human_yes = human_pass_count as f64 / n;
    let pe = p_judge_yes * p_human_yes + (1.0 - p_judge_yes) * (1.0 - p_human_yes);
    let kappa = if (1.0 - pe).abs() < f64::EPSILON {
        1.0
    } else {
        (po - pe) / (1.0 - pe)
    };

    Ok(CalibrationReport {
        exact_agreement: exact_matches as f64 / n,
        pass_threshold_agreement: po,
        kappa,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grade(question_id: &str, score: u8) -> GradeRecord {
        GradeRecord::grade(
            question_id.to_string(),
            Arm::Wiki,
            0,
            score,
            4,
            String::new(),
        )
        .unwrap()
    }

    #[test]
    fn parse_judge_reply_extracts_score_and_rationale() {
        let (score, rationale) =
            parse_judge_reply("SCORE: 3\nRATIONALE: Partially correct.").unwrap();
        assert_eq!(score, 3);
        assert_eq!(rationale, "Partially correct.");
    }

    #[test]
    fn parse_judge_reply_rejects_missing_score_line() {
        let err = parse_judge_reply("no score here").unwrap_err();
        assert!(err.to_string().contains("no score here"));
    }

    #[test]
    fn parse_judge_reply_rejects_unparseable_score() {
        let err = parse_judge_reply("SCORE: high\nRATIONALE: n/a").unwrap_err();
        assert!(err.to_string().contains("high"));
    }

    // Hand-computed: 4 items, human pass/fail = [pass, pass, fail, fail],
    // judge pass/fail = [pass, fail, fail, fail].
    //   po = 3/4 = 0.75 (items 1, 3, 4 agree; item 2 disagrees)
    //   p_judge_yes = 1/4 = 0.25, p_human_yes = 2/4 = 0.5
    //   pe = 0.25*0.5 + 0.75*0.5 = 0.5
    //   kappa = (0.75 - 0.5) / (1 - 0.5) = 0.5
    #[test]
    fn calibrate_computes_hand_checked_kappa() {
        let judge_grades = vec![
            grade("q1", 5),
            grade("q2", 2),
            grade("q3", 1),
            grade("q4", 0),
        ];
        let human_grades = vec![
            grade("q1", 5),
            grade("q2", 4),
            grade("q3", 2),
            grade("q4", 1),
        ];
        let report = calibrate(&judge_grades, &human_grades, 4).unwrap();
        assert!((report.exact_agreement - 0.25).abs() < 1e-9);
        assert!((report.pass_threshold_agreement - 0.75).abs() < 1e-9);
        assert!((report.kappa - 0.5).abs() < 1e-9);
    }

    #[test]
    fn calibrate_rejects_empty_labelled_subset() {
        let judge_grades = vec![grade("q1", 5)];
        let human_grades = vec![grade("q-not-graded", 5)];
        let err = calibrate(&judge_grades, &human_grades, 4).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
