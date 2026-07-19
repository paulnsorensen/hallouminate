//! Daemon request dispatcher.
//!
//! Each handler:
//!   - resolves the target corpus via `effective_corpora` so derived
//!     `repo:{name}:wiki` / `repo:{name}:corpus` corpora are visible;
//!   - for mutating ops, takes the per-corpus mutex AND the global
//!     write-lane permit in that order via
//!     `DaemonState::acquire_mutation_guard` (the shared helper that
//!     replaces the open-coded `lock_corpus + acquire_owned` pattern every
//!     handler used to repeat);
//!   - returns a `DaemonResponse::Err(InvalidParams, ...)` for caller
//!     errors and `DaemonResponse::Err(Internal, ...)` for daemon faults.
//!
//! The corpus-selection / path-traversal / glob-validation / atomic-write
//! helpers live in `hallouminate_domain::corpus` and are shared with the
//! MCP tools transport, closing the maintenance liability of two divergent
//! forks (and the security asymmetry where the daemon was using
//! `tokio::fs::write` while MCP used `atomic_write_no_follow`).

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use crate::report::{CorpusReport, IndexReport};
use hallouminate_adapters::LanceStore;
use hallouminate_config::{Config, ResolvedLayers, resolve_for_cwd};
use hallouminate_domain::common::{CorpusConfig, FileRef, Mtime, canonicalize_or_passthrough};
#[cfg(test)]
use hallouminate_domain::corpus::FileEntry;
use hallouminate_domain::corpus::scan;
use hallouminate_domain::corpus::{
    SlugResolution, blake3_bytes, find_wikilinks, normalize_slug, resolve_slug,
};
use hallouminate_domain::corpus::{
    WriteError, WriteErrorKind, atomic_write_no_follow, delete_no_follow,
    ensure_corpus_allows_file, first_corpus_root, list_corpus_files, pick_corpus, read_no_follow,
    resolve_read_root, safe_relative_path,
};
use hallouminate_domain::ground::{
    Format, GroundOpts, RenderOpts, Warning, ground, ground_union, render, trim_snippets,
};
use hallouminate_domain::indexer::HandlerRegistry;
use hallouminate_domain::indexer::{
    ApplyStats, ChunkStore, DEFAULT_BATCH_SIZE, FileSnapshot, IndexPlan, MtimeCandidate, apply,
    index_corpus, plan,
};
#[cfg(test)]
use hallouminate_domain::repository::{RepoCorpusKind, repo_corpus_name};
use hallouminate_domain::repository::{RepositoryConfig, default_wiki_for_cwd};

use super::ipc::{
    AddMarkdownRequest, AddMarkdownResult, BacklinksRequest, BacklinksResult, CorpusEntry,
    CorpusStatsResult, DaemonRequest, DaemonRequestPayload, DaemonResponse, DeleteMarkdownRequest,
    DeleteMarkdownResult, GroundRequest, GroundResult, IndexRequest, LineRange, ListFilesRequest,
    ListTreeRequest, ListTreeResult, PongResult, Position, ReadMarkdownRequest, ReadMarkdownResult,
};
use super::state::{DaemonState, RequestResources, WorkClass};
use super::status;

pub async fn dispatch(state: &DaemonState, req: DaemonRequest) -> DaemonResponse {
    // Resolve per-request config layering on every request: discover the
    // repo-layer `.hallouminate/config.toml` under `req.cwd` and merge with
    // the boot baseline. Discovery / merge failures surface to the client as
    // `InvalidParams` so a misconfigured workspace produces a clean error
    // instead of a silent fall-back to baseline-only.
    //
    // `state.baseline_xdg_path()` carries the baseline's source path (the
    // XDG path, or the `--config PATH` override) so scalar-conflict
    // messages name the file the user actually has to edit — AC #7.
    // Shutdown and Ping are config-independent control ops: handle them
    // before `resolve_for_cwd` so a stop request (or a liveness / version
    // probe) works even when the client's cwd has no discoverable repo
    // config. Ping reports the daemon binary's version (Curd C) so the MCP
    // bootstrap can detect cross-version skew; that probe must succeed
    // regardless of the probing client's cwd, hence the early return.
    if let DaemonRequestPayload::Shutdown = req.payload {
        state.shutdown_token().cancel();
        return DaemonResponse::ok(&"stopping");
    }
    if let DaemonRequestPayload::Ping = req.payload {
        return DaemonResponse::ok(&PongResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
        });
    }
    let req_cwd = req.cwd.clone();
    let (effective, layers) =
        match resolve_for_cwd(state.baseline(), &req.cwd, state.baseline_xdg_path()) {
            Ok(resolved) => resolved,
            Err(e) => return DaemonResponse::invalid_params(e.to_string()),
        };
    match req.payload {
        DaemonRequestPayload::Ping => {
            // Handled before `resolve_for_cwd` above; unreachable here.
            DaemonResponse::ok(&PongResult {
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
        }
        DaemonRequestPayload::Ground(req) => {
            handle_ground(state, &effective, &layers, &req_cwd, req).await
        }
        DaemonRequestPayload::Index(req) => handle_index(state, &effective, req).await,
        DaemonRequestPayload::ListCorpora => handle_list_corpora(&effective),
        DaemonRequestPayload::ListFiles(req) => handle_list_files(&effective, &req_cwd, req).await,
        DaemonRequestPayload::ListTree(req) => handle_list_tree(&effective, &req_cwd, req).await,
        DaemonRequestPayload::AddMarkdown(req) => handle_add_markdown(state, &effective, req).await,
        DaemonRequestPayload::ReadMarkdown(req) => {
            handle_read_markdown(&effective, &req_cwd, req).await
        }
        DaemonRequestPayload::DeleteMarkdown(req) => {
            handle_delete_markdown(state, &effective, req).await
        }
        DaemonRequestPayload::Backlinks(req) => handle_backlinks(&effective, &req_cwd, req).await,
        DaemonRequestPayload::CorpusStats { corpus } => {
            handle_corpus_stats(state, &effective, &req_cwd, corpus).await
        }
        DaemonRequestPayload::Status => DaemonResponse::ok(&status::report(state)),
        DaemonRequestPayload::Shutdown => {
            // Handled before `resolve_for_cwd` above; unreachable here.
            DaemonResponse::ok(&"stopping")
        }
    }
}

fn effective_corpora(cfg: &Config) -> Result<Vec<CorpusConfig>, DaemonResponse> {
    cfg.effective_corpora()
        .map_err(|e| DaemonResponse::internal(e.to_string()))
}

/// Resolve and validate an explicit corpus + relative path for the *mutating*
/// wiki handlers (`add` / `delete`). Runs the shared preamble every one of them
/// repeated verbatim: pick the named corpus, require it be single-root (Curd B
/// — multi-root corpora are read/search-only), sandbox the caller-supplied
/// path, confirm the corpus's filters allow it, and pre-flight the wiki root
/// for symlink swaps. Returns `(corpus, root, relative)` on success, or the
/// exact `DaemonResponse` the handler would have returned at the failing step.
///
/// `read_markdown` uses [`validate_wiki_read_path`] instead — reads walk every
/// configured root so a file under `paths[1..]` stays reachable.
fn validate_wiki_path(
    corpora: &[CorpusConfig],
    corpus_name: &str,
    path: &str,
) -> Result<(CorpusConfig, std::path::PathBuf, std::path::PathBuf), DaemonResponse> {
    let corpus = pick_corpus(corpora, Some(corpus_name))
        .map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    let root = require_single_root(&corpus)?;
    let relative =
        safe_relative_path(path).map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    let dest = root.join(&relative);
    ensure_corpus_allows_file(&corpus, &dest)
        .map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    ensure_wiki_root_safe(&corpus).map_err(DaemonResponse::invalid_params)?;
    Ok((corpus, root, relative))
}

/// Read-side counterpart to [`validate_wiki_path`]. Picks the named corpus,
/// sandboxes the path, then resolves the relative path against *every*
/// configured root (Curd B) — so a file searchable under `paths[1..]` is also
/// readable, closing the split surface where multi-root corpora were
/// searchable-but-unreadable. The glob filter and wiki-root symlink pre-flight
/// run against the resolved root, preserving single-root behaviour exactly.
fn validate_wiki_read_path(
    corpora: &[CorpusConfig],
    corpus_name: &str,
    path: &str,
) -> Result<(CorpusConfig, std::path::PathBuf, std::path::PathBuf), DaemonResponse> {
    let corpus = pick_corpus(corpora, Some(corpus_name))
        .map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    let relative =
        safe_relative_path(path).map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    let root = resolve_read_root(&corpus, &relative)
        .map_err(|WriteError { kind, source }| map_read_error(kind, source, &relative))?;
    let dest = root.join(&relative);
    ensure_corpus_allows_file(&corpus, &dest)
        .map_err(|e| DaemonResponse::invalid_params(e.into_inner()))?;
    ensure_wiki_root_safe(&corpus).map_err(DaemonResponse::invalid_params)?;
    Ok((corpus, root, relative))
}

/// Resolve the single root of a corpus, or refuse loudly when it is multi-root.
///
/// Mutations (`add` / `delete`) target exactly one root; a
/// multi-root corpus has no canonical write destination, so the daemon rejects
/// the mutation at request time with an `InvalidParams` error explaining *why*
/// (the asymmetric "searchable but not writable" contract reads as a bug
/// otherwise). Config validation is intentionally left unchanged — multi-root
/// corpora stay loadable for search and read.
fn require_single_root(corpus: &CorpusConfig) -> Result<std::path::PathBuf, DaemonResponse> {
    if corpus.paths.len() == 1 {
        return first_corpus_root(corpus)
            .map_err(|e| DaemonResponse::invalid_params(e.into_inner()));
    }
    Err(DaemonResponse::invalid_params(format!(
        "corpus {:?} has {} roots; mutations (add/delete) require a \
         single-root corpus — multi-root corpora are read- and search-only",
        corpus.name,
        corpus.paths.len(),
    )))
}

/// Read-side corpus selection with wiki-defaulting.
///
/// When `requested` is `None`, try to default to `repo:{name}:wiki` for the
/// repository whose `path` contains `cwd` — that's the wiki the LLM is
/// actually working in. If no repository matches (or the derived corpus
/// isn't present), fall through to `pick_corpus`'s existing single-corpus /
/// ambiguity behavior. Mutating handlers do NOT use this — they require
/// an explicit corpus to avoid accidental writes to the wrong wiki.
fn pick_corpus_or_default(
    corpora: &[CorpusConfig],
    repositories: &[RepositoryConfig],
    cwd: &Path,
    requested: Option<&str>,
) -> Result<CorpusConfig, hallouminate_domain::corpus::SandboxError> {
    if requested.is_none()
        && let Some(name) = default_wiki_for_cwd(repositories, cwd)
        && let Some(found) = corpora.iter().find(|c| c.name == name).cloned()
    {
        return Ok(found);
    }
    pick_corpus(corpora, requested)
}

fn handle_list_corpora(cfg: &Config) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let entries: Vec<CorpusEntry> = corpora
        .into_iter()
        .map(|c| CorpusEntry {
            name: c.name,
            paths: c.paths,
        })
        .collect();
    DaemonResponse::ok(&entries)
}

async fn handle_list_files(cfg: &Config, cwd: &Path, req: ListFilesRequest) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => c,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    // Ensure wiki dir exists so an unindexed repository wiki doesn't error
    // out the first list call.
    ensure_paths_exist(&corpus).await;
    match list_corpus_files(&corpus) {
        Ok(entries) => DaemonResponse::ok(&entries),
        Err(e) => DaemonResponse::internal(e.to_string()),
    }
}

async fn handle_list_tree(cfg: &Config, cwd: &Path, req: ListTreeRequest) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => c,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    ensure_paths_exist(&corpus).await;
    let root = match hallouminate_domain::corpus::build_corpus_tree(&corpus) {
        Ok(node) => node,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    DaemonResponse::ok(&ListTreeResult {
        corpus: corpus.name,
        root,
    })
}

