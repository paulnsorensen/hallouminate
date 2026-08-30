//! Model-backed ground retrieval evaluation for issue #288.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use hallouminate_adapters::{Embedder, FastembedCrossencoder};
use hallouminate_config::{Config, EmbeddingsConfig, SearchConfig, StorageConfig};
use hallouminate_daemon::{
    DaemonRequest, DaemonRequestPayload, DaemonResponse, GroundRequest, GroundResult, IndexReport,
    IndexRequest, connect_at,
};
use hallouminate_domain::common::{CorpusConfig, expand_tilde};
use hallouminate_domain::corpus::{blake3_bytes, load_tokenizer, scan};
use hallouminate_domain::ground::{DocFile, GroundResponse, Warning};
use hallouminate_domain::indexer::{Format, HandlerRegistry, PrepareCtx};
use serde::{Deserialize, Serialize};
use text_splitter::{Characters, ChunkSizer};

#[path = "it/common/mod.rs"]
mod common;
use common::daemon::DaemonHarness;

const SCHEMA_VERSION: u32 = 4;
const CORPUS_NAME: &str = "eval-wiki";
const BASELINE_ID: &str = "fusion-without-rerank";
const JINA_ID: &str = "fusion-with-jina-reranker-v1-turbo-en";
const JINA_MODEL: &str = "jina-reranker-v1-turbo-en";
const CHUNK_BUDGET_TOKENS: usize = 384;
/// Floor on labelled queries the evaluation must report over.
const MIN_LABELLED_QUERIES: usize = 40;
const GROUND_RPC_GRACE_MS: u64 = 60_000;
const EVALUATION_RERANK_TIMEOUT_MS: u64 = 5_000;
const FOOTNOTE_INVERSION_ID: &str = "footnote-inversion";
const MODEL_LOAD_MARKER: &str = "cold model load failed";
const RERANK_TIMEOUT_MARKER: &str = "rerank timeout";
const CROSSENCODER_UNAVAILABLE_MARKER: &str = "crossencoder unavailable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmSpec<'a> {
    id: &'a str,
    model: Option<&'a str>,
}

const BASELINE_ARM: ArmSpec<'static> = ArmSpec {
    id: BASELINE_ID,
    model: None,
};
const JINA_ARM: ArmSpec<'static> = ArmSpec {
    id: JINA_ID,
    model: Some(JINA_MODEL),
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArmDescriptor {
    id: String,
    model: Option<String>,
}

