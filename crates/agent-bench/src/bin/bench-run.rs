//! Session runner for the wiki-grounding benchmark pilot.
//!
//! Runs an offline agent fixture once per (question, arm, run_index).
//! Records one `SessionRecord` per session under the pinned MCP config.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use agent_bench::provenance::{RunMeta, fingerprint, load_questions, resolve_executable};
use agent_bench::{
    Arm, Manifest, Question, SessionRecord, TokenUsage, blake3_file_hash, load_toml, read_jsonl,
    replace_jsonl, repo_root, verify_agent_cli_version,
};
use anyhow::{Context, bail, ensure};
use clap::{Parser, ValueEnum};
use serde::Deserialize;

#[path = "support/run_sandbox.rs"]
mod run_sandbox;

use run_sandbox::{Sandbox, Views};

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
    /// Trusted local stand-in. Subject trials cannot use a model service.
    #[arg(long, value_name = "PATH")]
    offline_agent: Option<PathBuf>,
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
    let question_set = load_questions(&cli.questions, &manifest)?;
    let arms: &[Arm] = match cli.arm {
        ArmSelector::Both => &[Arm::Wiki, Arm::Baseline],
        ArmSelector::Wiki => &[Arm::Wiki],
        ArmSelector::Baseline => &[Arm::Baseline],
    };

    let Some(offline_agent) = cli.offline_agent else {
        bail!(
            "no isolated model authentication is available; use --offline-agent PATH for offline tests"
        );
    };
    let bin = offline_agent
        .canonicalize()
        .context("resolving offline agent")?;
    verify_agent_cli_version(
        bin.to_str().context("agent executable path is not UTF-8")?,
        &manifest.claude_code_version,
    )?;
    let checkouts = resolve_checkouts(&repo_root(), &manifest)?;
    let hallouminate = resolve_executable("AGENT_BENCH_HALLOUMINATE_BIN", "hallouminate")?;
    let agent_hash = blake3_file_hash(&bin)?;
    let scratch = tempfile::tempdir_in("/tmp")?;
    let sandbox = Sandbox::new(hallouminate, scratch.path())?;
    let subjects = scratch.path().join("subjects");
    let mut views = HashMap::new();
    for repo in &manifest.subject_repos {
        let checkout = &checkouts[&repo.name];
        let view = Views::prepare(subjects.join(&repo.name), checkout, &repo.commit)
            .with_context(|| format!("preparing isolated views for {:?}", repo.name))?;
        views.insert(repo.name.as_str(), view);
    }
    let view_hashes: BTreeMap<_, _> = views
        .iter()
        .map(|(&name, view)| (name, view.fingerprints()))
        .collect();
    let manifest_hash = fingerprint(&manifest)?;
    let meta = RunMeta {
        measurement_hash: fingerprint(&(
            include_bytes!("bench-run.rs").as_slice(),
            &manifest_hash,
            &agent_hash,
            sandbox.fingerprint()?,
            view_hashes,
        ))?,
        manifest_hash,
        agent_hash,
    };
    let mut questions = HashSet::new();
    for question in &question_set.questions {
        ensure!(
            questions.insert(question.id.as_str()),
            "duplicate question {:?}",
            question.id
        );
        ensure!(
            views.contains_key(question.repo.as_str()),
            "no pinned repo {:?}",
            question.repo
        );
    }

    let sessions_path = cli.out_dir.join("sessions.jsonl");
    let initialized = meta.check(&sessions_path)?;
    let mut records: Vec<SessionRecord> = if sessions_path.try_exists()? {
        read_jsonl(&sessions_path)?
    } else {
        Vec::new()
    };
    let mut index = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        let key = (record.question_id.clone(), record.arm, record.run_index);
        ensure!(
            index.insert(key, i).is_none(),
            "duplicate session in the existing ledger"
        );
    }
    let mut daemons = Vec::new();
    for view in views.values() {
        daemons.push(sandbox.start_daemon(view)?);
    }

    fs::create_dir_all(&cli.out_dir)?;
    let out_dir = cli.out_dir.canonicalize()?;
    if !initialized {
        meta.initialize(&sessions_path)?;
    }

    for question in &question_set.questions {
        let view = &views[question.repo.as_str()];
        for &arm in arms {
            for run_index in 0..cli.runs {
                let key = (question.id.clone(), arm, run_index);
                let position = index.get(&key).copied();
                if position.is_some() && !cli.force {
                    continue;
                }
                let mut agent = sandbox.agent(view, arm, &bin);
                agent.args(["--model", &manifest.model_ids.subject]);
                let record = run_session(agent, question, arm, run_index, &out_dir)?;
                if let Some(position) = position {
                    records[position] = record;
                } else {
                    index.insert(key, records.len());
                    records.push(record);
                }
                replace_jsonl(&sessions_path, &records)?;
            }
        }
    }
    Ok(())
}