async fn handle_corpus_stats(
    state: &DaemonState,
    cfg: &Config,
    cwd: &Path,
    corpus: Option<String>,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus_cfg =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, corpus.as_deref()) {
            Ok(c) => c,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let store = &res.store;
    let chunk_stats = match store.corpus_chunk_stats(&corpus_cfg.name).await {
        Ok(s) => s,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    // Ensure wiki dir exists so an unindexed wiki corpus doesn't error
    // out on a missing root — mirrors handle_list_files.
    ensure_paths_exist(&corpus_cfg).await;
    let disk_files = match list_corpus_files(&corpus_cfg) {
        Ok(f) => f,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let indexed_files = match store.list_files(&corpus_cfg.name).await {
        Ok(m) => m,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let indexed_paths: std::collections::HashSet<String> =
        indexed_files.into_iter().map(|s| s.file_ref).collect();
    let unindexed_files = disk_files
        .iter()
        .filter(|e| !indexed_paths.contains(&e.absolute_path))
        .count() as u64;
    DaemonResponse::ok(&CorpusStatsResult {
        corpus: corpus_cfg.name,
        indexed_files: chunk_stats.indexed_files,
        total_chunks: chunk_stats.total_chunks,
        last_indexed_ms: chunk_stats.last_indexed_ms,
        unindexed_files,
    })
}

async fn handle_ground(
    state: &DaemonState,
    cfg: &Config,
    layers: &ResolvedLayers,
    cwd: &Path,
    req: GroundRequest,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let store = &res.store;
    let opts = GroundOpts {
        top_files: req.top_files.unwrap_or(cfg.search.top_files_default),
        chunks_per_file: req
            .chunks_per_file
            .unwrap_or(cfg.search.chunks_per_file_default),
        limit: req.limit.unwrap_or(50),
        rerank_timeout: Duration::from_millis(cfg.search.rerank_timeout_ms),
    };

    // Union ground (#106): a no-corpus request from above all repos fans the
    // query across EVERY effective corpus and merges into one re-ranked set.
    // An explicit corpus, or a no-corpus request whose cwd defaults to a
    // single repo wiki, takes the unchanged single-corpus path below.
    let union = req.corpus.is_none() && default_wiki_for_cwd(&cfg.repositories, cwd).is_none();

    // Resolve the single corpus up front for the non-union path; on the union
    // path the corpus list is the whole `corpora` set.
    let single_corpus = if union {
        None
    } else {
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => Some(c),
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        }
    };

    // crossencoder is best-effort: if it's configured but failed to
    // load (e.g. model file vanished), log and ground without it
    // rather than refusing the request. Unconfigured paths return
    // Ok(None) and the rerank step is skipped entirely.
    let crossencoder = match state.crossencoder(cfg.search.crossencoder.as_deref()).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "crossencoder unavailable for this request; falling back to fusion-only ranking",
            );
            None
        }
    };
    // #139: moved (not borrowed) into a `Box<dyn Crossencoder>` so `ground`/
    // `ground_union` can hand it to `spawn_blocking` for the rerank timeout.
    let crossencoder_box: Option<Box<dyn hallouminate_domain::search::Crossencoder>> =
        crossencoder.map(|g| Box::new(g) as Box<dyn hallouminate_domain::search::Crossencoder>);

    let response = if let Some(corpus) = &single_corpus {
        ground(
            &req.query,
            &corpus.name,
            &corpus.paths,
            store.as_ref(),
            crossencoder_box,
            opts,
        )
        .await
    } else {
        let targets: Vec<(String, Vec<String>)> = corpora
            .iter()
            .map(|c| (c.name.clone(), c.paths.clone()))
            .collect();
        ground_union(&req.query, &targets, store.as_ref(), crossencoder_box, opts).await
    };
    let mut response = match response {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };

    // #135: stale-index detection.
    mark_stale(&mut response).await;

    // Surface the cross-repo resolution advisories (name-collision shadowing)
    // on the union response so the merge is auditable rather than silent.
    if union {
        for w in &layers.warnings {
            response.warnings.push(Warning {
                code: "cross-repo-union".to_string(),
                message: w.clone(),
            });
        }
    }

    let response = if let Some(limit) = req.snippet_chars {
        trim_snippets(&response, limit)
    } else {
        response
    };
    let outline = render(
        &response,
        Format::Outline,
        &RenderOpts {
            snippet_chars: None,
            path_prefix_strip: None,
        },
    );
    DaemonResponse::ok(&GroundResult { outline, response })
}

/// Map a mutation-guard acquisition failure onto the wire. The Hard-debt
/// bounded-wait expiry is the one retryable case (curd 2's backpressure gate
/// emits exactly `RETRYABLE_HARD_DEBT`); everything else stays `Internal`.
fn mutation_guard_err(msg: impl Into<String>) -> DaemonResponse {
    let msg = msg.into();
    if msg == super::backpressure::RETRYABLE_HARD_DEBT {
        DaemonResponse::retryable(msg)
    } else {
        DaemonResponse::internal(msg)
    }
}

async fn handle_index(state: &DaemonState, cfg: &Config, req: IndexRequest) -> DaemonResponse {
    // Reject ad-hoc paths_from unconditionally: the daemon protocol does not
    // accept it yet, and silently ignoring the field when a corpus is also
    // selected would let MCP clients believe the path list landed.
    if req.paths_from.is_some() {
        return DaemonResponse::invalid_params("paths_from is not supported via the daemon yet");
    }
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let selected: Vec<CorpusConfig> = if let Some(name) = req.corpus.as_deref() {
        match corpora.iter().find(|c| c.name == name) {
            Some(c) => vec![c.clone()],
            None => {
                return DaemonResponse::invalid_params(format!(
                    "corpus {name:?} not found in config"
                ));
            }
        }
    } else {
        if corpora.is_empty() {
            // Pre-daemon CLI/MCP treated "no corpora configured" as invalid
            // input. Match that shape so a misconfigured daemon doesn't
            // silently report "index ok" with zero work done.
            return DaemonResponse::invalid_params(
                "no corpora configured; add [[corpus]] or [[repository]] to config",
            );
        }
        corpora.clone()
    };

    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let store = &res.store;
    let registry = state.make_registry();

    let mut report = IndexReport::default();
    for corpus in selected {
        let guard = match state.acquire_mutation_guard(&corpus.name).await {
            Ok(g) => g,
            Err(msg) => return mutation_guard_err(msg),
        };
        ensure_paths_exist(&corpus).await;
        let missing = hallouminate_domain::corpus::missing_roots(&corpus);
        if !missing.is_empty() {
            let roots = missing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            if req.strict {
                return DaemonResponse::invalid_params(format!(
                    "corpus {:?}: root {roots} does not exist",
                    corpus.name
                ));
            }
            report.warnings.push(format!(
                "corpus {:?}: root {roots} does not exist; skipped",
                corpus.name
            ));
            continue;
        }
        let stats = match index_corpus(&corpus, store.as_ref(), &registry).await {
            Ok(s) => s,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        };
        drop(guard);
        report.corpora.push(CorpusReport {
            name: corpus.name.clone(),
            files_upserted: stats.files_upserted,
            files_touched: stats.files_touched,
            files_deleted: stats.files_deleted,
            files_skipped_empty: stats.files_skipped_empty,
            files_skipped_unreadable: stats.files_skipped_unreadable,
            chunks_inserted: stats.chunks_inserted,
            embeddings_inserted: stats.embeddings_inserted,
        });
    }
    DaemonResponse::ok(&report)
}

/// Classify the edit mode from the request. Returns an error response if more
/// than one mode selector is set.
enum EditMode {
    WholeFile,
    UnderHeading(String, Position),
    ReplaceLines(LineRange),
    ReplaceMatch(String),
}

fn classify_edit_mode(req: &AddMarkdownRequest) -> Result<EditMode, DaemonResponse> {
    let count = req.under_heading.is_some() as u8
        + req.replace_lines.is_some() as u8
        + req.replace_match.is_some() as u8;
    if count > 1 {
        return Err(DaemonResponse::invalid_params(
            "set at most one of under_heading / replace_lines / replace_match",
        ));
    }
    if let Some(h) = &req.under_heading {
        return Ok(EditMode::UnderHeading(h.clone(), req.position));
    }
    if let Some(r) = req.replace_lines {
        return Ok(EditMode::ReplaceLines(r));
    }
    if let Some(n) = &req.replace_match {
        return Ok(EditMode::ReplaceMatch(n.clone()));
    }
    Ok(EditMode::WholeFile)
}

/// Read an existing file for edit-mode operations, mapping [`WriteErrorKind`]
/// to the appropriate response so each edit-mode arm reduces to a single
/// `read_existing_text(...).await` call with an early-return on error.
///
/// `mode_label` appears in the "requires an existing file" message (e.g.
/// `"under_heading"`) so the caller knows which field triggered the read.
async fn read_existing_text(
    root: std::path::PathBuf,
    rel: std::path::PathBuf,
    mode_label: &str,
) -> Result<String, DaemonResponse> {
    let rel_disp = rel.display().to_string();
    let raw = match tokio::task::spawn_blocking(move || read_no_follow(&root, &rel)).await {
        Ok(Ok(b)) => b,
        Ok(Err(WriteError { kind, source })) => {
            return Err(match kind {
                WriteErrorKind::Io => {
                    if source.kind() == std::io::ErrorKind::NotFound {
                        DaemonResponse::invalid_params(format!(
                            "{mode_label} requires an existing file; {rel_disp} not found"
                        ))
                    } else {
                        tracing::error!(
                            target: "hallouminate::daemon",
                            error = %source,
                            path = %rel_disp,
                            "read_existing_text io error",
                        );
                        DaemonResponse::internal(format!("failed to read {rel_disp}: {source}"))
                    }
                }
                WriteErrorKind::Symlink | WriteErrorKind::InvalidPath => {
                    DaemonResponse::invalid_params(format!("refusing unsafe path {rel_disp}"))
                }
                WriteErrorKind::Exists => {
                    tracing::error!(
                        target: "hallouminate::daemon",
                        path = %rel_disp,
                        "read_existing_text: unexpected Exists variant on read path",
                    );
                    DaemonResponse::internal("unexpected Exists on read")
                }
            });
        }
        Err(e) => {
            tracing::error!(
                target: "hallouminate::daemon",
                error = %e,
                "read_existing_text read task panicked",
            );
            return Err(DaemonResponse::internal(format!("read task panicked: {e}")));
        }
    };
    String::from_utf8(raw)
        .map_err(|_| DaemonResponse::invalid_params("existing file is not valid UTF-8".to_string()))
}

