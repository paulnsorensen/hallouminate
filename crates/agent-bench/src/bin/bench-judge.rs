//! Arm-blind judge harness: grades recorded agent sessions against gold
//! answers using the rubric in `eval/agent-bench/prompts/judge-rubric.md`,
//! and optionally reports agreement against a human-labelled subset.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agent_bench::{Arm, GradeRecord, Question, QuestionSet, SessionRecord};
use anyhow::Context;
use clap::Parser;
use serde::Deserialize;

/// Path (relative to the repository root, not the process cwd) to the
/// judge rubric. Read from disk at runtime rather than `include_str!`'d, so
/// the manifest's `prompt_hashes` blake3 check (`bench-validate-manifest`)
/// actually verifies the file bench-judge uses, not a copy baked in at
/// compile time.
const RUBRIC_RELATIVE_PATH: &str = "eval/agent-bench/prompts/judge-rubric.md";

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
    let question_set: QuestionSet = agent_bench::load_json(&args.questions)?;
    let questions_by_id: HashMap<&str, &Question> = question_set
        .questions
        .iter()
        .map(|q| (q.id.as_str(), q))
        .collect();
    let sessions: Vec<SessionRecord> = agent_bench::read_jsonl(&args.sessions)?;
    let claude_bin =
        std::env::var("AGENT_BENCH_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let rubric = load_rubric()?;

    // Resumable ledger: load any existing grades, key them by the
    // (question_id, arm, run_index) triple, then rewrite the whole file in
    // place after every judged session (skip or overwrite). Mirrors
    // bench-run.rs's sessions.jsonl ledger, so a plain re-run neither
    // duplicates records (which would double `n`/`c` and the reported
    // token usage) nor loses progress to a mid-run crash.
    let mut records: Vec<GradeRecord> = if args.out.exists() {
        agent_bench::read_jsonl(&args.out)?
    } else {
        Vec::new()
    };
    let mut index: HashMap<(String, Arm, u32), usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.question_id.clone(), r.arm, r.run_index), i))
        .collect();

    for session in &sessions {
        let key = (session.question_id.clone(), session.arm, session.run_index);
        if index.contains_key(&key) && !args.force {
            continue;
        }
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
        let candidate_answer = redact_provenance(&session.answer_text);
        let prompt = render_judge_prompt(&rubric, question, &candidate_answer);
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

        if let Some(&pos) = index.get(&key) {
            records[pos] = grade;
        } else {
            index.insert(key, records.len());
            records.push(grade);
        }
        rewrite_grades_jsonl(&args.out, &records)?;
    }

    if let Some(human_path) = &args.calibrate {
        let human_grades: Vec<GradeRecord> = agent_bench::read_jsonl(human_path)?;
        let report = calibrate(&records, &human_grades, args.pass_threshold)?;
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

/// Rewrite `grades.jsonl` from scratch to reflect `records`. Simpler and
/// safer than patching individual lines in place, and cheap at pilot scale
/// (questions x arms x runs). Mirrors bench-run.rs's
/// `rewrite_sessions_jsonl`.
fn rewrite_grades_jsonl(path: &Path, records: &[GradeRecord]) -> anyhow::Result<()> {
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    for record in records {
        agent_bench::append_jsonl(path, record)?;
    }
    Ok(())
}

/// Repository root, resolved at compile time from this crate's location
/// under `crates/agent-bench` -- independent of the process cwd.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/agent-bench has a workspace root two levels up")
        .to_path_buf()
}

/// Read the judge rubric from disk at runtime, the way `bench-author`
/// reads its authoring prompt -- so the manifest's blake3 `prompt_hashes`
/// check is verifying the file this binary actually uses.
fn load_rubric() -> anyhow::Result<String> {
    let path = repo_root().join(RUBRIC_RELATIVE_PATH);
    std::fs::read_to_string(&path)
        .with_context(|| format!("reading judge rubric from {}", path.display()))
}

/// Render the arm-blind judge prompt. Takes only the rubric text, a
/// `Question`, and the candidate's (already redacted) answer text -- no
/// `Arm`, no `SessionRecord`, no MCP/hallouminate metadata, so leaking the
/// arm into the judge is structurally impossible rather than merely
/// avoided.
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

