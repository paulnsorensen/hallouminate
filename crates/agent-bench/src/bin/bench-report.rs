//! Stats/report generator for the wiki-grounding benchmark pilot.
//!
//! Joins `sessions.jsonl` (token usage, transcripts) with `grades.jsonl`
//! (pass/fail per session) against the question set, and emits a
//! `BenchReport` as `report.json` plus a human-readable `report.md` in
//! `--out-dir`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use agent_bench::{
    Arm, ArmSummary, BenchReport, GradeRecord, PairedCi, Question, QuestionArmStat, QuestionRow,
    QuestionSet, QuestionTag, SessionRecord, TokenUsage, load_json, read_jsonl,
};

#[derive(Debug, Parser)]
struct Cli {
    /// Path to sessions.jsonl (SessionRecord per line).
    #[arg(long)]
    sessions: PathBuf,
    /// Path to grades.jsonl (GradeRecord per line).
    #[arg(long)]
    grades: PathBuf,
    /// Path to the QuestionSet JSON file.
    #[arg(long)]
    questions: PathBuf,
    /// Directory to write report.json and report.md into.
    #[arg(long)]
    out_dir: PathBuf,
    /// Bootstrap resamples for the paired confidence interval. Rejected at
    /// parse time if zero: an empty resample pool reaches `percentile` with
    /// an empty slice, where `sorted.len() - 1` underflows.
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..))]
    resamples: u32,
    /// Seed for the bootstrap RNG, so runs are reproducible.
    #[arg(long)]
    seed: u64,
    /// Confidence level for the paired bootstrap interval.
    #[arg(long, default_value_t = 0.95)]
    confidence: f64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(&cli)
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    let question_set: QuestionSet = load_json(&cli.questions)?;
    let questions_by_id: HashMap<&str, &Question> = question_set
        .questions
        .iter()
        .map(|q| (q.id.as_str(), q))
        .collect();

    let sessions: Vec<SessionRecord> = read_jsonl(&cli.sessions)?;
    let sessions_by_key: HashMap<(String, Arm, u32), &SessionRecord> = sessions
        .iter()
        .map(|s| ((s.question_id.clone(), s.arm, s.run_index), s))
        .collect();
    let grades: Vec<GradeRecord> = read_jsonl(&cli.grades)?;

    // A session that failed to spawn, exited non-zero, or returned
    // unparseable JSON is recorded with a non-zero `exit_status` and
    // `TokenUsage::default()`. Counting it would make it a FREE FAILURE: it
    // would enter `n` for pass@k while contributing zero tokens, deflating
    // both the arm's pass rate and its measured cost, and polluting the
    // `tokens_to_correct_answer` denominator with runs that never spent a
    // token. That is directionally dangerous rather than merely noisy --
    // the wiki arm carries an extra MCP server and therefore an extra
    // failure mode, so its startup failures would systematically flatter
    // its own cost figure. Failed sessions are therefore excluded from `n`
    // entirely and reported as a separate per-arm count in `report.md`, so
    // they can never be silently absorbed into a pass rate.
    let failed_sessions = count_failed_sessions(&sessions);

    // Joined by grade, since a grade always implies the session it graded.
    // A session with no matching grade (not yet judged) is simply not part
    // of the report; a grade with no matching session is a data-integrity
    // error and aborts the report rather than being silently skipped.
    let mut per_question: BTreeMap<String, BTreeMap<Arm, Vec<(bool, TokenUsage)>>> =
        BTreeMap::new();
    for grade in &grades {
        let key = (grade.question_id.clone(), grade.arm, grade.run_index);
        let session = sessions_by_key.get(&key).ok_or_else(|| {
            anyhow::anyhow!(
                "grade for question {:?} (arm {:?}, run {}) has no matching session",
                grade.question_id,
                grade.arm,
                grade.run_index
            )
        })?;
        if session.exit_status != 0 {
            continue;
        }
        per_question
            .entry(grade.question_id.clone())
            .or_default()
            .entry(grade.arm)
            .or_default()
            .push((grade.pass, session.usage));
    }

    let mut question_rows = Vec::with_capacity(per_question.len());
    // (arm, tag) -> aggregated (n, c) pairs and totals; tag = None is the
    // all-tags aggregate. BTreeMap keys sort Arm ascending, then None
    // before Some(tag) ascending, which is exactly arm-then-aggregate-
    // then-per-tag report order.
    let mut groups: BTreeMap<(Arm, Option<QuestionTag>), GroupAgg> = BTreeMap::new();

    for (question_id, by_arm) in &per_question {
        let question = questions_by_id.get(question_id.as_str()).ok_or_else(|| {
            anyhow::anyhow!("graded question {question_id:?} is not in the question set")
        })?;

        let mut row_per_arm = BTreeMap::new();
        for (&arm, runs) in by_arm {
            let stat = question_arm_stat(runs);
            groups.entry((arm, None)).or_default().add(&stat);
            groups
                .entry((arm, Some(question.tag)))
                .or_default()
                .add(&stat);
            row_per_arm.insert(arm, stat);
        }

        question_rows.push(QuestionRow {
            question_id: question_id.clone(),
            repo: question.repo.clone(),
            tag: question.tag,
            per_arm: row_per_arm,
        });
    }

    let per_arm: Vec<ArmSummary> = groups
        .into_iter()
        .map(|((arm, tag), group)| ArmSummary {
            arm,
            tag,
            questions: group.pairs.len(),
            runs: group.runs,
            pass_at_k: aggregate_pass_at_k(&group.pairs),
            pass_pow_k: aggregate_pass_pow_k(&group.pairs),
            usage: group.usage,
            tokens_to_correct_answer: if group.passes > 0 {
                Some(group.usage.total() as f64 / group.passes as f64)
            } else {
                None
            },
        })
        .collect();

    let population = paired_population(&question_rows);
    let paired_ci = paired_bootstrap(
        "pass_rate_diff_wiki_minus_baseline",
        &population.correctness_diffs,
        cli.resamples,
        cli.seed,
        cli.confidence,
    );
    // The cost half of the (cost, correctness) pair the spec names. It is
    // rendered into report.md but NOT into report.json: `BenchReport` holds
    // a single `paired_ci` field, and extending that shared model type is
    // outside this change's ownership (see the handoff note).
    let cost_ci = paired_bootstrap(
        "mean_tokens_per_run_diff_wiki_minus_baseline",
        &population.cost_diffs,
        cli.resamples,
        cli.seed,
        cli.confidence,
    );

    let report = BenchReport {
        per_arm,
        per_question: question_rows,
        paired_ci,
    };

    fs::create_dir_all(&cli.out_dir)
        .with_context(|| format!("creating out-dir {}", cli.out_dir.display()))?;
    let report_json_path = cli.out_dir.join("report.json");
    let file = fs::File::create(&report_json_path)
        .with_context(|| format!("creating {}", report_json_path.display()))?;
    serde_json::to_writer_pretty(file, &report)
        .with_context(|| format!("writing {}", report_json_path.display()))?;

    let report_md_path = cli.out_dir.join("report.md");
    fs::write(
        &report_md_path,
        render_markdown(&report, &failed_sessions, &cost_ci, &population),
    )
    .with_context(|| format!("writing {}", report_md_path.display()))?;

    Ok(())
}