async fn handle_add_markdown(
    state: &DaemonState,
    cfg: &Config,
    mut req: AddMarkdownRequest,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (corpus, root, relative) = match validate_wiki_path(&corpora, &req.corpus, &req.path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // ── mode dispatch ────────────────────────────────────────────────────────
    let mode = match classify_edit_mode(&req) {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // Acquire the per-corpus mutation guard before any read-modify-write so the
    // entire read → compose → write sequence is atomic. Without this ordering
    // two concurrent edit-mode calls (or an edit racing a whole-file overwrite)
    // would both read the same pre-edit snapshot and the second write would
    // clobber the first silently — the classic lost-update bug.
    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(msg) => return mutation_guard_err(msg),
    };

    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };

    let force_overwrite: bool;
    match mode {
        EditMode::WholeFile => {
            // Unchanged whole-file path; `overwrite` governs as before.
            force_overwrite = req.overwrite;
        }
        EditMode::UnderHeading(heading, position) => {
            let existing =
                match read_existing_text(root.clone(), relative.clone(), "under_heading").await {
                    Ok(s) => s,
                    Err(resp) => return resp,
                };
            req.content = match hallouminate_domain::corpus::splice_under_heading(
                &existing,
                &heading,
                position,
                &req.content,
            ) {
                Ok(s) => s,
                Err(hallouminate_domain::corpus::SectionError::NotFound) => {
                    return DaemonResponse::invalid_params(format!(
                        "heading '{heading}' not found in {}",
                        relative.display()
                    ));
                }
                Err(hallouminate_domain::corpus::SectionError::Duplicate) => {
                    return DaemonResponse::invalid_params(format!(
                        "heading '{heading}' is ambiguous in {}",
                        relative.display()
                    ));
                }
            };
            force_overwrite = true;
        }
        EditMode::ReplaceLines(range) => {
            let existing =
                match read_existing_text(root.clone(), relative.clone(), "replace_lines").await {
                    Ok(s) => s,
                    Err(resp) => return resp,
                };
            req.content = match hallouminate_domain::corpus::replace_line_range(
                &existing,
                range,
                &req.content,
            ) {
                Ok(s) => s,
                Err(hallouminate_domain::corpus::RangeError::OutOfRange) => {
                    return DaemonResponse::invalid_params("line range out of range".to_string());
                }
                Err(hallouminate_domain::corpus::RangeError::Inverted) => {
                    return DaemonResponse::invalid_params("start > end".to_string());
                }
            };
            force_overwrite = true;
        }
        EditMode::ReplaceMatch(needle) => {
            let existing =
                match read_existing_text(root.clone(), relative.clone(), "replace_match").await {
                    Ok(s) => s,
                    Err(resp) => return resp,
                };
            req.content = match hallouminate_domain::corpus::replace_unique_match(
                &existing,
                &needle,
                &req.content,
            ) {
                Ok(s) => s,
                Err(hallouminate_domain::corpus::MatchError::NotFound) => {
                    return DaemonResponse::invalid_params("match not found".to_string());
                }
                Err(hallouminate_domain::corpus::MatchError::Ambiguous(n)) => {
                    return DaemonResponse::invalid_params(format!(
                        "match ambiguous \u{2014} {n} occurrences"
                    ));
                }
            };
            force_overwrite = true;
        }
    }

    // ── shared tail (lint runs on the COMPOSED file) ──────────────────────────
    // Advisory-only lint of the verbatim content. Never blocks or rewrites the
    // write — the messages ride back in the response so the author can fix in
    // a follow-up instead of discovering breakage on a later read.
    let mut warnings = hallouminate_domain::corpus::lint_markdown(&req.content);
    warnings.extend(hallouminate_domain::corpus::lint_frontmatter(&req.content));
    warnings.extend(hallouminate_domain::corpus::lint_claim_marks(&req.content));
    match hallouminate_domain::corpus::list_corpus_files(&corpus) {
        Ok(entries) => {
            let mut known_paths: Vec<String> = entries.into_iter().map(|e| e.path).collect();
            known_paths.push(req.path.clone());
            warnings.extend(hallouminate_domain::corpus::lint_wikilinks(
                &req.content,
                &known_paths,
            ));
        }
        Err(e) => {
            tracing::warn!(
                target: "hallouminate::daemon",
                error = %e,
                "skipping wikilink lint: failed to list corpus files",
            );
        }
    }

    // Symlink-safe atomic write via the shared sandbox helper. Walks every
    // path component with `O_NOFOLLOW | O_DIRECTORY`, so a symlinked
    // intermediate dir bounces with `WriteErrorKind::Symlink` instead of
    // letting the writer punch through to whatever the symlink targets.
    let write_root = root.clone();
    let write_relative = relative.clone();
    let error_relative = relative.clone();
    // Read scalars and the empty-content flag before consuming `req.content`,
    // so the move into `into_bytes()` avoids cloning a potentially large body.
    let overwrite = force_overwrite;
    let content_is_empty = req.content.trim().is_empty();
    let content_bytes = req.content.into_bytes();
    let written = tokio::task::spawn_blocking(move || {
        atomic_write_no_follow(&write_root, &write_relative, &content_bytes, overwrite)
    })
    .await;
    let dest = match written {
        Ok(Ok(p)) => p,
        Ok(Err(WriteError { kind, source })) => {
            let resp = match kind {
                WriteErrorKind::Exists => DaemonResponse::invalid_params(format!(
                    "{} already exists; pass overwrite=true to replace it",
                    error_relative.display()
                )),
                WriteErrorKind::Symlink | WriteErrorKind::InvalidPath => {
                    DaemonResponse::invalid_params(format!(
                        "refusing unsafe path {}: {source}",
                        error_relative.display()
                    ))
                }
                WriteErrorKind::Io => DaemonResponse::internal(source.to_string()),
            };
            return resp;
        }
        Err(join_err) => {
            // Non-retriable: a panicked blocking task means the write lane is
            // in an unknown state, so log at error (not warn) for alerting.
            tracing::error!(
                target: "hallouminate::daemon",
                error = %join_err,
                "add_markdown write task panicked",
            );
            return DaemonResponse::internal(format!("write task panicked: {join_err}"));
        }
    };

    // Empty content produces zero chunks; the indexer would just count the
    // file as `files_skipped_empty` and burn an embedder call on a no-op.
    // Short-circuit so tests that exercise just the filesystem-mutation lane
    // don't need the embedding model active. When the request is overwriting
    // a previously-indexed file with empty content, also prune the existing
    // LanceDB rows so searches stop returning the deleted body — the shared
    // `apply`/`EmptyFilePolicy::Evict` path (see `apply.rs`) does this for the
    // full `index_single_file` path, but the short-circuit below bypasses it.
    let mut stats = if content_is_empty {
        let mut stats = hallouminate_domain::indexer::ApplyStats {
            files_skipped_empty: 1,
            ..Default::default()
        };
        if overwrite {
            let file_ref = canonicalize_or_passthrough(&dest);
            if let Some(file_ref_str) = file_ref.as_path().to_str() {
                let store = &res.store;
                match store.get_file_snapshot(&corpus.name, file_ref_str).await {
                    Ok(Some(_)) => match store.delete_file(&corpus.name, file_ref_str).await {
                        Ok(()) => stats.files_deleted = 1,
                        Err(e) => return DaemonResponse::internal(e.to_string()),
                    },
                    Ok(None) => {}
                    Err(e) => return DaemonResponse::internal(e.to_string()),
                }
            }
        }
        stats
    } else {
        let store = &res.store;
        let registry = state.make_registry();
        match index_single_file(store, &registry, &corpus, &dest).await {
            Ok(s) => s,
            Err(e) => {
                // The write above already durably completed; a model/store
                // failure here must not hide that from the caller behind a
                // bare internal error. Surface it as a warning on the
                // otherwise-successful response and leave the file
                // unindexed for a later `index` call to repair.
                tracing::warn!(
                    target: "hallouminate::daemon",
                    error = %e,
                    path = %dest.display(),
                    "add_markdown: indexing failed after durable write",
                );
                warnings.push(format!(
                    "wrote {} but indexing failed: {e}; run `index` to repair search results",
                    relative.display()
                ));
                hallouminate_domain::indexer::ApplyStats::default()
            }
        }
    };

    // Auto-rebuild wiki indexes from the corpus root down to the parent of
    // the just-written file. The write already durably completed, so a
    // refresh failure here is reported as a warning on the successful
    // response instead of an internal error — the caller can see the
    // durable write and retry `index` rather than losing visibility into
    // it entirely.
    if is_wiki_corpus(&corpus) {
        match rebuild_wiki_indexes(state, cfg, &corpus, &root, &relative).await {
            Ok(extra) => fold_apply_stats(&mut stats, &extra),
            Err(msg) => {
                warnings.push(format!(
                    "wrote {} but ancestor index refresh failed: {msg}; run `index` to repair",
                    relative.display()
                ));
            }
        }
    }
    drop(guard);

    let report = IndexReport {
        corpora: vec![CorpusReport {
            name: corpus.name.clone(),
            files_upserted: stats.files_upserted,
            files_touched: stats.files_touched,
            files_deleted: stats.files_deleted,
            files_skipped_empty: stats.files_skipped_empty,
            files_skipped_unreadable: stats.files_skipped_unreadable,
            chunks_inserted: stats.chunks_inserted,
            embeddings_inserted: stats.embeddings_inserted,
        }],
        warnings: Vec::new(),
    };
    DaemonResponse::ok(&AddMarkdownResult {
        corpus: corpus.name,
        path: relative.to_string_lossy().into_owned(),
        absolute_path: dest.to_string_lossy().into_owned(),
        indexed: report,
        warnings,
    })
}

async fn handle_read_markdown(
    cfg: &Config,
    cwd: &Path,
    req: ReadMarkdownRequest,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Resolve, sandbox, and read entirely off the async dispatcher thread.
    // `validate_wiki_read_path` is not just CPU work: `resolve_read_root`
    // walks every configured root with `symlink_metadata`/`open_dir` and
    // `ensure_wiki_root_safe` stats the wiki root, then the symlink-safe
    // `read_no_follow` opens the leaf `O_RDONLY | O_NOFOLLOW`. Running all of
    // it on a blocking task keeps a slow or remote corpus root from stalling a
    // Tokio worker and delaying unrelated daemon requests.
    let corpus_name =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => c.name,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    let req_path = req.path;
    let resolved = tokio::task::spawn_blocking(move || {
        let (corpus, root, relative) = validate_wiki_read_path(&corpora, &corpus_name, &req_path)?;
        let bytes = read_no_follow(&root, &relative)
            .map_err(|WriteError { kind, source }| map_read_error(kind, source, &relative))?;
        Ok::<_, DaemonResponse>((corpus, root, relative, bytes))
    })
    .await;
    let (corpus, root, relative, bytes) = match resolved {
        Ok(Ok(t)) => t,
        Ok(Err(resp)) => return resp,
        Err(join_err) => {
            tracing::error!(
                target: "hallouminate::daemon",
                error = %join_err,
                "read_markdown read task panicked",
            );
            return DaemonResponse::internal(format!("read task panicked: {join_err}"));
        }
    };
    let dest = root.join(&relative);
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            return DaemonResponse::invalid_params(format!(
                "{} is not valid UTF-8: {e}",
                relative.display()
            ));
        }
    };
    DaemonResponse::ok(&ReadMarkdownResult {
        corpus: corpus.name,
        path: relative.to_string_lossy().into_owned(),
        absolute_path: dest.to_string_lossy().into_owned(),
        bytes: content.len() as u64,
        content,
    })
}

async fn handle_backlinks(cfg: &Config, cwd: &Path, req: BacklinksRequest) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => c,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    ensure_paths_exist(&corpus).await;
    let entries = match list_corpus_files(&corpus) {
        Ok(e) => e,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let full_slug = normalize_slug(&req.path);
    let bare_stem = Path::new(&req.path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .filter(|stem| *stem != full_slug);
    let entry_paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let target_slugs: std::collections::HashSet<String> = match &bare_stem {
        Some(stem) => match resolve_slug(stem, &entry_paths) {
            SlugResolution::Ambiguous(_) => std::iter::once(full_slug.clone()).collect(),
            _ => [full_slug.clone(), stem.clone()].into_iter().collect(),
        },
        None => std::iter::once(full_slug.clone()).collect(),
    };
    let corpus_name = corpus.name.clone();
    let req_path = req.path.clone();
    let scanned = tokio::task::spawn_blocking(move || {
        let mut backlinks: Vec<String> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        for entry in entries.into_iter().filter(|entry| entry.path != req_path) {
            match std::fs::read_to_string(&entry.absolute_path) {
                Ok(content) => {
                    if find_wikilinks(&content)
                        .iter()
                        .any(|link| target_slugs.contains(&normalize_slug(link)))
                    {
                        backlinks.push(entry.path);
                    }
                }
                Err(e) => failures.push((entry.path, e.to_string())),
            }
        }
        backlinks.sort();
        (backlinks, failures)
    })
    .await;
    let (backlinks, failures) = match scanned {
        Ok(r) => r,
        Err(join_err) => {
            tracing::error!(
                target: "hallouminate::daemon",
                error = %join_err,
                "backlinks scan task panicked",
            );
            return DaemonResponse::internal(format!("backlinks task panicked: {join_err}"));
        }
    };
    let mut warnings = Vec::new();
    for (path, error) in &failures {
        tracing::warn!(
            target: "hallouminate::daemon",
            corpus = %corpus_name,
            path = %path,
            error = %error,
            "backlinks scan: failed to read file; result is a partial scan",
        );
        warnings.push(format!(
            "could not read {path} in corpus {corpus_name}: {error}; backlinks result is incomplete"
        ));
    }
    DaemonResponse::ok(&BacklinksResult {
        corpus: corpus_name,
        path: req.path,
        backlinks,
        warnings,
    })
}

async fn handle_delete_markdown(
    state: &DaemonState,
    cfg: &Config,
    req: DeleteMarkdownRequest,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (corpus, root, relative) = match validate_wiki_path(&corpora, &req.corpus, &req.path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let dest = root.join(&relative);

    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(msg) => return mutation_guard_err(msg),
    };

    // Best-effort UX pre-check only: the `symlink_metadata` lookup produces
    // the precise "does not exist" / "refusing to delete symlink" / "not a
    // regular file" messages callers rely on. It is NOT the security boundary
    // — `delete_no_follow` below re-checks each path component and the leaf
    // with the kernel `statat(SYMLINK_NOFOLLOW)`, which is the authoritative
    // gate (and closes the TOCTOU window this stat-then-unlink cannot).
    let meta = match tokio::fs::symlink_metadata(&dest).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return DaemonResponse::invalid_params(format!(
                "{} does not exist",
                relative.display()
            ));
        }
        Err(e) => {
            return DaemonResponse::internal(format!("stat {}: {e}", dest.display()));
        }
    };
    if meta.file_type().is_symlink() {
        return DaemonResponse::invalid_params(format!(
            "refusing to delete symlink {}",
            relative.display()
        ));
    }
    if !meta.file_type().is_file() {
        return DaemonResponse::invalid_params(format!(
            "{} is not a regular file",
            relative.display()
        ));
    }

    // Compute canonical file_ref BEFORE unlinking so the LanceDB row we
    // prune matches what the indexer wrote.
    let file_ref = canonicalize_or_passthrough(&dest);
    let file_ref_str = match file_ref.as_path().to_str() {
        Some(s) => s.to_string(),
        None => {
            return DaemonResponse::internal(format!(
                "non-utf8 path: {}",
                file_ref.as_path().display()
            ));
        }
    };

    // Symlink-safe unlink via the shared sandbox helper: walks every parent
    // component with `O_NOFOLLOW` and refuses if the leaf is a symlink, so a
    // corpus-controlled symlinked directory cannot redirect the unlink
    // outside the corpus root.
    let delete_root = root.clone();
    let delete_relative = relative.clone();
    let error_relative = relative.clone();
    let deleted =
        tokio::task::spawn_blocking(move || delete_no_follow(&delete_root, &delete_relative)).await;
    match deleted {
        Ok(Ok(())) => {}
        Ok(Err(WriteError { kind, source })) => {
            return map_delete_error(kind, source, &error_relative);
        }
        Err(join_err) => {
            tracing::error!(
                target: "hallouminate::daemon",
                error = %join_err,
                "delete_markdown unlink task panicked",
            );
            return DaemonResponse::internal(format!("unlink task panicked: {join_err}"));
        }
    }
    let res = match state.resources_for(cfg).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    if let Err(e) = res.store.delete_file(&corpus.name, &file_ref_str).await {
        return DaemonResponse::internal(e.to_string());
    }

    // Auto-rebuild wiki indexes after the unlink so the parent index no
    // longer links to the deleted file. Same internal-error semantics as
    // the add_markdown path — partial regen would desync the wiki tree.
    if is_wiki_corpus(&corpus)
        && let Err(msg) = rebuild_wiki_indexes(state, cfg, &corpus, &root, &relative).await
    {
        drop(guard);
        return DaemonResponse::internal(msg);
    }
    drop(guard);

    DaemonResponse::ok(&DeleteMarkdownResult {
        corpus: corpus.name,
        path: relative.to_string_lossy().into_owned(),
        absolute_path: dest.to_string_lossy().into_owned(),
        file_ref: file_ref_str,
    })
}

