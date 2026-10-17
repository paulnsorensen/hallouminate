//! Frozen data model for the wiki-grounding benchmark pilot.
//!
//! Field names and serde renames here are the contract shared by every
//! sibling curd (session recorder, judge harness, stats/report). Do not
//! rename fields or change serde representations without updating all
//! consumers.

use std::collections::BTreeMap;
use std::ops::{Add, AddAssign};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which arm of the pilot a session/grade belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Wiki,
    Baseline,
}

/// Question difficulty/category tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionTag {
    WikiOnly,
    Greppable,
    Abstention,
}

/// Subject repository size classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeClass {
    Small,
    Large,
}

/// Token accounting for a single agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl TokenUsage {
    /// Sum of all four token counters.
    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

impl Add for TokenUsage {
    type Output = TokenUsage;

    fn add(self, rhs: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens + rhs.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens
                + rhs.cache_creation_input_tokens,
        }
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: TokenUsage) {
        *self = *self + rhs;
    }
}

impl std::iter::Sum<TokenUsage> for TokenUsage {
    fn sum<I: Iterator<Item = TokenUsage>>(iter: I) -> TokenUsage {
        iter.fold(TokenUsage::default(), Add::add)
    }
}

/// A single benchmark question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub repo: String,
    pub id: String,
    pub tag: QuestionTag,
    pub question: String,
    pub gold_answer: String,
    pub rubric_notes: String,
}

/// A collection of benchmark questions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionSet {
    pub questions: Vec<Question>,
}

/// A recorded agent session: one (question, arm, run) trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub question_id: String,
    pub repo: String,
    pub arm: Arm,
    pub run_index: u32,
    pub answer_text: String,
    pub usage: TokenUsage,
    pub transcript_path: PathBuf,
    pub wall_ms: u64,
    pub exit_status: i32,
}

/// A judge's grade for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradeRecord {
    pub question_id: String,
    pub arm: Arm,
    pub run_index: u32,
    /// 0..=5, validated at construction.
    pub score: u8,
    pub pass: bool,
    pub judge_rationale: String,
}

impl GradeRecord {
    /// Build a validated `GradeRecord`, deriving `pass` from `score` and
    /// `threshold`. Rejects `score > 5`.
    pub fn grade(
        question_id: String,
        arm: Arm,
        run_index: u32,
        score: u8,
        threshold: u8,
        judge_rationale: String,
    ) -> anyhow::Result<GradeRecord> {
        if score > 5 {
            anyhow::bail!("GradeRecord score must be in 0..=5, got {score}");
        }
        Ok(GradeRecord {
            question_id,
            arm,
            run_index,
            score,
            pass: GradeRecord::passes(score, threshold),
            judge_rationale,
        })
    }

    /// Whether `score` meets `threshold`.
    pub fn passes(score: u8, threshold: u8) -> bool {
        score >= threshold
    }
}

/// A subject repository under test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubjectRepo {
    pub name: String,
    pub url: String,
    pub commit: String,
    pub size_class: SizeClass,
}

/// A hashed reference to a prompt file, for manifest provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptRef {
    pub path: PathBuf,
    pub blake3: String,
}

/// Subject and judge model identifiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelIds {
    pub subject: String,
    pub judge: String,
}

/// Provenance manifest for one benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub model_ids: ModelIds,
    pub claude_code_version: String,
    pub subject_repos: Vec<SubjectRepo>,
    pub prompt_hashes: Vec<PromptRef>,
    pub question_set_hash: String,
    pub container_image_refs: Vec<String>,
    pub results_dir: PathBuf,
    /// Directory (relative to the repo root) under which subject repo
    /// checkouts live, at `<checkout_root>/<subject_repo.name>`. Every
    /// checkout must exist and have its `HEAD` match the pinned `commit`;
    /// a missing or drifted checkout is a hard failure, never a warning.
    pub checkout_root: PathBuf,
}

/// Pass-rate and usage summary for one arm (optionally filtered to one tag).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmSummary {
    pub arm: Arm,
    /// `None` means all tags aggregated.
    pub tag: Option<QuestionTag>,
    pub questions: usize,
    pub runs: usize,
    pub pass_at_k: BTreeMap<u32, f64>,
    pub pass_pow_k: BTreeMap<u32, f64>,
    pub usage: TokenUsage,
    pub tokens_to_correct_answer: Option<f64>,
}

/// Per-arm stats for one question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionArmStat {
    pub runs: usize,
    pub passes: usize,
    pub usage: TokenUsage,
    pub tokens_to_correct_answer: Option<f64>,
}

/// One row of the per-question report table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRow {
    pub question_id: String,
    pub repo: String,
    pub tag: QuestionTag,
    pub per_arm: BTreeMap<Arm, QuestionArmStat>,
}

/// Paired bootstrap confidence interval for a single metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedCi {
    pub metric: String,
    pub point_estimate: f64,
    pub lower: f64,
    pub upper: f64,
    pub confidence: f64,
    pub resamples: u32,
    pub seed: u64,
}

/// Full benchmark report: arm summaries, per-question rows, and paired CI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    pub per_arm: Vec<ArmSummary>,
    pub per_question: Vec<QuestionRow>,
    pub paired_ci: PairedCi,
}