/// Per-(arm, tag) accumulator: `pairs` holds one `(n, c)` per question in
/// the group (the input to the pass@k/pass^k estimators), alongside totals
/// used for the arm-level token-usage and tokens-to-correct-answer figures.
#[derive(Debug, Default)]
struct GroupAgg {
    pairs: Vec<(usize, usize)>,
    usage: TokenUsage,
    passes: usize,
    runs: usize,
}

impl GroupAgg {
    fn add(&mut self, stat: &QuestionArmStat) {
        self.pairs.push((stat.runs, stat.passes));
        self.usage += stat.usage;
        self.passes += stat.passes;
        self.runs += stat.runs;
    }
}

/// Count sessions that did not exit cleanly, per arm. Counted over ALL
/// sessions, graded or not: a session that never produced an answer may
/// never have been judged at all, and it must still be visible.
fn count_failed_sessions(sessions: &[SessionRecord]) -> BTreeMap<Arm, usize> {
    let mut failed = BTreeMap::new();
    for session in sessions {
        if session.exit_status != 0 {
            *failed.entry(session.arm).or_insert(0) += 1;
        }
    }
    failed
}

fn question_arm_stat(runs: &[(bool, TokenUsage)]) -> QuestionArmStat {
    let passes = runs.iter().filter(|(p, _)| *p).count();
    let usage: TokenUsage = runs.iter().map(|(_, u)| *u).sum();
    QuestionArmStat {
        runs: runs.len(),
        passes,
        usage,
        // A question with zero correct runs in this arm has no defined
        // "tokens spent per correct answer" — represented as `None`, not 0
        // or silently omitted, so the hardest (never-solved) questions
        // stay visible in the report rather than dropping out of the
        // average.
        tokens_to_correct_answer: if passes > 0 {
            Some(usage.total() as f64 / passes as f64)
        } else {
            None
        },
    }
}