/// Convert a since-epoch duration to a millisecond `i64` mtime, failing
/// cleanly when the value overflows `i64` instead of silently truncating the
/// `u128` (which `as i64` would do). Mtimes near `i64::MAX` ms are absurd in
/// practice, but a corrupt or attacker-controlled timestamp must error rather
/// than wrap into a bogus past/future mtime the indexer would trust.
fn mtime_ms_from_duration(dur: std::time::Duration, file: &Path) -> anyhow::Result<i64> {
    i64::try_from(dur.as_millis())
        .map_err(|_| anyhow::anyhow!("mtime overflows i64 on {}", file.display()))
}

/// Parse an RFC 3339 timestamp string (as stored in [`DocFile::mtime`]) back
/// to milliseconds since the Unix epoch. Returns `i64::MIN` on parse failure
/// so any real disk mtime compares as "newer" (i.e. safe-to-mark-stale) rather
/// than hiding drift behind a silent equal.
fn mtime_ms_from_rfc3339(rfc: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(i64::MIN)
}

/// Stat each doc in `response` off-thread and mark it stale when the on-disk
/// mtime is newer than the indexed mtime, or the file is missing.
///
/// Detection only — no re-index (that would force the read handler onto the
/// write lane and break lane separation).
async fn mark_stale(response: &mut hallouminate_domain::ground::GroundResponse) {
    let paths: Vec<String> = response.docs.keys().cloned().collect();
    // Clone for the join-error fallback, which needs to mark every doc stale.
    let paths_for_join_err = paths.clone();
    let indexed_mtimes: Vec<i64> = response
        .docs
        .values()
        .map(|doc| mtime_ms_from_rfc3339(&doc.mtime))
        .collect();
    let stale_flags: Vec<(String, bool)> = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .zip(indexed_mtimes)
            .map(|(path, indexed_ms)| {
                let canonical = canonicalize_or_passthrough(std::path::Path::new(&path));
                let stale = match std::fs::metadata(canonical.as_path()) {
                    Err(_) => true, // missing file counts as stale
                    Ok(meta) => {
                        let disk_s = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            // mtime unreadable (unsupported platform or FS error):
                            // fail toward stale so drift is never silently hidden.
                            .unwrap_or(i64::MAX);
                        // indexed_ms is parsed from RFC3339 with second
                        // precision; compare at the same granularity to
                        // avoid false positives from sub-second truncation.
                        disk_s > indexed_ms / 1000
                    }
                };
                (path, stale)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_else(|e| {
        // spawn_blocking task panicked or was cancelled: fail toward stale
        // so every doc in this response is marked stale rather than silently
        // hiding drift behind a false-not-stale.
        tracing::warn!(
            target: "hallouminate::daemon",
            error = %e,
            "mark_stale: spawn_blocking task failed; marking all docs stale",
        );
        paths_for_join_err.into_iter().map(|p| (p, true)).collect()
    });
    for (path, stale) in stale_flags {
        if let Some(doc) = response.docs.get_mut(&path) {
            doc.stale = stale;
        }
    }
}

/// Uses `block_in_place` internally, which panics on a current-thread
/// runtime — callers (and their tests) must run under the `multi_thread`
/// flavor.
/// Reindex a single file the daemon controls, reading its content and mtime
/// from the ambient path. Used by the write/add-markdown handlers, which
/// reindex a file they just wrote through the sandbox no-follow write path.
/// The untrusted watcher path must NOT use this — it reads no-follow bytes and
/// calls `index_single_file_with_content` directly to avoid a symlink-swap
/// TOCTOU between validation and content read.
pub(super) async fn index_single_file(
    store: &LanceStore,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
    file: &Path,
) -> anyhow::Result<hallouminate_domain::indexer::ApplyStats> {
    let mtime = tokio::fs::metadata(file).await?.modified()?;
    let bytes = tokio::fs::read(file).await?;
    index_single_file_with_content(store, registry, corpus, file, &bytes, mtime).await
}

/// Reindex a single file from already-read content and mtime, so the caller
/// controls how the bytes were obtained. The watcher passes bytes from a
/// no-follow read (`sandbox::read_no_follow_with_mtime`) so a corpus
/// contributor cannot swap a checked component to a symlink between the
/// validation and the content read.
pub(super) async fn index_single_file_with_content(
    store: &LanceStore,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
    file: &Path,
    bytes: &[u8],
    mtime: std::time::SystemTime,
) -> anyhow::Result<hallouminate_domain::indexer::ApplyStats> {
    let dur = mtime
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("pre-epoch mtime on {}", file.display()))?;
    let mtime_ms = mtime_ms_from_duration(dur, file)?;
    let file_ref = canonicalize_or_passthrough(file);
    let file_ref_str = file_ref
        .as_path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", file_ref.as_path().display()))?
        .to_string();
    let existing = store.get_file_snapshot(&corpus.name, &file_ref_str).await?;
    // Truncate-to-empty eviction (files_skipped_empty > 0 for a file that HAD
    // a snapshot) is handled inside `apply`'s mtime-fallthrough batch — see
    // `EmptyFilePolicy::Evict` in `src/domain/indexer/apply.rs` — so both this
    // single-file path and bulk `index_corpus` share one eviction rule.
    let stats = tokio::task::block_in_place(|| {
        // Content-hash gate (ADR daemon-rework-003): the bytes are already in
        // hand, so hash them once — inside block_in_place, since blake3 over
        // file bytes is blocking CPU work — and compare against the stored
        // snapshot. Never re-read the file from disk here: the caller's
        // no-follow read is the content of record, and a second read would
        // reopen the symlink-swap TOCTOU this function exists to close.
        let p = match existing {
            Some(snap) => {
                let hash = blake3_bytes(bytes);
                if hash == snap.content_hash {
                    if snap.mtime_ms == mtime_ms {
                        tracing::debug!(
                            target: "hallouminate::daemon",
                            corpus = %corpus.name,
                            file = %file_ref_str,
                            "reindex skipped: content hash and mtime match stored snapshot"
                        );
                        return Ok(ApplyStats::default());
                    }
                    tracing::debug!(
                        target: "hallouminate::daemon",
                        corpus = %corpus.name,
                        file = %file_ref_str,
                        "reindex skipped: content hash matches stored snapshot; bumping stored mtime"
                    );
                }
                // One candidate, hash pre-computed: `apply` touches the stored
                // mtime when the hash matches, and otherwise falls through to
                // a full re-index whose truncate-to-empty case still evicts
                // the prior rows (`EmptyFilePolicy::Evict`).
                IndexPlan {
                    upserts: Vec::new(),
                    mtime_touches: vec![MtimeCandidate {
                        file: file_ref.clone(),
                        snap,
                        new_mtime: Mtime(mtime_ms),
                        known_hash: Some(hash),
                    }],
                    deletes: Vec::new(),
                }
            }
            None => plan(vec![(file_ref.clone(), Mtime(mtime_ms))], HashMap::new()),
        };
        tokio::runtime::Handle::current().block_on(apply(
            p,
            store,
            registry,
            corpus,
            DEFAULT_BATCH_SIZE,
            Some((&file_ref, bytes)),
        ))
    })?;
    Ok(stats)
}

/// Non-blocking, incremental catch-up index over the daemon's watched
/// (baseline) corpora, run once at boot to pick up edits made while the daemon
/// was down (ADR-001 down-window). Per corpus it plans the disk-vs-index diff —
/// stat + snapshot compare, no model — and acquires the embedder only when the
/// plan has content to (re)embed, so an unchanged corpus never triggers an
/// embedding-model load.
pub(super) async fn catch_up_index(state: DaemonState) {
    // Boot reconciliation is in-flight work: hold a connection guard for the
    // whole sweep so idle-exit defers until it finishes instead of tearing a
    // reindex mid-scan under a small `idle_exit_secs` (ADR-003).
    let _conn = state.enter_connection(WorkClass::Internal);
    let corpora = match state.baseline().effective_corpora() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hallouminate::daemon", error = %e,
                "boot catch-up: could not enumerate baseline corpora; skipped");
            return;
        }
    };
    let res = match state.resources_for(state.baseline()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "hallouminate::daemon", error = %e,
                "boot catch-up: could not resolve baseline resources; skipped");
            return;
        }
    };
    let registry = state.make_registry();
    for corpus in corpora {
        if !hallouminate_domain::corpus::missing_roots(&corpus).is_empty() {
            continue; // absent root; watcher skips it too, later boot picks it up
        }
        let _guard = match state.acquire_mutation_guard(&corpus.name).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(target: "hallouminate::daemon", corpus = %corpus.name,
                    error = %e, "boot catch-up: could not lock corpus; skipped");
                continue;
            }
        };
        match catch_up_corpus(&res, &registry, &corpus).await {
            Ok(Some(stats)) => tracing::info!(target: "hallouminate::daemon",
                corpus = %corpus.name, files_upserted = stats.files_upserted,
                files_touched = stats.files_touched, files_deleted = stats.files_deleted,
                "boot catch-up: reindexed corpus changed during down-window"),
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "hallouminate::daemon", corpus = %corpus.name,
                error = %e, "boot catch-up: reindex failed; skipped"),
        }
    }
    state.heartbeat().bump(super::heartbeat::TaskName::CatchUp);
}

/// Plan + apply one corpus's down-window diff. `Ok(None)` = nothing changed
/// (no work, no model load); `Ok(Some(stats))` = reindexed.
async fn catch_up_corpus(
    res: &RequestResources,
    registry: &HandlerRegistry,
    corpus: &CorpusConfig,
) -> anyhow::Result<Option<hallouminate_domain::indexer::ApplyStats>> {
    let disk = scan(corpus)?;
    let db: HashMap<FileRef, FileSnapshot> = res
        .store
        .list_files(&corpus.name)
        .await?
        .into_iter()
        .map(|s| (FileRef::new(std::path::PathBuf::from(&s.file_ref)), s))
        .collect();
    let p = plan(disk, db);
    if p.upserts.is_empty() && p.mtime_touches.is_empty() && p.deletes.is_empty() {
        return Ok(None);
    }
    let stats = apply(
        p,
        res.store.as_ref(),
        registry,
        corpus,
        DEFAULT_BATCH_SIZE,
        None,
    )
    .await?;
    Ok(Some(stats))
}

/// Best-effort `mkdir -p` on daemon-managed corpus roots so a fresh
/// repository wiki (which only exists logically until the first write)
/// doesn't blow up the first `list_files` / `index` call. Restricted to
/// `repo:*:wiki` corpora so a typo'd `[[corpus]] paths = ...` surfaces as a
/// clear scan error instead of silently creating an empty directory and
/// reporting success.
async fn ensure_paths_exist(corpus: &CorpusConfig) {
    if !is_wiki_corpus(corpus) {
        return;
    }
    let paths: Vec<std::path::PathBuf> = corpus
        .paths
        .iter()
        .map(|raw| hallouminate_domain::common::expand_tilde(raw))
        .collect();
    let _ = tokio::task::spawn_blocking(move || {
        for path in paths {
            let _ = std::fs::create_dir_all(&path);
        }
    })
    .await;
}

