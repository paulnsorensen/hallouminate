//! Arm-blind judge harness: grades recorded agent sessions against gold
//! answers using the rubric in `eval/agent-bench/prompts/judge-rubric.md`,
//! and optionally reports agreement against a human-labelled subset.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agent_bench::grading::{Grading, index_sessions};
use agent_bench::provenance::{grade_key, resolve_executable, session_key};
use agent_bench::{Arm, GradeRecord, SessionRecord};
use anyhow::{Context, ensure};
use clap::Parser;
use serde::Deserialize;

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
    /// Path to the run's provenance manifest (TOML). Required, not
    /// optional: the manifest is what pins the judge model
    /// (`model_ids.judge`, passed to the judge as `--model`) and the rubric
    /// blake3 (`prompt_hashes`). An optional pin that silently no-ops when
    /// the flag is omitted would let a sweep run on a rotated default model
    /// while the manifest certifies otherwise -- exactly the provenance
    /// claim the manifest exists to make.
    #[arg(long)]
    manifest: PathBuf,
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
    /// Overwrite existing grades instead of skipping already-graded
    /// (question, arm, run_index) triples.
    #[arg(long)]
    force: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> anyhow::Result<()> {
    let grading = Grading::load(
        &args.manifest,
        &args.questions,
        &args.sessions,
        args.pass_threshold,
    )?;
    let sessions: Vec<SessionRecord> = agent_bench::read_jsonl(&args.sessions)?;
    let sessions_by_key = index_sessions(&sessions)?;
    let mut records = if !args.out.try_exists()? {
        Vec::new()
    } else if args.force {
        grading.read_grades_for_regrade(&args.out, &sessions_by_key)?
    } else {
        grading.read_grades(&args.out, &sessions_by_key)?
    };
    let mut index = HashMap::new();
    for (i, entry) in records.iter().enumerate() {
        let (question_id, arm, run_index) = grade_key(&entry.grade);
        index.insert((question_id.to_owned(), arm, run_index), i);
    }

    let claude_bin = resolve_executable("AGENT_BENCH_CLAUDE_BIN", "claude")?;
    ensure!(
        agent_bench::blake3_file_hash(&claude_bin)? == grading.meta.agent_hash,
        "judge CLI binary does not match the pinned measurement executable"
    );
    agent_bench::verify_agent_cli_version(
        claude_bin
            .to_str()
            .context("agent executable path is not UTF-8")?,
        &grading.manifest.claude_code_version,
    )?;
    for session in &sessions {
        if session.exit_status != 0 {
            continue;
        }
        let (id, arm, run_index) = session_key(session);
        let key = (id.to_owned(), arm, run_index);
        let position = index.get(&key).copied();
        if position.is_some() && !args.force {
            continue;
        }
        let prompt = grading.prompt(session)?;
        let reply = invoke_judge(&claude_bin, &grading.manifest.model_ids.judge, &prompt)
            .with_context(|| {
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

        let grade = grading.bind(grade, session, &prompt)?;
        if let Some(position) = position {
            records[position] = grade;
        } else {
            index.insert(key, records.len());
            records.push(grade);
        }
        agent_bench::replace_jsonl(&args.out, &records)?;
    }

    if let Some(human_path) = &args.calibrate {
        let human_grades: Vec<GradeRecord> = agent_bench::read_jsonl(human_path)?;
        let grades: Vec<GradeRecord> = records.iter().map(|entry| entry.grade.clone()).collect();
        let report = calibrate(&grades, &human_grades, args.pass_threshold)?;
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

    if records.is_empty() && (args.force || !args.out.try_exists()?) {
        agent_bench::replace_jsonl(&args.out, &records)?;
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct JudgeCliOutput {
    result: String,
}

/// Run the judge model binary (`claude --model <model> -p --output-format
/// json`, or its `AGENT_BENCH_CLAUDE_BIN` override) with `prompt` on stdin,
/// and return the `result` field of its JSON reply.
///
/// `model` comes from the manifest's `model_ids.judge`. Passing it
/// explicitly is what makes the manifest's pin real: without `--model` the
/// judge runs on whatever the CLI's default happens to be that week, and a
/// default rotation would silently invalidate a sweep the manifest still
/// certifies as reproducible.
///
/// The prompt is written to the child's stdin on a dedicated thread while
/// this thread drains the child's stdout/stderr via `wait_with_output`.
/// Writing the whole prompt synchronously before reading any output would
/// deadlock once the prompt exceeds the OS pipe buffer (as little as 16
/// KiB on macOS) if the child starts producing output before it has fully
/// drained stdin: the child blocks writing stdout (nobody is reading it
/// yet) while this process blocks writing stdin (nobody is reading that
/// either). This is the same pipe deadlock `std::process::Command`'s own
/// docs call out.
fn invoke_judge(bin: &Path, model: &str, prompt: &str) -> anyhow::Result<String> {
    let mut child = Command::new(bin)
        .args([
            "--model",
            model,
            "-p",
            "--output-format",
            "json",
            "--strict-mcp-config",
            "--setting-sources",
            "",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning judge model binary {bin:?}"))?;
    let mut stdin = child.stdin.take().expect("child stdin was piped");
    let prompt_owned = prompt.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(prompt_owned.as_bytes()));

    let output = child
        .wait_with_output()
        .context("waiting for judge model process")?;

    let write_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("judge model stdin writer thread panicked"))?;
    if let Err(err) = write_result {
        // A child that exits (successfully or not) without draining all of
        // stdin closes its read end, which surfaces here as a broken pipe;
        // that is not itself a failure worth reporting. Any other write
        // error is.
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(err).context("writing prompt to judge model stdin");
        }
    }

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
/// score to 0 or to a pass -- always a hard error naming the offending text.
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
