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
//! helpers live in `crate::domain::corpus::sandbox` and are shared with the
//! MCP tools transport, closing the maintenance liability of two divergent
//! forks (and the security asymmetry where the daemon was using
//! `tokio::fs::write` while MCP used `atomic_write_no_follow`).

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::adapters::lance::LanceStore;
use crate::app::cli::{CorpusReport, IndexReport};
use crate::app::config::{Config, ResolvedLayers, resolve_for_cwd};
use crate::domain::common::{CorpusConfig, FileRef, Mtime, canonicalize_or_passthrough};
#[cfg(test)]
use crate::domain::corpus::sandbox::FileEntry;
use crate::domain::corpus::sandbox::{
    WriteError, WriteErrorKind, atomic_write_no_follow, delete_no_follow,
    ensure_corpus_allows_file, first_corpus_root, list_corpus_files, pick_corpus, read_no_follow,
    resolve_read_root, safe_relative_path,
};
use crate::domain::corpus::{MarkdownChunker, blake3_file};
use crate::domain::ground::{
    Format, GroundOpts, RenderOpts, Warning, ground, ground_union, render, trim_snippets,
};
use crate::domain::indexer::{DEFAULT_BATCH_SIZE, FileSnapshot, apply, index_corpus, plan};
#[cfg(test)]
use crate::domain::repository::{RepoCorpusKind, repo_corpus_name};
use crate::domain::repository::{RepositoryConfig, default_wiki_for_cwd};

