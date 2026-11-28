//! Validates a benchmark `QuestionSet` against the pilot's authoring rules
//! (id uniqueness and prefixing, repo/tag balance floors, abstention gold
//! answers, non-empty fields) and, optionally, its freeze hash against a
//! manifest.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_bench::{Manifest, Question, QuestionSet, QuestionTag};
use anyhow::Context;
use clap::Parser;

/// Minimum total question count across all repos.
const MIN_QUESTIONS: usize = 24;
/// Minimum question count contributed by any single repo.
const MIN_QUESTIONS_PER_REPO: usize = 12;
/// Minimum `abstention`-tagged questions per repo.
const MIN_ABSTENTION_PER_REPO: usize = 1;
/// Minimum `wiki-only`-tagged questions per repo.
const MIN_WIKI_ONLY_PER_REPO: usize = 3;
/// Minimum `greppable`-tagged questions per repo.
const MIN_GREPPABLE_PER_REPO: usize = 3;

/// Id-prefix convention: a question's `id` must start with its own `repo`
/// value followed by this separator, e.g. repo `"foo"` requires ids like
/// `"foo-001"`.
const ID_REPO_SEPARATOR: &str = "-";

/// An `abstention` question's `gold_answer` must contain this phrase
/// (case-insensitive), asserting the fact is not recorded rather than
/// leaving abstention correctness to a fuzzy heuristic.
const ABSTENTION_GOLD_ANSWER_MARKER: &str = "not recorded";