/// Sum `extra` into `into` so the daemon's IndexReport reflects both the
/// initial single-file write and the cascade of index.md rewrites that
/// followed it. Without this, the auto-built indexes would be silently
/// re-embedded but the report would still claim `files_upserted = 1`.
fn fold_apply_stats(
    into: &mut hallouminate_domain::indexer::ApplyStats,
    extra: &hallouminate_domain::indexer::ApplyStats,
) {
    into.files_upserted += extra.files_upserted;
    into.files_touched += extra.files_touched;
    into.files_deleted += extra.files_deleted;
    into.files_skipped_empty += extra.files_skipped_empty;
    into.files_skipped_unreadable += extra.files_skipped_unreadable;
    into.chunks_inserted += extra.chunks_inserted;
    into.embeddings_inserted += extra.embeddings_inserted;
}

/// Walk from `root` down to the parent of `file_relative`, rewriting each
/// directory's `index.md` between INDEX-START / INDEX-END markers. Returns
/// the aggregate of `index_single_file` stats for every regenerated index
/// file so the caller can roll them into the response. The dir that owns
/// `file_relative` is skipped when the file itself IS that dir's `index.md`
/// — the LLM's verbatim write is the final word for the leaf file, and
/// regenerating would clobber it.
async fn rebuild_wiki_indexes(
    state: &DaemonState,
    cfg: &Config,
    corpus: &CorpusConfig,
    root: &Path,
    file_relative: &Path,
) -> Result<hallouminate_domain::indexer::ApplyStats, String> {
    use hallouminate_domain::corpus::{
        INDEX_FILENAME, ancestor_dirs, compose_index_md, is_index_md,
    };

    let written_is_index = is_index_md(file_relative);
    let mut totals = hallouminate_domain::indexer::ApplyStats::default();
    let dirs = ancestor_dirs(root, file_relative);
    let res = state.resources_for(cfg).await.map_err(|e| e.to_string())?;
    let store = &res.store;
    let registry = state.make_registry();

    for dir in &dirs {
        let index_path = dir.join(INDEX_FILENAME);
        // Skip the dir that owns the file we just wrote if that file IS
        // its index.md — the author's verbatim write wins. `Path::parent`
        // returns `Some(Path::new(""))` for a top-level filename, so this
        // covers root-level `index.md` writes too via `root.join("") == root`.
        if written_is_index
            && let Some(parent) = file_relative.parent()
            && dir == &root.join(parent)
        {
            continue;
        }

        let existing = {
            let owned_root = root.to_path_buf();
            let owned_relative = match index_path.strip_prefix(root) {
                Ok(p) => p.to_path_buf(),
                Err(_) => {
                    return Err(format!(
                        "index path {} not under root",
                        index_path.display()
                    ));
                }
            };
            match tokio::task::spawn_blocking(move || read_no_follow(&owned_root, &owned_relative))
                .await
            {
                // Treat a symlinked index.md as invalid (as if absent) so a
                // child directory's index.md symlink can't smuggle content
                // from outside the corpus into the composed ancestor index.
                Ok(Err(e)) if matches!(e.kind, WriteErrorKind::Symlink) => None,
                Ok(Err(e))
                    if matches!(e.kind, WriteErrorKind::Io)
                        && e.source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Ok(Err(e)) => {
                    return Err(format!("read {}: {}", index_path.display(), e.source));
                }
                Ok(Ok(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                Err(e) => {
                    return Err(format!("read {} join failed: {e}", index_path.display()));
                }
            }
        };

        let is_root = dir == root;
        let (new_content, outcome) = compose_index_md(root, dir, is_root, existing.as_deref())
            .map_err(|e| format!("compose index {}: {e}", dir.display()))?;
        match outcome {
            hallouminate_domain::corpus::RewriteOutcome::NoMarkers
            | hallouminate_domain::corpus::RewriteOutcome::Unchanged => continue,
            hallouminate_domain::corpus::RewriteOutcome::Created
            | hallouminate_domain::corpus::RewriteOutcome::Updated => {}
        }

        // Use the same atomic-write-no-follow path that AddMarkdown uses so
        // the auto-index inherits its symlink safety.
        let rel = match index_path.strip_prefix(root) {
            Ok(p) => p.to_path_buf(),
            Err(_) => {
                return Err(format!(
                    "index path {} not under root",
                    index_path.display()
                ));
            }
        };
        let write_root = root.to_path_buf();
        let write_rel = rel.clone();
        let bytes = new_content.into_bytes();
        let written = tokio::task::spawn_blocking(move || {
            atomic_write_no_follow(&write_root, &write_rel, &bytes, true)
        })
        .await
        .map_err(|e| {
            tracing::error!(
                target: "hallouminate::daemon",
                error = %e,
                "rebuild_wiki_indexes write task panicked",
            );
            format!("index write task panicked: {e}")
        })?;
        let dest = match written {
            Ok(p) => p,
            Err(WriteError { kind, source }) => {
                return Err(format!(
                    "writing index {} failed ({:?}): {source}",
                    index_path.display(),
                    kind,
                ));
            }
        };

        // Refresh LanceDB rows for the just-rewritten index.md. The store owns
        // embedding now (opt-in via its own `Option<Embedder>`): reindex is
        // lexical-only when embeddings are disabled, embedded when enabled.
        let stats = index_single_file(store, &registry, corpus, &dest)
            .await
            .map_err(|e| format!("reindex {}: {e}", dest.display()))?;
        fold_apply_stats(&mut totals, &stats);
    }
    Ok(totals)
}

/// True when `corpus.name` is a derived `repo:{name}:wiki` corpus produced
/// by `effective_corpora()`. The daemon owns those directories (creates
/// them on demand, enforces no-symlink safety), so a few helpers behave
/// differently for them than for user-declared `[[corpus]]` entries.
fn is_wiki_corpus(corpus: &CorpusConfig) -> bool {
    corpus.name.starts_with("repo:") && corpus.name.ends_with(":wiki")
}

/// UX-quality early-exit, NOT the security boundary. Refuses up front on a
/// wiki corpus whose `.hallouminate` parent or `wiki` leaf is already a
/// symlink, producing a clear error instead of a cryptic mid-write failure.
/// The repository's `path` is user-configured (so trusted), but
/// `.hallouminate` and `wiki` are daemon-managed names that a malicious
/// repository payload could swap for symlinks.
///
/// The actual security gate is the `O_NOFOLLOW` component walk inside
/// `atomic_write_no_follow` / `read_no_follow` / `delete_no_follow`: the
/// kernel refuses to traverse a symlinked component there, which holds even
/// for the TOCTOU window this check cannot close (a swap between this stat
/// and the subsequent open). This pre-flight only improves the error message;
/// removing it would not weaken safety. The daemon also serializes wiki
/// mutations behind the per-corpus mutex, so a swap in the narrow window races
/// the daemon's own consistency model.
fn ensure_wiki_root_safe(corpus: &CorpusConfig) -> Result<(), String> {
    if !is_wiki_corpus(corpus) {
        return Ok(());
    }
    let Some(raw) = corpus.paths.first() else {
        return Ok(());
    };
    let root = hallouminate_domain::common::expand_tilde(raw);
    if let Some(parent) = root.parent()
        && let Ok(meta) = std::fs::symlink_metadata(parent)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "wiki corpus {} is unsafe: parent {} is a symlink",
            corpus.name,
            parent.display(),
        ));
    }
    if let Ok(meta) = std::fs::symlink_metadata(&root)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "wiki corpus {} is unsafe: wiki root is a symlink",
            corpus.name,
        ));
    }
    Ok(())
}

/// Shared error mapping for `read_no_follow` failures — keeps
/// `handle_read_markdown` flat while preserving the distinct
/// NotFound / Symlink / IO shapes callers depend on.
fn map_read_error(kind: WriteErrorKind, source: std::io::Error, relative: &Path) -> DaemonResponse {
    match kind {
        WriteErrorKind::Symlink => DaemonResponse::invalid_params(format!(
            "refusing to read symlink {}: {source}",
            relative.display()
        )),
        WriteErrorKind::InvalidPath => {
            if source.kind() == std::io::ErrorKind::NotFound {
                DaemonResponse::invalid_params(format!("{} does not exist", relative.display()))
            } else {
                DaemonResponse::invalid_params(format!(
                    "refusing unsafe path {}: {source}",
                    relative.display()
                ))
            }
        }
        WriteErrorKind::Io => {
            if source.kind() == std::io::ErrorKind::NotFound {
                DaemonResponse::invalid_params(format!("{} does not exist", relative.display()))
            } else {
                DaemonResponse::internal(format!("read {}: {source}", relative.display()))
            }
        }
        WriteErrorKind::Exists => DaemonResponse::internal(source.to_string()),
    }
}

/// Shared error mapping for `delete_no_follow` failures — mirrors
/// `map_read_error` so the two handlers share one error vocabulary.
fn map_delete_error(
    kind: WriteErrorKind,
    source: std::io::Error,
    relative: &Path,
) -> DaemonResponse {
    match kind {
        WriteErrorKind::Symlink => DaemonResponse::invalid_params(format!(
            "refusing to delete symlink {}: {source}",
            relative.display()
        )),
        WriteErrorKind::InvalidPath => {
            if source.kind() == std::io::ErrorKind::NotFound {
                DaemonResponse::invalid_params(format!("{} does not exist", relative.display()))
            } else {
                DaemonResponse::invalid_params(format!(
                    "refusing unsafe path {}: {source}",
                    relative.display()
                ))
            }
        }
        WriteErrorKind::Io => {
            if source.kind() == std::io::ErrorKind::NotFound {
                DaemonResponse::invalid_params(format!("{} does not exist", relative.display()))
            } else {
                DaemonResponse::internal(format!("unlink {}: {source}", relative.display()))
            }
        }
        WriteErrorKind::Exists => DaemonResponse::internal(source.to_string()),
    }
}

#[cfg(test)]
use serde_json::Value;

/// Test helper for the corpus-name vocabulary the daemon dispatcher resolves
/// through. Gated behind `#[cfg(test)]` because production handlers reach
/// straight into `effective_corpora()` and never call this name constructor.
#[cfg(test)]
fn derived_corpus_name(repo_name: &str, kind: RepoCorpusKind) -> Result<String, String> {
    repo_corpus_name(repo_name, kind).map_err(|e| e.to_string())
}

/// Canonical `Ping` reply payload — dispatch() encodes a [`PongResult`]
/// carrying the daemon binary's version; this helper lets tests match against
/// the same shape without hand-rolling the JSON.
#[cfg(test)]
fn pong_value() -> Value {
    serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })
}

#[cfg(test)]
mod tests {
    //! Dispatch-level tests. The corpus-boundary helpers
    //! (`safe_relative_path`, `pick_corpus`, `ensure_corpus_allows_file`,
    //! `first_corpus_root`, `atomic_write_no_follow`, `list_corpus_files`)
    //! moved to `hallouminate_domain::corpus` and are tested there once,
    //! against a single contract. These tests only cover the daemon-only
    //! helpers (`derived_corpus_name`, `pong_value`).

    use super::*;

    #[test]
    fn derived_corpus_name_emits_canonical_string_for_valid_inputs() {
        let name = derived_corpus_name("tern", RepoCorpusKind::Wiki)
            .expect("valid repo name must succeed");
        assert_eq!(name, "repo:tern:wiki");
    }