use super::ipc::{
    AddMarkdownRequest, AddMarkdownResult, CorpusEntry, DaemonRequest, DaemonRequestPayload,
    DaemonResponse, DeleteMarkdownRequest, DeleteMarkdownResult, GroundRequest, GroundResult,
    IndexRequest, ListFilesRequest, ListTreeRequest, ListTreeResult, PongResult,
    ReadMarkdownRequest, ReadMarkdownResult,
};
use super::state::DaemonState;

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
        DaemonRequestPayload::ListFiles(req) => handle_list_files(&effective, &req_cwd, req),
        DaemonRequestPayload::ListTree(req) => handle_list_tree(&effective, &req_cwd, req),
        DaemonRequestPayload::AddMarkdown(req) => handle_add_markdown(state, &effective, req).await,
        DaemonRequestPayload::ReadMarkdown(req) => {
            handle_read_markdown(&effective, &req_cwd, req).await
        }
        DaemonRequestPayload::DeleteMarkdown(req) => {
            handle_delete_markdown(state, &effective, req).await
        }
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
) -> Result<CorpusConfig, crate::domain::corpus::sandbox::SandboxError> {
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

fn handle_list_files(cfg: &Config, cwd: &Path, req: ListFilesRequest) -> DaemonResponse {
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
    ensure_paths_exist(&corpus);
    match list_corpus_files(&corpus) {
        Ok(entries) => DaemonResponse::ok(&entries),
        Err(e) => DaemonResponse::internal(e.to_string()),
    }
}

fn handle_list_tree(cfg: &Config, cwd: &Path, req: ListTreeRequest) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus =
        match pick_corpus_or_default(&corpora, &cfg.repositories, cwd, req.corpus.as_deref()) {
            Ok(c) => c,
            Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
        };
    ensure_paths_exist(&corpus);
    let root = match crate::domain::corpus::sandbox::build_corpus_tree(&corpus) {
        Ok(node) => node,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    DaemonResponse::ok(&ListTreeResult {
        corpus: corpus.name,
        root,
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
    let store = state.store();
    let opts = GroundOpts {
        top_files: req.top_files.unwrap_or(cfg.search.top_files_default),
        chunks_per_file: req
            .chunks_per_file
            .unwrap_or(cfg.search.chunks_per_file_default),
        limit: req.limit.unwrap_or(50),
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

    // Embeddings-OFF: skip the embedder entirely and let the search run the
    // lexical-only path. ON: borrow the shared embedder (lazy-loaded).
    let mut embedder = if state.embeddings_enabled() {
        match state.embedder().await {
            Ok(g) => Some(g),
            Err(e) => return DaemonResponse::internal(e.to_string()),
        }
    } else {
        None
    };
    // crossencoder is best-effort: if it's configured but failed to
    // load (e.g. model file vanished), log and ground without it
    // rather than refusing the request. Unconfigured paths return
    // Ok(None) and the rerank step is skipped entirely.
    let mut crossencoder = match state.crossencoder(cfg.search.crossencoder.as_deref()).await {
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
    let crossencoder_dyn: Option<&mut dyn crate::domain::search::Crossencoder> = crossencoder
        .as_mut()
        .map(|g| &mut **g as &mut dyn crate::domain::search::Crossencoder);
    let embedder_dyn: Option<&mut dyn crate::domain::embeddings::EmbedBatch> = embedder
        .as_mut()
        .map(|g| &mut **g as &mut dyn crate::domain::embeddings::EmbedBatch);

    let response = if let Some(corpus) = &single_corpus {
        ground(
            &req.query,
            &corpus.name,
            &corpus.paths,
            &store,
            embedder_dyn,
            crossencoder_dyn,
            opts,
        )
        .await
    } else {
        let targets: Vec<(String, Vec<String>)> = corpora
            .iter()
            .map(|c| (c.name.clone(), c.paths.clone()))
            .collect();
        ground_union(
            &req.query,
            &targets,
            &store,
            embedder_dyn,
            crossencoder_dyn,
            opts,
        )
        .await
    };
    let mut response = match response {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    drop(crossencoder);
    drop(embedder);

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

    let store = state.store();
    let chunker = state.make_chunker();

    let mut report = IndexReport::default();
    for corpus in selected {
        let guard = match state.acquire_mutation_guard(&corpus.name).await {
            Ok(g) => g,
            Err(msg) => return DaemonResponse::internal(msg),
        };
        ensure_paths_exist(&corpus);
        let missing = crate::domain::corpus::missing_roots(&corpus);
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
        let mut embedder = if state.embeddings_enabled() {
            match state.embedder().await {
                Ok(g) => Some(g),
                Err(e) => return DaemonResponse::internal(e.to_string()),
            }
        } else {
            None
        };
        let embedder_dyn: Option<&mut dyn crate::domain::embeddings::EmbedBatch> = embedder
            .as_mut()
            .map(|g| &mut **g as &mut dyn crate::domain::embeddings::EmbedBatch);
        let stats = match index_corpus(&corpus, &store, embedder_dyn, &chunker).await {
            Ok(s) => s,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        };
        drop(embedder);
        drop(guard);
        report.corpora.push(CorpusReport {
            name: corpus.name.clone(),
            files_upserted: stats.files_upserted,
            files_touched: stats.files_touched,
            files_deleted: stats.files_deleted,
            files_skipped_empty: stats.files_skipped_empty,
            chunks_inserted: stats.chunks_inserted,
            embeddings_inserted: stats.embeddings_inserted,
        });
    }
    DaemonResponse::ok(&report)
}

async fn handle_add_markdown(
    state: &DaemonState,
    cfg: &Config,
    req: AddMarkdownRequest,
) -> DaemonResponse {
    let corpora = match effective_corpora(cfg) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (corpus, root, relative) = match validate_wiki_path(&corpora, &req.corpus, &req.path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // Advisory-only lint of the verbatim content. Never blocks or rewrites the
    // write — the messages ride back in the response so the author can fix in
    // a follow-up instead of discovering breakage on a later read.
    let mut warnings = crate::domain::corpus::lint_markdown(&req.content);
    warnings.extend(crate::domain::corpus::lint_frontmatter(&req.content));
    warnings.extend(crate::domain::corpus::lint_claim_marks(&req.content));

    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(msg) => return DaemonResponse::internal(msg),
    };

    // Symlink-safe atomic write via the shared sandbox helper. Walks every
    // path component with `O_NOFOLLOW | O_DIRECTORY`, so a symlinked
    // intermediate dir bounces with `WriteErrorKind::Symlink` instead of
    // letting the writer punch through to whatever the symlink targets.
    let write_root = root.clone();
    let write_relative = relative.clone();
    let error_relative = relative.clone();
    // Read scalars and the empty-content flag before consuming `req.content`,
    // so the move into `into_bytes()` avoids cloning a potentially large body.
    let overwrite = req.overwrite;
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
    // LanceDB rows so searches stop returning the deleted body — the full
    // `index_single_file` path does this via `files_skipped_empty > 0 &&
    // had_snapshot`, but the short-circuit below bypasses that loop.
    let mut stats = if content_is_empty {
        let mut stats = crate::domain::indexer::ApplyStats {
            files_skipped_empty: 1,
            ..Default::default()
        };
        if overwrite {
            let file_ref = canonicalize_or_passthrough(&dest);
            if let Some(file_ref_str) = file_ref.as_path().to_str() {
                let store = state.store();
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
        let store = state.store();
        let chunker = state.make_chunker();
        let mut embedder = if state.embeddings_enabled() {
            match state.embedder().await {
                Ok(g) => Some(g),
                Err(e) => return DaemonResponse::internal(e.to_string()),
            }
        } else {
            None
        };
        let embedder_dyn: Option<&mut dyn crate::domain::embeddings::EmbedBatch> = embedder
            .as_mut()
            .map(|g| &mut **g as &mut dyn crate::domain::embeddings::EmbedBatch);
        match index_single_file(&store, embedder_dyn, &chunker, &corpus, &dest).await {
            Ok(s) => s,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        }
    };

    // Auto-rebuild wiki indexes from the corpus root down to the parent of
    // the just-written file. Failures here surface as `Internal` and the
    // mutation guard is dropped — partial index regen would leave the wiki
    // in a less-coherent state than aborting outright.
    if is_wiki_corpus(&corpus) {
        match rebuild_wiki_indexes(state, &corpus, &root, &relative).await {
            Ok(extra) => fold_apply_stats(&mut stats, &extra),
            Err(msg) => {
                drop(guard);
                return DaemonResponse::internal(msg);
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
        Err(msg) => return DaemonResponse::internal(msg),
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
    if let Err(e) = state.store().delete_file(&corpus.name, &file_ref_str).await {
        return DaemonResponse::internal(e.to_string());
    }

    // Auto-rebuild wiki indexes after the unlink so the parent index no
    // longer links to the deleted file. Same internal-error semantics as
    // the add_markdown path — partial regen would desync the wiki tree.
    if is_wiki_corpus(&corpus)
        && let Err(msg) = rebuild_wiki_indexes(state, &corpus, &root, &relative).await
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

pub(super) async fn index_single_file(
    store: &LanceStore,
    embedder: Option<&mut dyn crate::domain::embeddings::EmbedBatch>,
    chunker: &MarkdownChunker<tokenizers::Tokenizer>,
    corpus: &CorpusConfig,
    file: &Path,
) -> anyhow::Result<crate::domain::indexer::ApplyStats> {
    let meta = tokio::fs::metadata(file).await?;
    let modified = meta.modified()?;
    let dur = modified
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
    let had_snapshot = existing.is_some();
    let mut db: HashMap<FileRef, FileSnapshot> = HashMap::new();
    if let Some(snap) = existing {
        let hash_changed_without_mtime = if snap.mtime_ms == mtime_ms {
            blake3_file(file)? != snap.content_hash.as_str()
        } else {
            false
        };
        if !hash_changed_without_mtime {
            db.insert(file_ref.clone(), snap);
        }
    }
    let p = plan(vec![(file_ref, Mtime(mtime_ms))], db);
    let mut stats = apply(p, store, embedder, chunker, corpus, DEFAULT_BATCH_SIZE).await?;
    if stats.files_skipped_empty > 0 && had_snapshot {
        store.delete_file(&corpus.name, &file_ref_str).await?;
        stats.files_deleted += 1;
    }
    Ok(stats)
}

/// Best-effort `mkdir -p` on daemon-managed corpus roots so a fresh
/// repository wiki (which only exists logically until the first write)
/// doesn't blow up the first `list_files` / `index` call. Restricted to
/// `repo:*:wiki` corpora so a typo'd `[[corpus]] paths = ...` surfaces as a
/// clear scan error instead of silently creating an empty directory and
/// reporting success.
fn ensure_paths_exist(corpus: &CorpusConfig) {
    if !is_wiki_corpus(corpus) {
        return;
    }
    for raw in &corpus.paths {
        let path = crate::domain::common::expand_tilde(raw);
        let _ = std::fs::create_dir_all(&path);
    }
}

/// Sum `extra` into `into` so the daemon's IndexReport reflects both the
/// initial single-file write and the cascade of index.md rewrites that
/// followed it. Without this, the auto-built indexes would be silently
/// re-embedded but the report would still claim `files_upserted = 1`.
fn fold_apply_stats(
    into: &mut crate::domain::indexer::ApplyStats,
    extra: &crate::domain::indexer::ApplyStats,
) {
    into.files_upserted += extra.files_upserted;
    into.files_touched += extra.files_touched;
    into.files_deleted += extra.files_deleted;
    into.files_skipped_empty += extra.files_skipped_empty;
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
    corpus: &CorpusConfig,
    root: &Path,
    file_relative: &Path,
) -> Result<crate::domain::indexer::ApplyStats, String> {
    use crate::domain::corpus::index_md::{
        INDEX_FILENAME, ancestor_dirs, compose_index_md, is_index_md,
    };

    let written_is_index = is_index_md(file_relative);
    let mut totals = crate::domain::indexer::ApplyStats::default();
    let dirs = ancestor_dirs(root, file_relative);
    let store = state.store();
    let chunker = state.make_chunker();

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

        let existing = match std::fs::read_to_string(&index_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("read {}: {e}", index_path.display())),
        };

        let is_root = dir == root;
        let (new_content, outcome) = compose_index_md(dir, is_root, existing.as_deref())
            .map_err(|e| format!("compose index {}: {e}", dir.display()))?;
        match outcome {
            crate::domain::corpus::index_md::RewriteOutcome::NoMarkers
            | crate::domain::corpus::index_md::RewriteOutcome::Unchanged => continue,
            crate::domain::corpus::index_md::RewriteOutcome::Created
            | crate::domain::corpus::index_md::RewriteOutcome::Updated => {}
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

        // Refresh LanceDB rows for the just-rewritten index.md. Embeddings
        // are opt-in: when enabled, the embedder must load (a cold-cache or
        // network failure fails the mutation, same shape as the primary
        // add_markdown path); when disabled, reindex lexical-only.
        let mut embedder = if state.embeddings_enabled() {
            match state.embedder().await {
                Ok(g) => Some(g),
                Err(e) => return Err(format!("embedder: {e}")),
            }
        } else {
            None
        };
        let embedder_dyn: Option<&mut dyn crate::domain::embeddings::EmbedBatch> = embedder
            .as_mut()
            .map(|g| &mut **g as &mut dyn crate::domain::embeddings::EmbedBatch);
        let stats = index_single_file(&store, embedder_dyn, &chunker, corpus, &dest)
            .await
            .map_err(|e| format!("reindex {}: {e}", dest.display()))?;
        drop(embedder);
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
    let root = crate::domain::common::expand_tilde(raw);
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
    //! moved to `crate::domain::corpus::sandbox` and are tested there once,
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

    use crate::app::daemon::ErrorKind;

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
}
