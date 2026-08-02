//! Session runner for the wiki-grounding benchmark pilot.
//!
//! Spawns `claude -p --output-format json` once per (question, arm,
//! run_index) under the pinned per-arm MCP config, recording one
//! `SessionRecord` per session to `<out-dir>/sessions.jsonl`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, ValueEnum};
use serde::Deserialize;

use agent_bench::{
    Arm, Manifest, Question, QuestionSet, SessionRecord, TokenUsage, append_jsonl,
    blake3_file_hash, load_json, load_toml, read_jsonl,
};

/// Directory (relative to the repository root, not the process cwd) holding
/// the per-arm MCP configs. The wiki arm launches with `--mcp-config
/// <ARM_CONFIG_DIR>/wiki-arm.mcp.json`; the baseline arm with
/// `<ARM_CONFIG_DIR>/baseline-arm.mcp.json`.
const ARM_CONFIG_DIR: &str = "eval/agent-bench/config";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArmSelector {
    Both,
    Wiki,
    Baseline,
}

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the provenance manifest (TOML), carrying the frozen
    /// `question_set_hash` checked against `--questions` before any session
    /// is spawned.
    #[arg(long)]
    manifest: PathBuf,
    /// Path to the question set (JSON).
    #[arg(long)]
    questions: PathBuf,
    /// Which arm(s) to run.
    #[arg(long, value_enum, default_value_t = ArmSelector::Both)]
    arm: ArmSelector,
    /// Runs per (question, arm).
    #[arg(long, default_value_t = 10)]
    runs: u32,
    /// Directory for `sessions.jsonl` and `traces/`.
    #[arg(long)]
    out_dir: PathBuf,
    /// Overwrite existing records instead of skipping them.
    #[arg(long)]
    force: bool,
}

/// The subset of `claude -p --output-format json` stdout this harness reads.
/// Deliberately narrow: extra fields in the real payload are ignored.
#[derive(Debug, Deserialize)]
struct AgentOutput {
    result: String,
    usage: TokenUsage,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let manifest: Manifest = load_toml(&cli.manifest)?;
    let actual_hash = blake3_file_hash(&cli.questions)?;
    if actual_hash != manifest.question_set_hash {
        bail!(
            "question-set hash drift: manifest.question_set_hash = {}, but {} hashes to {} \
             \u{2014} refusing to start any measured run",
            manifest.question_set_hash,
            cli.questions.display(),
            actual_hash,
        );
    }

    let question_set: QuestionSet = load_json(&cli.questions)?;
    let arms: Vec<Arm> = match cli.arm {
        ArmSelector::Both => vec![Arm::Wiki, Arm::Baseline],
        ArmSelector::Wiki => vec![Arm::Wiki],
        ArmSelector::Baseline => vec![Arm::Baseline],
    };

    fs::create_dir_all(&cli.out_dir)
        .with_context(|| format!("creating out-dir {}", cli.out_dir.display()))?;
    let out_dir = cli
        .out_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing out-dir {}", cli.out_dir.display()))?;
    let sessions_path = out_dir.join("sessions.jsonl");

    // Resumable ledger: load any existing records, key them by the
    // (question_id, arm, run_index) triple, then rewrite the whole file in
    // place after every session (skip or overwrite). This keeps
    // `sessions.jsonl` duplicate-free under both plain re-runs and
    // `--force`, and checkpoints progress so a mid-run crash still leaves a
    // resumable ledger.
    let mut records: Vec<SessionRecord> = if sessions_path.exists() {
        read_jsonl(&sessions_path)?
    } else {
        Vec::new()
    };
    let mut index: HashMap<(String, Arm, u32), usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.question_id.clone(), r.arm, r.run_index), i))
        .collect();

    let repo_root = repo_root();
    let bin = std::env::var("AGENT_BENCH_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

    for question in &question_set.questions {
        for &arm in &arms {
            let config_path = arm_config_path(&repo_root, arm);
            for run_index in 0..cli.runs {
                let key = (question.id.clone(), arm, run_index);
                if let Some(&pos) = index.get(&key) {
                    if !cli.force {
                        continue;
                    }
                    let record =
                        run_session(&bin, &config_path, question, arm, run_index, &out_dir)?;
                    records[pos] = record;
                } else {
                    let record =
                        run_session(&bin, &config_path, question, arm, run_index, &out_dir)?;
                    index.insert(key, records.len());
                    records.push(record);
                }
                rewrite_sessions_jsonl(&sessions_path, &records)?;
            }
        }
    }

    Ok(())
}