/// Unbiased pass@k estimator (Chen et al. 2021, "Evaluating Large Language
/// Models Trained on Code"): the probability that at least one of `k`
/// independently sampled runs passes, given `n` observed runs of which `c`
/// passed.
///
///   pass@k = 1 - C(n-c, k) / C(n, k)
///
/// Computed as the falling-ratio product `C(a,k)/C(b,k) = prod_{i=0}^{k-1}
/// (a-i)/(b-i)` rather than materializing binomial coefficients, so it
/// stays numerically stable for large `n`/`k` instead of overflowing `u64`.
/// Callers must guard `k <= n`.
fn pass_at_k(n: usize, c: usize, k: usize) -> f64 {
    debug_assert!(k <= n, "pass_at_k: k={k} exceeds n={n}");
    let fail = n - c;
    if k > fail {
        return 1.0;
    }
    let mut ratio = 1.0;
    for i in 0..k {
        ratio *= (fail - i) as f64 / (n - i) as f64;
    }
    1.0 - ratio
}

/// Unbiased pass^k estimator: the probability that ALL of `k` independently
/// sampled runs pass, given `n` observed runs of which `c` passed.
///
///   pass^k = C(c, k) / C(n, k)
///
/// Same falling-ratio-product technique as `pass_at_k`. Callers must guard
/// `k <= n`.
fn pass_pow_k(n: usize, c: usize, k: usize) -> f64 {
    debug_assert!(k <= n, "pass_pow_k: k={k} exceeds n={n}");
    if k > c {
        return 0.0;
    }
    let mut ratio = 1.0;
    for i in 0..k {
        ratio *= (c - i) as f64 / (n - i) as f64;
    }
    ratio
}

/// Average the per-question pass@k estimator across `pairs` (one `(n, c)`
/// per question), for every `k` in `1..=min(n)` across the group. Using
/// `min(n)` rather than `max(n)` means every reported `k` is averaged over
/// ALL questions in the group -- the same population `ArmSummary.questions`
/// reports -- instead of a k-dependent subset that silently shrinks as k
/// grows while `questions` keeps reporting the full count. The tradeoff:
/// pass@k above the hardest-judged question's run count is not reported at
/// all, rather than reported over a smaller, undisclosed population; that
/// cutoff is visible directly as the highest key in the map.
fn aggregate_pass_at_k(pairs: &[(usize, usize)]) -> BTreeMap<u32, f64> {
    aggregate(pairs, pass_at_k)
}

fn aggregate_pass_pow_k(pairs: &[(usize, usize)]) -> BTreeMap<u32, f64> {
    aggregate(pairs, pass_pow_k)
}

fn aggregate(
    pairs: &[(usize, usize)],
    estimator: fn(usize, usize, usize) -> f64,
) -> BTreeMap<u32, f64> {
    let min_n = pairs.iter().map(|&(n, _)| n).min().unwrap_or(0);
    let mut map = BTreeMap::new();
    for k in 1..=min_n {
        let sum: f64 = pairs.iter().map(|&(n, c)| estimator(n, c, k)).sum();
        map.insert(k as u32, sum / pairs.len() as f64);
    }
    map
}