    #[test]
    fn derived_corpus_name_surfaces_underlying_error_as_string() {
        let err =
            derived_corpus_name("", RepoCorpusKind::Wiki).expect_err("empty repo name must fail");
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn pong_value_carries_the_daemon_binary_version() {
        // Curd C: the Ping reply is a `{ "version": <CARGO_PKG_VERSION> }`
        // envelope, not the bare `"pong"` string — the MCP bootstrap reads
        // this field to detect cross-version skew.
        assert_eq!(pong_value()["version"], env!("CARGO_PKG_VERSION"));
    }

    /// A normal mtime (well under `i64::MAX` ms) converts to the exact
    /// millisecond count — the happy path the indexer relies on for change
    /// detection.
    #[test]
    fn mtime_ms_from_duration_passes_through_normal_value() {
        let dur = std::time::Duration::from_millis(1_700_000_000_000);
        let got =
            mtime_ms_from_duration(dur, Path::new("/tmp/a.md")).expect("a sane mtime must convert");
        assert_eq!(got, 1_700_000_000_000_i64);
    }

    /// Boundary: exactly `i64::MAX` milliseconds is the largest representable
    /// mtime and must convert, not error. Pins the conversion to `<`-overflow
    /// semantics so a future off-by-one (rejecting the max valid value) is
    /// caught.
    #[test]
    fn mtime_ms_from_duration_accepts_i64_max_milliseconds() {
        let max_ms = u64::try_from(i64::MAX).unwrap();
        let dur = std::time::Duration::from_millis(max_ms);
        let got = mtime_ms_from_duration(dur, Path::new("/tmp/max.md"))
            .expect("i64::MAX ms is representable and must convert");
        assert_eq!(got, i64::MAX);
    }

    /// WHY this matters: the old `.as_millis() as i64` silently truncated the
    /// `u128`, so a duration whose millisecond count exceeds `i64::MAX` would
    /// wrap into a bogus (possibly negative) mtime the indexer would then
    /// trust for change detection — masking edits or forcing needless
    /// re-embeds. `i64::try_from` must reject it loudly instead. Tests the
    /// first value past the boundary (`i64::MAX + 1` ms), so an off-by-one in
    /// the bound would be caught alongside the gross-overflow case.
    #[test]
    fn mtime_ms_from_duration_errors_one_past_i64_max() {
        let overflow_ms = u64::try_from(i64::MAX).unwrap() + 1;
        let dur = std::time::Duration::from_millis(overflow_ms);
        let err = mtime_ms_from_duration(dur, Path::new("/tmp/huge.md"))
            .expect_err("an mtime past i64::MAX ms must error, not truncate");
        let msg = err.to_string();
        assert!(
            msg.contains("overflows i64") && msg.contains("huge.md"),
            "overflow error must name the cause and file: {msg}",
        );
    }

    /// FileEntry is re-exported from the shared sandbox module to keep the
    /// daemon's response struct serializing the same shape as before the
    /// extract — list_files clients depend on the `{ path, absolute_path }`
    /// field names.
    #[test]
    fn file_entry_re_export_keeps_field_names() {
        let entry = FileEntry {
            path: "a.md".to_string(),
            absolute_path: "/r/a.md".to_string(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["path"], "a.md");
        assert_eq!(json["absolute_path"], "/r/a.md");
    }

    // ── validate_wiki_path: the shared add/read/delete preamble ──────────
    //
    // Curd B folded the five-step validation that `handle_add_markdown`,
    // `handle_read_markdown`, and `handle_delete_markdown` each repeated
    // verbatim onto `validate_wiki_path`. Its doc promises the handlers'
    // error shapes survive "byte-for-byte" and that every step still fires.
    // The underlying helpers are unit-tested in `sandbox`, but nothing pinned
    // the wiring: that the helper maps each failing step onto `InvalidParams`
    // (not `Internal`) and that no middle step (e.g. the glob check) was
    // dropped in the extraction. These tests lock that seam.

    fn wiki_corpus_at(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "repo:tern:wiki".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        }
    }

    fn assert_invalid_params(resp: DaemonResponse, needle: &str) {
        match resp {
            DaemonResponse::Err { kind, message } => {
                assert_eq!(
                    kind,
                    ErrorKind::InvalidParams,
                    "a validation failure must surface as InvalidParams, not a \
                     server fault: {message}",
                );
                assert!(
                    message.contains(needle),
                    "error must explain the failing step (want {needle:?}): {message}",
                );
            }
            DaemonResponse::Ok { result } => {
                panic!("expected an InvalidParams error, got Ok({result:?})");
            }
        }
    }

    #[test]
    fn validate_wiki_path_returns_corpus_root_and_relative_on_valid_input() {
        // Happy path: a known corpus + a glob-allowed relative file resolves to
        // the tuple the handlers then write/read/delete against. The relative
        // path is returned verbatim (not joined onto root) so the caller keeps
        // building `root.join(relative)` exactly as before the extract.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let corpus = wiki_corpus_at(&root);
        let (got_corpus, got_root, got_relative) = validate_wiki_path(
            std::slice::from_ref(&corpus),
            "repo:tern:wiki",
            "notes/a.md",
        )
        .expect("valid corpus + path must resolve");
        assert_eq!(got_corpus.name, "repo:tern:wiki");
        assert_eq!(got_root, root);
        assert_eq!(got_relative, std::path::PathBuf::from("notes/a.md"));
    }

    #[test]
    fn validate_wiki_path_maps_unknown_corpus_to_invalid_params() {
        // First step (`pick_corpus`): an unknown corpus name is caller error,
        // so it must be InvalidParams — never Internal, which clients retry.
        let tmp = tempfile::tempdir().unwrap();
        let corpus = wiki_corpus_at(tmp.path());
        let resp = validate_wiki_path(std::slice::from_ref(&corpus), "repo:nope:wiki", "a.md")
            .expect_err("unknown corpus must fail validation");
        assert_invalid_params(resp, "not found");
    }

    #[test]
    fn validate_wiki_path_rejects_path_traversal_as_invalid_params() {
        // Third step (`safe_relative_path`): a `..` escape must be refused at
        // the boundary, mapped to InvalidParams. Proves the path-sandbox step
        // is still wired into the shared preamble.
        let tmp = tempfile::tempdir().unwrap();
        let corpus = wiki_corpus_at(tmp.path());
        let resp = validate_wiki_path(
            std::slice::from_ref(&corpus),
            "repo:tern:wiki",
            "../../etc/passwd",
        )
        .expect_err("path traversal must fail validation");
        assert_invalid_params(resp, "normal file components");
    }

    #[test]
    fn validate_wiki_path_enforces_corpus_globs_as_invalid_params() {
        // Fourth step (`ensure_corpus_allows_file`): a glob-allowed path passes
        // the earlier steps but is excluded by the corpus's `**/*.md` include
        // set. This is the middle step most easily dropped in a refactor — a
        // `.txt` path that sails through `safe_relative_path` would silently be
        // accepted if the glob check were lost. Pin that it still fires.
        let tmp = tempfile::tempdir().unwrap();
        let corpus = wiki_corpus_at(tmp.path());
        let resp = validate_wiki_path(
            std::slice::from_ref(&corpus),
            "repo:tern:wiki",
            "notes/a.txt",
        )
        .expect_err("a non-markdown path must be rejected by corpus globs");
        assert_invalid_params(resp, "not included by corpus globs");
    }

    // ── dispatch + resolve_for_cwd integration ──────────────────────────
    //
    // These tests cover AC #2 from .cheese/specs/repo-config-discovery.md:
    // dispatch consumes `req.cwd` via `resolve_for_cwd` on every request,
    // and a discovery / merge failure surfaces to the client as
    // `InvalidParams` (not a silent fall-back to baseline-only).

    use std::path::Path;

    use crate::ErrorKind;

    /// Build a `DaemonState` with a baseline `Config` that points its
    /// ground_dir at a tempdir-local subdir. Embedder load is tolerated
    /// failure on first run (see `DaemonState::open`), so a cold cache
    /// doesn't break tests that don't exercise the embedder.
    async fn state_with_ground(ground_dir: &Path, baseline_toml: &str) -> DaemonState {
        let toml = format!(
            "{baseline_toml}\n[storage]\nground_dir = \"{}\"\n",
            ground_dir.display(),
        );
        let cfg: Config = toml::from_str(&toml).expect("baseline toml parses");
        DaemonState::open(cfg, None)
            .await
            .expect("open daemon state")
    }

    fn write_repo_layer(repo_root: &Path, body: &str) {
        let cfg_dir = repo_root.join(".hallouminate");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir .hallouminate");
        std::fs::write(cfg_dir.join("config.toml"), body).expect("write repo config");
    }

    #[tokio::test]
    async fn catch_up_reindexes_changed_corpus_and_skips_unchanged() {
        // AC #6: a corpus edited during the daemon's down-window is picked up
        // by the boot catch-up sweep; a second pass over an unchanged corpus
        // does no work (Ok(None)) and loads no model. Embeddings OFF keeps it
        // hermetic.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("docs_src");
        std::fs::create_dir_all(&root).expect("mkdir docs_src");
        let ground = tmp.path().join("ground");
        let baseline = format!(
            "[[corpus]]\nname = \"docs\"\npaths = [\"{}\"]\nglobs = [\"**/*.md\"]\n[embeddings]\nenabled = false\n",
            root.display(),
        );
        let state = state_with_ground(&ground, &baseline).await;
        std::fs::write(root.join("a.md"), "# Title\n\nbody\n").expect("write a.md");

        catch_up_index(state.clone()).await;
        assert_eq!(
            state.store().list_files("docs").await.expect("list").len(),
            1,
            "boot catch-up must index the file created during the down-window",
        );

        let corpus = state
            .baseline()
            .effective_corpora()
            .expect("corpora")
            .into_iter()
            .find(|c| c.name == "docs")
            .expect("docs corpus present");
        let res = state
            .resources_for(state.baseline())
            .await
            .expect("resources_for");
        assert!(
            catch_up_corpus(&res, &state.make_registry(), &corpus)
                .await
                .expect("catch_up_corpus")
                .is_none(),
            "an unchanged corpus must produce no work (Ok(None)) and load no model",
        );
    }

    #[tokio::test]
    async fn dispatch_ping_is_config_independent_and_reports_version() {
        // Curd C: `Ping` is a config-independent control op handled BEFORE
        // `resolve_for_cwd`, so it returns the versioned pong envelope even
        // when `cwd` has no discoverable repo config — a liveness/version
        // probe must not depend on the probing client's working directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        // Deliberately do NOT seed a repo layer; resolve_for_cwd would fail
        // for a config-dependent op, but Ping must still succeed.
        let cwd = tmp.path().to_path_buf();
        let ground = tmp.path().join("ground");
        let state = state_with_ground(&ground, "").await;

        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::Ping,
        };
        let resp = dispatch(&state, req).await;
        match resp {
            DaemonResponse::Ok { result } => {
                assert_eq!(result, pong_value(), "ping must return the versioned pong");
            }
            DaemonResponse::Err { kind, message } => {
                panic!("ping must succeed regardless of cwd; got {kind:?}: {message}");
            }
        }
    }

