//! Session runner for the wiki-grounding benchmark pilot.
//!
//! Spawns `claude -p --output-format json` once per (question, arm,
//! run_index) under the pinned per-arm MCP config, recording one
//! `SessionRecord` per session to `<out-dir>/sessions.jsonl`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use agent_bench::{
    Arm, Manifest, Question, QuestionSet, SessionRecord, TokenUsage, append_jsonl,
    blake3_file_hash, load_json, load_toml, read_jsonl, repo_root, verify_agent_cli_version,
};

/// Directory (relative to the repository root, not the process cwd) holding
/// the per-arm MCP configs. The wiki arm launches with `--mcp-config
/// <ARM_CONFIG_DIR>/wiki-arm.mcp.json`; the baseline arm with
/// `<ARM_CONFIG_DIR>/baseline-arm.mcp.json`.
const ARM_CONFIG_DIR: &str = "eval/agent-bench/config";

/// Sidecar in `--out-dir` recording which question set and which authored
/// wikis the `sessions.jsonl` ledger beside it was produced under.
/// `SessionRecord`'s field set is frozen and shared with the judge and
/// report curds, so the ledger's treatment identity lives here rather than
/// on every record.
const RUN_META_FILE: &str = "run-meta.json";

/// Path, relative to a subject repo checkout, of the authored wiki that is
/// the wiki arm's entire treatment.
const WIKI_RELATIVE_DIR: &str = ".hallouminate/wiki";

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

/// The agent CLI invocation's run-invariant parts: which binary to spawn and
/// the subject model every session must be pinned to.
#[derive(Debug, Clone, Copy)]
struct AgentCli<'a> {
    bin: &'a str,
    subject_model: &'a str,
}

/// Contents of `<out-dir>/run-meta.json`.
#[derive(Debug, Serialize, Deserialize)]
struct RunMeta {
    question_set_hash: String,
    /// blake3 digest of each subject repo's authored wiki tree, keyed by
    /// subject repo name. Absent in metadata written before wiki
    /// provenance was recorded, which reads as an empty map and so cannot
    /// be shown to match anything but an empty wiki.
    #[serde(default)]
    wiki_hashes: BTreeMap<String, String>,
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
    let checkouts = resolve_checkouts(&repo_root, &manifest)?;

    // A ledger is keyed on (question_id, arm, run_index) alone, so once the
    // treatment changes every one of those triples still resolves and every
    // completed session is skipped -- silently mixing two treatments into
    // one arm. The question set and the authored wiki are both treatments,
    // so both are recorded and both refuse resumption across a change.
    let wiki_hashes = wiki_tree_hashes(&manifest, &checkouts)?;
    let meta_path = out_dir.join(RUN_META_FILE);
    if !records.is_empty() && !cli.force {
        check_ledger_provenance(&meta_path, &manifest.question_set_hash, &wiki_hashes)?;
    }
    write_run_meta(&meta_path, &manifest.question_set_hash, &wiki_hashes)?;