/// Repository root, resolved at compile time from this crate's location
/// under `crates/agent-bench` — independent of the process cwd.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/agent-bench has a parent directory")
        .parent()
        .expect("crates/ has a parent directory")
        .to_path_buf()
}

fn arm_config_path(repo_root: &Path, arm: Arm) -> PathBuf {
    let file = match arm {
        Arm::Wiki => "wiki-arm.mcp.json",
        Arm::Baseline => "baseline-arm.mcp.json",
    };
    repo_root.join(ARM_CONFIG_DIR).join(file)
}

fn arm_dir_name(arm: Arm) -> &'static str {
    match arm {
        Arm::Wiki => "wiki",
        Arm::Baseline => "baseline",
    }
}

/// Run one (question, arm, run_index) session and return a structurally
/// valid `SessionRecord` regardless of outcome. A session that fails to
/// spawn, exits non-zero, or returns output that is not valid JSON is
/// recorded as an error (non-zero `exit_status`, a diagnostic
/// `answer_text`, zero usage) rather than dropped or silently treated as a
/// clean zero-usage success.
fn run_session(
    bin: &str,
    config_path: &Path,
    question: &Question,
    arm: Arm,
    run_index: u32,
    out_dir: &Path,
) -> anyhow::Result<SessionRecord> {
    let start = Instant::now();
    let spawned = Command::new(bin)
        .arg("-p")
        .arg(&question.question)
        .arg("--output-format")
        .arg("json")
        .arg("--mcp-config")
        .arg(config_path)
        .output();
    let wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let (stdout, mut exit_status) = match spawned {
        Ok(output) => (output.stdout, output.status.code().unwrap_or(-1)),
        Err(err) => (
            format!("bench-run: failed to spawn agent {bin:?}: {err}").into_bytes(),
            -1,
        ),
    };

    let transcript_path = out_dir
        .join("traces")
        .join(&question.id)
        .join(arm_dir_name(arm))
        .join(format!("{run_index}.json"));
    fs::create_dir_all(
        transcript_path
            .parent()
            .expect("transcript_path always has a parent"),
    )
    .with_context(|| format!("creating transcript dir for {}", transcript_path.display()))?;
    fs::write(&transcript_path, &stdout)
        .with_context(|| format!("writing transcript {}", transcript_path.display()))?;

    let (answer_text, usage) = if exit_status == 0 {
        match serde_json::from_slice::<AgentOutput>(&stdout) {
            Ok(parsed) => (parsed.result, parsed.usage),
            Err(err) => {
                exit_status = -1;
                (
                    format!("bench-run: agent output was not valid JSON: {err}"),
                    TokenUsage::default(),
                )
            }
        }
    } else {
        (
            String::from_utf8_lossy(&stdout).into_owned(),
            TokenUsage::default(),
        )
    };

    Ok(SessionRecord {
        question_id: question.id.clone(),
        repo: question.repo.clone(),
        arm,
        run_index,
        answer_text,
        usage,
        transcript_path,
        wall_ms,
        exit_status,
    })
}

/// Rewrite `sessions.jsonl` from scratch to reflect `records`. Simpler and
/// safer than patching individual lines in place, and cheap at pilot scale
/// (questions × arms × runs).
fn rewrite_sessions_jsonl(path: &Path, records: &[SessionRecord]) -> anyhow::Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    for record in records {
        append_jsonl(path, record)?;
    }
    Ok(())
}
