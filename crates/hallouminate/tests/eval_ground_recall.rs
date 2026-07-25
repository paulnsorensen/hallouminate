//! Model-backed ground retrieval evaluation for issue #288.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hallouminate_config::{Config, EmbeddingsConfig, SearchConfig, StorageConfig};
use hallouminate_daemon::{
    DaemonRequest, DaemonRequestPayload, DaemonResponse, GroundRequest, GroundResult, IndexReport,
    IndexRequest, connect_at,
};
use hallouminate_domain::common::CorpusConfig;
use hallouminate_domain::corpus::{blake3_bytes, load_tokenizer, scan};
use hallouminate_domain::ground::{DocFile, GroundResponse};
use hallouminate_domain::indexer::{Format, HandlerRegistry, PrepareCtx};
use hallouminate_domain::search::{DEFAULT_CROSSENCODER_MODEL, SUPPORTED_CROSSENCODER_MODELS};
use serde::{Deserialize, Serialize};
use text_splitter::{Characters, ChunkSizer};

#[path = "it/common/mod.rs"]
mod common;
use common::daemon::DaemonHarness;

const SCHEMA_VERSION: u32 = 1;
const CORPUS_NAME: &str = "eval-wiki";
const LEXICAL_BASELINE_ID: &str = "lexical-without-rerank";
const BASELINE_ID: &str = "fusion-without-rerank";
const EMBEDDING_MODEL: &str = "BAAI/bge-small-en-v1.5";
const CHUNK_BUDGET_TOKENS: usize = 384;
const REAL_EMBED_CACHE: &str = "~/.cache/hallouminate/fastembed";
const RERANK_TIMEOUT_MS: u64 = 300_000;
const GROUND_RPC_TIMEOUT_MS: u64 = RERANK_TIMEOUT_MS + 60_000;
const MIN_MRR_GAIN: f64 = 0.05;
const MAX_ADDED_P50_MS: i64 = 500;
const FOOTNOTE_INVERSION_ID: &str = "footnote-inversion";

#[derive(Debug, Clone, Copy)]
struct CandidateDefinition {
    id: &'static str,
    weight_bytes: u64,
}

