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
use crate::domain::common::{CorpusConfig, FileRef, Mtime, canonicalize_or_passthrough};
#[cfg(test)]
use crate::domain::corpus::sandbox::FileEntry;
use crate::domain::corpus::sandbox::{
    WriteError, WriteErrorKind, atomic_write_no_follow, delete_no_follow, ensure_corpus_allows_file,
    first_corpus_root, list_corpus_files, pick_corpus, read_no_follow, safe_relative_path,
};
use crate::domain::corpus::{MarkdownChunker, blake3_file};
use crate::domain::embeddings::Embedder;
use crate::domain::ground::{Format, GroundOpts, RenderOpts, ground, render, trim_snippets};
use crate::domain::indexer::{DEFAULT_BATCH_SIZE, FileSnapshot, apply, index_corpus, plan};
#[cfg(test)]
use crate::domain::repository::{RepoCorpusKind, repo_corpus_name};

use super::ipc::{
    AddMarkdownRequest, AddMarkdownResult, CorpusEntry, DaemonRequest, DaemonRequestPayload,
    DaemonResponse, DeleteMarkdownRequest, DeleteMarkdownResult, GroundRequest, GroundResult,
    IndexRequest, ListFilesRequest, ReadMarkdownRequest, ReadMarkdownResult,
};
use super::state::DaemonState;

pub async fn dispatch(state: &DaemonState, req: DaemonRequest) -> DaemonResponse {
    match req.payload {
        DaemonRequestPayload::Ping => DaemonResponse::ok(&"pong"),
        DaemonRequestPayload::Ground(req) => handle_ground(state, req).await,
        DaemonRequestPayload::Index(req) => handle_index(state, req).await,
        DaemonRequestPayload::ListCorpora => handle_list_corpora(state),
        DaemonRequestPayload::ListFiles(req) => handle_list_files(state, req),
        DaemonRequestPayload::AddMarkdown(req) => handle_add_markdown(state, req).await,
        DaemonRequestPayload::ReadMarkdown(req) => handle_read_markdown(state, req).await,
        DaemonRequestPayload::DeleteMarkdown(req) => handle_delete_markdown(state, req).await,
    }
}

fn effective_corpora(state: &DaemonState) -> Result<Vec<CorpusConfig>, DaemonResponse> {
    state
        .cfg()
        .effective_corpora()
        .map_err(|e| DaemonResponse::internal(e.to_string()))
}