impl ArmDescriptor {
    fn from_spec(spec: ArmSpec<'_>) -> Self {
        let model = spec.model.map(|model| model.to_string());
        Self {
            id: spec.id.to_string(),
            model,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EmbeddingConfiguration {
    model: String,
    quantized: bool,
    cache_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChunkIdentity {
    file: String,
    heading_path: Vec<String>,
    line_start: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelledQuery {
    id: String,
    query: String,
    expected_chunk: ChunkIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QualityMetrics {
    recall_at_5: f64,
    mrr: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LatencyMetrics {
    cold_load_ms: u64,
    warm_p50_ms: u64,
    warm_p95_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueryMeasurement {
    id: String,
    latency_ms: u64,
    rank: Option<usize>,
    expected_top: ChunkIdentity,
    actual_top: Option<ChunkIdentity>,
    top_chunk_pass: bool,
    rerank_completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_signal: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ArmMeasurement {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    quality: QualityMetrics,
    latency: LatencyMetrics,
    queries: Vec<QueryMeasurement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ChangeDisposition {
    Improved,
    Unchanged,
    Regressed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueryChange {
    id: String,
    baseline_rank: Option<usize>,
    candidate_rank: Option<usize>,
    rank_delta: Option<i64>,
    rank_disposition: ChangeDisposition,
    baseline_top: Option<ChunkIdentity>,
    candidate_top: Option<ChunkIdentity>,
    top_chunk_changed: bool,
    top_chunk_assertion: ChangeDisposition,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ChangeCounts {
    improved: usize,
    unchanged: usize,
    regressed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ArmComparison {
    baseline_id: String,
    candidate_id: String,
    counts: ChangeCounts,
    queries: Vec<QueryChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FailureDiagnostic {
    arm_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_id: Option<String>,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EvalArtifact {
    schema_version: u32,
    query_set_digest: String,
    embedding: EmbeddingConfiguration,
    ripgrep_version: String,
    requested_arms: Vec<ArmDescriptor>,
    measurements: Vec<ArmMeasurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison: Option<ArmComparison>,
    timeout_count: usize,
    timeouts: Vec<FailureDiagnostic>,
    model_load_failures: Vec<FailureDiagnostic>,
    errors: Vec<FailureDiagnostic>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_root() -> PathBuf {
    fs::canonicalize(repo_root().join("eval/fixtures/wiki"))
        .expect("canonicalize eval fixture root")
}

fn fixture_corpus() -> CorpusConfig {
    CorpusConfig {
        name: CORPUS_NAME.into(),
        paths: vec![fixture_root().to_string_lossy().into_owned()],
        globs: vec!["**/*.md".into()],
        ..Default::default()
    }
}

fn query_path() -> PathBuf {
    repo_root().join("eval/queries.json")
}

fn baseline_path() -> PathBuf {
    repo_root().join("eval/baseline.json")
}

fn measurement_output_path(output: &Path) -> PathBuf {
    if output.is_absolute() {
        output.to_path_buf()
    } else {
        repo_root().join(output)
    }
}

fn baseline_arms() -> Vec<ArmSpec<'static>> {
    vec![BASELINE_ARM]
}

fn diagnostic_arms() -> Vec<ArmSpec<'static>> {
    vec![BASELINE_ARM, JINA_ARM]
}

fn embedding_configuration() -> EmbeddingConfiguration {
    let embeddings = EmbeddingsConfig::default();
    EmbeddingConfiguration {
        model: embeddings.model,
        quantized: embeddings.quantized,
        cache_dir: embeddings.cache_dir,
    }
}

fn new_artifact(
    query_set_digest: String,
    ripgrep_version: String,
    arms: &[ArmSpec<'_>],
) -> EvalArtifact {
    let mut requested_arms = Vec::with_capacity(arms.len());
    for arm in arms {
        requested_arms.push(ArmDescriptor::from_spec(*arm));
    }
    EvalArtifact {
        schema_version: SCHEMA_VERSION,
        query_set_digest,
        embedding: embedding_configuration(),
        ripgrep_version,
        requested_arms,
        measurements: Vec::new(),
        comparison: None,
        timeout_count: 0,
        timeouts: Vec::new(),
        model_load_failures: Vec::new(),
        errors: Vec::new(),
    }
}

/// `rg --version`'s banner is one short line; anything far longer is a
/// wrapper or an unrelated binary, not provenance worth committing.
const MAX_RIPGREP_VERSION_LEN: usize = 200;

fn parse_rg_version_output(bin: &str, stdout: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(stdout)
        .with_context(|| format!("`{bin} --version` produced non-UTF8 output"))?;
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .with_context(|| format!("`{bin} --version` produced no output"))?;
    ensure!(
        first_line.starts_with("ripgrep "),
        "`{bin} --version` is not a ripgrep banner: {first_line}"
    );
    ensure!(
        first_line.len() <= MAX_RIPGREP_VERSION_LEN,
        "`{bin} --version` banner is {} bytes, over the {MAX_RIPGREP_VERSION_LEN}-byte cap",
        first_line.len()
    );
    Ok(first_line.to_string())
}

fn ripgrep_version() -> Result<String> {
    ripgrep_version_from_binary("rg")
}

fn ripgrep_version_from_binary(bin: &str) -> Result<String> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("run `{bin} --version` for eval provenance"))?;
    ensure!(
        output.status.success(),
        "`{bin} --version` exited non-zero ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_rg_version_output(bin, &output.stdout)
}

fn load_queries() -> Result<(Vec<LabelledQuery>, String)> {
    let path = query_path();
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let queries: Vec<LabelledQuery> =
        serde_json::from_slice(&bytes).context("parse eval/queries.json")?;
    validate_labels(&queries)?;
    Ok((queries, blake3::hash(&bytes).to_hex().to_string()))
}

fn validate_labels(queries: &[LabelledQuery]) -> Result<()> {
    ensure!(!queries.is_empty(), "eval query set is empty");
    let mut ids = BTreeSet::new();
    for query in queries {
        ensure!(!query.id.trim().is_empty(), "query id is empty");
        ensure!(
            ids.insert(query.id.as_str()),
            "duplicate query id {}",
            query.id
        );
        ensure!(
            !query.query.trim().is_empty(),
            "query {} has no text",
            query.id
        );
        ensure!(
            !query.expected_chunk.file.trim().is_empty(),
            "query {} has no expected file",
            query.id
        );
        ensure!(
            !query.expected_chunk.heading_path.is_empty(),
            "query {} has no expected heading path",
            query.id
        );
        ensure!(
            query.expected_chunk.line_start > 0,
            "query {} has invalid expected line_start",
            query.id
        );
        ensure!(
            fixture_root().join(&query.expected_chunk.file).is_file(),
            "query {} expects missing fixture {}",
            query.id,
            query.expected_chunk.file
        );
    }

    let required = [
        FOOTNOTE_INVERSION_ID,
        "worktree-isolation",
        "paraphrase-app-boundary",
        "lexical-distractor",
    ];
    for required_id in required {
        ensure!(
            ids.contains(required_id),
            "missing required query {required_id}"
        );
    }

    let mut footnote = None;
    for query in queries {
        if query.id == FOOTNOTE_INVERSION_ID {
            footnote = Some(query);
            break;
        }
    }
    let Some(footnote) = footnote else {
        return Err(anyhow::anyhow!("required footnote query disappeared"));
    };
    ensure!(footnote.expected_chunk.file == "architecture.md");
    ensure!(footnote.expected_chunk.heading_path == ["Architecture"]);
    ensure!(footnote.expected_chunk.line_start == 1);
    Ok(())
}

fn prepared_fixture_chunk_identities<S>(sizer: S) -> Result<Vec<ChunkIdentity>>
where
    S: ChunkSizer + Clone + Send + Sync + 'static,
{
    let corpus = fixture_corpus();
    let fixture = fixture_root();
    let registry = HandlerRegistry::new(sizer, CHUNK_BUDGET_TOKENS);
    let mut identities = Vec::new();
    for scanned in scan(&corpus)? {
        let path = scanned.file.as_path();
        let bytes = fs::read(path).with_context(|| format!("read fixture {}", path.display()))?;
        let Some(extension) = path.extension() else {
            return Err(anyhow::anyhow!(
                "fixture has no extension: {}",
                path.display()
            ));
        };
        let Some(extension) = extension.to_str() else {
            return Err(anyhow::anyhow!(
                "fixture extension is not UTF-8: {}",
                path.display()
            ));
        };
        ensure!(
            extension.eq_ignore_ascii_case("md"),
            "fixture is not markdown: {}",
            path.display()
        );
        let prepared = registry.handler(Format::Markdown).prepare(&PrepareCtx {
            corpus_key: &scanned.corpus_key,
            file: &scanned.file,
            mtime: scanned.mtime,
            bytes: &bytes,
            content_hash: blake3_bytes(&bytes),
            indexed_at_ms: 0,
        })?;
        let file = path
            .strip_prefix(&fixture)
            .with_context(|| format!("fixture path escaped root: {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        for chunk in prepared.chunks {
            identities.push(ChunkIdentity {
                file: file.clone(),
                heading_path: chunk.heading_path,
                line_start: u32::try_from(chunk.line_start).context("chunk line exceeds u32")?,
            });
        }
    }
    Ok(identities)
}

fn validate_expected_chunks(queries: &[LabelledQuery], prepared: &[ChunkIdentity]) -> Result<()> {
    for query in queries {
        ensure!(
            prepared.contains(&query.expected_chunk),
            "query {} expected chunk does not exist in prepared fixture: {:?}",
            query.id,
            query.expected_chunk
        );
    }
    Ok(())
}

fn validate_fixture_labels<S>(queries: &[LabelledQuery], sizer: S) -> Result<()>
where
    S: ChunkSizer + Clone + Send + Sync + 'static,
{
    let prepared = prepared_fixture_chunk_identities(sizer)?;
    validate_expected_chunks(queries, &prepared)
}

fn validate_fixture_labels_with_production_tokenizer(queries: &[LabelledQuery]) -> Result<()> {
    let model = EmbeddingsConfig::default().model;
    let tokenizer = load_tokenizer(&model)
        .with_context(|| format!("load {model} tokenizer for label preflight"))?;
    validate_fixture_labels(queries, tokenizer)
}

fn rerank_timeout_ms(arm: ArmSpec<'_>) -> u64 {
    match arm.model {
        Some(_) => EVALUATION_RERANK_TIMEOUT_MS,
        None => SearchConfig::default().rerank_timeout_ms,
    }
}

fn build_config(arm: ArmSpec<'_>, ground_dir: &Path) -> Config {
    let search = SearchConfig {
        crossencoder: arm.model.map(|model| model.to_string()),
        rerank_timeout_ms: rerank_timeout_ms(arm),
        ..Default::default()
    };
    Config {
        corpora: vec![fixture_corpus()],
        search,
        embeddings: EmbeddingsConfig::default(),
        storage: StorageConfig {
            ground_dir: ground_dir.to_string_lossy().into_owned(),
        },
        ..Default::default()
    }
}

fn ranked_docs(docs: &BTreeMap<String, DocFile>) -> Vec<(&String, &DocFile)> {
    let mut ranked: Vec<_> = docs.iter().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .score
            .partial_cmp(&left.1.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.cmp(right.0))
    });
    ranked
}

fn rank_of_expected(ranked: &[(&String, &DocFile)], expected_file: &str) -> Option<usize> {
    for (index, (absolute_path, doc)) in ranked.iter().enumerate() {
        if doc.path.as_deref() == Some(expected_file) || absolute_path.ends_with(expected_file) {
            return Some(index + 1);
        }
    }
    None
}

fn top_identity(ranked: &[(&String, &DocFile)]) -> Option<ChunkIdentity> {
    let (absolute_path, doc) = ranked.first().copied()?;
    let chunk = doc.chunks.first()?;
    let file = match &doc.path {
        Some(path) => path.clone(),
        None => Path::new(absolute_path).file_name()?.to_str()?.to_string(),
    };
    Some(ChunkIdentity {
        file,
        heading_path: chunk.heading_path.clone(),
        line_start: chunk.line_range[0],
    })
}

async fn cold_load_models(arm: ArmSpec<'_>) -> Result<u64> {
    let embeddings = EmbeddingsConfig::default();
    let model = embeddings.model;
    let quantized = embeddings.quantized;
    let cache_dir = expand_tilde(&embeddings.cache_dir);
    let crossencoder = arm.model.map(|model| model.to_string());
    let started = Instant::now();
    let loaded = tokio::task::spawn_blocking(move || -> Result<()> {
        let _embedder = Embedder::try_new(&model, quantized, &cache_dir)?;
        if let Some(model) = crossencoder {
            let _crossencoder = FastembedCrossencoder::try_new(&model, &cache_dir)?;
        }
        Ok(())
    })
    .await
    .with_context(|| format!("{MODEL_LOAD_MARKER} for {}: loader task failed", arm.id))?;
    loaded.with_context(|| format!("{MODEL_LOAD_MARKER} for {}", arm.id))?;
    Ok(started.elapsed().as_millis() as u64)
}

async fn index_fixture(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
    arm: ArmSpec<'_>,
) -> Result<()> {
    let _: IndexReport = client
        .call(DaemonRequest {
            cwd: cwd.to_path_buf(),
            payload: DaemonRequestPayload::Index(IndexRequest {
                corpus: Some(CORPUS_NAME.into()),
                paths_from: None,
                strict: true,
            }),
        })
        .await
        .with_context(|| format!("index fixture for {}", arm.id))?;
    Ok(())
}

fn ground_rpc_timeout(arm: ArmSpec<'_>) -> Duration {
    Duration::from_millis(rerank_timeout_ms(arm).saturating_add(GROUND_RPC_GRACE_MS))
}

async fn ground_query(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
    arm: ArmSpec<'_>,
    query: &LabelledQuery,
) -> Result<GroundResponse> {
    let response = client
        .call_raw_with_timeout(
            DaemonRequest {
                cwd: cwd.to_path_buf(),
                payload: DaemonRequestPayload::Ground(GroundRequest {
                    query: query.query.clone(),
                    corpus: Some(CORPUS_NAME.into()),
                    top_files: Some(10),
                    chunks_per_file: Some(1),
                    limit: Some(50),
                    snippet_chars: None,
                }),
            },
            ground_rpc_timeout(arm),
        )
        .await
        .with_context(|| format!("ground query {}", query.id))?;
    let result: GroundResult = match response {
        DaemonResponse::Ok { result } => {
            serde_json::from_value(result).context("decode ground response")?
        }
        DaemonResponse::Err { kind, message } => {
            return Err(anyhow::anyhow!(
                "daemon ground failed ({kind:?}): {message}"
            ));
        }
    };
    Ok(result.response)
}

fn top_rerank_signal(ranked: &[(&String, &DocFile)]) -> Option<f64> {
    for (_, doc) in ranked {
        if let Some(signal) = doc.z_score {
            return Some(signal);
        }
        if let Some(chunk) = doc.chunks.first()
            && let Some(signal) = chunk.z_score
        {
            return Some(signal);
        }
    }
    None
}

const RERANK_TIMEOUT_CODE: &str = "rerank-timeout";
const CROSSENCODER_UNAVAILABLE_CODE: &str = "crossencoder-unavailable";

/// A fused retrieval signal broke mid-run — a ripgrep pass that failed, timed
/// out, resolved to nothing, or emitted unparseable events silently changes
/// ranking, so the sweep would record a degraded run as a valid measurement and
/// bake it into the committed baseline. These codes, and only these, fail the
/// eval.
const DEGRADED_SIGNAL_CODES: [&str; 4] = [
    "ripgrep-unresolved",
    "ripgrep-unparseable",
    "ripgrep-failed",
    "ripgrep-timeout",
];

/// Rerank fell back to plain fusion. `rerank_completion` owns and judges these
/// two per arm, so the signal check must not re-judge them.
const RERANK_FALLBACK_CODES: [&str; 2] = [RERANK_TIMEOUT_CODE, CROSSENCODER_UNAVAILABLE_CODE];

/// Informational only — they describe what the query covered, not a broken
/// signal, so they must never fail the eval.
const ADVISORY_WARNING_CODES: [&str; 3] =
    ["code-repos-empty", "cross-repo-union", "index-coverage"];

/// Every `Warning.code` the domain and daemon crates emit. Re-derive with:
///
/// ```text
/// rg -n 'code:\s*"' --glob '*.rs' crates/
/// ```
///
/// `producer_warning_codes_are_classified_exactly_once` pins this against the
/// three sets above, so a new producer-side warning breaks a test at its source
/// instead of silently reaching the eval gate unclassified.
const PRODUCER_WARNING_CODES: [&str; 9] = [
    "ripgrep-unresolved",
    "ripgrep-unparseable",
    "ripgrep-failed",
    "ripgrep-timeout",
    RERANK_TIMEOUT_CODE,
    CROSSENCODER_UNAVAILABLE_CODE,
    "code-repos-empty",
    "cross-repo-union",
    "index-coverage",
];

/// Which of `run_arm`'s two sweeps a response came from. Only the measured
/// sweep is a measurement; the warmup exists to absorb cold-start cost and
/// nothing it produces reaches an artifact, so a transient *signal* degradation
/// there must not abort the run. `rerank_completion` still judges both sweeps —
/// a rerank fallback means the arm is misconfigured, not warming up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepKind {
    Warmup,
    Measured,
}

fn rerank_completion(response: &GroundResponse, arm: ArmSpec<'_>, query_id: &str) -> Result<bool> {
    let mut timed_out = false;
    let mut unavailable = false;
    for warning in &response.warnings {
        if warning.code == RERANK_TIMEOUT_CODE {
            timed_out = true;
        }
        if warning.code == CROSSENCODER_UNAVAILABLE_CODE {
            unavailable = true;
        }
    }

    match arm.model {
        Some(model) => {
            ensure!(
                !timed_out,
                "{RERANK_TIMEOUT_MARKER} for query {query_id} in {} ({model}); fusion fallback is not a measurement",
                arm.id
            );
            ensure!(
                !unavailable,
                "{CROSSENCODER_UNAVAILABLE_MARKER} for query {query_id} in {} ({model}); fusion fallback is not a measurement",
                arm.id
            );
            Ok(true)
        }
        None => {
            ensure!(
                !timed_out && !unavailable,
                "baseline query {query_id} unexpectedly used a rerank fallback"
            );
            Ok(false)
        }
    }
}

fn ensure_signals_intact(response: &GroundResponse, query_id: &str, kind: SweepKind) -> Result<()> {
    if kind == SweepKind::Warmup {
        return Ok(());
    }
    for warning in &response.warnings {
        ensure!(
            !DEGRADED_SIGNAL_CODES.contains(&warning.code.as_str()),
            "degraded a retrieval signal for query {query_id} ({}): {}; a degraded run is not a measurement",
            warning.code,
            warning.message
        );
    }
    Ok(())
}

async fn run_sweep(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
    arm: ArmSpec<'_>,
    queries: &[LabelledQuery],
    kind: SweepKind,
) -> Result<Vec<QueryMeasurement>> {
    let mut measurements = Vec::with_capacity(queries.len());
    for query in queries {
        let response = ground_query(client, cwd, arm, query).await?;
        let rerank_completed = rerank_completion(&response, arm, &query.id)?;
        ensure_signals_intact(&response, &query.id, kind)?;
        let ranked = ranked_docs(&response.docs);
        let rank = rank_of_expected(&ranked, &query.expected_chunk.file);
        let actual_top = top_identity(&ranked);
        let rerank_signal = top_rerank_signal(&ranked);
        if arm.model.is_none() {
            ensure!(
                rerank_signal.is_none(),
                "baseline query {} unexpectedly has a rerank signal",
                query.id
            );
        }
        let top_chunk_pass = actual_top.as_ref() == Some(&query.expected_chunk);
        measurements.push(QueryMeasurement {
            id: query.id.clone(),
            latency_ms: response.took_ms,
            rank,
            expected_top: query.expected_chunk.clone(),
            actual_top,
            top_chunk_pass,
            rerank_completed,
            rerank_signal,
        });
    }
    Ok(measurements)
}

fn nearest_rank_percentile(values: &[u64], percentile: usize) -> Result<u64> {
    ensure!(
        !values.is_empty(),
        "cannot compute percentile of an empty sweep"
    );
    ensure!(
        (1..=100).contains(&percentile),
        "percentile must be between 1 and 100"
    );
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percentile).div_ceil(100);
    Ok(sorted[rank - 1])
}

fn quality_for(queries: &[QueryMeasurement]) -> Result<QualityMetrics> {
    ensure!(!queries.is_empty(), "arm has no query measurements");
    let mut recall_hits = 0;
    let mut reciprocal_sum = 0.0;
    for measurement in queries {
        if let Some(rank) = measurement.rank {
            if rank <= 5 {
                recall_hits += 1;
            }
            reciprocal_sum += 1.0 / rank as f64;
        }
    }
    Ok(QualityMetrics {
        recall_at_5: recall_hits as f64 / queries.len() as f64,
        mrr: reciprocal_sum / queries.len() as f64,
    })
}

fn latency_for(cold_load_ms: u64, queries: &[QueryMeasurement]) -> Result<LatencyMetrics> {
    let mut latencies = Vec::with_capacity(queries.len());
    for query in queries {
        latencies.push(query.latency_ms);
    }
    Ok(LatencyMetrics {
        cold_load_ms,
        warm_p50_ms: nearest_rank_percentile(&latencies, 50)?,
        warm_p95_ms: nearest_rank_percentile(&latencies, 95)?,
    })
}

async fn run_arm(arm: ArmSpec<'_>, queries: &[LabelledQuery]) -> Result<ArmMeasurement> {
    let cold_load_ms = cold_load_models(arm).await?;
    let tmp = tempfile::tempdir().context("create arm tempdir")?;
    let ground_dir = tmp.path().join("ground");
    let harness = DaemonHarness::spawn(build_config(arm, &ground_dir)).await;
    let client = connect_at(harness.socket())
        .await
        .with_context(|| format!("connect eval daemon for {}", arm.id))?;
    index_fixture(&client, harness.cwd(), arm).await?;

    let _warmup = run_sweep(&client, harness.cwd(), arm, queries, SweepKind::Warmup).await?;
    let measured = run_sweep(&client, harness.cwd(), arm, queries, SweepKind::Measured).await?;
    drop(client);
    harness.shutdown().await.context("shutdown eval daemon")?;

    let model = arm.model.map(|model| model.to_string());
    Ok(ArmMeasurement {
        id: arm.id.to_string(),
        model,
        quality: quality_for(&measured)?,
        latency: latency_for(cold_load_ms, &measured)?,
        queries: measured,
    })
}

fn rank_disposition(baseline: Option<usize>, candidate: Option<usize>) -> ChangeDisposition {
    match (baseline, candidate) {
        (Some(baseline), Some(candidate)) if candidate < baseline => ChangeDisposition::Improved,
        (Some(baseline), Some(candidate)) if candidate == baseline => ChangeDisposition::Unchanged,
        (Some(_baseline), Some(_candidate)) => ChangeDisposition::Regressed,
        (None, Some(_candidate)) => ChangeDisposition::Improved,
        (None, None) => ChangeDisposition::Unchanged,
        (Some(_baseline), None) => ChangeDisposition::Regressed,
    }
}

fn assertion_disposition(baseline: bool, candidate: bool) -> ChangeDisposition {
    match (baseline, candidate) {
        (false, true) => ChangeDisposition::Improved,
        (false, false) => ChangeDisposition::Unchanged,
        (true, true) => ChangeDisposition::Unchanged,
        (true, false) => ChangeDisposition::Regressed,
    }
}

fn increment_counts(counts: &mut ChangeCounts, disposition: ChangeDisposition) {
    match disposition {
        ChangeDisposition::Improved => counts.improved += 1,
        ChangeDisposition::Unchanged => counts.unchanged += 1,
        ChangeDisposition::Regressed => counts.regressed += 1,
    }
}

fn query_by_id<'a>(queries: &'a [QueryMeasurement], id: &str) -> Option<&'a QueryMeasurement> {
    queries.iter().find(|query| query.id == id)
}

fn build_comparison(
    baseline: &ArmMeasurement,
    candidate: &ArmMeasurement,
) -> Result<ArmComparison> {
    let mut counts = ChangeCounts::default();
    let mut queries = Vec::with_capacity(baseline.queries.len());
    for baseline_query in &baseline.queries {
        let candidate_query = query_by_id(&candidate.queries, &baseline_query.id)
            .with_context(|| format!("{} is missing query {}", candidate.id, baseline_query.id))?;
        let disposition = rank_disposition(baseline_query.rank, candidate_query.rank);
        increment_counts(&mut counts, disposition);
        let rank_delta = match (baseline_query.rank, candidate_query.rank) {
            (Some(baseline), Some(candidate)) => {
                let baseline = i64::try_from(baseline).context("baseline rank exceeds i64")?;
                let candidate = i64::try_from(candidate).context("candidate rank exceeds i64")?;
                Some(baseline - candidate)
            }
            (Some(_baseline), None) => None,
            (None, Some(_candidate)) => None,
            (None, None) => None,
        };
        queries.push(QueryChange {
            id: baseline_query.id.clone(),
            baseline_rank: baseline_query.rank,
            candidate_rank: candidate_query.rank,
            rank_delta,
            rank_disposition: disposition,
            baseline_top: baseline_query.actual_top.clone(),
            candidate_top: candidate_query.actual_top.clone(),
            top_chunk_changed: baseline_query.actual_top != candidate_query.actual_top,
            top_chunk_assertion: assertion_disposition(
                baseline_query.top_chunk_pass,
                candidate_query.top_chunk_pass,
            ),
        });
    }
    ensure!(
        candidate.queries.len() == baseline.queries.len(),
        "{} has an unexpected query count",
        candidate.id
    );
    Ok(ArmComparison {
        baseline_id: baseline.id.clone(),
        candidate_id: candidate.id.clone(),
        counts,
        queries,
    })
}

fn requested_descriptors(arms: &[ArmSpec<'_>]) -> Vec<ArmDescriptor> {
    let mut descriptors = Vec::with_capacity(arms.len());
    for arm in arms {
        descriptors.push(ArmDescriptor::from_spec(*arm));
    }
    descriptors
}

fn arm_for_descriptor(descriptor: &ArmDescriptor) -> Option<ArmSpec<'static>> {
    diagnostic_arms()
        .into_iter()
        .find(|arm| descriptor == &ArmDescriptor::from_spec(*arm))
}

fn validate_measurement(measurement: &ArmMeasurement, arm: ArmSpec<'_>) -> Result<()> {
    ensure!(measurement.id == arm.id, "arm id mismatch");
    ensure!(
        measurement.model.as_deref() == arm.model,
        "arm model mismatch"
    );
    ensure!(
        measurement.quality.recall_at_5.is_finite()
            && (0.0..=1.0).contains(&measurement.quality.recall_at_5),
        "{} has invalid Recall@5",
        arm.id
    );
    ensure!(
        measurement.quality.mrr.is_finite() && (0.0..=1.0).contains(&measurement.quality.mrr),
        "{} has invalid MRR",
        arm.id
    );
    ensure!(
        measurement.latency.warm_p50_ms <= measurement.latency.warm_p95_ms,
        "{} has p50 above p95",
        arm.id
    );
    ensure!(!measurement.queries.is_empty(), "{} has no queries", arm.id);
    let mut ids = BTreeSet::new();
    for query in &measurement.queries {
        ensure!(
            ids.insert(query.id.as_str()),
            "duplicate query {}",
            query.id
        );
        match arm.model {
            Some(_model) => ensure!(
                query.rerank_completed,
                "{} query {} did not complete reranking",
                arm.id,
                query.id
            ),
            None => {
                ensure!(
                    !query.rerank_completed,
                    "{} query {} unexpectedly completed reranking",
                    arm.id,
                    query.id
                );
                ensure!(
                    query.rerank_signal.is_none(),
                    "{} query {} has an unexpected rerank signal",
                    arm.id,
                    query.id
                );
            }
        }
    }
    Ok(())
}

fn validate_artifact_common(artifact: &EvalArtifact) -> Result<()> {
    ensure!(
        artifact.schema_version == SCHEMA_VERSION,
        "unsupported eval schema"
    );
    ensure!(
        !artifact.query_set_digest.is_empty(),
        "missing query-set digest"
    );
    ensure!(
        !artifact.ripgrep_version.is_empty(),
        "missing ripgrep version"
    );
    ensure!(
        artifact.embedding == embedding_configuration(),
        "evaluation does not use production embedding defaults"
    );
    ensure!(!artifact.requested_arms.is_empty(), "no requested arms");
    ensure!(
        artifact.timeout_count == artifact.timeouts.len(),
        "timeout count does not match timeout diagnostics"
    );
    let mut requested = BTreeSet::new();
    for descriptor in &artifact.requested_arms {
        ensure!(
            requested.insert(descriptor.id.as_str()),
            "duplicate requested arm"
        );
        ensure!(
            arm_for_descriptor(descriptor).is_some(),
            "unsupported requested arm {}",
            descriptor.id
        );
    }
    let mut measured = BTreeSet::new();
    for measurement in &artifact.measurements {
        ensure!(
            measured.insert(measurement.id.as_str()),
            "duplicate measurement"
        );
        let descriptor = ArmDescriptor {
            id: measurement.id.clone(),
            model: measurement.model.clone(),
        };
        let Some(arm) = arm_for_descriptor(&descriptor) else {
            return Err(anyhow::anyhow!(
                "unsupported measurement {}",
                measurement.id
            ));
        };
        ensure!(
            artifact.requested_arms.contains(&descriptor),
            "measurement {} was not requested",
            measurement.id
        );
        validate_measurement(measurement, arm)?;
    }
    Ok(())
}

fn validate_baseline_artifact(artifact: &EvalArtifact) -> Result<()> {
    validate_artifact_common(artifact)?;
    ensure!(
        artifact.requested_arms == requested_descriptors(&baseline_arms()),
        "baseline must request only production fusion without a reranker"
    );
    ensure!(
        artifact.measurements.len() == 1,
        "baseline must contain one measurement"
    );
    ensure!(
        artifact.measurements[0].id == BASELINE_ID,
        "baseline arm is missing"
    );
    ensure!(
        artifact.comparison.is_none(),
        "baseline must not contain a comparison"
    );
    ensure!(artifact.timeouts.is_empty(), "baseline contains timeouts");
    ensure!(
        artifact.model_load_failures.is_empty(),
        "baseline contains model-load failures"
    );
    ensure!(artifact.errors.is_empty(), "baseline contains errors");
    Ok(())
}

fn validate_diagnostic_artifact(artifact: &EvalArtifact, complete: bool) -> Result<()> {
    validate_artifact_common(artifact)?;
    ensure!(
        artifact.requested_arms == requested_descriptors(&diagnostic_arms()),
        "diagnostic must request exactly production fusion and Jina v1"
    );
    ensure!(
        artifact.measurements.len() <= 2,
        "diagnostic has too many measurements"
    );
    if complete {
        ensure!(artifact.measurements.len() == 2, "diagnostic is incomplete");
        ensure!(
            artifact.timeouts.is_empty(),
            "complete diagnostic has timeouts"
        );
        ensure!(
            artifact.model_load_failures.is_empty(),
            "complete diagnostic has model-load failures"
        );
        ensure!(artifact.errors.is_empty(), "complete diagnostic has errors");
        let comparison = artifact
            .comparison
            .as_ref()
            .context("complete diagnostic has no comparison")?;
        let expected = build_comparison(&artifact.measurements[0], &artifact.measurements[1])?;
        ensure!(
            comparison == &expected,
            "diagnostic comparison is inconsistent"
        );
    } else {
        ensure!(
            !artifact.timeouts.is_empty()
                || !artifact.model_load_failures.is_empty()
                || !artifact.errors.is_empty(),
            "partial diagnostic has no failure"
        );
        ensure!(
            artifact.comparison.is_none(),
            "partial diagnostic has a comparison"
        );
    }
    Ok(())
}

fn failure_query_id(message: &str) -> Option<String> {
    let marker = "for query ";
    let index = message.find(marker)?;
    let tail = &message[index + marker.len()..];
    let id = tail.split_whitespace().next()?;
    Some(id.to_string())
}

fn record_failure(artifact: &mut EvalArtifact, arm: ArmSpec<'_>, error: &anyhow::Error) {
    let message = format!("{error:#}");
    let diagnostic = FailureDiagnostic {
        arm_id: arm.id.to_string(),
        query_id: failure_query_id(&message),
        message: message.clone(),
    };
    if message.contains(MODEL_LOAD_MARKER) || message.contains(CROSSENCODER_UNAVAILABLE_MARKER) {
        artifact.model_load_failures.push(diagnostic);
    } else if message.contains(RERANK_TIMEOUT_MARKER) {
        artifact.timeouts.push(diagnostic);
        artifact.timeout_count += 1;
    } else {
        artifact.errors.push(diagnostic);
    }
}

async fn measure_diagnostic(
    queries: &[LabelledQuery],
    query_set_digest: String,
    output: &Path,
    baseline: &Path,
) -> Result<EvalArtifact> {
    validate_fixture_labels_with_production_tokenizer(queries)?;
    let arms = diagnostic_arms();
    let mut artifact = new_artifact(query_set_digest, ripgrep_version()?, &arms);
    for arm in arms {
        match run_arm(arm, queries).await {
            Ok(measurement) => artifact.measurements.push(measurement),
            Err(error) => {
                record_failure(&mut artifact, arm, &error);
                validate_diagnostic_artifact(&artifact, false)?;
                write_measurement_artifact(output, baseline, &artifact)
                    .context("persist partial diagnostic artifact")?;
                return Err(error);
            }
        }
    }
    artifact.comparison = Some(build_comparison(
        &artifact.measurements[0],
        &artifact.measurements[1],
    )?);
    validate_diagnostic_artifact(&artifact, true)?;
    Ok(artifact)
}

async fn measure_baseline(
    queries: &[LabelledQuery],
    query_set_digest: String,
) -> Result<EvalArtifact> {
    validate_fixture_labels_with_production_tokenizer(queries)?;
    let arms = baseline_arms();
    let mut artifact = new_artifact(query_set_digest, ripgrep_version()?, &arms);
    artifact
        .measurements
        .push(run_arm(BASELINE_ARM, queries).await?);
    validate_baseline_artifact(&artifact)?;
    Ok(artifact)
}

fn compare_against_baseline(committed: &EvalArtifact, current: &EvalArtifact) -> Result<()> {
    validate_baseline_artifact(committed).context("committed baseline is invalid")?;
    validate_baseline_artifact(current).context("current baseline measurement is invalid")?;
    ensure!(
        committed.query_set_digest == current.query_set_digest,
        "query-set digest changed; remeasure and explicitly update the baseline"
    );
    let committed = &committed.measurements[0];
    let current = &current.measurements[0];
    ensure!(
        current.quality.recall_at_5 + f64::EPSILON >= committed.quality.recall_at_5,
        "Recall@5 regressed from {} to {}",
        committed.quality.recall_at_5,
        current.quality.recall_at_5
    );
    ensure!(
        current.quality.mrr + f64::EPSILON >= committed.quality.mrr,
        "MRR regressed from {} to {}",
        committed.quality.mrr,
        current.quality.mrr
    );
    for committed_query in &committed.queries {
        let current_query = query_by_id(&current.queries, &committed_query.id)
            .with_context(|| format!("current run is missing query {}", committed_query.id))?;
        ensure!(
            !committed_query.top_chunk_pass || current_query.top_chunk_pass,
            "query {} regressed from top-chunk pass to fail",
            current_query.id
        );
    }
    Ok(())
}

fn normalize_path_for_comparison(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let mut lexical = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => lexical.push(prefix.as_os_str()),
            std::path::Component::RootDir => lexical.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                ensure!(lexical.pop(), "path escapes its filesystem root");
            }
            std::path::Component::Normal(segment) => lexical.push(segment),
        }
    }

    let mut existing = lexical.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for segment in missing.iter().rev() {
                    canonical.push(segment);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let segment = existing
                    .file_name()
                    .context("path has no existing canonical ancestor")?;
                missing.push(segment.to_os_string());
                existing = existing
                    .parent()
                    .context("path has no existing canonical ancestor")?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("canonicalize {}", existing.display()));
            }
        }
    }
}

fn ensure_measurement_output_is_not_baseline(output: &Path, baseline: &Path) -> Result<()> {
    let output = normalize_path_for_comparison(output)?;
    let baseline = normalize_path_for_comparison(baseline)?;
    ensure!(
        output != baseline,
        "HALLOUMINATE_EVAL_OUTPUT resolves to eval/baseline.json"
    );
    Ok(())
}

fn write_measurement_artifact(
    output: &Path,
    baseline: &Path,
    artifact: &EvalArtifact,
) -> Result<()> {
    ensure_measurement_output_is_not_baseline(output, baseline)?;
    write_artifact(output, artifact)
}

fn write_artifact(path: &Path, artifact: &EvalArtifact) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(anyhow::anyhow!("output path has no parent"));
    };
    fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(artifact).context("serialize eval artifact")?;
    bytes.push(b'\n');

    let temp = tempfile::NamedTempFile::new_in(parent).context("create temporary artifact")?;
    fs::write(temp.path(), bytes)
        .with_context(|| format!("write temporary artifact in {}", parent.display()))?;
    temp.persist(path)
        .with_context(|| format!("rename temporary artifact to {}", path.display()))?;
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[test]
fn wiki_authoring_guidance_has_context_and_retrieval_policy() {
    let root = repo_root();
    let skill_path = root.join("plugins/hallouminate/skills/wiki-ingest/SKILL.md");
    let conventions_path = root.join(".hallouminate/wiki/wiki-conventions.md");
    let skill = fs::read_to_string(&skill_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", skill_path.display()));
    let conventions = fs::read_to_string(&conventions_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", conventions_path.display()));

    for (path, text) in [(&skill_path, &skill), (&conventions_path, &conventions)] {
        assert!(
            text.contains("Every H2/H3 section must open self-contained"),
            "{} lacks the H2/H3 self-contained opening rule",
            path.display()
        );
        assert!(
            text.contains("do not generate index-time or per-chunk LLM context"),
            "{} permits generated retrieval context",
            path.display()
        );
    }

    for required in [
        "`sources/<slug>.md` page",
        "search the corpus for both the canonical URL and",
        "The exact title, publisher or author, source type, date, contribution, and canonical URL",
        "During claim decomposition, before any drafting, freeze the retrieval probes",
        "Before journaling, verify the complete topic-and-source write set with the frozen probes",
        "require the intended page at rank 1",
        "require it within the top 3",
        "revise only the H1, lead, headings, or section opening once",
        "restore every overwritten page from its preimage",
        "`written-with-retrieval-warning`",
        "observed top three",
    ] {
        assert!(
            skill.contains(required),
            "wiki-ingest lacks policy: {required}"
        );
    }
}

#[test]
fn query_labels_are_complete_and_digest_is_stable() {
    let (queries, digest) = load_queries().expect("load eval queries");
    // The expanded set must stay at or above the labelled-query floor the
    // evaluation is required to report over; see eval/README.md.
    assert!(
        queries.len() >= MIN_LABELLED_QUERIES,
        "eval query set shrank to {} labelled queries, below the {MIN_LABELLED_QUERIES} floor",
        queries.len()
    );
    assert_eq!(digest.len(), 64);
    validate_fixture_labels(&queries, Characters).expect("labels match prepared fixture chunks");
}

#[test]
fn prepared_chunk_validation_rejects_misspelled_heading_and_line() {
    let (queries, _) = load_queries().expect("load eval queries");
    let prepared =
        prepared_fixture_chunk_identities(Characters).expect("prepare fixture chunk identities");

    let mut misspelled_heading = queries.clone();
    misspelled_heading[0].expected_chunk.heading_path[0].push_str(" misspelled");
    assert!(validate_expected_chunks(&misspelled_heading, &prepared).is_err());

    let mut wrong_line = queries;
    wrong_line[0].expected_chunk.line_start += 1;
    assert!(validate_expected_chunks(&wrong_line, &prepared).is_err());
}

#[test]
fn nearest_rank_percentiles_cover_p50_and_p95() {
    let even = [40, 10, 30, 20];
    assert_eq!(nearest_rank_percentile(&even, 50).expect("p50"), 20);
    assert_eq!(nearest_rank_percentile(&even, 95).expect("p95"), 40);
    assert_eq!(
        nearest_rank_percentile(&[30, 10, 20], 50).expect("odd p50"),
        20
    );
    assert!(nearest_rank_percentile(&[], 50).is_err());
    assert!(nearest_rank_percentile(&[1], 0).is_err());
    assert!(nearest_rank_percentile(&[1], 101).is_err());
}

#[test]
fn evaluation_arms_derive_from_production_defaults() {
    let temp = tempfile::tempdir().expect("tempdir");
    let embeddings = EmbeddingsConfig::default();
    assert_eq!(embeddings.model, "snowflake/snowflake-arctic-embed-s");
    assert!(!embeddings.quantized);
    assert_eq!(embeddings.cache_dir, "~/.cache/hallouminate/fastembed");

    let baseline = build_config(BASELINE_ARM, temp.path());
    assert_eq!(baseline.embeddings, embeddings);
    assert_eq!(baseline.search, SearchConfig::default());

    let candidate = build_config(JINA_ARM, temp.path());
    let expected_search = SearchConfig {
        crossencoder: Some(JINA_MODEL.to_string()),
        rerank_timeout_ms: EVALUATION_RERANK_TIMEOUT_MS,
        ..Default::default()
    };
    assert_eq!(candidate.embeddings, embeddings);
    assert_eq!(candidate.search, expected_search);
    assert!(
        candidate.search.rerank_timeout_ms > SearchConfig::default().rerank_timeout_ms,
        "the Jina measurement needs its own bounded timeout so a slow valid inference does not become a fallback"
    );

    assert_eq!(baseline_arms(), [BASELINE_ARM]);
    assert_eq!(diagnostic_arms(), [BASELINE_ARM, JINA_ARM]);
}

#[test]
fn ground_rpc_deadline_exceeds_each_arm_rerank_timeout() {
    for arm in [BASELINE_ARM, JINA_ARM] {
        assert!(ground_rpc_timeout(arm) > Duration::from_millis(rerank_timeout_ms(arm)));
    }
}

fn synthetic_identity(file: &str) -> ChunkIdentity {
    ChunkIdentity {
        file: file.into(),
        heading_path: vec![file.into()],
        line_start: 1,
    }
}

fn synthetic_query(
    id: &str,
    expected_file: &str,
    rank: Option<usize>,
    actual_file: Option<&str>,
    rerank_signal: Option<f64>,
) -> QueryMeasurement {
    let expected_top = synthetic_identity(expected_file);
    let actual_top = actual_file.map(synthetic_identity);
    let top_chunk_pass = actual_top.as_ref() == Some(&expected_top);
    QueryMeasurement {
        id: id.into(),
        latency_ms: 10,
        rank,
        expected_top,
        actual_top,
        top_chunk_pass,
        rerank_completed: false,
        rerank_signal,
    }
}

fn synthetic_measurement(arm: ArmSpec<'_>, mut queries: Vec<QueryMeasurement>) -> ArmMeasurement {
    let rerank_completed = arm.model.is_some();
    for query in &mut queries {
        query.rerank_completed = rerank_completed;
    }
    let model = arm.model.map(|model| model.to_string());
    ArmMeasurement {
        id: arm.id.into(),
        model,
        quality: quality_for(&queries).expect("quality"),
        latency: latency_for(5, &queries).expect("latency"),
        queries,
    }
}

/// Fixed provenance for synthetic fixtures. They are never written as a
/// measurement, so they must not depend on whatever `rg` the dev box runs.
const SYNTHETIC_RIPGREP_VERSION: &str = "ripgrep 14.1.1 (rev synthetic)";

fn response_with_warning(code: &str, message: &str) -> GroundResponse {
    GroundResponse {
        query: "request envelope shape".into(),
        took_ms: 0,
        stats: Default::default(),
        docs: BTreeMap::new(),
        code: BTreeMap::new(),
        warnings: vec![Warning {
            code: code.into(),
            message: message.into(),
        }],
    }
}

fn synthetic_baseline_artifact() -> EvalArtifact {
    let mut artifact = new_artifact(
        "digest".into(),
        SYNTHETIC_RIPGREP_VERSION.into(),
        &baseline_arms(),
    );
    artifact.measurements.push(synthetic_measurement(
        BASELINE_ARM,
        vec![
            synthetic_query("first", "first.md", Some(1), Some("first.md"), None),
            synthetic_query("second", "second.md", Some(2), Some("other.md"), None),
        ],
    ));
    validate_baseline_artifact(&artifact).expect("synthetic baseline");
    artifact
}

fn synthetic_diagnostic_artifact() -> EvalArtifact {
    let mut artifact = new_artifact(
        "digest".into(),
        SYNTHETIC_RIPGREP_VERSION.into(),
        &diagnostic_arms(),
    );
    artifact.measurements.push(synthetic_measurement(
        BASELINE_ARM,
        vec![
            synthetic_query("first", "first.md", Some(2), Some("first.md"), None),
            synthetic_query("second", "second.md", None, Some("other.md"), None),
            synthetic_query("third", "third.md", Some(1), Some("third.md"), None),
        ],
    ));
    artifact.measurements.push(synthetic_measurement(
        JINA_ARM,
        vec![
            synthetic_query("first", "first.md", Some(1), Some("first.md"), Some(2.0)),
            synthetic_query("second", "second.md", None, Some("second.md"), Some(2.0)),
            synthetic_query("third", "third.md", None, Some("other.md"), Some(2.0)),
        ],
    ));
    artifact.comparison = Some(
        build_comparison(&artifact.measurements[0], &artifact.measurements[1]).expect("comparison"),
    );
    validate_diagnostic_artifact(&artifact, true).expect("synthetic diagnostic");
    artifact
}

#[test]
fn diagnostic_artifact_has_two_arms_counts_and_no_decision_fields() {
    let artifact = synthetic_diagnostic_artifact();
    let comparison = artifact.comparison.as_ref().expect("comparison");
    assert_eq!(comparison.counts.improved, 1);
    assert_eq!(comparison.counts.unchanged, 1);
    assert_eq!(comparison.counts.regressed, 1);
    assert_eq!(comparison.queries.len(), 3);

    let json = serde_json::to_string(&artifact).expect("serialize diagnostic");
    assert!(!json.contains("qualified"));
    assert!(!json.contains("selected"));
    assert!(json.contains("recall_at_5"));
    assert!(json.contains("warm_p50_ms"));
    assert!(json.contains("warm_p95_ms"));
    assert!(json.contains("timeout_count"));
    assert!(json.contains("model_load_failures"));
    assert!(json.contains("ripgrep_version"));
}

#[test]
fn partial_diagnostic_records_timeout_before_validation() {
    let mut artifact = new_artifact(
        "digest".into(),
        SYNTHETIC_RIPGREP_VERSION.into(),
        &diagnostic_arms(),
    );
    let error = anyhow::anyhow!("{RERANK_TIMEOUT_MARKER} for query first in {JINA_ID}");
    record_failure(&mut artifact, JINA_ARM, &error);
    validate_diagnostic_artifact(&artifact, false).expect("partial diagnostic");
    assert_eq!(artifact.timeout_count, 1);
    assert_eq!(artifact.timeouts[0].query_id.as_deref(), Some("first"));
}

#[test]
fn committed_baseline_matches_the_current_schema_and_records_provenance() {
    let bytes = fs::read(baseline_path()).expect("read eval/baseline.json");
    // Probe the version before decoding: a stale baseline is missing fields the
    // current schema requires, so serde would report the missing field rather
    // than the schema mismatch that actually explains it.
    ensure_baseline_schema_current(&bytes).expect("committed baseline is the current schema");
    let baseline: EvalArtifact = serde_json::from_slice(&bytes).expect("parse eval/baseline.json");
    assert!(
        baseline.ripgrep_version.starts_with("ripgrep "),
        "committed baseline must record real ripgrep provenance: {:?}",
        baseline.ripgrep_version
    );
    validate_baseline_artifact(&baseline).expect("committed baseline validates");
}

#[test]
fn validation_rejects_an_empty_ripgrep_version() {
    let mut artifact = synthetic_baseline_artifact();
    artifact.ripgrep_version = String::new();
    let error = validate_baseline_artifact(&artifact)
        .expect_err("an empty provenance record must not validate");
    assert!(
        error.to_string().contains("missing ripgrep version"),
        "{error}"
    );
}

#[test]
fn compare_against_baseline_ignores_ripgrep_version_mismatch() {
    let mut committed = synthetic_baseline_artifact();
    let mut current = synthetic_baseline_artifact();
    committed.ripgrep_version = "ripgrep 13.0.0 (rev abc)".into();
    current.ripgrep_version = "ripgrep 14.1.1 (rev def)".into();
    compare_against_baseline(&committed, &current)
        .expect("ripgrep_version mismatch must not affect comparison");
}

#[test]
fn parse_rg_version_output_rejects_empty_stdout() {
    let error = parse_rg_version_output("rg", b"").expect_err("empty stdout must fail");
    assert!(error.to_string().contains("produced no output"), "{error}");
}

#[test]
fn parse_rg_version_output_rejects_whitespace_only_stdout() {
    let error =
        parse_rg_version_output("rg", b"   \n\n").expect_err("whitespace-only stdout must fail");
    assert!(error.to_string().contains("produced no output"), "{error}");
}

#[test]
fn parse_rg_version_output_trims_and_takes_first_line() {
    let stdout = b"  ripgrep 14.1.1 (rev abc123)  \nfeatures: pcre2, simd-accel\n";
    let version = parse_rg_version_output("rg", stdout).expect("parse rg version");
    assert_eq!(version, "ripgrep 14.1.1 (rev abc123)");
}

#[test]
fn parse_rg_version_output_skips_leading_blank_lines() {
    let stdout = b"\n   \nripgrep 14.1.1 (rev abc123)\nfeatures: pcre2\n";
    let version = parse_rg_version_output("rg", stdout).expect("parse rg version");
    assert_eq!(version, "ripgrep 14.1.1 (rev abc123)");
}

#[test]
fn parse_rg_version_output_rejects_non_utf8_stdout() {
    let error = parse_rg_version_output("rg", b"ripgrep 14.1.1 \xff\xfe")
        .expect_err("non-UTF8 output must fail loud, not decode lossily");
    assert!(error.to_string().contains("non-UTF8"), "{error}");
}

#[test]
fn parse_rg_version_output_rejects_a_foreign_banner() {
    let error = parse_rg_version_output("rg", b"GNU grep 3.11\n")
        .expect_err("a non-ripgrep banner must not be recorded as ripgrep provenance");
    assert!(
        error.to_string().contains("not a ripgrep banner"),
        "{error}"
    );
}

#[test]
fn parse_rg_version_output_rejects_an_implausibly_long_banner() {
    let stdout = format!("ripgrep {}\n", "9".repeat(MAX_RIPGREP_VERSION_LEN));
    let error = parse_rg_version_output("rg", stdout.as_bytes())
        .expect_err("an over-long banner must not be recorded");
    assert!(error.to_string().contains("over the"), "{error}");
}

/// `rg` is a hard runtime dependency of the eval harness. On a developer
/// machine without it, skip with a visible notice; in CI, fail closed instead
/// of reporting green on zero executed assertions.
fn require_rg() -> bool {
    if Command::new("rg").arg("--version").output().is_ok() {
        return true;
    }
    assert!(
        env::var_os("CI").is_none(),
        "rg not found on PATH; rg is a hard runtime dependency and must be installed in CI"
    );
    eprintln!("SKIP: rg not found on PATH; skipping ripgrep provenance test locally");
    false
}

/// Serializes every test in this binary that execs a child process.
///
/// The harness runs tests on parallel threads. A sibling thread's `fork` can
/// inherit an open write handle to a stub script written moments earlier, so
/// the following `execve` fails with `ETXTBSY` ("text file busy") even though
/// the script is complete and executable. Observed as a real intermittent
/// failure of `ripgrep_version_fails_loud_on_nonzero_exit`: green 5/5 in
/// isolation, red once across 7 full-suite runs. Serializing the execs closes
/// the window; the four affected tests are sub-millisecond, so the cost is nil.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

fn exec_guard() -> std::sync::MutexGuard<'static, ()> {
    // A panicking test must not cascade into its siblings via lock poisoning —
    // the lock guards process-spawn ordering, not shared mutable state.
    EXEC_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stub_binary(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, script).expect("write stub script");
    let mut perms = fs::metadata(&path).expect("stat stub script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod stub script");
    path
}

#[test]
fn ripgrep_version_returns_installed_version() {
    let _exec = exec_guard();
    if !require_rg() {
        return;
    }
    let version = ripgrep_version().expect("rg --version must succeed in eval environment");
    assert!(
        version.starts_with("ripgrep "),
        "unexpected rg --version output: {version}"
    );
}

#[test]
fn ripgrep_version_fails_loud_when_binary_missing() {
    // Guarded too: a failed exec still forks first, and that fork can inherit
    // a sibling test's open write handle to its stub script.
    let _exec = exec_guard();
    let error = ripgrep_version_from_binary("definitely-not-a-real-binary-xyz123")
        .expect_err("a missing binary must fail loud, not silently degrade");
    let message = format!("{error:#}");
    assert!(
        message.contains("definitely-not-a-real-binary-xyz123")
            && message.contains("for eval provenance"),
        "error must name the spawn that failed: {message}"
    );
}

#[test]
fn ripgrep_version_fails_loud_on_nonzero_exit() {
    let _exec = exec_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let script = stub_binary(
        dir.path(),
        "broken-rg",
        "#!/bin/sh\necho 'error while loading shared libraries' >&2\nexit 127\n",
    );
    let error = ripgrep_version_from_binary(script.to_str().expect("utf8 path"))
        .expect_err("a non-zero exit must fail loud, not silently degrade");
    let message = format!("{error:#}");
    assert!(
        message.contains("exited non-zero")
            && message.contains("127")
            && message.contains("error while loading shared libraries"),
        "error must carry the exit status and stderr: {message}"
    );
}

#[test]
fn ripgrep_version_fails_loud_on_empty_output() {
    let _exec = exec_guard();
    // GNU coreutils `true --version` prints a version banner, so it can't
    // stand in for a silent binary here; a throwaway script that ignores
    // its arguments and exits clean can.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = stub_binary(dir.path(), "silent-ok", "#!/bin/sh\nexit 0\n");
    let error = ripgrep_version_from_binary(script.to_str().expect("utf8 path"))
        .expect_err("empty output must fail loud, not record a placeholder");
    let message = format!("{error:#}");
    assert!(
        message.contains("produced no output"),
        "error must explain the failure: {message}"
    );
}

#[test]
fn diagnostic_rejects_rerank_fallback_warnings() {
    for code in RERANK_FALLBACK_CODES {
        let response = response_with_warning(code, "fallback");
        let error = rerank_completion(&response, JINA_ARM, "first")
            .expect_err("diagnostic must reject fallback warning");
        assert!(
            error
                .to_string()
                .contains("fusion fallback is not a measurement"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn completed_rerank_is_not_inferred_from_z_score() {
    let measurement = synthetic_measurement(
        JINA_ARM,
        vec![synthetic_query(
            "degenerate",
            "file.md",
            Some(1),
            Some("file.md"),
            None,
        )],
    );
    assert!(measurement.queries[0].rerank_completed);
    assert!(measurement.queries[0].rerank_signal.is_none());
    validate_measurement(&measurement, JINA_ARM).expect("degenerate z-score remains completed");
}

#[test]
fn comparator_rejects_metric_and_top_chunk_regressions() {
    let committed = synthetic_baseline_artifact();

    let mut metric_regression = committed.clone();
    metric_regression.measurements[0].queries[1].rank = None;
    metric_regression.measurements[0].quality =
        quality_for(&metric_regression.measurements[0].queries).expect("quality");
    assert!(compare_against_baseline(&committed, &metric_regression).is_err());

    let mut chunk_regression = committed.clone();
    chunk_regression.measurements[0].queries[0].actual_top = None;
    chunk_regression.measurements[0].queries[0].top_chunk_pass = false;
    assert!(compare_against_baseline(&committed, &chunk_regression).is_err());
}

#[test]
fn artifact_write_isolated_to_requested_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let baseline = temp.path().join("eval/baseline.json");
    let output = temp.path().join(".context/result.json");
    fs::create_dir_all(baseline.parent().expect("baseline parent")).expect("mkdir eval");
    fs::write(&baseline, b"committed baseline sentinel").expect("write baseline");
    let before = fs::read(&baseline).expect("read baseline before");

    write_artifact(&output, &synthetic_diagnostic_artifact()).expect("write output");

    assert_eq!(fs::read(&baseline).expect("read baseline after"), before);
    assert!(output.is_file());
    let mut entry_count = 0;
    for entry in fs::read_dir(temp.path().join(".context")).expect("read output dir") {
        entry.expect("output entry");
        entry_count += 1;
    }
    assert_eq!(entry_count, 1);
}

#[test]
fn measurement_output_alias_cannot_overwrite_baseline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let eval = temp.path().join("eval");
    let baseline = eval.join("baseline.json");
    fs::create_dir_all(&eval).expect("mkdir eval");
    fs::write(&baseline, b"committed baseline sentinel").expect("write baseline");
    let before = fs::read(&baseline).expect("read baseline before");
    let alias = eval.join("../eval/baseline.json");

    let error = write_measurement_artifact(&alias, &baseline, &synthetic_diagnostic_artifact())
        .expect_err("baseline alias must be rejected");

    assert!(
        error
            .to_string()
            .contains("HALLOUMINATE_EVAL_OUTPUT resolves to eval/baseline.json")
    );
    assert_eq!(fs::read(&baseline).expect("read baseline after"), before);
}

#[test]
fn hard_link_measurement_output_preserves_baseline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let eval = temp.path().join("eval");
    let baseline = eval.join("baseline.json");
    let output = temp.path().join(".context/result.json");
    fs::create_dir_all(&eval).expect("mkdir eval");
    fs::write(&baseline, b"committed baseline sentinel").expect("write baseline");
    fs::create_dir_all(output.parent().expect("output parent")).expect("mkdir context");
    fs::hard_link(&baseline, &output).expect("create hard-link output");
    let before = fs::read(&baseline).expect("read baseline before");

    write_measurement_artifact(&output, &baseline, &synthetic_diagnostic_artifact())
        .expect("write output through hard link");

    assert_eq!(fs::read(&baseline).expect("read baseline after"), before);
    assert_ne!(fs::read(&output).expect("read output after"), before);
}

#[test]
fn relative_measurement_output_resolves_against_repo_root() {
    let output = measurement_output_path(Path::new(".context/result.json"));
    let expected = repo_root().join(".context/result.json");
    assert_eq!(output, expected);
}

#[tokio::test]
#[ignore = "loads production embeddings and the Jina v1 reranker"]
async fn eval_ground_recall_measure() -> Result<()> {
    let output = env::var_os("HALLOUMINATE_EVAL_OUTPUT")
        .context("HALLOUMINATE_EVAL_OUTPUT must name the diagnostic artifact")?;
    let output = measurement_output_path(Path::new(&output));
    let baseline = baseline_path();
    ensure_measurement_output_is_not_baseline(&output, &baseline)?;
    let baseline_before = read_optional(&baseline)?;
    let (queries, query_set_digest) = load_queries()?;
    let artifact = measure_diagnostic(&queries, query_set_digest, &output, &baseline).await?;
    write_measurement_artifact(&output, &baseline, &artifact)?;
    let baseline_after = read_optional(&baseline)?;
    ensure!(
        baseline_before == baseline_after,
        "measurement modified eval/baseline.json"
    );
    println!("{}", serde_json::to_string_pretty(&artifact)?);
    Ok(())
}

/// `EvalArtifact` gains required fields as the schema moves, so decoding the
/// full artifact first turns a stale baseline into a serde "missing field"
/// error. Probe the version separately so the mismatch is reported by the
/// check that owns it.
#[derive(Deserialize)]
struct ArtifactSchemaProbe {
    schema_version: u32,
}

fn ensure_baseline_schema_current(bytes: &[u8]) -> Result<()> {
    let probe: ArtifactSchemaProbe =
        serde_json::from_slice(bytes).context("read schema_version from eval/baseline.json")?;
    ensure!(
        probe.schema_version == SCHEMA_VERSION,
        "eval/baseline.json is schema {}, but this evaluation requires {SCHEMA_VERSION}; regenerate the baseline",
        probe.schema_version
    );
    Ok(())
}

#[tokio::test]
#[ignore = "loads production embeddings and enforces the committed baseline"]
async fn eval_ground_recall_enforce() -> Result<()> {
    let baseline_bytes = fs::read(baseline_path()).context("read eval/baseline.json")?;
    ensure_baseline_schema_current(&baseline_bytes)?;
    let baseline: EvalArtifact =
        serde_json::from_slice(&baseline_bytes).context("parse eval/baseline.json")?;
    validate_baseline_artifact(&baseline).context("validate eval/baseline.json")?;

    let (queries, query_set_digest) = load_queries()?;
    ensure!(
        query_set_digest == baseline.query_set_digest,
        "query-set digest disagrees with eval/baseline.json"
    );
    let current = measure_baseline(&queries, query_set_digest).await?;
    compare_against_baseline(&baseline, &current)?;
    println!("{}", serde_json::to_string_pretty(&current)?);
    Ok(())
}

#[test]
fn degraded_ripgrep_signal_is_not_a_valid_measurement() {
    for code in DEGRADED_SIGNAL_CODES {
        let response = response_with_warning(code, "ripgrep pass degraded");
        let error = ensure_signals_intact(&response, "envelope-shape", SweepKind::Measured)
            .expect_err("a degraded ripgrep signal must not be measured as a baseline result");
        let message = error.to_string();
        assert!(
            message.contains(code) && message.contains("for query envelope-shape"),
            "error must name the warning code and the query: {message}"
        );
    }
}

#[test]
fn warmup_sweep_tolerates_degraded_signals() {
    for code in DEGRADED_SIGNAL_CODES {
        let response = response_with_warning(code, "ripgrep pass degraded");
        ensure_signals_intact(&response, "envelope-shape", SweepKind::Warmup).unwrap_or_else(
            |error| panic!("{code} in the throwaway warmup must not abort the run: {error}"),
        );
    }
}

#[test]
fn rerank_warnings_remain_owned_by_rerank_completion() {
    for code in RERANK_FALLBACK_CODES {
        let response = response_with_warning(code, "rerank fell back to fusion");
        ensure_signals_intact(&response, "envelope-shape", SweepKind::Measured).unwrap_or_else(
            |error| panic!("{code} is rerank_completion's to judge, not the signal check: {error}"),
        );
    }
}

#[test]
fn advisory_warnings_do_not_fail_the_eval() {
    for code in ADVISORY_WARNING_CODES {
        let response = response_with_warning(code, "informational");
        ensure_signals_intact(&response, "envelope-shape", SweepKind::Measured).unwrap_or_else(
            |error| panic!("{code} is advisory and must not fail the eval: {error}"),
        );
    }
}

/// Collects every `code: "<literal>"` a Rust source constructs.
fn collect_warning_code_literals(text: &str, found: &mut BTreeSet<String>) {
    const MARKER: &str = "code: \"";
    let mut rest = text;
    while let Some(start) = rest.find(MARKER) {
        let after = &rest[start + MARKER.len()..];
        let Some(end) = after.find('"') else { break };
        found.insert(after[..end].to_string());
        rest = &after[end..];
    }
}

/// Every `Warning.code` the workspace's own sources construct, read from disk.
///
/// This is what makes `PRODUCER_WARNING_CODES` a pin rather than a wish: the
/// list is checked against the producers instead of against itself, so adding a
/// warning code anywhere under `crates/*/src` fails a test here until it is
/// classified.
fn scan_producer_warning_codes() -> BTreeSet<String> {
    fn visit(dir: &Path, found: &mut BTreeSet<String>) {
        let entries =
            fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("read dir entry").path();
            if path.is_dir() {
                visit(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                collect_warning_code_literals(&text, found);
            }
        }
    }

    let crates = repo_root().join("crates");
    let mut found = BTreeSet::new();
    let entries =
        fs::read_dir(&crates).unwrap_or_else(|error| panic!("read {}: {error}", crates.display()));
    for entry in entries {
        let src = entry.expect("read crate dir entry").path().join("src");
        if src.is_dir() {
            visit(&src, &mut found);
        }
    }
    assert!(
        !found.is_empty(),
        "scanned {} and found no warning codes at all — the scan is broken, not the sources",
        crates.display()
    );
    found
}

#[test]
fn producer_warning_codes_match_the_workspace_sources() {
    let declared: BTreeSet<String> = PRODUCER_WARNING_CODES
        .iter()
        .map(|code| (*code).to_string())
        .collect();
    assert_eq!(
        scan_producer_warning_codes(),
        declared,
        "PRODUCER_WARNING_CODES is out of date with crates/*/src. Every warning \
         code a producer emits must be classified into DEGRADED_SIGNAL_CODES, \
         RERANK_FALLBACK_CODES, or ADVISORY_WARNING_CODES — an unclassified code \
         reaches the eval gate and is silently tolerated as a valid measurement."
    );
}

#[test]
fn producer_warning_codes_are_classified_exactly_once() {
    for code in PRODUCER_WARNING_CODES {
        let mut classes = Vec::new();
        if DEGRADED_SIGNAL_CODES.contains(&code) {
            classes.push("degraded");
        }
        if RERANK_FALLBACK_CODES.contains(&code) {
            classes.push("rerank-fallback");
        }
        if ADVISORY_WARNING_CODES.contains(&code) {
            classes.push("advisory");
        }
        assert_eq!(
            classes.len(),
            1,
            "{code} must belong to exactly one warning class, found {classes:?}"
        );
    }
}

#[test]
fn stale_baseline_schema_is_reported_as_a_version_mismatch() {
    let error = ensure_baseline_schema_current(br#"{"schema_version": 3}"#)
        .expect_err("a schema-3 baseline must be rejected by the version check");
    assert!(
        error.to_string().contains("regenerate the baseline"),
        "error must name the schema move: {error}"
    );
    ensure_baseline_schema_current(br#"{"schema_version": 4}"#)
        .expect("the current schema must pass the probe");
}