    if arms.contains(&Arm::Wiki) {
        for repo in &manifest.subject_repos {
            let checkout = checkouts
                .get(&repo.name)
                .unwrap_or_else(|| panic!("no resolved checkout for repo {:?}", repo.name));
            check_no_source_corpus_leak(checkout, &repo.name)?;
        }
    }
    let bin = std::env::var("AGENT_BENCH_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    verify_agent_cli_version(&bin, &manifest.claude_code_version)?;
    let agent = AgentCli {
        bin: &bin,
        subject_model: &manifest.model_ids.subject,
    };

    for question in &question_set.questions {
        for &arm in &arms {
            let config_path = arm_config_path(&repo_root, arm);
            let checkout = checkouts
                .get(&question.repo)
                .unwrap_or_else(|| panic!("no resolved checkout for repo {:?}", question.repo));
            for run_index in 0..cli.runs {
                let key = (question.id.clone(), arm, run_index);
                if let Some(&pos) = index.get(&key) {
                    if !cli.force {
                        continue;
                    }
                    let record = run_session(
                        agent,
                        &config_path,
                        question,
                        arm,
                        run_index,
                        &out_dir,
                        checkout,
                    )?;
                    records[pos] = record;
                } else {
                    let record = run_session(
                        agent,
                        &config_path,
                        question,
                        arm,
                        run_index,
                        &out_dir,
                        checkout,
                    )?;
                    index.insert(key, records.len());
                    records.push(record);
                }
                rewrite_sessions_jsonl(&sessions_path, &records)?;
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

/// Deserialize-only mirror of `hallouminate_domain::repository::RepositoryConfig`
/// (`crates/hallouminate-domain/src/repository.rs:27-40`), just enough to read
/// `[[repository]]` entries out of a checkout's `.hallouminate/config.toml`.
#[derive(Debug, Deserialize)]
struct RepoConfigEntry {
    name: String,
    #[serde(default)]
    corpus_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RepoLayerConfig {
    #[serde(default)]
    repository: Vec<RepoConfigEntry>,
}

/// Hard-fails if `checkout`'s `.hallouminate/config.toml` declares
/// `corpus_paths` under ANY `[[repository]]` entry. The wiki arm's entire
/// measurement claim rests on the wiki corpus being the only corpus the
/// agent can reach; a non-empty `corpus_paths` derives a
/// `repo:{name}:corpus` source corpus
/// (`crates/hallouminate-domain/src/repository.rs:104-125`) that would leak
/// semantic source search into the "wiki" measurement. No config file at
/// all is fine — no corpus derivation happens without it.
///
/// Every entry is checked, not just the one whose `name` matches the
/// benchmark manifest's: the benchmark name is a label the operator picked
/// for the checkout directory, while the checkout's own config names itself
/// whatever its author chose. Matching on the name made the sole
/// enforcement point for the wiki-only invariant fail open on the most
/// likely real configuration. `repo_name` names the subject repo in the
/// diagnostic only.
fn check_no_source_corpus_leak(checkout: &Path, repo_name: &str) -> anyhow::Result<()> {
    let config_path = checkout.join(".hallouminate").join("config.toml");
    if !config_path.exists() {
        return Ok(());
    }
    let config: RepoLayerConfig = load_toml(&config_path)?;
    for entry in &config.repository {
        if !entry.corpus_paths.is_empty() {
            bail!(
                "subject repo {:?}: {} declares corpus_paths = {:?} under [[repository]] name = {:?} \u{2014} the wiki arm requires no source corpus in this checkout, or the measured effect stops being \"the wiki\"",
                repo_name,
                config_path.display(),
                entry.corpus_paths,
                entry.name,
            );
        }
    }
    Ok(())
}

/// blake3 digest of one checkout's authored wiki tree: every file under
/// `.hallouminate/wiki`, folded in sorted relative-path order so the digest
/// depends on content and layout but not on directory-iteration order. An
/// absent wiki directory digests as the empty tree, which is what a
/// baseline-only or pre-authoring run legitimately has.
fn wiki_tree_hash(checkout: &Path) -> anyhow::Result<String> {
    let wiki_dir = checkout.join(WIKI_RELATIVE_DIR);
    let mut files = Vec::new();
    collect_wiki_files(&wiki_dir, &wiki_dir, &mut files)?;
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for (relative, file_hash) in &files {
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_wiki_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading wiki directory {}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("reading wiki directory entry in {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_wiki_files(root, &path, out)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((relative, blake3_file_hash(&path)?));
        }
    }
    Ok(())
}

/// Digest every subject repo's authored wiki, keyed by repo name.
///
/// Computed for every subject repo regardless of which arms this invocation
/// runs: one `--out-dir` holds one ledger shared by both arms, so a
/// baseline-only invocation still appends to the ledger the wiki arm's
/// records live in.
fn wiki_tree_hashes(
    manifest: &Manifest,
    checkouts: &HashMap<String, PathBuf>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for repo in &manifest.subject_repos {
        let checkout = checkouts
            .get(&repo.name)
            .unwrap_or_else(|| panic!("no resolved checkout for repo {:?}", repo.name));
        hashes.insert(repo.name.clone(), wiki_tree_hash(checkout)?);
    }
    Ok(hashes)
}

/// Refuse to resume a ledger produced under a different question set or a
/// different authored wiki.
fn check_ledger_provenance(
    meta_path: &Path,
    manifest_hash: &str,
    wiki_hashes: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if !meta_path.exists() {
        bail!(
            "existing sessions.jsonl carries no recorded question-set hash ({} is missing), \
             so it cannot be shown to match the manifest's question_set_hash {} \u{2014} \
             re-run with --force to overwrite it, or point --out-dir at a fresh directory",
            meta_path.display(),
            manifest_hash,
        );
    }
    let meta: RunMeta = load_json(meta_path)?;
    if meta.question_set_hash != manifest_hash {
        bail!(
            "question-set re-freeze: the existing sessions.jsonl was produced under \
             question_set_hash {}, but the manifest pins {} \u{2014} resuming would skip \
             every already-recorded session and grade the previous set's answers against \
             the new gold answers; re-run with --force to re-run them, or point --out-dir \
             at a fresh directory",
            meta.question_set_hash,
            manifest_hash,
        );
    }
    check_ledger_wiki_hashes(&meta, wiki_hashes)?;
    Ok(())
}

/// Refuse to resume a ledger whose recorded wiki digests differ from the
/// wikis on disk now. The wiki *is* the wiki arm's treatment, so editing or
/// re-authoring it mid-run puts pre-edit and post-edit sessions in one arm
/// and averages two treatments into one number, with no error and no
/// warning.
fn check_ledger_wiki_hashes(
    meta: &RunMeta,
    wiki_hashes: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if &meta.wiki_hashes == wiki_hashes {
        return Ok(());
    }
    let mut changed = Vec::new();
    for name in meta.wiki_hashes.keys().chain(wiki_hashes.keys()) {
        let recorded = meta.wiki_hashes.get(name);
        let current = wiki_hashes.get(name);
        if recorded == current {
            continue;
        }
        let describe = |hash: Option<&String>| match hash {
            Some(hash) => hash.clone(),
            None => "not recorded".to_string(),
        };
        let entry = format!(
            "{name}: ledger {}, on disk {}",
            describe(recorded),
            describe(current),
        );
        if !changed.contains(&entry) {
            changed.push(entry);
        }
    }
    bail!(
        "authored wiki re-freeze: the existing sessions.jsonl was produced under a different \
         {WIKI_RELATIVE_DIR} tree \u{2014} [{}]; the wiki is the wiki arm's treatment, so \
         resuming would keep the pre-edit sessions and average two treatments into one arm; \
         re-run with --force to re-run them, or point --out-dir at a fresh directory",
        changed.join("; "),
    );
}

fn write_run_meta(
    meta_path: &Path,
    manifest_hash: &str,
    wiki_hashes: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let meta = RunMeta {
        question_set_hash: manifest_hash.to_string(),
        wiki_hashes: wiki_hashes.clone(),
    };
    let json = serde_json::to_string_pretty(&meta)?;
    fs::write(meta_path, json)
        .with_context(|| format!("writing run metadata to {}", meta_path.display()))
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
    agent: AgentCli<'_>,
    config_path: &Path,
    question: &Question,
    arm: Arm,
    run_index: u32,
    out_dir: &Path,
    checkout: &Path,
) -> anyhow::Result<SessionRecord> {
    let start = Instant::now();
    let spawned = Command::new(agent.bin)
        .current_dir(checkout)
        .arg("-p")
        .arg(&question.question)
        .arg("--output-format")
        .arg("json")
        .arg("--mcp-config")
        .arg(config_path)
        // Without this the CLI resolves whatever default model is current,
        // so `manifest.model_ids.subject` would be recorded provenance the
        // run never honoured.
        .arg("--model")
        .arg(agent.subject_model)
        // Ambient user/project MCP config can attach servers to the baseline
        // arm, whose entire defining property is having no MCP server
        // attached; --strict-mcp-config keeps each arm's server set exactly
        // what --mcp-config declares. --setting-sources "" blocks ambient
        // hooks/permissions/settings from leaking into measured sessions,
        // which otherwise vary per machine with nothing in the artifacts
        // explaining the resulting delta.
        //
        // --safe-mode was considered and rejected: it also disables
        // --mcp-config servers, which would silently run every arm
        // native-tools-only and turn every reported delta into noise.
        .arg("--strict-mcp-config")
        .arg("--setting-sources")
        .arg("")
        .output();
    let wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let (stdout, mut exit_status) = match spawned {
        Ok(output) => (output.stdout, output.status.code().unwrap_or(-1)),
        Err(err) => (
            format!("bench-run: failed to spawn agent {:?}: {err}", agent.bin).into_bytes(),
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