fn handle_list_corpora(state: &DaemonState) -> DaemonResponse {
    let corpora = match effective_corpora(state) {
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

fn handle_list_files(state: &DaemonState, req: ListFilesRequest) -> DaemonResponse {
    let corpora = match effective_corpora(state) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus = match pick_corpus(&corpora, req.corpus.as_deref()) {
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

async fn handle_ground(state: &DaemonState, req: GroundRequest) -> DaemonResponse {
    let cfg = state.cfg().clone();
    let corpora = match effective_corpora(state) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus = match pick_corpus(&corpora, req.corpus.as_deref()) {
        Ok(c) => c,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let store = state.store();
    let opts = GroundOpts {
        top_files: req.top_files.unwrap_or(cfg.search.top_files_default),
        chunks_per_file: req
            .chunks_per_file
            .unwrap_or(cfg.search.chunks_per_file_default),
        limit: req.limit.unwrap_or(50),
    };
    let mut embedder = match state.embedder().await {
        Ok(g) => g,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    let response = match ground(&req.query, &corpus.name, &store, &mut *embedder, opts).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::internal(e.to_string()),
    };
    drop(embedder);
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

async fn handle_index(state: &DaemonState, req: IndexRequest) -> DaemonResponse {
    // Reject ad-hoc paths_from unconditionally: the daemon protocol does not
    // accept it yet, and silently ignoring the field when a corpus is also
    // selected would let MCP clients believe the path list landed.
    if req.paths_from.is_some() {
        return DaemonResponse::invalid_params("paths_from is not supported via the daemon yet");
    }
    let corpora = match effective_corpora(state) {
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
        let mut embedder = match state.embedder().await {
            Ok(g) => g,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        };
        let stats = match index_corpus(&corpus, &store, &mut *embedder, &chunker).await {
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

async fn handle_add_markdown(state: &DaemonState, req: AddMarkdownRequest) -> DaemonResponse {
    let corpora = match effective_corpora(state) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus = match pick_corpus(&corpora, Some(&req.corpus)) {
        Ok(c) => c,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let root = match first_corpus_root(&corpus) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let relative = match safe_relative_path(&req.path) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let dest = root.join(&relative);
    if let Err(e) = ensure_corpus_allows_file(&corpus, &dest) {
        return DaemonResponse::invalid_params(e.into_inner());
    }
    if let Err(msg) = ensure_wiki_root_safe(&corpus) {
        return DaemonResponse::invalid_params(msg);
    }

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
    let content_bytes = req.content.clone().into_bytes();
    let overwrite = req.overwrite;
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
    let stats = if req.content.trim().is_empty() {
        let mut stats = crate::domain::indexer::ApplyStats {
            files_skipped_empty: 1,
            ..Default::default()
        };
        if req.overwrite {
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
        let mut embedder = match state.embedder().await {
            Ok(g) => g,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        };
        match index_single_file(&store, &mut embedder, &chunker, &corpus, &dest).await {
            Ok(s) => s,
            Err(e) => return DaemonResponse::internal(e.to_string()),
        }
    };
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
    };
    DaemonResponse::ok(&AddMarkdownResult {
        corpus: corpus.name,
        path: relative.to_string_lossy().into_owned(),
        absolute_path: dest.to_string_lossy().into_owned(),
        indexed: report,
    })
}

async fn handle_read_markdown(state: &DaemonState, req: ReadMarkdownRequest) -> DaemonResponse {
    let corpora = match effective_corpora(state) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus = match pick_corpus(&corpora, Some(&req.corpus)) {
        Ok(c) => c,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let root = match first_corpus_root(&corpus) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let relative = match safe_relative_path(&req.path) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let dest = root.join(&relative);
    if let Err(e) = ensure_corpus_allows_file(&corpus, &dest) {
        return DaemonResponse::invalid_params(e.into_inner());
    }
    if let Err(msg) = ensure_wiki_root_safe(&corpus) {
        return DaemonResponse::invalid_params(msg);
    }

    // Symlink-safe read via the shared sandbox helper: walks every parent
    // component with `O_NOFOLLOW` and opens the leaf `O_RDONLY | O_NOFOLLOW`
    // so a symlinked intermediate dir or leaf bounces with the same
    // `WriteErrorKind::Symlink` shape `add_markdown` uses for writes.
    let read_root = root.clone();
    let read_relative = relative.clone();
    let error_relative = relative.clone();
    let read = tokio::task::spawn_blocking(move || read_no_follow(&read_root, &read_relative)).await;
    let bytes = match read {
        Ok(Ok(b)) => b,
        Ok(Err(WriteError { kind, source })) => {
            return map_read_error(kind, source, &error_relative);
        }
        Err(join_err) => {
            return DaemonResponse::internal(format!("read task panicked: {join_err}"));
        }
    };
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

async fn handle_delete_markdown(state: &DaemonState, req: DeleteMarkdownRequest) -> DaemonResponse {
    let corpora = match effective_corpora(state) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let corpus = match pick_corpus(&corpora, Some(&req.corpus)) {
        Ok(c) => c,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let root = match first_corpus_root(&corpus) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let relative = match safe_relative_path(&req.path) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::invalid_params(e.into_inner()),
    };
    let dest = root.join(&relative);
    if let Err(e) = ensure_corpus_allows_file(&corpus, &dest) {
        return DaemonResponse::invalid_params(e.into_inner());
    }
    if let Err(msg) = ensure_wiki_root_safe(&corpus) {
        return DaemonResponse::invalid_params(msg);
    }

    let guard = match state.acquire_mutation_guard(&corpus.name).await {
        Ok(g) => g,
        Err(msg) => return DaemonResponse::internal(msg),
    };

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
    let deleted = tokio::task::spawn_blocking(move || {
        delete_no_follow(&delete_root, &delete_relative)
    })
    .await;
    match deleted {
        Ok(Ok(())) => {}
        Ok(Err(WriteError { kind, source })) => {
            return map_delete_error(kind, source, &error_relative);
        }
        Err(join_err) => {
            return DaemonResponse::internal(format!("unlink task panicked: {join_err}"));
        }
    }
    if let Err(e) = state.store().delete_file(&corpus.name, &file_ref_str).await {
        return DaemonResponse::internal(e.to_string());
    }
    drop(guard);

    DaemonResponse::ok(&DeleteMarkdownResult {
        corpus: corpus.name,
        path: relative.to_string_lossy().into_owned(),
        absolute_path: dest.to_string_lossy().into_owned(),
        file_ref: file_ref_str,
    })
}

async fn index_single_file(
    store: &LanceStore,
    embedder: &mut Embedder,
    chunker: &MarkdownChunker<tokenizers::Tokenizer>,
    corpus: &CorpusConfig,
    file: &Path,
) -> anyhow::Result<crate::domain::indexer::ApplyStats> {
    let meta = tokio::fs::metadata(file).await?;
    let modified = meta.modified()?;
    let mtime_ms = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("pre-epoch mtime on {}", file.display()))?
        .as_millis() as i64;
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

/// True when `corpus.name` is a derived `repo:{name}:wiki` corpus produced
/// by `effective_corpora()`. The daemon owns those directories (creates
/// them on demand, enforces no-symlink safety), so a few helpers behave
/// differently for them than for user-declared `[[corpus]]` entries.
fn is_wiki_corpus(corpus: &CorpusConfig) -> bool {
    corpus.name.starts_with("repo:") && corpus.name.ends_with(":wiki")
}

/// Refuse to operate on a wiki corpus whose `.hallouminate` parent or
/// `wiki` leaf is a symlink. The repository's `path` is user-configured (so
/// trusted), but `.hallouminate` and `wiki` are daemon-managed names that a
/// malicious repository payload could swap for symlinks before the daemon
/// runs the no-follow component walk inside the sandbox. Best-effort
/// pre-flight — TOCTOU-vulnerable between this check and the subsequent
/// open, but the daemon serializes wiki mutations behind the per-corpus
/// mutex so a swap during the narrow window would race the daemon's own
/// consistency model.
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

/// Canonical `Ping` reply payload — dispatch() encodes `&"pong"` directly,
/// this helper lets tests match against the same literal without
/// hand-rolling the JSON.
#[cfg(test)]
fn pong_value() -> Value {
    Value::String("pong".into())
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
    fn pong_value_returns_pong_string_literal() {
        assert_eq!(pong_value(), Value::String("pong".to_string()));
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
}