const CANDIDATE_DEFINITIONS: &[CandidateDefinition] = &[
    CandidateDefinition {
        id: "bge-reranker-base",
        weight_bytes: 1_112_459_588,
    },
    CandidateDefinition {
        id: "bge-reranker-v2-m3",
        weight_bytes: 2_271_197_135,
    },
    CandidateDefinition {
        id: "jina-reranker-v1-turbo-en",
        weight_bytes: 151_296_975,
    },
    CandidateDefinition {
        id: "jina-reranker-v2-base-multiligual",
        weight_bytes: 1_114_040_223,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CandidateSpec {
    id: String,
    weight_bytes: u64,
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
struct Metrics {
    recall_at_5: f64,
    mrr: f64,
    p50_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueryMeasurement {
    id: String,
    latency_ms: u64,
    rank: Option<usize>,
    expected_top: ChunkIdentity,
    actual_top: Option<ChunkIdentity>,
    top_chunk_pass: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_signal: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct VariantMeasurement {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weight_bytes: Option<u64>,
    metrics: Metrics,
    qualifies: bool,
    mrr_gain: f64,
    added_p50_ms: i64,
    queries: Vec<QueryMeasurement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DecisionOutcome {
    Selected,
    NoneQualified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Decision {
    outcome: DecisionOutcome,
    selected_model: Option<String>,
    default_crossencoder: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Thresholds {
    min_mrr_gain: f64,
    max_added_p50_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EvalArtifact {
    schema_version: u32,
    query_set_digest: String,
    thresholds: Thresholds,
    candidates: Vec<CandidateSpec>,
    variants: Vec<VariantMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<Decision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalMode {
    Lexical,
    Fusion,
}

impl RetrievalMode {
    fn baseline_id(self) -> &'static str {
        match self {
            Self::Lexical => LEXICAL_BASELINE_ID,
            Self::Fusion => BASELINE_ID,
        }
    }

    fn embeddings_enabled(self) -> bool {
        match self {
            Self::Lexical => false,
            Self::Fusion => true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct VariantSpec<'a> {
    id: &'a str,
    model: Option<&'a str>,
    weight_bytes: Option<u64>,
    mode: RetrievalMode,
    selection_candidate: bool,
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

fn candidate_specs() -> Vec<CandidateSpec> {
    CANDIDATE_DEFINITIONS
        .iter()
        .map(|candidate| CandidateSpec {
            id: candidate.id.to_string(),
            weight_bytes: candidate.weight_bytes,
        })
        .collect()
}

fn variant_specs() -> Vec<VariantSpec<'static>> {
    vec![
        VariantSpec {
            id: BASELINE_ID,
            model: None,
            weight_bytes: None,
            mode: RetrievalMode::Fusion,
            selection_candidate: false,
        },
        VariantSpec {
            id: LEXICAL_BASELINE_ID,
            model: None,
            weight_bytes: None,
            mode: RetrievalMode::Lexical,
            selection_candidate: false,
        },
        VariantSpec {
            id: "lexical-with-bge-reranker-base",
            model: Some("bge-reranker-base"),
            weight_bytes: Some(1_112_459_588),
            mode: RetrievalMode::Lexical,
            selection_candidate: false,
        },
        VariantSpec {
            id: "fusion-with-bge-reranker-base",
            model: Some("bge-reranker-base"),
            weight_bytes: Some(1_112_459_588),
            mode: RetrievalMode::Fusion,
            selection_candidate: true,
        },
        VariantSpec {
            id: "lexical-with-bge-reranker-v2-m3",
            model: Some("bge-reranker-v2-m3"),
            weight_bytes: Some(2_271_197_135),
            mode: RetrievalMode::Lexical,
            selection_candidate: false,
        },
        VariantSpec {
            id: "fusion-with-bge-reranker-v2-m3",
            model: Some("bge-reranker-v2-m3"),
            weight_bytes: Some(2_271_197_135),
            mode: RetrievalMode::Fusion,
            selection_candidate: true,
        },
        VariantSpec {
            id: "lexical-with-jina-reranker-v1-turbo-en",
            model: Some("jina-reranker-v1-turbo-en"),
            weight_bytes: Some(151_296_975),
            mode: RetrievalMode::Lexical,
            selection_candidate: false,
        },
        VariantSpec {
            id: "fusion-with-jina-reranker-v1-turbo-en",
            model: Some("jina-reranker-v1-turbo-en"),
            weight_bytes: Some(151_296_975),
            mode: RetrievalMode::Fusion,
            selection_candidate: true,
        },
        VariantSpec {
            id: "lexical-with-jina-reranker-v2-base-multiligual",
            model: Some("jina-reranker-v2-base-multiligual"),
            weight_bytes: Some(1_114_040_223),
            mode: RetrievalMode::Lexical,
            selection_candidate: false,
        },
        VariantSpec {
            id: "fusion-with-jina-reranker-v2-base-multiligual",
            model: Some("jina-reranker-v2-base-multiligual"),
            weight_bytes: Some(1_114_040_223),
            mode: RetrievalMode::Fusion,
            selection_candidate: true,
        },
    ]
}

fn variant_spec(id: &str) -> Option<VariantSpec<'static>> {
    variant_specs().into_iter().find(|spec| spec.id == id)
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

    let footnote = queries
        .iter()
        .find(|query| query.id == FOOTNOTE_INVERSION_ID)
        .expect("required id checked above");
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
        let format = path
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| extension.eq_ignore_ascii_case("md"))
            .map(|_| Format::Markdown)
            .with_context(|| format!("fixture is not markdown: {}", path.display()))?;
        let prepared = registry.handler(format).prepare(&PrepareCtx {
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
    let tokenizer = load_tokenizer(EMBEDDING_MODEL)
        .with_context(|| format!("load {EMBEDDING_MODEL} tokenizer for label preflight"))?;
    validate_fixture_labels(queries, tokenizer)
}

fn build_config(variant: VariantSpec<'_>, ground_dir: &Path) -> Config {
    Config {
        corpora: vec![fixture_corpus()],
        search: SearchConfig {
            crossencoder: variant.model.map(str::to_string),
            rerank_timeout_ms: RERANK_TIMEOUT_MS,
            ..Default::default()
        },
        embeddings: EmbeddingsConfig {
            enabled: variant.mode.embeddings_enabled(),
            model: EMBEDDING_MODEL.into(),
            quantized: true,
            cache_dir: REAL_EMBED_CACHE.into(),
            ..Default::default()
        },
        storage: StorageConfig {
            ground_dir: ground_dir.to_string_lossy().into_owned(),
        },
        ..Default::default()
    }
}

fn ranked_docs(docs: &BTreeMap<String, DocFile>) -> Vec<(&String, &DocFile)> {
    let mut ranked: Vec<_> = docs.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    ranked
}

fn rank_of_expected(ranked: &[(&String, &DocFile)], expected_file: &str) -> Option<usize> {
    ranked
        .iter()
        .position(|(absolute_path, doc)| {
            doc.path.as_deref() == Some(expected_file) || absolute_path.ends_with(expected_file)
        })
        .map(|index| index + 1)
}

fn top_identity(ranked: &[(&String, &DocFile)]) -> Option<ChunkIdentity> {
    let (absolute_path, doc) = ranked.first()?;
    let chunk = doc.chunks.first()?;
    let file = doc.path.clone().or_else(|| {
        Path::new(absolute_path.as_str())
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    })?;
    Some(ChunkIdentity {
        file,
        heading_path: chunk.heading_path.clone(),
        line_start: chunk.line_range[0],
    })
}

async fn index_fixture(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
    variant: VariantSpec<'_>,
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
        .with_context(|| format!("index fixture for {}", variant.id))?;
    Ok(())
}

async fn ground_query(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
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
            Duration::from_millis(GROUND_RPC_TIMEOUT_MS),
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

async fn run_sweep(
    client: &hallouminate_daemon::DaemonClient,
    cwd: &Path,
    variant: VariantSpec<'_>,
    queries: &[LabelledQuery],
) -> Result<Vec<QueryMeasurement>> {
    let mut measurements = Vec::with_capacity(queries.len());
    for query in queries {
        let response = ground_query(client, cwd, query).await?;
        let ranked = ranked_docs(&response.docs);
        let rank = rank_of_expected(&ranked, &query.expected_chunk.file);
        let actual_top = top_identity(&ranked);
        let rerank_signal = ranked
            .first()
            .and_then(|(_, doc)| doc.z_score.or_else(|| doc.chunks.first()?.z_score));
        if variant.model.is_some() {
            ensure!(
                rerank_signal.is_some(),
                "{} query {} has no rerank signal; timeout fallback is an incomplete measurement",
                variant.id,
                query.id
            );
        } else {
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
            rerank_signal,
        });
    }
    Ok(measurements)
}

async fn run_variant(
    variant: VariantSpec<'_>,
    queries: &[LabelledQuery],
) -> Result<VariantMeasurement> {
    let tmp = tempfile::tempdir().context("create variant tempdir")?;
    let ground_dir = tmp.path().join("ground");
    let harness = DaemonHarness::spawn(build_config(variant, &ground_dir)).await;
    let client = connect_at(harness.socket())
        .await
        .with_context(|| format!("connect eval daemon for {}", variant.id))?;
    index_fixture(&client, harness.cwd(), variant).await?;

    let _warmup = run_sweep(&client, harness.cwd(), variant, queries).await?;
    let measured = run_sweep(&client, harness.cwd(), variant, queries).await?;
    drop(client);
    harness.shutdown().await.context("shutdown eval daemon")?;

    let metrics = metrics_for(&measured)?;
    Ok(VariantMeasurement {
        id: variant.id.to_string(),
        model: variant.model.map(str::to_string),
        weight_bytes: variant.weight_bytes,
        metrics,
        qualifies: false,
        mrr_gain: 0.0,
        added_p50_ms: 0,
        queries: measured,
    })
}

async fn measure_all(queries: &[LabelledQuery], query_set_digest: String) -> Result<EvalArtifact> {
    validate_fixture_labels_with_production_tokenizer(queries)?;

    let mut variants = Vec::new();
    for variant in variant_specs() {
        variants.push(run_variant(variant, queries).await?);
    }
    apply_baseline_deltas(&mut variants)?;
    let artifact = EvalArtifact {
        schema_version: SCHEMA_VERSION,
        query_set_digest,
        thresholds: locked_thresholds(),
        candidates: candidate_specs(),
        variants,
        decision: None,
    };
    validate_artifact(&artifact)?;
    Ok(artifact)
}

fn locked_thresholds() -> Thresholds {
    Thresholds {
        min_mrr_gain: MIN_MRR_GAIN,
        max_added_p50_ms: MAX_ADDED_P50_MS,
    }
}

fn nearest_rank_p50(latencies: &[u64]) -> Result<u64> {
    ensure!(
        !latencies.is_empty(),
        "cannot compute p50 of an empty sweep"
    );
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    Ok(sorted[(sorted.len() - 1) / 2])
}

fn metrics_for(queries: &[QueryMeasurement]) -> Result<Metrics> {
    ensure!(!queries.is_empty(), "variant has no query measurements");
    let recall_hits = queries
        .iter()
        .filter(|measurement| measurement.rank.is_some_and(|rank| rank <= 5))
        .count();
    let reciprocal_sum: f64 = queries
        .iter()
        .map(|measurement| {
            measurement
                .rank
                .map(|rank| 1.0 / rank as f64)
                .unwrap_or(0.0)
        })
        .sum();
    let latencies: Vec<u64> = queries
        .iter()
        .map(|measurement| measurement.latency_ms)
        .collect();
    Ok(Metrics {
        recall_at_5: recall_hits as f64 / queries.len() as f64,
        mrr: reciprocal_sum / queries.len() as f64,
        p50_ms: nearest_rank_p50(&latencies)?,
    })
}

fn added_p50(candidate: u64, baseline: u64) -> Result<i64> {
    let candidate = i64::try_from(candidate).context("candidate p50 exceeds i64")?;
    let baseline = i64::try_from(baseline).context("baseline p50 exceeds i64")?;
    Ok(candidate - baseline)
}

fn candidate_qualifies(mrr_gain: f64, added_p50_ms: i64) -> bool {
    mrr_gain >= MIN_MRR_GAIN && added_p50_ms <= MAX_ADDED_P50_MS
}

fn apply_baseline_deltas(variants: &mut [VariantMeasurement]) -> Result<()> {
    let baselines: BTreeMap<&str, Metrics> = [LEXICAL_BASELINE_ID, BASELINE_ID]
        .into_iter()
        .map(|id| {
            let metrics = variants
                .iter()
                .find(|variant| variant.id == id)
                .with_context(|| format!("missing {id} baseline"))?
                .metrics
                .clone();
            Ok((id, metrics))
        })
        .collect::<Result<_>>()?;

    for variant in variants {
        let spec =
            variant_spec(&variant.id).with_context(|| format!("unknown variant {}", variant.id))?;
        let baseline_id = spec.mode.baseline_id();
        if variant.id == baseline_id {
            variant.mrr_gain = 0.0;
            variant.added_p50_ms = 0;
            variant.qualifies = false;
            continue;
        }
        let baseline = baselines
            .get(baseline_id)
            .with_context(|| format!("missing {baseline_id} metrics"))?;
        variant.mrr_gain = variant.metrics.mrr - baseline.mrr;
        variant.added_p50_ms = added_p50(variant.metrics.p50_ms, baseline.p50_ms)?;
        variant.qualifies =
            spec.selection_candidate && candidate_qualifies(variant.mrr_gain, variant.added_p50_ms);
    }
    Ok(())
}

fn validate_candidate_inventory(candidates: &[CandidateSpec]) -> Result<()> {
    ensure!(
        candidates == candidate_specs(),
        "candidate inventory or weight bytes changed"
    );
    let actual: BTreeSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    let supported: BTreeSet<&str> = SUPPORTED_CROSSENCODER_MODELS.iter().copied().collect();
    ensure!(
        actual == supported,
        "candidate inventory does not exactly match pinned fastembed support"
    );
    ensure!(
        actual.len() == candidates.len(),
        "candidate inventory contains duplicates"
    );
    Ok(())
}

fn validate_artifact(artifact: &EvalArtifact) -> Result<()> {
    ensure!(
        artifact.schema_version == SCHEMA_VERSION,
        "unsupported eval schema"
    );
    ensure!(
        !artifact.query_set_digest.is_empty(),
        "missing query-set digest"
    );
    ensure!(
        artifact.thresholds == locked_thresholds(),
        "qualification thresholds changed"
    );
    validate_candidate_inventory(&artifact.candidates)?;

    let specs = variant_specs();
    ensure!(
        artifact.variants.len() == specs.len(),
        "missing or extra variant results"
    );
    let mut seen_variants = BTreeSet::new();
    let baseline = artifact
        .variants
        .iter()
        .find(|variant| variant.id == BASELINE_ID)
        .context("missing fusion-without-rerank results")?;
    let baseline_query_ids: BTreeSet<&str> = baseline
        .queries
        .iter()
        .map(|measurement| measurement.id.as_str())
        .collect();
    ensure!(
        baseline_query_ids.len() == baseline.queries.len(),
        "duplicate baseline query results"
    );
    let baseline_labels: BTreeMap<&str, ChunkIdentity> = baseline
        .queries
        .iter()
        .map(|measurement| (measurement.id.as_str(), measurement.expected_top.clone()))
        .collect();

    for spec in specs {
        let variant = artifact
            .variants
            .iter()
            .find(|variant| variant.id == spec.id)
            .with_context(|| format!("missing variant {}", spec.id))?;
        ensure!(
            seen_variants.insert(variant.id.as_str()),
            "duplicate variant {}",
            variant.id
        );
        ensure!(
            variant.model.as_deref() == spec.model,
            "variant {} model mismatch",
            variant.id
        );
        ensure!(
            variant.weight_bytes == spec.weight_bytes,
            "variant {} weight bytes mismatch",
            variant.id
        );
        let query_ids: BTreeSet<&str> = variant
            .queries
            .iter()
            .map(|measurement| measurement.id.as_str())
            .collect();
        ensure!(
            query_ids.len() == variant.queries.len(),
            "variant {} has duplicate queries",
            variant.id
        );
        ensure!(
            query_ids == baseline_query_ids,
            "variant {} has missing or extra query results",
            variant.id
        );
        for measurement in &variant.queries {
            ensure!(
                baseline_labels.get(measurement.id.as_str()) == Some(&measurement.expected_top),
                "variant {} query {} has a different expected chunk",
                variant.id,
                measurement.id
            );
            ensure!(
                measurement.top_chunk_pass
                    == (measurement.actual_top.as_ref() == Some(&measurement.expected_top)),
                "variant {} query {} has an inconsistent top-chunk result",
                variant.id,
                measurement.id
            );
        }
        ensure!(
            variant.metrics == metrics_for(&variant.queries)?,
            "variant {} metrics do not match query results",
            variant.id
        );

        let footnote = variant
            .queries
            .iter()
            .find(|measurement| measurement.id == FOOTNOTE_INVERSION_ID)
            .with_context(|| format!("variant {} is missing footnote-inversion", variant.id))?;
        ensure!(
            footnote.expected_top
                == (ChunkIdentity {
                    file: "architecture.md".into(),
                    heading_path: vec!["Architecture".into()],
                    line_start: 1,
                }),
            "variant {} has the wrong footnote-inversion label",
            variant.id
        );
        ensure!(
            footnote.top_chunk_pass,
            "variant {} failed footnote-inversion",
            variant.id
        );

        if spec.model.is_some() {
            ensure!(
                variant
                    .queries
                    .iter()
                    .all(|measurement| measurement.rerank_signal.is_some()),
                "variant {} has an incomplete rerank sweep",
                variant.id
            );
            let mode_baseline = artifact
                .variants
                .iter()
                .find(|candidate| candidate.id == spec.mode.baseline_id())
                .with_context(|| format!("missing {} results", spec.mode.baseline_id()))?;
            let expected_gain = variant.metrics.mrr - mode_baseline.metrics.mrr;
            let expected_added = added_p50(variant.metrics.p50_ms, mode_baseline.metrics.p50_ms)?;
            ensure!(
                (variant.mrr_gain - expected_gain).abs() < f64::EPSILON,
                "variant {} has an invalid MRR gain",
                variant.id
            );
            ensure!(
                variant.added_p50_ms == expected_added,
                "variant {} has invalid added p50",
                variant.id
            );
            let expected_qualification =
                spec.selection_candidate && candidate_qualifies(expected_gain, expected_added);
            ensure!(
                variant.qualifies == expected_qualification,
                "variant {} has an invalid qualification result",
                variant.id
            );
        } else {
            ensure!(
                variant
                    .queries
                    .iter()
                    .all(|measurement| measurement.rerank_signal.is_none()),
                "baseline {} has a rerank signal",
                variant.id
            );
            ensure!(
                variant.id == spec.mode.baseline_id(),
                "non-reranked variant {} is not its mode baseline",
                variant.id
            );
            ensure!(!variant.qualifies && variant.mrr_gain == 0.0 && variant.added_p50_ms == 0);
        }
    }
    Ok(())
}

fn compare_candidate_order(left: &VariantMeasurement, right: &VariantMeasurement) -> Ordering {
    left.added_p50_ms
        .cmp(&right.added_p50_ms)
        .then_with(|| left.weight_bytes.cmp(&right.weight_bytes))
        .then_with(|| left.id.cmp(&right.id))
}

fn ordered_qualified_candidates(artifact: &EvalArtifact) -> Vec<&VariantMeasurement> {
    let mut candidates: Vec<_> = artifact
        .variants
        .iter()
        .filter(|variant| {
            variant.qualifies
                && variant_spec(&variant.id).is_some_and(|spec| spec.selection_candidate)
        })
        .collect();
    candidates.sort_by(|left, right| compare_candidate_order(left, right));
    candidates
}

fn compare_against_baseline(committed: &EvalArtifact, current: &EvalArtifact) -> Result<()> {
    validate_artifact(committed).context("committed baseline is invalid")?;
    validate_artifact(current).context("current measurement is invalid")?;
    ensure!(
        committed.query_set_digest == current.query_set_digest,
        "query-set digest changed; remeasure and explicitly update the baseline"
    );
    ensure!(
        committed.candidates == current.candidates,
        "candidate inventory changed"
    );
    ensure!(
        committed.thresholds == current.thresholds,
        "qualification contract changed"
    );

    for committed_variant in &committed.variants {
        let current_variant = current
            .variants
            .iter()
            .find(|variant| variant.id == committed_variant.id)
            .with_context(|| format!("current run is missing {}", committed_variant.id))?;
        ensure!(
            current_variant.metrics.recall_at_5 + f64::EPSILON
                >= committed_variant.metrics.recall_at_5,
            "{} Recall@5 regressed from {} to {}",
            committed_variant.id,
            committed_variant.metrics.recall_at_5,
            current_variant.metrics.recall_at_5
        );
        ensure!(
            current_variant.metrics.mrr + f64::EPSILON >= committed_variant.metrics.mrr,
            "{} MRR regressed from {} to {}",
            committed_variant.id,
            committed_variant.metrics.mrr,
            current_variant.metrics.mrr
        );
        for committed_query in &committed_variant.queries {
            let current_query = current_variant
                .queries
                .iter()
                .find(|measurement| measurement.id == committed_query.id)
                .with_context(|| {
                    format!(
                        "{} is missing query {}",
                        current_variant.id, committed_query.id
                    )
                })?;
            ensure!(
                !committed_query.top_chunk_pass || current_query.top_chunk_pass,
                "{} query {} regressed from top-chunk pass to fail",
                current_variant.id,
                current_query.id
            );
        }
    }
    Ok(())
}

fn cli_template_crossencoder_settings() -> Result<(Option<String>, Vec<String>)> {
    let path = repo_root().join("crates/hallouminate/src/cli/config-default.toml");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read CLI config template {}", path.display()))?;
    let parsed: toml::Value = toml::from_str(&text).context("parse CLI config template")?;
    let active = parsed
        .get("search")
        .and_then(|search| search.get("crossencoder"))
        .map(|value| {
            value
                .as_str()
                .context("active CLI crossencoder default is not a string")
                .map(str::to_string)
        })
        .transpose()?;

    let mut commented = Vec::new();
    for line in text.lines() {
        let Some(comment) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let assignment = comment.trim();
        let Some((key, _)) = assignment.split_once('=') else {
            continue;
        };
        if key.trim() != "crossencoder" {
            continue;
        }
        let value: toml::Value = toml::from_str(&format!("[search]\n{assignment}"))
            .context("parse commented CLI crossencoder opt-in")?;
        let model = value
            .get("search")
            .and_then(|search| search.get("crossencoder"))
            .and_then(toml::Value::as_str)
            .context("commented CLI crossencoder opt-in is not a string")?;
        commented.push(model.to_string());
    }
    Ok((active, commented))
}

fn validate_recorded_decision(baseline: &EvalArtifact) -> Result<()> {
    let decision = baseline
        .decision
        .as_ref()
        .context("baseline has no recorded decision")?;
    let qualified = ordered_qualified_candidates(baseline);
    let runtime_default = SearchConfig::default().crossencoder;
    let (template_active, template_commented) = cli_template_crossencoder_settings()?;
    match decision.outcome {
        DecisionOutcome::Selected => {
            let selected = decision
                .selected_model
                .as_deref()
                .context("selected decision has no model")?;
            ensure!(
                decision.default_crossencoder.as_deref() == Some(selected),
                "selected decision default disagrees with its model"
            );
            let winner = qualified
                .first()
                .context("selected decision has no qualifying candidate")?;
            ensure!(
                winner.model.as_deref() == Some(selected),
                "recorded selection is not the deterministic fusion winner"
            );
            ensure!(
                runtime_default.as_deref() == Some(selected),
                "SearchConfig default disagrees with selected model"
            );
            ensure!(
                DEFAULT_CROSSENCODER_MODEL == selected,
                "domain default model disagrees with selected model"
            );
            ensure!(
                template_active.as_deref() == Some(selected),
                "active CLI template default disagrees with selected model"
            );
        }
        DecisionOutcome::NoneQualified => {
            ensure!(
                qualified.is_empty(),
                "none-qualified decision has qualifying fusion candidates"
            );
            ensure!(decision.selected_model.is_none());
            ensure!(decision.default_crossencoder.is_none());
            ensure!(
                runtime_default.is_none(),
                "SearchConfig must keep reranking disabled by default"
            );
            ensure!(
                template_active.is_none(),
                "CLI template must not activate a crossencoder default"
            );
            ensure!(
                SUPPORTED_CROSSENCODER_MODELS.contains(&DEFAULT_CROSSENCODER_MODEL),
                "domain opt-in default is not a supported model"
            );
            ensure!(
                template_commented == [DEFAULT_CROSSENCODER_MODEL.to_string()],
                "CLI template must document only the domain opt-in model as commented"
            );
        }
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(artifact).context("serialize eval artifact")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[test]
fn wiki_authoring_guidance_is_chunk_contextual() {
    let root = repo_root();
    let paths = [
        root.join("plugins/hallouminate/skills/wiki-ingest/SKILL.md"),
        root.join(".hallouminate/wiki/wiki-conventions.md"),
    ];
    for path in paths {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
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
}

#[test]
fn query_labels_are_complete_and_digest_is_stable() {
    let (queries, digest) = load_queries().expect("load eval queries");
    assert_eq!(queries.len(), 12);
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
fn nearest_rank_p50_uses_lower_middle_for_even_sweeps() {
    assert_eq!(nearest_rank_p50(&[40, 10, 30, 20]).expect("p50"), 20);
    assert_eq!(nearest_rank_p50(&[30, 10, 20]).expect("p50"), 20);
    assert!(nearest_rank_p50(&[]).is_err());
}

#[test]
fn ground_rpc_deadline_exceeds_rerank_timeout() {
    assert!(GROUND_RPC_TIMEOUT_MS > RERANK_TIMEOUT_MS);
}
#[test]
fn candidate_inventory_exactly_matches_pinned_fastembed() {
    validate_candidate_inventory(&candidate_specs()).expect("candidate inventory");
    assert_eq!(candidate_specs()[0].weight_bytes, 1_112_459_588);
    assert_eq!(candidate_specs()[1].weight_bytes, 2_271_197_135);
    assert_eq!(candidate_specs()[2].weight_bytes, 151_296_975);
    assert_eq!(candidate_specs()[3].weight_bytes, 1_114_040_223);
}

#[test]
fn reporting_matrix_has_exact_lexical_and_fusion_variants() {
    let specs = variant_specs();
    let ids: Vec<&str> = specs.iter().map(|spec| spec.id).collect();
    assert_eq!(
        ids,
        [
            "fusion-without-rerank",
            "lexical-without-rerank",
            "lexical-with-bge-reranker-base",
            "fusion-with-bge-reranker-base",
            "lexical-with-bge-reranker-v2-m3",
            "fusion-with-bge-reranker-v2-m3",
            "lexical-with-jina-reranker-v1-turbo-en",
            "fusion-with-jina-reranker-v1-turbo-en",
            "lexical-with-jina-reranker-v2-base-multiligual",
            "fusion-with-jina-reranker-v2-base-multiligual",
        ]
    );
    assert_eq!(
        specs.iter().filter(|spec| spec.selection_candidate).count(),
        4
    );
    for spec in specs {
        if spec.selection_candidate {
            assert_eq!(spec.mode, RetrievalMode::Fusion);
            assert!(spec.model.is_some());
        } else if spec.mode == RetrievalMode::Lexical {
            assert!(!spec.selection_candidate);
        }
    }
}

fn ordering_fixture(id: &str, added_p50_ms: i64, weight_bytes: u64) -> VariantMeasurement {
    VariantMeasurement {
        id: id.into(),
        model: Some(id.into()),
        weight_bytes: Some(weight_bytes),
        metrics: Metrics {
            recall_at_5: 1.0,
            mrr: 1.0,
            p50_ms: 1,
        },
        qualifies: true,
        mrr_gain: MIN_MRR_GAIN,
        added_p50_ms,
        queries: Vec::new(),
    }
}

#[test]
fn qualification_thresholds_and_ordering_are_deterministic() {
    assert!(candidate_qualifies(0.05, 500));
    assert!(!candidate_qualifies(0.049, 500));
    assert!(!candidate_qualifies(0.05, 501));

    let mut variants = vec![
        ordering_fixture("z-model", 100, 20),
        ordering_fixture("a-model", 100, 20),
        ordering_fixture("slow-small", 101, 1),
        ordering_fixture("fast-large", 99, 1_000),
    ];
    variants.sort_by(compare_candidate_order);
    let ids: Vec<&str> = variants.iter().map(|variant| variant.id.as_str()).collect();
    assert_eq!(ids, ["fast-large", "a-model", "z-model", "slow-small"]);

    let mut artifact = synthetic_artifact();
    for id in [LEXICAL_BASELINE_ID, BASELINE_ID] {
        artifact
            .variants
            .iter_mut()
            .find(|variant| variant.id == id)
            .expect("mode baseline")
            .metrics
            .mrr = 0.5;
    }
    for id in [
        "lexical-with-bge-reranker-base",
        "fusion-with-bge-reranker-base",
    ] {
        artifact
            .variants
            .iter_mut()
            .find(|variant| variant.id == id)
            .expect("reranked variant")
            .metrics
            .mrr = 0.6;
    }
    apply_baseline_deltas(&mut artifact.variants).expect("apply mode deltas");
    let lexical = artifact
        .variants
        .iter()
        .find(|variant| variant.id == "lexical-with-bge-reranker-base")
        .expect("lexical variant");
    let fusion = artifact
        .variants
        .iter()
        .find(|variant| variant.id == "fusion-with-bge-reranker-base")
        .expect("fusion variant");
    assert!(!lexical.qualifies);
    assert!(fusion.qualifies);
}

fn synthetic_query(id: &str, rerank_signal: Option<f64>) -> QueryMeasurement {
    let expected = if id == FOOTNOTE_INVERSION_ID {
        ChunkIdentity {
            file: "architecture.md".into(),
            heading_path: vec!["Architecture".into()],
            line_start: 1,
        }
    } else {
        ChunkIdentity {
            file: "other.md".into(),
            heading_path: vec!["Other".into()],
            line_start: 1,
        }
    };
    QueryMeasurement {
        id: id.into(),
        latency_ms: 10,
        rank: Some(1),
        expected_top: expected.clone(),
        actual_top: Some(expected),
        top_chunk_pass: true,
        rerank_signal,
    }
}

fn synthetic_artifact() -> EvalArtifact {
    let mut variants: Vec<VariantMeasurement> = variant_specs()
        .into_iter()
        .map(|spec| {
            let signal = spec.model.map(|_| 2.0);
            let queries = vec![
                synthetic_query(FOOTNOTE_INVERSION_ID, signal),
                synthetic_query("other", signal),
            ];
            VariantMeasurement {
                id: spec.id.into(),
                model: spec.model.map(str::to_string),
                weight_bytes: spec.weight_bytes,
                metrics: metrics_for(&queries).expect("metrics"),
                qualifies: false,
                mrr_gain: 0.0,
                added_p50_ms: 0,
                queries,
            }
        })
        .collect();
    apply_baseline_deltas(&mut variants).expect("deltas");
    EvalArtifact {
        schema_version: SCHEMA_VERSION,
        query_set_digest: "digest".into(),
        thresholds: locked_thresholds(),
        candidates: candidate_specs(),
        variants,
        decision: None,
    }
}

#[test]
fn none_qualified_decision_matches_runtime_and_cli_opt_in_defaults() {
    let mut baseline = synthetic_artifact();
    baseline.decision = Some(Decision {
        outcome: DecisionOutcome::NoneQualified,
        selected_model: None,
        default_crossencoder: None,
    });
    validate_recorded_decision(&baseline).expect("none-qualified defaults agree");
}

#[test]
fn comparator_rejects_metric_and_top_chunk_regressions() {
    let committed = synthetic_artifact();

    let mut metric_regression = committed.clone();
    let candidate = metric_regression
        .variants
        .iter_mut()
        .find(|variant| variant.id == "fusion-with-bge-reranker-base")
        .expect("candidate");
    candidate.queries[1].rank = None;
    candidate.metrics = metrics_for(&candidate.queries).expect("metrics");
    apply_baseline_deltas(&mut metric_regression.variants).expect("deltas");
    assert!(compare_against_baseline(&committed, &metric_regression).is_err());

    let mut chunk_regression = committed.clone();
    let candidate = chunk_regression
        .variants
        .iter_mut()
        .find(|variant| variant.id == "fusion-with-bge-reranker-base")
        .expect("candidate");
    candidate.queries[1].actual_top = None;
    candidate.queries[1].top_chunk_pass = false;
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

    write_artifact(&output, &synthetic_artifact()).expect("write output");

    assert_eq!(fs::read(&baseline).expect("read baseline after"), before);
    assert!(output.is_file());
    let entries: Vec<_> = fs::read_dir(temp.path().join(".context"))
        .expect("read output dir")
        .collect();
    assert_eq!(entries.len(), 1);
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

    let error = write_measurement_artifact(&alias, &baseline, &synthetic_artifact())
        .expect_err("baseline alias must be rejected");

    assert!(
        error
            .to_string()
            .contains("HALLOUMINATE_EVAL_OUTPUT resolves to eval/baseline.json")
    );
    assert_eq!(fs::read(&baseline).expect("read baseline after"), before);
}

#[test]
fn relative_measurement_output_resolves_against_repo_root() {
    let output = measurement_output_path(Path::new(".context/result.json"));
    let expected = repo_root().join(".context/result.json");

    assert_eq!(output, expected);
}

#[tokio::test]
#[ignore = "downloads and measures every pinned fastembed reranker"]
async fn eval_ground_recall_measure() -> Result<()> {
    let output = env::var_os("HALLOUMINATE_EVAL_OUTPUT")
        .context("HALLOUMINATE_EVAL_OUTPUT must name the proposed measurement artifact")?;
    let output = measurement_output_path(Path::new(&output));
    let baseline = baseline_path();
    ensure_measurement_output_is_not_baseline(&output, &baseline)?;
    let baseline_before = read_optional(&baseline)?;
    let (queries, query_set_digest) = load_queries()?;
    let artifact = measure_all(&queries, query_set_digest).await?;
    ensure!(
        artifact.decision.is_none(),
        "measurement must not record a decision"
    );
    write_measurement_artifact(&output, &baseline, &artifact)?;
    let baseline_after = read_optional(&baseline)?;
    ensure!(
        baseline_before == baseline_after,
        "measurement modified eval/baseline.json"
    );
    println!("{}", serde_json::to_string_pretty(&artifact)?);
    Ok(())
}

#[tokio::test]
#[ignore = "downloads rerankers and enforces the committed retrieval baseline"]
async fn eval_ground_recall_enforce() -> Result<()> {
    let baseline_bytes = fs::read(baseline_path()).context("read eval/baseline.json")?;
    let baseline: EvalArtifact =
        serde_json::from_slice(&baseline_bytes).context("parse eval/baseline.json")?;
    validate_artifact(&baseline).context("validate eval/baseline.json")?;
    validate_recorded_decision(&baseline)?;

    let (queries, query_set_digest) = load_queries()?;
    ensure!(
        query_set_digest == baseline.query_set_digest,
        "query-set digest disagrees with eval/baseline.json"
    );
    let current = measure_all(&queries, query_set_digest).await?;
    compare_against_baseline(&baseline, &current)?;
    println!("{}", serde_json::to_string_pretty(&current)?);
    Ok(())
}