    #[tokio::test]
    async fn dispatch_above_all_repos_falls_back_to_baseline_corpora() {
        // Issue #102: a `cwd` whose ancestry contains neither `.hallouminate/`
        // nor `.git` until the filesystem root must NOT hard-error every op.
        // The discovery walk reaches the root with no boundary, so dispatch
        // resolves baseline-only — the baseline-declared corpora stay reachable
        // from a parent-of-repos directory instead of being stranded.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let ground = tmp.path().join("ground");
        // Baseline declares a globally-searchable corpus (the XDG analogue of
        // `cheese-global` in the issue repro).
        let state = state_with_ground(
            &ground,
            "[[corpus]]\nname = \"cheese-global\"\npaths = [\"/srv/cheese-global\"]\n",
        )
        .await;

        // ListCorpora is config-dependent (it runs `resolve_for_cwd`); from
        // above all repos it must list the baseline corpus, not error.
        let req = DaemonRequest {
            cwd: cwd.clone(),
            payload: DaemonRequestPayload::ListCorpora,
        };
        let resp = dispatch(&state, req).await;
        match resp {
            DaemonResponse::Ok { result } => {
                let entries = result.as_array().expect("ListCorpora returns an array");
                let names: Vec<&str> = entries
                    .iter()
                    .filter_map(|e| e.get("name").and_then(serde_json::Value::as_str))
                    .collect();
                assert!(
                    names.contains(&"cheese-global"),
                    "baseline corpus must be reachable from above all repos; got {names:?}",
                );
            }
            // Unusual CI sandbox whose tmp tree sits inside a checkout trips a
            // `.git` boundary before the root — that path is still a hard error
            // (covered explicitly below), and acceptable here.
            DaemonResponse::Err { kind, message } => {
                assert_eq!(kind, ErrorKind::InvalidParams, "{message}");
                assert!(message.contains("stopped at repo root"), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn dispatch_inside_repo_without_config_still_hard_errors() {
        // Issue #102 keeps the deliberate in-repo strictness: a `.git` boundary
        // with no `.hallouminate/config.toml` between cwd and the repo root must
        // still fail every config-dependent op rather than silently fall back to
        // baseline-only. Only the no-`.git` parent-dir case soft-falls-back.
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().to_path_buf();
        std::fs::create_dir(repo_root.join(".git")).expect("mkdir .git");
        let cwd = repo_root.join("src");
        std::fs::create_dir_all(&cwd).expect("mkdir nested");
        let ground = tmp.path().join("ground");
        let state = state_with_ground(&ground, "").await;

        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ListCorpora,
        };
        let resp = dispatch(&state, req).await;
        match resp {
            DaemonResponse::Err { kind, message } => {
                assert_eq!(
                    kind,
                    ErrorKind::InvalidParams,
                    "discovery failure must map to InvalidParams: {message}",
                );
                assert!(
                    message.contains("stopped at repo root"),
                    "in-repo discovery error must explain the boundary: {message}",
                );
            }
            DaemonResponse::Ok { result } => {
                panic!("must not fall back to baseline-only inside a repo; got Ok({result:?})");
            }
        }
    }

    #[tokio::test]
    async fn dispatch_with_scalar_conflict_returns_config_error() {
        // Baseline explicitly sets `embeddings.cache_dir = "/a"`; repo layer
        // explicitly sets `embeddings.cache_dir = "/b"`. `merge_layers`
        // refuses the conflict, and dispatch must propagate that as
        // `InvalidParams` with the offending field named.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        write_repo_layer(&cwd, "[embeddings]\ncache_dir = \"/b\"\n");
        let ground = tmp.path().join("ground");
        let state = state_with_ground(&ground, "[embeddings]\ncache_dir = \"/a\"\n").await;

        // ListCorpora runs `resolve_for_cwd`; Ping no longer does (Curd C).
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ListCorpora,
        };
        let resp = dispatch(&state, req).await;
        match resp {
            DaemonResponse::Err { kind, message } => {
                assert_eq!(
                    kind,
                    ErrorKind::InvalidParams,
                    "merge conflict must map to InvalidParams: {message}",
                );
                assert!(
                    message.contains("embeddings.cache_dir"),
                    "conflict error must name the field: {message}",
                );
                assert!(
                    message.contains("\"/a\"") && message.contains("\"/b\""),
                    "conflict error must show both values: {message}",
                );
            }
            DaemonResponse::Ok { result } => {
                panic!("scalar conflict must error; got Ok({result:?})");
            }
        }
    }

    /// AC #7 regression: when the daemon was booted with a known baseline
    /// source path (XDG or `--config PATH`), scalar-conflict messages must
    /// name that path so the user knows which file holds the offending
    /// baseline value. Before this curd, dispatch passed `xdg_path: None`
    /// hard-coded and conflict messages said `"(XDG baseline)"` instead.
    #[tokio::test]
    async fn dispatch_scalar_conflict_names_baseline_xdg_path_when_known() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        write_repo_layer(&cwd, "[embeddings]\ncache_dir = \"/b\"\n");
        let ground = tmp.path().join("ground");
        let baseline_path = tmp.path().join("baseline.toml");
        // Construct state with an explicit baseline source path. The path
        // itself doesn't have to be the file the toml came from (the daemon
        // already has the parsed Config in memory by this point); we just
        // need a sentinel string to verify it lands in the diagnostic.
        let baseline_toml = format!(
            "[embeddings]\ncache_dir = \"/a\"\n[storage]\nground_dir = \"{}\"\n",
            ground.display(),
        );
        let cfg: Config = toml::from_str(&baseline_toml).expect("baseline parses");
        let state = DaemonState::open(cfg, Some(baseline_path.clone()))
            .await
            .expect("open with xdg_path");

        // ListCorpora runs `resolve_for_cwd`; Ping no longer does (Curd C).
        let req = DaemonRequest {
            cwd,
            payload: DaemonRequestPayload::ListCorpora,
        };
        let resp = dispatch(&state, req).await;
        let DaemonResponse::Err { message, .. } = resp else {
            panic!("scalar conflict must error");
        };
        assert!(
            message.contains(&baseline_path.display().to_string()),
            "conflict message must name the baseline source path: {message}",
        );
        assert!(
            !message.contains("(XDG baseline)"),
            "must not fall back to the unsourced placeholder: {message}",
        );
    }

    // --- #135: mtime_ms_from_rfc3339 ---

    #[test]
    fn mtime_ms_from_rfc3339_parses_known_timestamp() {
        // 2026-04-30T10:11:23Z in ms since epoch.
        let ms = mtime_ms_from_rfc3339("2026-04-30T10:11:23Z");
        assert_eq!(ms, 1_777_543_883_000_i64);
    }

    #[test]
    fn mtime_ms_from_rfc3339_returns_i64_min_for_invalid_input() {
        // Invalid timestamps must produce i64::MIN so any real disk mtime
        // compares as newer — safe-to-mark-stale rather than hiding drift.
        let ms = mtime_ms_from_rfc3339("not-a-date");
        assert_eq!(ms, i64::MIN);
    }

    // --- #135: stale-detection ---
    // These tests call `mark_stale` directly so they exercise the real
    // production function; deleting the `mark_stale` call from
    // `handle_ground` would leave this wiring untested and require
    // the integration test in tests/daemon.rs to catch it.

    #[tokio::test]
    async fn stale_false_when_file_unchanged_since_index() {
        // Build a GroundResponse whose indexed mtime matches the file's real
        // disk mtime — stale must be false.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("doc.md");
        std::fs::write(&file, "# Title\n\nBody text.\n").expect("write");

        let meta = std::fs::metadata(&file).expect("stat");
        let disk_ms = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let indexed_mtime = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(disk_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let abs = canonicalize_or_passthrough(&file);
        let abs_str = abs.as_path().to_str().unwrap().to_string();
        let mut docs = std::collections::BTreeMap::new();
        docs.insert(
            abs_str.clone(),
            hallouminate_domain::ground::DocFile {
                summary: None,
                keywords: vec![],
                score: 0.5,
                z_score: None,
                mtime: indexed_mtime,
                corpus: "test".into(),
                path: None,
                stale: false,
                chunks: vec![],
            },
        );
        let mut response = hallouminate_domain::ground::GroundResponse {
            query: "test".into(),
            took_ms: 0,
            stats: hallouminate_domain::ground::Stats { hits: 1 },
            docs,
            code: std::collections::BTreeMap::new(),
            warnings: vec![],
        };

        mark_stale(&mut response).await;

        assert!(
            !response.docs[&abs_str].stale,
            "file unchanged since index must not be stale"
        );
    }

    #[tokio::test]
    async fn stale_true_when_file_modified_after_index() {
        // Simulate an indexed mtime one second in the past — stale must be true.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("doc.md");
        std::fs::write(&file, "# Title\n\nOriginal.\n").expect("write");

        let meta = std::fs::metadata(&file).expect("stat");
        let disk_ms = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let past_ms = disk_ms - 1_000; // indexed one second earlier
        let indexed_mtime = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(past_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let abs = canonicalize_or_passthrough(&file);
        let abs_str = abs.as_path().to_str().unwrap().to_string();
        let mut docs = std::collections::BTreeMap::new();
        docs.insert(
            abs_str.clone(),
            hallouminate_domain::ground::DocFile {
                summary: None,
                keywords: vec![],
                score: 0.5,
                z_score: None,
                mtime: indexed_mtime,
                corpus: "test".into(),
                path: None,
                stale: false,
                chunks: vec![],
            },
        );
        let mut response = hallouminate_domain::ground::GroundResponse {
            query: "test".into(),
            took_ms: 0,
            stats: hallouminate_domain::ground::Stats { hits: 1 },
            docs,
            code: std::collections::BTreeMap::new(),
            warnings: vec![],
        };

        mark_stale(&mut response).await;

        assert!(
            response.docs[&abs_str].stale,
            "file modified after index must be stale"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corpus_stats_counts_indexed_and_unindexed_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir wiki");
        let ground = tmp.path().join("ground");

        let repo_config = format!(
            "[[corpus]]\nname = \"test\"\npaths = [\"{}\"]\n",
            corpus_dir.display()
        );
        write_repo_layer(tmp.path(), &repo_config);
        let state = state_with_ground(&ground, "[embeddings]\nenabled = false\n").await;

        // Write 2 files and index them.
        std::fs::write(corpus_dir.join("a.md"), "# Doc A\n\nContent A.\n").expect("write a");
        std::fs::write(corpus_dir.join("b.md"), "# Doc B\n\nContent B.\n").expect("write b");

        let index_resp = dispatch(
            &state,
            DaemonRequest {
                cwd: tmp.path().to_path_buf(),
                payload: DaemonRequestPayload::Index(IndexRequest {
                    corpus: Some("test".to_string()),
                    paths_from: None,
                    strict: false,
                }),
            },
        )
        .await;
        assert!(
            matches!(index_resp, DaemonResponse::Ok { .. }),
            "index must succeed: {index_resp:?}"
        );

        // Add a third file without re-indexing — this becomes the unindexed count.
        std::fs::write(corpus_dir.join("c.md"), "# Doc C\n\nUnindexed.\n").expect("write c");

        let resp = dispatch(
            &state,
            DaemonRequest {
                cwd: tmp.path().to_path_buf(),
                payload: DaemonRequestPayload::CorpusStats { corpus: None },
            },
        )
        .await;
        let DaemonResponse::Ok { result } = resp else {
            panic!("corpus_stats must succeed: {resp:?}");
        };
        let stats: CorpusStatsResult =
            serde_json::from_value(result).expect("parse CorpusStatsResult");
        assert_eq!(stats.indexed_files, 2, "two files were indexed");
        assert_eq!(stats.unindexed_files, 1, "one file added without re-index");
        assert!(
            stats.last_indexed_ms.is_some(),
            "indexed corpus must carry a timestamp"
        );
        assert_eq!(stats.corpus, "test");
    }

    /// WHY: files excluded by the corpus `exclude` globs are out of scope and
    /// must not inflate `unindexed_files`. Without this, an excluded file that
    /// happens to match the `globs` include pattern would be counted as missing
    /// from the index, even though the corpus is intentionally ignoring it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corpus_stats_excludes_glob_excluded_files_from_unindexed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_dir = tmp.path().join("wiki");
        std::fs::create_dir_all(&corpus_dir).expect("mkdir wiki");
        let ground = tmp.path().join("ground");

        // Corpus includes *.md but explicitly excludes "excluded.md".
        let repo_config = format!(
            concat!(
                "[[corpus]]\nname = \"test\"\n",
                "paths = [\"{}\"]",
                "\nglobs = [\"**/*.md\"]\nexclude = [\"**/excluded.md\"]\n"
            ),
            corpus_dir.display()
        );
        write_repo_layer(tmp.path(), &repo_config);
        let state = state_with_ground(&ground, "[embeddings]\nenabled = false\n").await;

        // Write and index one normal file.
        std::fs::write(corpus_dir.join("indexed.md"), "# Indexed\n\nContent.\n")
            .expect("write indexed");
        let index_resp = dispatch(
            &state,
            DaemonRequest {
                cwd: tmp.path().to_path_buf(),
                payload: DaemonRequestPayload::Index(IndexRequest {
                    corpus: Some("test".to_string()),
                    paths_from: None,
                    strict: false,
                }),
            },
        )
        .await;
        assert!(
            matches!(index_resp, DaemonResponse::Ok { .. }),
            "index must succeed: {index_resp:?}"
        );

        // Write the excluded file to disk WITHOUT indexing it.
        // It matches the include glob (*.md) but is excluded by name.
        std::fs::write(
            corpus_dir.join("excluded.md"),
            "# Excluded\n\nShould not be counted as unindexed.\n",
        )
        .expect("write excluded");

        let resp = dispatch(
            &state,
            DaemonRequest {
                cwd: tmp.path().to_path_buf(),
                payload: DaemonRequestPayload::CorpusStats { corpus: None },
            },
        )
        .await;
        let DaemonResponse::Ok { result } = resp else {
            panic!("corpus_stats must succeed: {resp:?}");
        };
        let stats: CorpusStatsResult =
            serde_json::from_value(result).expect("parse CorpusStatsResult");
        assert_eq!(stats.indexed_files, 1, "one file was indexed");
        // excluded.md is out of scope — it must not appear in unindexed_files.
        assert_eq!(
            stats.unindexed_files, 0,
            "excluded file must not count toward unindexed_files"
        );
    }

    /// WHY: a freshly-configured wiki corpus whose directory has never been
    /// created must return zeroed stats (indexed_files=0, unindexed_files=0),
    /// not an internal error. Without `ensure_paths_exist`, `list_corpus_files`
    /// → `scan()` fails fatally on the missing root.
    #[tokio::test]
    async fn corpus_stats_returns_zeroed_result_for_never_created_wiki_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path();
        // The wiki dir (.hallouminate/wiki) is intentionally NOT created.
        let ground = tmp.path().join("ground");

        let repo_config = format!(
            "[[repository]]\nname = \"myrepo\"\npath = \"{}\"\n",
            repo_root.display()
        );
        write_repo_layer(repo_root, &repo_config);
        let state = state_with_ground(&ground, "").await;

        let resp = dispatch(
            &state,
            DaemonRequest {
                cwd: repo_root.to_path_buf(),
                payload: DaemonRequestPayload::CorpusStats {
                    corpus: Some("repo:myrepo:wiki".to_string()),
                },
            },
        )
        .await;

        let DaemonResponse::Ok { result } = resp else {
            panic!(
                "corpus_stats on a never-created wiki corpus must return Ok, not an error: {resp:?}"
            );
        };
        let stats: CorpusStatsResult =
            serde_json::from_value(result).expect("parse CorpusStatsResult");
        assert_eq!(stats.indexed_files, 0, "no files indexed yet");
        assert_eq!(stats.total_chunks, 0, "no chunks yet");
        assert_eq!(stats.unindexed_files, 0, "empty dir has no unindexed files");
        assert!(
            stats.last_indexed_ms.is_none(),
            "never-indexed corpus must have null timestamp"
        );
        assert_eq!(stats.corpus, "repo:myrepo:wiki");
    }

    // ── index_single_file eviction policy ────────────────────────────────
    //
    // Regression for the multi-format-ingestion finding: on the single-file
    // (watch / add_markdown) path, a present-but-unreadable re-extraction
    // (corrupt workbook, non-UTF-8 text, unsupported type) must RETAIN the
    // file's last-good rows — matching bulk `index_corpus`, which never
    // deletes a file still on disk. Only a genuine truncate-to-empty re-index
    // evicts. Before the fix both skip kinds routed through
    // `files_skipped_empty`, so a transient parse failure silently dropped the
    // file from search.

    fn spreadsheet_corpus_at(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.csv".into()],
            exclude: vec![],
            global: false,
        }
    }

    /// Open an embeddings-OFF store. The eviction policy is independent of
    /// embeddings, so `None` keeps the test free of the embedding model.
    async fn open_off_store(dir: &Path) -> LanceStore {
        LanceStore::open_or_create(dir, "BAAI/bge-small-en-v1.5", false, false, None)
            .await
            .expect("open store")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_retains_last_good_rows_when_reindex_is_unreadable() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let file = corpus_dir.path().join("data.csv");
        let corpus = spreadsheet_corpus_at(corpus_dir.path());
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);

        // 1. Index a valid CSV: rows land in the index. Compute `file_ref`
        //    after the write so it canonicalizes the same way `index_single_file`
        //    does (on macOS, tempdirs symlink /var → /private/var, so a
        //    canonicalize of a not-yet-existing path would passthrough uncanonicalized
        //    and mismatch the stored row).
        std::fs::write(&file, "name,note\nbolt,sturdy fastener\n").unwrap();
        let file_ref = canonicalize_or_passthrough(&file)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();
        let s1 = index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index of a valid file must succeed");
        assert_eq!(s1.files_upserted, 1, "valid CSV indexes");
        let after_good = store.corpus_chunk_stats(&corpus.name).await.unwrap();
        assert!(
            after_good.total_chunks > 0,
            "the valid CSV must produce indexed rows"
        );

        // 2. Corrupt the file in place, then re-index. The extraction fails →
        //    counted as unreadable, NOT empty; the last-good rows must survive.
        std::fs::write(&file, b"\xff\xfe\x00 not,a valid\x00 spreadsheet").unwrap();
        let s2 = index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("a corrupt re-extraction must not hard-error");
        assert_eq!(
            s2.files_skipped_unreadable, 1,
            "a corrupt re-extraction is an unreadable skip"
        );
        assert_eq!(
            s2.files_skipped_empty, 0,
            "an extraction failure must NOT be counted as truncate-to-empty"
        );
        assert_eq!(
            s2.files_deleted, 0,
            "a present-but-unreadable file must NOT be evicted from the index"
        );
        let after_corrupt = store.corpus_chunk_stats(&corpus.name).await.unwrap();
        assert_eq!(
            after_corrupt.total_chunks, after_good.total_chunks,
            "last-good rows must survive a transient parse failure"
        );
        assert!(
            store
                .get_file_snapshot(&corpus.name, &file_ref)
                .await
                .unwrap()
                .is_some(),
            "the file's snapshot row must still be present after an unreadable re-index"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_evicts_when_reindex_truncates_to_empty() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        // A markdown corpus so a truncate-to-empty re-index produces zero
        // chunks (the genuine empty case the eviction branch was written for).
        let file = corpus_dir.path().join("note.md");
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);
        let file_ref = canonicalize_or_passthrough(&file)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        std::fs::write(&file, "# Note\n\nspice melange harvested on Arrakis\n").unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");
        assert!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks
                > 0,
            "the valid markdown must index rows"
        );