/// Resolves and verifies every subject repo's checkout under
/// `manifest.checkout_root`. Hard-fails if a checkout directory is missing
/// or its `HEAD` does not match the pinned commit — a drifted checkout
/// silently invalidates every number this harness produces.
fn resolve_checkouts(
    repo_root: &Path,
    manifest: &Manifest,
) -> anyhow::Result<HashMap<String, PathBuf>> {
    let mut checkouts = HashMap::new();
    for repo in &manifest.subject_repos {
        let path = Path::new(&repo.name);
        ensure!(
            path.file_name()
                .is_some_and(|name| name == path.as_os_str()),
            "invalid subject repo directory name {:?}",
            repo.name
        );
        ensure!(
            !checkouts.contains_key(&repo.name),
            "duplicate subject repo {:?}",
            repo.name
        );
        let checkout = repo_root.join(&manifest.checkout_root).join(&repo.name);
        if !checkout.is_dir() {
            bail!(
                "subject repo {:?}: checkout not found at {} \u{2014} clone and check out commit {} there before running",
                repo.name,
                checkout.display(),
                repo.commit,
            );
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .with_context(|| format!("running git rev-parse HEAD in {}", checkout.display()))?;
        if !output.status.success() {
            bail!(
                "subject repo {:?}: `git -C {} rev-parse HEAD` failed: {}",
                repo.name,
                checkout.display(),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let actual_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if actual_sha != repo.commit {
            bail!(
                "subject repo {:?}: checkout at {} has HEAD {}, but the manifest pins commit {} \u{2014} refusing to start any measured run against a drifted checkout",
                repo.name,
                checkout.display(),
                actual_sha,
                repo.commit,
            );
        }
        checkouts.insert(repo.name.clone(), checkout);
    }
    Ok(checkouts)
}

/// Run one (question, arm, run_index) session and return a structurally
/// valid `SessionRecord` regardless of outcome. A session that fails to
/// spawn, exits non-zero, or returns output that is not valid JSON is
/// recorded as an error (non-zero `exit_status`, a diagnostic
/// `answer_text`, zero usage) rather than dropped or silently treated as a
/// clean zero-usage success.
fn run_session(
    mut agent: Command,
    question: &Question,
    arm: Arm,
    run_index: u32,
    out_dir: &Path,
) -> anyhow::Result<SessionRecord> {
    let start = Instant::now();
    let spawned = agent
        .arg("-p")
        .arg(&question.question)
        .args([
            "--output-format",
            "json",
            "--mcp-config",
            "/run/bench/mcp.json",
            "--strict-mcp-config",
            "--setting-sources",
            "",
        ])
        .output();
    let wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let (stdout, stderr, mut exit_status) = match spawned {
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        }) => (stdout, stderr, status.code().unwrap_or(-1)),
        Err(err) => (
            format!("bench-run: failed to spawn isolated agent: {err}").into_bytes(),
            Vec::new(),
            -1,
        ),
    };

    let trace_dir = out_dir.join("traces");
    fs::create_dir_all(&trace_dir)?;
    let mut transcript = tempfile::Builder::new()
        .prefix("trial-")
        .suffix(".json")
        .tempfile_in(trace_dir)?;
    transcript.write_all(&stdout)?;
    transcript.as_file().sync_all()?;
    let (_, transcript_path) = transcript.keep()?;

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
        let mut diagnostic = String::from_utf8_lossy(&stdout).into_owned();
        if !stderr.is_empty() {
            if !diagnostic.is_empty() && !diagnostic.ends_with('\n') {
                diagnostic.push('\n');
            }
            diagnostic.push_str(&String::from_utf8_lossy(&stderr));
        }
        (diagnostic, TokenUsage::default())
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