/// Paired bootstrap CI for the between-arm pass-rate difference
/// (Wiki minus Baseline), resampling QUESTIONS (not runs) as the paired
/// unit: each resample draws questions with replacement and takes both
/// arms' pass rates for each drawn question, so within-question
/// correlation between the arms is preserved rather than washed out.
///
/// Only questions with at least one run in both arms contribute a
/// (cost, correctness) pair; correctness is the per-question pass rate.
/// If no question has data in both arms, the CI collapses to a zero-width
/// point at 0.0 rather than panicking on an empty resample pool.
/// The paired population behind every CI: one (correctness, cost) pair per
/// question that has runs in BOTH arms, plus the ids of the questions that
/// could not be paired.
///
/// Dropped ids are carried rather than discarded so `report.md` can name
/// them: a CI over 3 questions and a CI over 24 otherwise print identically.
#[derive(Debug, Default)]
struct PairedPopulation {
    /// Per-question wiki-minus-baseline pass-rate difference.
    correctness_diffs: Vec<f64>,
    /// Per-question wiki-minus-baseline mean tokens per run.
    ///
    /// Mean tokens per run, not `tokens_to_correct_answer`: the latter is
    /// undefined for a question with zero passes in an arm, so pairing on it
    /// would silently drop exactly the hardest questions from the cost CI's
    /// population -- the same invisible-population defect this struct exists
    /// to close.
    cost_diffs: Vec<f64>,
    /// Question ids present in the report but missing runs in an arm.
    dropped: Vec<String>,
}

fn paired_population(rows: &[QuestionRow]) -> PairedPopulation {
    let mut population = PairedPopulation::default();
    for row in rows {
        let paired = row
            .per_arm
            .get(&Arm::Wiki)
            .zip(row.per_arm.get(&Arm::Baseline))
            .filter(|(wiki, baseline)| wiki.runs > 0 && baseline.runs > 0);
        let Some((wiki, baseline)) = paired else {
            population.dropped.push(row.question_id.clone());
            continue;
        };
        population.correctness_diffs.push(
            wiki.passes as f64 / wiki.runs as f64 - baseline.passes as f64 / baseline.runs as f64,
        );
        population.cost_diffs.push(
            wiki.usage.total() as f64 / wiki.runs as f64
                - baseline.usage.total() as f64 / baseline.runs as f64,
        );
    }
    population
}

fn paired_bootstrap(
    metric: &str,
    diffs: &[f64],
    resamples: u32,
    seed: u64,
    confidence: f64,
) -> PairedCi {
    let point_estimate = if diffs.is_empty() {
        0.0
    } else {
        diffs.iter().sum::<f64>() / diffs.len() as f64
    };

    let (lower, upper) = if diffs.is_empty() {
        (0.0, 0.0)
    } else {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut resample_means = Vec::with_capacity(resamples as usize);
        for _ in 0..resamples {
            let mut sum = 0.0;
            for _ in 0..diffs.len() {
                let idx = rng.random_range(0..diffs.len());
                sum += diffs[idx];
            }
            resample_means.push(sum / diffs.len() as f64);
        }
        resample_means.sort_by(|a, b| a.partial_cmp(b).expect("bootstrap means are never NaN"));
        let alpha = (1.0 - confidence) / 2.0;
        (
            percentile(&resample_means, alpha),
            percentile(&resample_means, 1.0 - alpha),
        )
    };

    PairedCi {
        metric: metric.to_string(),
        point_estimate,
        lower,
        upper,
        confidence,
        resamples,
        seed,
    }
}

/// Linear-interpolated percentile of a pre-sorted slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = p * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = pos - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

