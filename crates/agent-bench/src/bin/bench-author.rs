//! Drives budgeted agent wiki-authoring for one subject repo and logs usage.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use agent_bench::{Manifest, TokenUsage, repo_root};
use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;
use serde_json::Value;

/// Default agent CLI binary, overridable via `AGENT_BENCH_CLAUDE_BIN`.
const DEFAULT_CLAUDE_BIN: &str = "claude";

/// Fixed follow-up message sent on every turn after the first; the agent's
/// own session (resumed via `--continue`) carries the actual context.
const CONTINUE_MESSAGE: &str = "Continue.";

/// How many characters of a malformed response to include in diagnostics.
const EXCERPT_LEN: usize = 500;

/// Hard cap on authoring turns. The budget alone does not bound the loop: a
/// turn that never signals completion and reports no usage advances neither
/// exit condition, so the runner respawns forever.
const MAX_TURNS: usize = 200;

#[derive(Parser)]
#[command(about = "Drive budgeted agent wiki-authoring for one subject repo")]
struct Args {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    repo: String,
    #[arg(long = "budget-tokens")]
    budget_tokens: u64,
    #[arg(long = "out-dir")]
    out_dir: PathBuf,
}

#[derive(Serialize)]
struct AuthoringLogEntry {
    turn: usize,
    usage: TokenUsage,
    cumulative: TokenUsage,
    wall_ms: u64,
}

#[derive(Serialize)]
struct AuthoringSummary {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    total_tokens: u64,
    turns: usize,
    budget_tokens: u64,
    wiki_dir: PathBuf,
    prompt_blake3: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let manifest: Manifest = load_manifest(&args.manifest)?;
    let repo = manifest
        .subject_repos
        .iter()
        .find(|r| r.name == args.repo)
        .ok_or_else(|| {
            let known: Vec<&str> = manifest
                .subject_repos
                .iter()
                .map(|r| r.name.as_str())
                .collect();
            anyhow::anyhow!(
                "unknown repo {:?}; known repos: [{}]",
                args.repo,
                known.join(", ")
            )
        })?;

    let workspace_root = repo_root();
    let checkout = workspace_root
        .join(&manifest.checkout_root)
        .join(&repo.name);
    if !checkout.is_dir() {
        bail!(
            "subject repo {:?}: checkout not found at {} \u{2014} clone and check out commit {} there before authoring",
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
            "subject repo {:?}: checkout at {} has HEAD {}, but the manifest pins commit {} \u{2014} refusing to author against a drifted checkout",
            repo.name,
            checkout.display(),
            actual_sha,
            repo.commit,
        );
    }