#[derive(Parser)]
#[command(about = "Validate a benchmark QuestionSet against the pilot's authoring rules")]
struct Args {
    /// Path to the QuestionSet JSON file.
    #[arg(long)]
    questions: PathBuf,
    /// Path to the Manifest TOML file. Required unless `--print-hash` is
    /// the only requested action.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Recompute the questions file's blake3 hash and fail loudly if it
    /// diverges from `manifest.question_set_hash`.
    #[arg(long = "check-freeze")]
    check_freeze: bool,
    /// Print the questions file's blake3 hash (exactly, for recording in a
    /// manifest's `question_set_hash`) and exit without running the other
    /// validation rules.
    #[arg(long = "print-hash")]
    print_hash: bool,
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

/// Runs the requested action. `Ok(true)` means success (exit 0); `Ok(false)`
/// means validation ran and found violations, already printed to stderr
/// (exit 1); `Err` means a hard failure (bad path, malformed file).
fn run(args: &Args) -> anyhow::Result<bool> {
    if args.print_hash {
        let hash = agent_bench::blake3_file_hash(&args.questions)
            .with_context(|| format!("hashing questions file at {}", args.questions.display()))?;
        println!("{hash}");
        return Ok(true);
    }

    let manifest_path = args
        .manifest
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--manifest is required unless --print-hash is set"))?;
    let manifest: Manifest = agent_bench::load_toml(manifest_path)?;
    let question_set: QuestionSet = agent_bench::load_json(&args.questions)?;

    let mut violations = validate(&question_set, &manifest);

    if args.check_freeze {
        let actual_hash = agent_bench::blake3_file_hash(&args.questions)
            .with_context(|| format!("hashing questions file at {}", args.questions.display()))?;
        if actual_hash != manifest.question_set_hash {
            violations.push(format!(
                "[freeze] questions file hash {actual_hash} does not match manifest.question_set_hash {}",
                manifest.question_set_hash
            ));
        }
    }

    if violations.is_empty() {
        let repo_count = question_set
            .questions
            .iter()
            .map(|q| q.repo.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        println!(
            "OK: {} questions across {} repos passed all validation rules",
            question_set.questions.len(),
            repo_count
        );
        Ok(true)
    } else {
        for violation in &violations {
            eprintln!("{violation}");
        }
        Ok(false)
    }
}

/// Runs every authoring rule against `question_set` and returns every
/// violation found, each naming the failing rule and the offending question
/// id (or repo, for aggregate rules).
fn validate(question_set: &QuestionSet, manifest: &Manifest) -> Vec<String> {
    let mut violations = Vec::new();

    check_duplicate_ids(&question_set.questions, &mut violations);
    check_id_prefix(&question_set.questions, &mut violations);
    check_known_repo(&question_set.questions, manifest, &mut violations);
    check_non_empty_fields(&question_set.questions, &mut violations);
    check_abstention_gold_answer(&question_set.questions, &mut violations);

    let by_repo = group_by_repo(&question_set.questions);
    check_total_count(&question_set.questions, &mut violations);
    check_per_repo_count(&by_repo, &mut violations);
    check_per_repo_tag_balance(&by_repo, &mut violations);

    violations
}

fn group_by_repo(questions: &[Question]) -> BTreeMap<&str, Vec<&Question>> {
    let mut by_repo: BTreeMap<&str, Vec<&Question>> = BTreeMap::new();
    for question in questions {
        by_repo
            .entry(question.repo.as_str())
            .or_default()
            .push(question);
    }
    by_repo
}

fn check_duplicate_ids(questions: &[Question], violations: &mut Vec<String>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for question in questions {
        *seen.entry(question.id.as_str()).or_insert(0) += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            violations.push(format!(
                "[rule 1: unique ids] id {id:?} appears {count} times"
            ));
        }
    }
}

fn check_id_prefix(questions: &[Question], violations: &mut Vec<String>) {
    for question in questions {
        let expected_prefix = format!("{}{ID_REPO_SEPARATOR}", question.repo);
        if !question.id.starts_with(&expected_prefix) {
            violations.push(format!(
                "[rule 2: id prefix] question {:?} has repo {:?} but its id does not start with {:?}",
                question.id, question.repo, expected_prefix
            ));
        }
    }
}

fn check_known_repo(questions: &[Question], manifest: &Manifest, violations: &mut Vec<String>) {
    for question in questions {
        if !manifest
            .subject_repos
            .iter()
            .any(|repo| repo.name == question.repo)
        {
            violations.push(format!(
                "[rule 3: known repo] question {:?} references repo {:?}, which is not in manifest.subject_repos",
                question.id, question.repo
            ));
        }
    }
}

fn check_total_count(questions: &[Question], violations: &mut Vec<String>) {
    if questions.len() < MIN_QUESTIONS {
        violations.push(format!(
            "[rule 4: total count] total question count {} is below MIN_QUESTIONS ({MIN_QUESTIONS})",
            questions.len()
        ));
    }
}

fn check_per_repo_count(by_repo: &BTreeMap<&str, Vec<&Question>>, violations: &mut Vec<String>) {
    for (repo, questions) in by_repo {
        if questions.len() < MIN_QUESTIONS_PER_REPO {
            violations.push(format!(
                "[rule 4: per-repo count] repo {repo:?} has {} questions, below the per-repo floor of {MIN_QUESTIONS_PER_REPO}",
                questions.len()
            ));
        }
    }
}

fn check_per_repo_tag_balance(
    by_repo: &BTreeMap<&str, Vec<&Question>>,
    violations: &mut Vec<String>,
) {
    for (repo, questions) in by_repo {
        let count = |tag: QuestionTag| questions.iter().filter(|q| q.tag == tag).count();
        let abstention = count(QuestionTag::Abstention);
        let wiki_only = count(QuestionTag::WikiOnly);
        let greppable = count(QuestionTag::Greppable);

        if abstention < MIN_ABSTENTION_PER_REPO {
            violations.push(format!(
                "[rule 5: tag balance] repo {repo:?} has {abstention} abstention questions, below the required floor of {MIN_ABSTENTION_PER_REPO}"
            ));
        }
        if wiki_only < MIN_WIKI_ONLY_PER_REPO {
            violations.push(format!(
                "[rule 5: tag balance] repo {repo:?} has {wiki_only} wiki-only questions, below the required floor of {MIN_WIKI_ONLY_PER_REPO}"
            ));
        }
        if greppable < MIN_GREPPABLE_PER_REPO {
            violations.push(format!(
                "[rule 5: tag balance] repo {repo:?} has {greppable} greppable questions, below the required floor of {MIN_GREPPABLE_PER_REPO}"
            ));
        }
    }
}

fn check_abstention_gold_answer(questions: &[Question], violations: &mut Vec<String>) {
    for question in questions {
        if question.tag == QuestionTag::Abstention
            && !question
                .gold_answer
                .to_lowercase()
                .contains(ABSTENTION_GOLD_ANSWER_MARKER)
        {
            violations.push(format!(
                "[rule 6: abstention gold answer] question {:?} is tagged abstention but its gold_answer does not contain {:?}",
                question.id, ABSTENTION_GOLD_ANSWER_MARKER
            ));
        }
    }
}

fn check_non_empty_fields(questions: &[Question], violations: &mut Vec<String>) {
    for question in questions {
        if question.gold_answer.trim().is_empty() {
            violations.push(format!(
                "[rule 7: non-empty fields] question {:?} has an empty or whitespace-only gold_answer",
                question.id
            ));
        }
        if question.rubric_notes.trim().is_empty() {
            violations.push(format!(
                "[rule 7: non-empty fields] question {:?} has an empty or whitespace-only rubric_notes",
                question.id
            ));
        }
    }
}