/// Provenance tokens that would tell the judge which arm produced an
/// answer: the product name, the tool the wiki arm calls, and the two arm
/// names themselves. Matched whole-word and case-insensitively (see
/// `redact_provenance`), so `wiki` is redacted but `wikipedia` is not.
const PROVENANCE_TOKENS: &[&str] = &["hallouminate", "mcp", "wiki", "baseline"];

/// Redact provenance tokens from a candidate answer before it reaches the
/// judge prompt. `answer_text` is otherwise passed through unscrubbed, so a
/// wiki-arm answer that says "Per the `.hallouminate/wiki/architecture.md`
/// page..." would tell the judge which arm produced it -- this is the only
/// line of defense against that leak, since `render_judge_prompt` never
/// sees the `Arm` at all.
///
/// Matching is whole-word (the token must be bounded by non-alphanumeric,
/// non-underscore characters on both sides) and case-insensitive, not a
/// blanket substring replace: `hallouminate` and `mcp` are the harness's
/// own product/tool names, which no answer about an unrelated subject repo
/// has a legitimate reason to mention, so any whole-word occurrence is
/// treated as leaked provenance. `wiki`/`baseline` are literally the arm
/// names; matching them whole-word (not substring) means "wikipedia" or a
/// subject repo's own "lastbaseline" identifier survive untouched, while a
/// path like `.hallouminate/wiki/architecture.md` is fully redacted
/// because "hallouminate" and "wiki" are each their own word there.
fn redact_provenance(answer: &str) -> String {
    const REDACTED: &str = "[REDACTED]";
    let chars: Vec<char> = answer.chars().collect();
    let mut out = String::with_capacity(answer.len());
    let mut i = 0;
    while i < chars.len() {
        let matched_end = PROVENANCE_TOKENS.iter().find_map(|token| {
            let token_chars: Vec<char> = token.chars().collect();
            let end = i + token_chars.len();
            if end > chars.len() {
                return None;
            }
            let body_matches = chars[i..end]
                .iter()
                .zip(token_chars.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b);
            if !body_matches {
                return None;
            }
            let before_ok = i == 0 || !is_word_char(chars[i - 1]);
            let after_ok = end == chars.len() || !is_word_char(chars[end]);
            (before_ok && after_ok).then_some(end)
        });
        match matched_end {
            Some(end) => {
                out.push_str(REDACTED);
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[derive(Debug, Deserialize)]
struct JudgeCliOutput {
    result: String,
}

/// Run the judge model binary (`claude -p --output-format json`, or its
/// `AGENT_BENCH_CLAUDE_BIN` override) with `prompt` on stdin, and return the
/// `result` field of its JSON reply.
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
fn invoke_judge(bin: &str, prompt: &str) -> anyhow::Result<String> {
    let mut child = Command::new(bin)
        .args(["-p", "--output-format", "json"])
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

    #[test]
    fn redact_provenance_removes_tool_and_path_tokens_but_preserves_prose() {
        let redacted = redact_provenance(
            "Per the `.hallouminate/wiki/architecture.md` page (looked up via mcp), \
             the Wikipedia article on backoff was not needed. See BASELINE_TIMEOUT_MS.",
        );
        let lower = redacted.to_lowercase();
        assert!(!lower.contains("hallouminate"), "{redacted}");
        assert!(!lower.contains("mcp"), "{redacted}");
        assert!(
            lower.contains("wikipedia"),
            "whole-word match must not clip 'wikipedia': {redacted}"
        );
        assert!(
            lower.contains("baseline_timeout_ms"),
            "whole-word match must not clip an identifier containing 'baseline': {redacted}"
        );
    }

    #[test]
    fn rubric_is_read_from_disk_and_matches_the_file_on_disk() {
        let path = repo_root().join(RUBRIC_RELATIVE_PATH);
        let loaded = load_rubric().unwrap();
        let loaded_hash = blake3::hash(loaded.as_bytes()).to_hex().to_string();
        let file_hash = agent_bench::blake3_file_hash(&path).unwrap();
        assert_eq!(loaded_hash, file_hash);
    }
}