    let prompt_path = workspace_root.join("eval/agent-bench/prompts/wiki-authoring.md");
    let prompt_text = std::fs::read_to_string(&prompt_path)
        .with_context(|| format!("reading authoring prompt from {}", prompt_path.display()))?;
    let prompt_hash = agent_bench::blake3_file_hash(&prompt_path)
        .with_context(|| format!("hashing authoring prompt at {}", prompt_path.display()))?;

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("creating out-dir {}", args.out_dir.display()))?;
    let log_path = args.out_dir.join("authoring-log.jsonl");
    let wiki_dir = checkout.join(".hallouminate").join("wiki");
    let claude_bin =
        std::env::var("AGENT_BENCH_CLAUDE_BIN").unwrap_or_else(|_| DEFAULT_CLAUDE_BIN.to_string());
    agent_bench::verify_agent_cli_version(&claude_bin, &manifest.claude_code_version)?;
    let subject_model = manifest.model_ids.subject.as_str();
    let mut cumulative = TokenUsage::default();
    let mut turn = 0usize;

    loop {
        if turn >= MAX_TURNS {
            bail!(
                "authoring did not complete within the {MAX_TURNS}-turn cap (consumed {} of {} budget tokens) \u{2014} the agent never signalled completion",
                cumulative.total(),
                args.budget_tokens,
            );
        }
        let started = Instant::now();
        let mut command = Command::new(&claude_bin);
        command.current_dir(&checkout);
        // Without --model the CLI resolves whatever default model is
        // current, making `manifest.model_ids.subject` provenance the
        // authoring run never honoured.
        if turn == 0 {
            command.args([
                "-p",
                &prompt_text,
                "--output-format",
                "json",
                "--model",
                subject_model,
                // No --mcp-config is passed at all, so --strict-mcp-config
                // here means "no MCP servers" -- intended: wiki authoring
                // uses native tools only. --setting-sources "" keeps ambient
                // hooks/permissions/settings out of the authoring run.
                "--strict-mcp-config",
                "--setting-sources",
                "",
            ]);
        } else {
            command.args([
                "-p",
                CONTINUE_MESSAGE,
                "--output-format",
                "json",
                "--continue",
                "--model",
                subject_model,
                "--strict-mcp-config",
                "--setting-sources",
                "",
            ]);
        }
        let output = command
            .output()
            .with_context(|| format!("invoking agent CLI {claude_bin:?} for turn {turn}"))?;
        let wall_ms = started.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        let result: Value = serde_json::from_str(&stdout).map_err(|_| {
            anyhow::anyhow!(
                "turn {turn}: agent CLI produced non-JSON output: {}",
                excerpt(&stdout)
            )
        })?;
        let usage_value = result.get("usage").ok_or_else(|| {
            anyhow::anyhow!(
                "turn {turn}: agent CLI JSON result had no `usage` object: {}",
                excerpt(&stdout)
            )
        })?;
        let usage = parse_usage(usage_value).with_context(|| {
            format!(
                "turn {turn}: agent CLI usage object was malformed: {}",
                excerpt(&stdout)
            )
        })?;
        // A real turn always costs tokens. A well-formed reply reporting no
        // usage at all means the CLI never reached the model (auth failure,
        // MCP startup failure); it advances neither loop exit, so treat it
        // as the error it is rather than respawning against it.
        if usage.total() == 0 {
            bail!(
                "turn {turn}: agent CLI reported zero total token usage, which no real turn does \u{2014} \
                 the CLI most likely never reached the model: {}",
                excerpt(&stdout)
            );
        }

        cumulative += usage;
        agent_bench::append_jsonl(
            &log_path,
            &AuthoringLogEntry {
                turn,
                usage,
                cumulative,
                wall_ms,
            },
        )
        .with_context(|| format!("appending authoring log to {}", log_path.display()))?;

        if cumulative.total() > args.budget_tokens {
            bail!(
                "authoring budget exceeded: budget={} tokens, consumed={} tokens (stopped at turn {turn})",
                args.budget_tokens,
                cumulative.total()
            );
        }

        let completed = is_completed(&result);
        turn += 1;
        if completed {
            break;
        }
    }

    let summary = AuthoringSummary {
        input_tokens: cumulative.input_tokens,
        output_tokens: cumulative.output_tokens,
        cache_read_input_tokens: cumulative.cache_read_input_tokens,
        cache_creation_input_tokens: cumulative.cache_creation_input_tokens,
        total_tokens: cumulative.total(),
        turns: turn,
        budget_tokens: args.budget_tokens,
        wiki_dir,
        prompt_blake3: prompt_hash,
    };
    let summary_path = args.out_dir.join("authoring-summary.json");
    let file = std::fs::File::create(&summary_path)
        .with_context(|| format!("creating {}", summary_path.display()))?;
    serde_json::to_writer_pretty(file, &summary)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    Ok(())
}

fn load_manifest(path: &Path) -> anyhow::Result<Manifest> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => agent_bench::load_toml(path),
        _ => agent_bench::load_json(path),
    }
}

fn parse_usage(value: &Value) -> anyhow::Result<TokenUsage> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("usage field was not a JSON object"))?;
    let field = |name: &str| obj.get(name).and_then(Value::as_u64).unwrap_or(0);
    Ok(TokenUsage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
    })
}

/// A turn signals authoring completion when its JSON result reports
/// `"subtype": "success"` together with `"is_error": false`. Any other
/// well-formed result (an in-progress `subtype`, or `is_error: true`) is
/// treated as "not yet done" and the loop continues, bounded by the budget.
fn is_completed(result: &Value) -> bool {
    result.get("subtype").and_then(Value::as_str) == Some("success")
        && result.get("is_error").and_then(Value::as_bool) == Some(false)
}

fn excerpt(text: &str) -> String {
    let mut out: String = text.chars().take(EXCERPT_LEN).collect();
    if text.chars().count() > EXCERPT_LEN {
        out.push_str("...");
    }
    out
}