fn render_markdown(
    report: &BenchReport,
    failed_sessions: &BTreeMap<Arm, usize>,
    cost_ci: &PairedCi,
    population: &PairedPopulation,
) -> String {
    let mut out = String::new();
    out.push_str("# Wiki-Grounding Benchmark Report\n\n");

    // Printed before anything else: a failed session is excluded from every
    // figure below, so the count of what was dropped has to be as visible
    // as the figures it was dropped from.
    out.push_str("## Failed sessions (excluded from every figure below)\n\n");
    if failed_sessions.is_empty() {
        out.push_str("- none\n");
    } else {
        for (arm, count) in failed_sessions {
            out.push_str(&format!("- {arm:?}: {count}\n"));
        }
    }
    out.push('\n');

    out.push_str("## Per-arm summary\n\n");
    for summary in &report.per_arm {
        let tag_label = match summary.tag {
            Some(tag) => format!("{tag:?}"),
            None => "all".to_string(),
        };
        out.push_str(&format!("### {:?} / {tag_label}\n\n", summary.arm));
        out.push_str(&format!("- questions: {}\n", summary.questions));
        out.push_str(&format!("- runs: {}\n", summary.runs));
        for (k, v) in &summary.pass_at_k {
            out.push_str(&format!("- pass@{k}: {v:.4}\n"));
        }
        for (k, v) in &summary.pass_pow_k {
            out.push_str(&format!("- pass^{k}: {v:.4}\n"));
        }
        match summary.tokens_to_correct_answer {
            Some(t) => out.push_str(&format!("- tokens_to_correct_answer: {t:.2}\n")),
            None => out.push_str("- tokens_to_correct_answer: none (zero correct runs)\n"),
        }
        // Four-field token decomposition, printed explicitly so cache reads
        // are visible rather than hidden inside a single total.
        out.push_str("- token usage:\n");
        out.push_str(&format!(
            "  - input_tokens: {}\n",
            summary.usage.input_tokens
        ));
        out.push_str(&format!(
            "  - output_tokens: {}\n",
            summary.usage.output_tokens
        ));
        out.push_str(&format!(
            "  - cache_read_input_tokens: {}\n",
            summary.usage.cache_read_input_tokens
        ));
        out.push_str(&format!(
            "  - cache_creation_input_tokens: {}\n",
            summary.usage.cache_creation_input_tokens
        ));
        out.push_str(&format!("  - total: {}\n", summary.usage.total()));
        out.push('\n');
    }

    out.push_str("## Per-question detail\n\n");
    for row in &report.per_question {
        out.push_str(&format!(
            "### {} ({}, {:?})\n\n",
            row.question_id, row.repo, row.tag
        ));
        // Arm has exactly two variants (Wiki, Baseline), so `len() > 1` ==
        // `== 2`: only flag when BOTH arms are present and both zero, not a
        // single-arm question (which would otherwise vacuously satisfy
        // `.values().all(...)` on one element).
        let flagged = row.per_arm.len() > 1 && row.per_arm.values().all(|s| s.passes == 0);
        if flagged {
            out.push_str("**FLAGGED: zero passes in both arms**\n\n");
        }
        for (arm, stat) in &row.per_arm {
            let tokens = stat
                .tokens_to_correct_answer
                .map(|t| format!("{t:.2}"))
                .unwrap_or_else(|| "none".to_string());
            out.push_str(&format!(
                "- {arm:?}: {}/{} passed, tokens_to_correct_answer={tokens}\n",
                stat.passes, stat.runs
            ));
        }
        out.push('\n');
    }

    out.push_str("## Paired bootstrap CI\n\n");
    out.push_str(&format!("- metric: {}\n", report.paired_ci.metric));
    out.push_str(&format!(
        "- point_estimate: {:.4}\n",
        report.paired_ci.point_estimate
    ));
    out.push_str(&format!(
        "- {:.0}% CI: [{:.4}, {:.4}]\n",
        report.paired_ci.confidence * 100.0,
        report.paired_ci.lower,
        report.paired_ci.upper
    ));
    out.push_str(&format!("- resamples: {}\n", report.paired_ci.resamples));
    out.push_str(&format!("- seed: {}\n", report.paired_ci.seed));
    out.push_str(&format!(
        "- paired questions: {}\n",
        population.correctness_diffs.len()
    ));
    out.push('\n');

    // The cost CI lives in report.md only. `BenchReport` carries a single
    // `paired_ci` field, and that shared model type is consumed by other
    // binaries, so a second interval cannot enter report.json without
    // changing it. Tracked as a follow-up.
    out.push_str("## Paired bootstrap CI (token cost)\n\n");
    out.push_str(&format!("- metric: {}\n", cost_ci.metric));
    out.push_str(&format!(
        "- point_estimate: {:.4}\n",
        cost_ci.point_estimate
    ));
    out.push_str(&format!(
        "- {:.0}% CI: [{:.4}, {:.4}]\n",
        cost_ci.confidence * 100.0,
        cost_ci.lower,
        cost_ci.upper
    ));
    out.push_str(&format!("- resamples: {}\n", cost_ci.resamples));
    out.push_str(&format!("- seed: {}\n", cost_ci.seed));
    out.push_str(&format!(
        "- paired questions: {}\n",
        population.cost_diffs.len()
    ));
    out.push('\n');

    // Named, never silently discarded: a CI computed over a shrunken
    // population reads as a stronger result than it is, so the questions
    // that fell out have to appear next to the interval they are missing
    // from.
    out.push_str("## Questions dropped from the paired CIs\n\n");
    if population.dropped.is_empty() {
        out.push_str("- none\n");
    } else {
        for id in &population.dropped {
            out.push_str(&format!("- {id} (missing runs in an arm)\n"));
        }
    }

    out
}
