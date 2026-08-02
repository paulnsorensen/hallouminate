//! Validates a benchmark provenance `Manifest` against the pilot's pinning
//! rules: two size-distinct subject repos, full-SHA commits, digest-pinned
//! container images, prompt-hash pins that match the files on disk, a
//! well-formed question-set hash, and a `results_dir` confined to
//! `eval/agent-bench`.

use std::path::{Path, PathBuf};

use agent_bench::{Manifest, SizeClass, repo_root};
use clap::Parser;

/// `results_dir` must live under this prefix, relative to the repo root.
const RESULTS_DIR_PREFIX: &str = "eval/agent-bench";

#[derive(Parser)]
#[command(about = "Validate a benchmark provenance Manifest against the pilot's pinning rules")]
struct Args {
    /// Path to the Manifest TOML file.
    #[arg(long)]
    manifest: PathBuf,
}

fn main() {
    let args = Args::parse();
    match run(&args) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    }
}

/// Runs validation. `Ok(true)` means success (exit 0); `Ok(false)` means
/// validation ran and found violations, already printed to stderr (exit 1);
/// `Err` means a hard failure (bad path, malformed file).
fn run(args: &Args) -> anyhow::Result<bool> {
    let manifest: Manifest = agent_bench::load_toml(&args.manifest)?;
    let repo_root = repo_root();

    let violations = validate(&manifest, &repo_root);

    if violations.is_empty() {
        println!(
            "OK: manifest with {} subject repos and {} prompt hashes passed all validation rules",
            manifest.subject_repos.len(),
            manifest.prompt_hashes.len()
        );
        Ok(true)
    } else {
        for violation in &violations {
            eprintln!("{violation}");
        }
        Ok(false)
    }
}

/// Runs every pinning rule against `manifest` and returns every violation
/// found, each naming the failing rule and the offending field.
fn validate(manifest: &Manifest, repo_root: &Path) -> Vec<String> {
    let mut violations = Vec::new();

    check_size_classes(manifest, &mut violations);
    check_commits(manifest, &mut violations);
    check_container_images(manifest, &mut violations);
    check_prompt_hashes(manifest, repo_root, &mut violations);
    check_question_set_hash(manifest, &mut violations);
    check_results_dir(manifest, &mut violations);
    check_checkout_root(manifest, &mut violations);

    violations
}

fn check_size_classes(manifest: &Manifest, violations: &mut Vec<String>) {
    let small = manifest
        .subject_repos
        .iter()
        .filter(|repo| repo.size_class == SizeClass::Small)
        .count();
    let large = manifest
        .subject_repos
        .iter()
        .filter(|repo| repo.size_class == SizeClass::Large)
        .count();
    if manifest.subject_repos.len() != 2 || small != 1 || large != 1 {
        violations.push(format!(
            "[rule 1: size classes] subject_repos must contain exactly one small and one large repo, found {small} small and {large} large ({} total)",
            manifest.subject_repos.len()
        ));
    }
}

/// A commit SHA must be exactly 40 hex characters.
fn is_full_sha(commit: &str) -> bool {
    commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())
}

fn check_commits(manifest: &Manifest, violations: &mut Vec<String>) {
    for repo in &manifest.subject_repos {
        if !is_full_sha(&repo.commit) {
            violations.push(format!(
                "[rule 2: full SHA commit] subject_repos entry {:?} has commit {:?}, which is not a full 40-hex SHA",
                repo.name, repo.commit
            ));
        }
    }
}

fn check_container_images(manifest: &Manifest, violations: &mut Vec<String>) {
    for image_ref in &manifest.container_image_refs {
        let digest_ok = image_ref.split_once("@sha256:").is_some_and(|(_, digest)| {
            digest.len() == 64
                && digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        });
        if !digest_ok {
            violations.push(format!(
                "[rule 3: digest-pinned image] container_image_refs entry {image_ref:?} is not digest-pinned (missing or malformed @sha256:<64 lowercase hex>)"
            ));
        }
    }
}

fn check_prompt_hashes(manifest: &Manifest, repo_root: &Path, violations: &mut Vec<String>) {
    for prompt in &manifest.prompt_hashes {
        let resolved = repo_root.join(&prompt.path);
        if !resolved.is_file() {
            violations.push(format!(
                "[rule 4: prompt hash] prompt_hashes entry {:?} does not exist on disk at {}",
                prompt.path,
                resolved.display()
            ));
            continue;
        }
        match agent_bench::blake3_file_hash(&resolved) {
            Ok(actual) if actual == prompt.blake3 => {}
            Ok(actual) => {
                violations.push(format!(
                    "[rule 4: prompt hash] prompt_hashes entry {:?} has blake3 {:?} but the file on disk hashes to {actual:?}",
                    prompt.path, prompt.blake3
                ));
            }
            Err(err) => {
                violations.push(format!(
                    "[rule 4: prompt hash] prompt_hashes entry {:?} could not be hashed: {err:#}",
                    prompt.path
                ));
            }
        }
    }
}

fn check_question_set_hash(manifest: &Manifest, violations: &mut Vec<String>) {
    let hash = &manifest.question_set_hash;
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        violations.push(format!(
            "[rule 5: question_set_hash] question_set_hash {hash:?} is not a non-empty 64-hex string"
        ));
    }
}

fn check_results_dir(manifest: &Manifest, violations: &mut Vec<String>) {
    let results_dir = &manifest.results_dir;
    if results_dir.is_absolute() {
        violations.push(format!(
            "[rule 6: results_dir] results_dir {:?} must be a relative path, not absolute",
            results_dir
        ));
        return;
    }
    if results_dir
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        violations.push(format!(
            "[rule 6: results_dir] results_dir {:?} must not contain '..' traversal",
            results_dir
        ));
        return;
    }
    if !results_dir.starts_with(RESULTS_DIR_PREFIX) {
        violations.push(format!(
            "[rule 6: results_dir] results_dir {:?} must be relative under {RESULTS_DIR_PREFIX}",
            results_dir
        ));
    }
}

/// `checkout_root` must be a relative path with no `..` traversal.
fn check_checkout_root(manifest: &Manifest, violations: &mut Vec<String>) {
    let checkout_root = &manifest.checkout_root;
    if checkout_root.is_absolute() {
        violations.push(format!(
            "[rule 7: checkout_root] checkout_root {:?} must be a relative path, not absolute",
            checkout_root
        ));
        return;
    }
    if checkout_root
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        violations.push(format!(
            "[rule 7: checkout_root] checkout_root {:?} must not contain '..' traversal",
            checkout_root
        ));
    }
}