        // Truncate to empty: zero chunks → genuine empty → eviction fires.
        std::fs::write(&file, "").unwrap();
        let s = index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("re-index of a now-empty file must not hard-error");
        assert_eq!(
            s.files_skipped_empty, 1,
            "a truncate-to-empty re-index is the genuine empty case"
        );
        assert_eq!(
            s.files_deleted, 1,
            "the genuine empty case must still evict the prior rows"
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks,
            0,
            "the truncated file's rows must be removed from the index"
        );
        assert!(
            store
                .get_file_snapshot(&corpus.name, &file_ref)
                .await
                .unwrap()
                .is_none(),
            "the truncated file's snapshot row must be gone after eviction"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_evicts_when_reindex_truncates_to_empty_with_unchanged_mtime() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let file = corpus_dir.path().join("note.md");
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![corpus_dir.path().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);
        let file_ref = canonicalize_or_passthrough(&file)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        std::fs::write(&file, "# Note\n\nspice melange harvested on Arrakis\n").unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");
        assert!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks
                > 0,
            "the valid markdown must index rows"
        );

        // Truncate to empty but pin the mtime back to its pre-truncation value:
        // the regression case is a same-second truncate-to-empty where the
        // filesystem mtime does not advance. It must still be evicted, not
        // silently routed into the upsert path as if it were a brand-new file.
        let indexed_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, "").unwrap();
        std::fs::File::open(&file)
            .unwrap()
            .set_modified(indexed_mtime)
            .unwrap();

        let s = index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("re-index of a now-empty file with unchanged mtime must not hard-error");
        assert_eq!(
            s.files_skipped_empty, 1,
            "a truncate-to-empty re-index is the genuine empty case"
        );
        assert_eq!(
            s.files_deleted, 1,
            "the genuine empty case must still evict prior rows even when mtime did not change"
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks,
            0,
            "the truncated file's rows must be removed from the index"
        );
        assert!(
            store
                .get_file_snapshot(&corpus.name, &file_ref)
                .await
                .unwrap()
                .is_none(),
            "the truncated file's snapshot row must be gone after eviction"
        );
    }

    // ADR daemon-rework-003: the single-file reindex gate compares the blake3
    // of the bytes ALREADY READ (the watcher's no-follow read) against the
    // stored snapshot's content_hash. Hash-equal must skip re-chunk/re-embed
    // and report zero upserts; the comparison must never re-read the file
    // from disk, which would reopen the symlink-swap TOCTOU window and judge
    // content the caller never read.

    fn md_corpus_at(root: &Path) -> CorpusConfig {
        CorpusConfig {
            name: "docs".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_skips_rechunk_when_content_hash_and_mtime_match_snapshot() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let file = corpus_dir.path().join("note.md");
        let corpus = md_corpus_at(corpus_dir.path());
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);

        let content: &[u8] = b"# Note\n\nthe spice must flow\n";
        std::fs::write(&file, content).unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");
        let baseline = store.corpus_chunk_stats(&corpus.name).await.unwrap();
        assert!(baseline.total_chunks > 0, "first index must produce rows");

        // Swap the DISK content after the (simulated) no-follow read: the
        // gate must judge the bytes in hand, not a second disk read.
        std::fs::write(&file, "# Note\n\nswapped after the read\n").unwrap();

        let stats =
            index_single_file_with_content(&store, &registry, &corpus, &file, content, mtime)
                .await
                .expect("hash-equal reindex must succeed");
        assert_eq!(
            stats.files_upserted, 0,
            "identical content must not be re-chunked or re-embedded"
        );
        assert_eq!(stats.files_touched, 0, "identical mtime needs no touch");
        assert_eq!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks,
            baseline.total_chunks,
            "chunk rows must be untouched by a noop reindex"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_touches_mtime_without_rechunk_when_content_hash_matches() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let corpus_dir = corpus_dir.path().canonicalize().unwrap();
        let file = corpus_dir.join("note.md");
        let corpus = md_corpus_at(&corpus_dir);
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);
        let file_ref = canonicalize_or_passthrough(&file)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        let content: &[u8] = b"# Note\n\nthe spice must flow\n";
        std::fs::write(&file, content).unwrap();
        // Backdate so the explicit `later` mtime below stays strictly in
        // the past: a mtime at/after indexing time is smudged by the
        // racy-clean guard (`smudge_racy_mtime`), which would fail the
        // exact-mtime assertion here.
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");
        let baseline = store.corpus_chunk_stats(&corpus.name).await.unwrap();

        // Remove the file from disk entirely: a hash-equal gate that only
        // needs a stored-mtime bump must not read the disk at all.
        std::fs::remove_file(&file).unwrap();

        let later = mtime + Duration::from_secs(2);
        let later_ms = i64::try_from(
            later
                .duration_since(UNIX_EPOCH)
                .expect("post-epoch")
                .as_millis(),
        )
        .expect("mtime fits i64");
        let stats =
            index_single_file_with_content(&store, &registry, &corpus, &file, content, later)
                .await
                .expect("hash-equal reindex with moved mtime must succeed without disk access");
        assert_eq!(
            stats.files_upserted, 0,
            "identical content must not be re-chunked or re-embedded"
        );
        assert_eq!(
            stats.files_touched, 1,
            "moved mtime takes the touch fast path"
        );
        let snap = store
            .get_file_snapshot(&corpus.name, &file_ref)
            .await
            .unwrap()
            .expect("snapshot must survive a touch");
        assert_eq!(
            snap.mtime_ms, later_ms,
            "stored mtime must advance to the new value"
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks,
            baseline.total_chunks,
            "chunk rows must be untouched by a mtime-only touch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_reindexes_and_stores_fresh_snapshot_when_content_hash_differs() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let corpus_dir = corpus_dir.path().canonicalize().unwrap();
        let file = corpus_dir.join("note.md");
        let corpus = md_corpus_at(&corpus_dir);
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);
        let file_ref = canonicalize_or_passthrough(&file)
            .as_path()
            .to_str()
            .unwrap()
            .to_string();

        std::fs::write(&file, "# Note\n\nthe spice must flow\n").unwrap();
        // Backdate for the same reason as the touch-path test above: keep
        // `later` strictly past so the racy-clean smudge stays out of the
        // exact-mtime assertion.
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");

        let new_content: &[u8] = b"# Note\n\na completely different harvest\n";
        std::fs::write(&file, new_content).unwrap();
        let later = mtime + Duration::from_secs(2);
        let later_ms = i64::try_from(
            later
                .duration_since(UNIX_EPOCH)
                .expect("post-epoch")
                .as_millis(),
        )
        .expect("mtime fits i64");

        let stats =
            index_single_file_with_content(&store, &registry, &corpus, &file, new_content, later)
                .await
                .expect("hash-unequal reindex must succeed");
        assert_eq!(
            stats.files_upserted, 1,
            "changed content must re-index in full"
        );
        let snap = store
            .get_file_snapshot(&corpus.name, &file_ref)
            .await
            .unwrap()
            .expect("snapshot must exist after re-index");
        assert_eq!(
            snap.content_hash,
            blake3_bytes(new_content),
            "stored snapshot must carry the fresh content hash"
        );
        assert_eq!(
            snap.mtime_ms, later_ms,
            "stored mtime must advance to the new value"
        );
    }

    /// The gate on the ambient-path wrapper (add-markdown / write handlers):
    /// re-running `index_single_file` over an untouched file must be a full
    /// noop — zero upserts (feeding the noop-reindex counter), zero touches,
    /// zero embeddings — not a silent re-chunk/re-embed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_single_file_rerun_on_untouched_file_is_a_full_noop() {
        use text_splitter::Characters;

        let store_dir = tempfile::tempdir().unwrap();
        let corpus_dir = tempfile::tempdir().unwrap();
        let file = corpus_dir.path().join("note.md");
        let corpus = md_corpus_at(corpus_dir.path());
        let store = open_off_store(store_dir.path()).await;
        let registry = HandlerRegistry::new(Characters, 1500);

        std::fs::write(&file, "# Note\n\nthe spice must flow\n").unwrap();
        index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("first index must succeed");
        let baseline = store.corpus_chunk_stats(&corpus.name).await.unwrap();

        let stats = index_single_file(&store, &registry, &corpus, &file)
            .await
            .expect("re-index of an untouched file must succeed");
        assert_eq!(
            stats,
            hallouminate_domain::indexer::ApplyStats::default(),
            "an untouched file must produce all-zero stats (noop reindex)"
        );
        assert_eq!(
            store
                .corpus_chunk_stats(&corpus.name)
                .await
                .unwrap()
                .total_chunks,
            baseline.total_chunks,
            "chunk rows must be untouched by a noop reindex"
        );
    }
}
