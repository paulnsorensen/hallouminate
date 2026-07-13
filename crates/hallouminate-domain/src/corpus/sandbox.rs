//! Corpus-boundary helpers shared by the daemon dispatcher and the MCP
//! tools transport. Forks of these helpers used to live in both
//! `src/app/daemon/dispatch.rs` and `src/adapters/mcp/tools.rs`; sharing
//! them here closes the maintenance liability (and an error-message drift
//! between the two copies) called out in the spec's age review.
//!
//! The functions break down into three groups:
//!
//! 1. **Corpus selection / validation** — `pick_corpus`, `first_corpus_root`,
//!    `safe_relative_path`, `ensure_corpus_allows_file`, `build_globset`.
//!    Pure logic; no I/O. Returns `Result<_, SandboxError>` so callers can
//!    map straight to their preferred error type (anyhow for MCP, `String`
//!    for daemon JSON envelopes).
//!
//! 2. **`atomic_write_no_follow`** — the symlink-safe writer the MCP path
//!    already used. The daemon's previous `tokio::fs::create_dir_all` +
//!    `tokio::fs::write` path only checked the leaf for symlinks and missed
//!    symlinked intermediate directory components; this shared implementation
//!    walks every path component through a `cap-std` `Dir` capability and
//!    pre-checks each with `symlink_metadata`, so a symlinked dir-component
//!    bounces with `WriteError { kind: Symlink, .. }`.
//!
//! 3. **`list_corpus_files`** — scan-and-format helper both transports
//!    expose via `list_files` and the daemon's add-markdown ack.
//!
//! 4. **`read_no_follow` / `delete_no_follow`** — symlink-safe read + unlink
//!    counterparts to `atomic_write_no_follow`. The pre-refactor daemon used
//!    `tokio::fs::read` / `tokio::fs::remove_file` after a leaf-only
//!    `symlink_metadata` check; that left intermediate-symlink escapes wide
//!    open. These helpers walk every parent component through the same
//!    cap-std `Dir` capability and pre-check each component for symlinks,
//!    so a symlinked dir component bounces with `WriteError { kind: Symlink,
//!    .. }` instead of leaking files outside the corpus root.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::common::{CorpusConfig, expand_tilde};
use crate::corpus::scan;

/// Caller-supplied input failure when validating a corpus / path pair.
///
/// Carried as a plain `String` because the daemon's JSON envelopes and the
/// MCP transport's `anyhow::Error` both want the message as text. Callers
/// that want richer typing (e.g. `anyhow`) wrap on the way out.
#[derive(Debug)]
pub struct SandboxError(String);

impl SandboxError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SandboxError {}

impl From<SandboxError> for String {
    fn from(err: SandboxError) -> Self {
        err.0
    }
}

/// Pick a corpus by name from `corpora`. If `requested` is `None`, returns
/// the only configured corpus or errors with a message naming the right
/// config keys (`[[corpus]]` AND `[[repository]]` — the daemon's wiki tenant
/// derives from the latter, so omitting it from the hint sends users to the
/// wrong place).
pub fn pick_corpus(
    corpora: &[CorpusConfig],
    requested: Option<&str>,
) -> Result<CorpusConfig, SandboxError> {
    if let Some(name) = requested {
        return corpora
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .ok_or_else(|| SandboxError::new(format!("corpus {name:?} not found in config")));
    }
    match corpora {
        [] => Err(SandboxError::new(
            "no corpora configured; add [[corpus]] or [[repository]] to config",
        )),
        [only] => Ok(only.clone()),
        _ => Err(SandboxError::new(
            "corpus required when multiple corpora configured; pass corpus",
        )),
    }
}

/// Tilde-expanded first configured root path for `corpus`.
pub fn first_corpus_root(corpus: &CorpusConfig) -> Result<PathBuf, SandboxError> {
    let raw = corpus
        .paths
        .first()
        .ok_or_else(|| SandboxError::new(format!("corpus {:?} has no paths", corpus.name)))?;
    Ok(expand_tilde(raw))
}

/// Reject anything that isn't a non-empty relative path made of normal file
/// components. Closes the path-traversal boundary for add/read/delete
/// markdown — `..`, absolute paths, and `.`-segments would all reach outside
/// the corpus root.
pub fn safe_relative_path(raw: &str) -> Result<PathBuf, SandboxError> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(SandboxError::new("path must be a non-empty relative path"));
    }
    // NUL bytes in path components can't be passed to syscalls — reject at
    // the boundary so the caller gets the historical "path contains a NUL
    // byte" message and `SandboxError` shape rather than letting the failure
    // surface as `EINVAL` deeper in the sandbox writer. (Defense in depth:
    // `classify_*_io_error` also maps `EINVAL` to `InvalidPath`, but the
    // boundary check gives a clearer error.)
    if raw.contains('\0') {
        return Err(SandboxError::new("path contains a NUL byte"));
    }
    if raw.ends_with('/') || raw == "." || raw.ends_with("/.") {
        return Err(SandboxError::new("path must name a file"));
    }
    if raw.starts_with("./") || raw.contains("/./") {
        return Err(SandboxError::new(
            "path must contain only normal file components",
        ));
    }
    if !matches!(path.components().next_back(), Some(Component::Normal(_))) {
        return Err(SandboxError::new("path must name a file"));
    }
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(SandboxError::new(
            "path must contain only normal file components",
        ));
    }
    Ok(path.to_path_buf())
}

/// Confirm `path` matches the corpus's include globs and isn't excluded.
pub fn ensure_corpus_allows_file(corpus: &CorpusConfig, path: &Path) -> Result<(), SandboxError> {
    let include = build_globset(&corpus.globs).map_err(|e| SandboxError::new(e.to_string()))?;
    if matches!(include.as_ref(), Some(inc) if !inc.is_match(path)) {
        return Err(SandboxError::new("path is not included by corpus globs"));
    }
    let exclude = build_globset(&corpus.exclude).map_err(|e| SandboxError::new(e.to_string()))?;
    if matches!(exclude.as_ref(), Some(ex) if ex.is_match(path)) {
        return Err(SandboxError::new("path is excluded by corpus rules"));
    }
    Ok(())
}

/// Compile a set of glob patterns. Returns `None` for an empty list so the
/// caller can short-circuit "no rules" before allocating.
pub fn build_globset(patterns: &[String]) -> anyhow::Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)?;
        builder.add(glob);
    }
    Ok(Some(builder.build()?))
}

/// One entry returned by [`list_corpus_files`]. Serialized as-is into the
/// daemon and MCP response payloads, so the field shape is part of both
/// transports' public contract. Deserialize lets daemon clients (CLI,
/// MCP) decode the daemon's `ListFiles` response back into typed entries.
///
/// # Wire contract (v1 — stable)
///
/// The field names `path` and `absolute_path` and their `String` types are
/// part of the v1 daemon/MCP wire contract — they land verbatim in
/// `structuredContent` and the daemon's `ListFiles` JSON. Renaming or
/// retyping a field is a breaking change for every deployed client; do not
/// touch them without a wire-version bump. (`dispatch.rs` asserts the field
/// names so an accidental rename fails the build.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub absolute_path: String,
}

/// Enumerate every file visible in `corpus`, returning paths relative to the
/// matching configured root when possible.
pub fn list_corpus_files(corpus: &CorpusConfig) -> anyhow::Result<Vec<FileEntry>> {
    let roots: Vec<PathBuf> = corpus
        .paths
        .iter()
        .map(|p| {
            let expanded = expand_tilde(p);
            canonicalize_or_log(expanded)
        })
        .collect();
    let mut entries: Vec<FileEntry> = scan(corpus)
        .map_err(anyhow::Error::from)?
        .into_iter()
        .map(|(file, _)| {
            let absolute = file.into_path_buf();
            let relative = roots
                .iter()
                .find_map(|r| absolute.strip_prefix(r).ok())
                .unwrap_or(absolute.as_path());
            FileEntry {
                path: relative.to_string_lossy().into_owned(),
                absolute_path: absolute.to_string_lossy().into_owned(),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Canonicalize `expanded`, falling back to the non-canonical path when the
/// root does not yet exist on disk (a freshly-configured wiki corpus whose
/// directory hasn't been created). Logs at `debug` on fallback because a
/// non-canonical `absolute_path` can diverge from the canonical `file_ref`
/// LanceDB stores, so the divergence is traceable rather than silent.
fn canonicalize_or_log(expanded: PathBuf) -> PathBuf {
    match std::fs::canonicalize(&expanded) {
        Ok(canonical) => canonical,
        Err(e) => {
            tracing::debug!(
                target: "hallouminate::corpus",
                path = %expanded.display(),
                error = %e,
                "canonicalize failed; using non-canonical path (corpus root may not exist yet)"
            );
            expanded
        }
    }
}

/// One node in the directory tree returned by `build_corpus_tree`. The root
/// node has `path == ""` and represents the corpus' first configured root.
/// `files` carries direct-child files visible in the corpus; `subdirs`
/// carries immediate child directories that themselves contain markdown.
/// Empty directories (no markdown anywhere beneath) are pruned so the tree
/// mirrors what `list_corpus_files` would surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeNode {
    pub path: String,
    pub absolute_path: String,
    pub files: Vec<FileEntry>,
    pub subdirs: Vec<TreeNode>,
}

/// Build a directory tree for `corpus` keyed on its first configured root.
///
/// Multi-root corpora collapse to the first root for the tree — wiki writes
/// already target it exclusively, and a cross-root tree has no canonical
/// path representation. Honors include/exclude globs by reusing the same
/// `scan` pass that `list_corpus_files` uses, then groups results by
/// directory so a navigator (LLM or human) can browse without re-parsing
/// the flat list.
pub fn build_corpus_tree(corpus: &CorpusConfig) -> anyhow::Result<TreeNode> {
    let raw_root = corpus
        .paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("corpus {:?} has no paths", corpus.name))?;
    let expanded = expand_tilde(raw_root);
    let root_abs = canonicalize_or_log(expanded);
    let files = list_corpus_files(corpus)?;
    let mut node = TreeNode {
        path: String::new(),
        absolute_path: root_abs.to_string_lossy().into_owned(),
        files: Vec::new(),
        subdirs: Vec::new(),
    };
    for entry in files {
        // Skip entries that don't actually live under the first root.
        // `list_corpus_files` strips `path` against whichever root matched,
        // so a file under `paths[1..]` still carries a relative `path` and
        // the old `absolute_path == path` test let it slip through. Strip
        // against `root_abs` explicitly so a non-first-root entry yields
        // `Err` and gets dropped.
        let Ok(rel_path) = Path::new(&entry.absolute_path).strip_prefix(&root_abs) else {
            continue;
        };
        let rel_path = rel_path.to_path_buf();
        insert_into_tree(&mut node, &rel_path, entry, &root_abs);
    }
    sort_tree(&mut node);
    Ok(node)
}

fn insert_into_tree(node: &mut TreeNode, rel: &Path, entry: FileEntry, root_abs: &Path) {
    let components: Vec<&OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if components.is_empty() {
        return;
    }
    if components.len() == 1 {
        node.files.push(entry);
        return;
    }
    let head = components[0];
    let head_str = head.to_string_lossy().into_owned();
    let child = match node.subdirs.iter().position(|n| {
        Path::new(&n.path)
            .components()
            .next_back()
            .map(|c| matches!(c, Component::Normal(s) if s == head))
            .unwrap_or(false)
    }) {
        Some(idx) => &mut node.subdirs[idx],
        None => {
            let child_rel = if node.path.is_empty() {
                head_str.clone()
            } else {
                format!("{}/{}", node.path, head_str)
            };
            let child_abs = root_abs.join(&child_rel);
            node.subdirs.push(TreeNode {
                path: child_rel,
                absolute_path: child_abs.to_string_lossy().into_owned(),
                files: Vec::new(),
                subdirs: Vec::new(),
            });
            node.subdirs.last_mut().expect("just pushed")
        }
    };
    let tail: PathBuf = components[1..].iter().collect();
    insert_into_tree(child, &tail, entry, root_abs);
}

fn sort_tree(node: &mut TreeNode) {
    node.files.sort_by(|a, b| a.path.cmp(&b.path));
    node.subdirs.sort_by(|a, b| a.path.cmp(&b.path));
    for child in &mut node.subdirs {
        sort_tree(child);
    }
}

// ── atomic_write_no_follow ──────────────────────────────────────────────
//
// Atomic writer that walks every path component through a `cap-std` `Dir`
// capability and refuses to traverse a symlinked component. Previously a
// fork of this lived only in the MCP transport while the daemon used
// `tokio::fs::write` + a leaf `symlink_metadata` check — which missed
// symlinked intermediate directories. Sharing the implementation closes that
// asymmetry and gives both transports the same write contract.
//
// `cap-std`'s default open APIs follow symlinks during path resolution as
// long as the target stays within the capability (an outside-pointing
// symlink errors with `EscapeAttempt`). To preserve the historical
// "refuse ALL symlinks in the path" contract — and the
// `WriteErrorKind::Symlink` discrimination both transports already rely on —
// every parent component and the leaf are pre-checked with
// `Dir::symlink_metadata` before the corresponding `open_dir` / `open` /
// `remove_file` / `rename` call. The capability still guarantees the
// post-write resolved path stays under the root.

/// Why a sandbox write, read, or delete failed.
///
/// Callers branch on the kind to map the failure onto their transport's
/// error vocabulary (the daemon's JSON-RPC codes, the MCP transport's
/// `ErrorData`). `Symlink` and `InvalidPath` are normalized across platforms
/// by the `classify_*_errno` helpers so a caller sees one stable kind
/// regardless of whether Linux or macOS raised the underlying errno.
#[derive(Debug)]
pub enum WriteErrorKind {
    /// File exists and `overwrite=false`.
    Exists,
    /// A path component was a symlink.
    Symlink,
    /// The relative path names a non-directory parent or non-file target.
    InvalidPath,
    /// Any other I/O failure.
    Io,
}

/// A failed sandbox filesystem operation: its classified [`WriteErrorKind`]
/// plus the originating [`std::io::Error`] for diagnostics. Returned by
/// [`atomic_write_no_follow`], [`read_no_follow`], and [`delete_no_follow`].
#[derive(Debug)]
pub struct WriteError {
    pub kind: WriteErrorKind,
    pub source: std::io::Error,
}

impl WriteError {
    pub fn new(kind: WriteErrorKind, source: std::io::Error) -> Self {
        Self { kind, source }
    }
}

/// Atomically write `content` to `<root>/<relative>`, creating intermediate
/// directories under `root` and walking every component with `O_NOFOLLOW` so
/// no symlinked path component can redirect the write outside `root`. Returns
/// the absolute path written.
///
/// # Errors
///
/// Returns [`WriteError`] with:
/// - [`WriteErrorKind::Exists`] when the target exists and `overwrite` is false.
/// - [`WriteErrorKind::Symlink`] when any path component (intermediate dir or
///   the leaf) is a symlink.
/// - [`WriteErrorKind::InvalidPath`] when `relative` is not a non-empty path of
///   normal file components, names a non-file target, or carries a NUL byte.
/// - [`WriteErrorKind::Io`] for any other underlying I/O failure.
pub fn atomic_write_no_follow(
    root: &Path,
    relative: &Path,
    content: &[u8],
    overwrite: bool,
) -> Result<PathBuf, WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;
    if overwrite {
        atomic_replace(&parent, file_name.as_os_str(), content)?;
    } else {
        write_new_file(&parent, file_name.as_os_str(), content)?;
    }
    Ok(root.join(relative))
}

/// Read the contents of `<root>/<relative>` after walking every parent
/// component through a `cap-std` `Dir` capability and pre-checking each for
/// symlinks. The leaf is opened only after a `symlink_metadata` check, so a
/// symlinked leaf bounces with `WriteErrorKind::Symlink`. Mirrors
/// `atomic_write_no_follow`'s component-walk so a single change to the
/// no-follow logic stays in one place.
///
/// # Errors
///
/// Returns [`WriteError`] with:
/// - [`WriteErrorKind::Symlink`] when any path component or the leaf is a symlink.
/// - [`WriteErrorKind::InvalidPath`] when `relative` is not a non-empty path of
///   normal file components, a parent component is not a directory, or the path
///   carries a NUL byte.
/// - [`WriteErrorKind::Io`] when the file is absent or any other read fails.
pub fn read_no_follow(root: &Path, relative: &Path) -> Result<Vec<u8>, WriteError> {
    read_no_follow_with_mtime(root, relative).map(|(bytes, _)| bytes)
}

/// Read the contents of `<root>/<relative>` and its mtime from one no-follow
/// resolution, collapsing the watcher's prior check-then-ambient-read into a
/// single cap-std handle: bytes and mtime come from the same opened file, so
/// a symlink swapped in after a separate `fs::metadata` call can no longer
/// desync the two reads. The residual `symlink_metadata` + `open` split
/// inside the component walk (see [`open_or_create_child_dir`]) is bounded to
/// the corpus root by cap-std, not atomic at the syscall level.
///
/// # Errors
///
/// Same as [`read_no_follow`].
pub fn read_no_follow_with_mtime(
    root: &Path,
    relative: &Path,
) -> Result<(Vec<u8>, SystemTime), WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;
    reject_symlink_leaf(&parent, file_name.as_os_str())?;
    let cap_file = parent
        .open(file_name.as_os_str())
        .map_err(classify_path_io_error)?;
    let mut file = cap_file.into_std();
    let mtime = file
        .metadata()
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?
        .modified()
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    Ok((buf, mtime))
}

/// Find which configured root of `corpus` holds `relative`, walking the roots
/// in `corpus.paths` order. Returns the first root under which `relative`
/// resolves to an existing regular file reached without traversing a symlinked
/// component. Single-root corpora have exactly one candidate, so the behaviour
/// is unchanged from resolving against [`first_corpus_root`].
///
/// This is the read-side counterpart to the mutation handlers'
/// single-root requirement: `ground` / `list_files` already walk every root,
/// so a file under `paths[1..]` is searchable; without this, `read_markdown`
/// would resolve only `paths[0]` and a searchable file would be unreadable.
///
/// The probe is **non-mutating** — unlike [`read_no_follow`], it never creates
/// intermediate directories, so probing a root that does not contain the file
/// leaves that root untouched.
///
/// # Errors
///
/// Returns [`WriteError`] with:
/// - [`WriteErrorKind::Symlink`] when a candidate root resolves `relative`
///   through a symlinked path component or leaf. This is a hard stop — it is
///   surfaced immediately rather than masked by trying the next root.
/// - [`WriteErrorKind::InvalidPath`] when `relative` is not a non-empty path of
///   normal file components.
/// - [`WriteErrorKind::Io`] carrying a `NotFound` source when no configured
///   root contains the file, matching the shape [`read_no_follow`] returns for
///   a missing file (so callers map both to the same "does not exist" error).
pub fn resolve_read_root(corpus: &CorpusConfig, relative: &Path) -> Result<PathBuf, WriteError> {
    let names = normal_components(relative)?;
    let (dirs, file) = names.split_at(names.len() - 1);
    let file_name = file
        .first()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;

    let mut last_err: Option<WriteError> = None;
    for raw in &corpus.paths {
        let root = expand_tilde(raw);
        match probe_leaf_exists(&root, dirs, file_name.as_os_str()) {
            Ok(true) => return Ok(root),
            Ok(false) => last_err = Some(not_found_error()),
            // A symlink escape is a security stop, not a "try the next root"
            // miss — surface it so the caller refuses the read.
            Err(e) if matches!(e.kind, WriteErrorKind::Symlink) => return Err(e),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(not_found_error))
}

/// Symlink-safe, NON-mutating existence probe used by [`resolve_read_root`].
/// Walks every parent component through a `cap-std` `Dir` capability without
/// creating anything, then checks the leaf is an existing regular file reached
/// without traversing a symlink.
///
/// Returns `Ok(true)` when the leaf is a present regular file, `Ok(false)`
/// when a component or the leaf is simply absent (or a parent component is a
/// non-directory, so the file cannot live here) — both let the caller try the
/// next root. Returns `Err(WriteErrorKind::Symlink)` when a component or the
/// leaf is a symlink.
fn probe_leaf_exists(
    root: &Path,
    dirs: &[OsString],
    file_name: &OsStr,
) -> Result<bool, WriteError> {
    let mut current = match Dir::open_ambient_dir(root, ambient_authority()) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(WriteError::new(WriteErrorKind::Io, e)),
    };
    for dir in dirs {
        match current.symlink_metadata(dir.as_os_str()) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()));
                }
                if !meta.is_dir() {
                    // A non-directory parent component means the file cannot
                    // resolve under this root; let the caller try the next.
                    return Ok(false);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(classify_path_io_error(e)),
        }
        current = match current.open_dir(dir.as_os_str()) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(classify_path_io_error(e)),
        };
    }
    match current.symlink_metadata(file_name) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()))
            } else {
                Ok(ft.is_file())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(classify_path_io_error(e)),
    }
}

/// A `WriteError` carrying a `NotFound` source — the shape `read_no_follow`
/// produces for a missing leaf, so [`resolve_read_root`] callers map a
/// no-root-contains-it miss to the same "does not exist" response.
fn not_found_error() -> WriteError {
    WriteError::new(
        WriteErrorKind::Io,
        std::io::Error::from(std::io::ErrorKind::NotFound),
    )
}

/// Unlink `<root>/<relative>` after walking every parent component through a
/// `cap-std` `Dir` capability. Refuses to delete when the leaf is a symlink
/// (the pre-fix daemon path silently followed parent symlinks via
/// `tokio::fs::remove_file`, letting a corpus-controlled directory symlink
/// redirect the unlink outside the corpus root).
///
/// # Errors
///
/// Returns [`WriteError`] with:
/// - [`WriteErrorKind::Symlink`] when any path component or the leaf is a symlink.
/// - [`WriteErrorKind::InvalidPath`] when `relative` is not a non-empty path of
///   normal file components, or the leaf is not a regular file.
/// - [`WriteErrorKind::Io`] when the file is absent or the unlink fails.
pub fn delete_no_follow(root: &Path, relative: &Path) -> Result<(), WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;

    // Pre-flight: `symlink_metadata` so the symlink-leaf case surfaces as
    // `Symlink` (and not-a-regular-file surfaces as `InvalidPath`) before we
    // call `remove_file`. cap-std's `Dir::remove_file` doesn't follow a
    // symlink either, but the daemon's "deleted X" success message would
    // otherwise be misleading if the leaf were a symlink.
    let meta = parent
        .symlink_metadata(file_name.as_os_str())
        .map_err(classify_create_io_error)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()));
    }
    if !ft.is_file() {
        return Err(invalid_path_error("target is not a regular file"));
    }

    parent
        .remove_file(file_name.as_os_str())
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))
}

fn normal_components(path: &Path) -> Result<Vec<OsString>, WriteError> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            _ => {
                return Err(invalid_path_error(
                    "path must contain only normal file components",
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(invalid_path_error("path must name a file"));
    }
    Ok(out)
}

fn open_parent_dir(root: &Path, dirs: &[OsString]) -> Result<Dir, WriteError> {
    let mut current = open_root_dir(root)?;
    for dir in dirs {
        current = open_or_create_child_dir(&current, dir.as_os_str())?;
    }
    Ok(current)
}

fn deepest_existing_ancestor(root: &Path) -> Result<(PathBuf, Vec<OsString>), WriteError> {
    let mut missing = Vec::new();
    let mut current = root.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()));
            }
            Ok(meta) if meta.is_dir() => {
                missing.reverse();
                return Ok((current, missing));
            }
            Ok(_) => {
                return Err(invalid_path_error(
                    "corpus root path component is not a directory",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(invalid_path_error(
                        "corpus root has no existing ancestor directory",
                    ));
                };
                missing.push(name.to_os_string());
                if !current.pop() {
                    return Err(invalid_path_error(
                        "corpus root has no existing ancestor directory",
                    ));
                }
            }
            Err(e) => return Err(WriteError::new(WriteErrorKind::Io, e)),
        }
    }
}

fn open_root_dir(root: &Path) -> Result<Dir, WriteError> {
    // Never bootstrap the write capability by ambient-opening `root` itself:
    // a symlink swapped into place after the caller's preflight check (e.g.
    // `ensure_wiki_root_safe`) would be silently followed by
    // `open_ambient_dir`, redirecting every subsequent no-follow component
    // open onto whatever the symlink targets. Instead we ambient-open the
    // deepest already-existing ancestor of `root` and open/create every
    // remaining component -- including `root`'s own leaf -- through the same
    // no-follow `open_or_create_child_dir` used for every interior path
    // segment, so a raced root symlink bounces with `WriteErrorKind::Symlink`
    // exactly like a raced intermediate directory does.
    let (anchor, remaining) = deepest_existing_ancestor(root)?;
    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    for name in &remaining {
        current = open_or_create_child_dir(&current, name.as_os_str())?;
    }
    Ok(current)
}

fn open_or_create_child_dir(parent: &Dir, name: &OsStr) -> Result<Dir, WriteError> {
    // Try metadata first so we can refuse symlinked intermediate components
    // with the historical `WriteErrorKind::Symlink` discrimination. cap-std's
    // own `open_dir` would silently follow a symlink that resolves inside the
    // capability, which is a contract relaxation we don't want for the
    // corpus sandbox.
    //
    // **Known TOCTOU window**: the pre-migration code used a single
    // `openat(..., O_NOFOLLOW | O_DIRECTORY)` syscall that the kernel
    // evaluated atomically. The migration splits that into
    // `symlink_metadata` + `open_dir`, opening a window in which an
    // attacker who can plant entries inside the corpus root can race a
    // symlink in between the two calls. The blast radius is bounded by
    // cap-std's capability: even a raced symlink can only redirect within
    // the corpus root (an outside-pointing symlink fails with
    // `EscapeAttempt`), so the security guarantee that nothing writes
    // outside the corpus still holds. What's relaxed is the strict
    // "refuse ALL symlinks, even in-corpus ones" contract the existing
    // tests assert. Anticipated in the cap-std migration spec's Risk
    // Register; closing the window would require either upstream cap-std
    // exposing `nofollow` on `OpenOptions` or a manual `openat` fallback.
    match parent.symlink_metadata(name) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()));
            }
            if !meta.is_dir() {
                return Err(invalid_path_error("path component is not a directory"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Restore the pre-cap-std `mkdirat(0o755)` mode explicitly.
            // cap-std's bare `create_dir` requests `0o777`-before-umask, so
            // under a permissive umask (000/002) intermediate dirs end up
            // group/other-writable — looser than the old `mkdirat(0o755)`,
            // which capped at `0o755 & ~umask` (never group/other write) at
            // any umask. `DirBuilderExt::mode` is unix-only; on non-unix
            // (Windows) unix mode bits don't apply, so the bare `create_dir`
            // is the correct fallback.
            #[cfg(unix)]
            let created = {
                use cap_std::fs::{DirBuilder, DirBuilderExt};
                let mut builder = DirBuilder::new();
                builder.mode(0o755);
                parent.create_dir_with(name, &builder)
            };
            #[cfg(not(unix))]
            let created = parent.create_dir(name);

            match created {
                Ok(()) => {}
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Race: another process created the directory between
                    // our `symlink_metadata` and the create. Fall through
                    // to the open and let it resolve the entry.
                }
                Err(create_err) => return Err(classify_path_io_error(create_err)),
            }
        }
        Err(e) => return Err(classify_path_io_error(e)),
    }

    parent.open_dir(name).map_err(classify_path_io_error)
}

fn reject_symlink_leaf(parent: &Dir, name: &OsStr) -> Result<(), WriteError> {
    match parent.symlink_metadata(name) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()))
            } else {
                Ok(())
            }
        }
        Err(e) => Err(classify_path_io_error(e)),
    }
}

/// Default permission bits for a brand-new corpus file -- an explicit mode
/// instead of trusting the OS default (`0o666` minus umask), mirroring the
/// same explicit-mode restore `open_or_create_child_dir` applies to
/// directories (`0o755`).
const NEW_FILE_MODE: u32 = 0o644;

fn write_new_file(parent: &Dir, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    // `create_new(true)` maps to `O_CREAT | O_EXCL`. The pre-existing-leaf
    // case (including a pre-existing symlink) surfaces as `AlreadyExists`,
    // which classifies to `WriteErrorKind::Exists`. There's no race window
    // for a swap-in symlink to be followed because `O_EXCL` refuses any
    // existing entry.
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        opts.mode(NEW_FILE_MODE);
    }
    let cap_file = parent
        .open_with(name, &opts)
        .map_err(classify_create_io_error)?;
    let mut file = cap_file.into_std();
    write_and_sync(&mut file, content)?;
    drop(file);
    fsync_dir(parent)
}

fn atomic_replace(parent: &Dir, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    let mode = validate_replace_target(parent, name)?;
    let (temp_name, mut file) = create_temp_file(parent, name, mode)?;
    if let Err(err) = write_and_sync(&mut file, content) {
        cleanup_temp(parent, &temp_name);
        return Err(err);
    }
    drop(file);

    if let Err(e) = parent.rename(temp_name.as_os_str(), parent, name) {
        cleanup_temp(parent, &temp_name);
        return Err(classify_create_io_error(e));
    }
    fsync_dir(parent)
}

/// Confirms `name` is either absent or an existing regular file, and returns
/// the mode the replacement temp file must be created with: the
/// destination's own mode when it exists -- so an overwrite can never
/// broaden permissions -- or [`NEW_FILE_MODE`] when there is nothing to
/// inherit from.
fn validate_replace_target(parent: &Dir, name: &OsStr) -> Result<u32, WriteError> {
    match parent.symlink_metadata(name) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                Err(WriteError::new(WriteErrorKind::Symlink, symlink_io_error()))
            } else if ft.is_file() {
                #[cfg(unix)]
                {
                    use cap_std::fs::PermissionsExt;
                    Ok(meta.permissions().mode() & 0o777)
                }
                #[cfg(not(unix))]
                {
                    Ok(NEW_FILE_MODE)
                }
            } else {
                Err(invalid_path_error("target is not a regular file"))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NEW_FILE_MODE),
        Err(e) => Err(classify_create_io_error(e)),
    }
}

fn create_temp_file(
    parent: &Dir,
    name: &OsStr,
    mode: u32,
) -> Result<(OsString, std::fs::File), WriteError> {
    for attempt in 0..100 {
        let mut temp = OsString::from(".");
        temp.push(name);
        temp.push(format!(
            ".hallouminate-{}-{attempt}.tmp",
            std::process::id()
        ));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        match parent.open_with(temp.as_os_str(), &opts) {
            Ok(cap_file) => return Ok((temp, cap_file.into_std())),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(classify_create_io_error(e)),
        }
    }
    Err(WriteError::new(
        WriteErrorKind::Io,
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary filename collision",
        ),
    ))
}

fn write_and_sync(file: &mut std::fs::File, content: &[u8]) -> Result<(), WriteError> {
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))
}

fn fsync_dir(dir: &Dir) -> Result<(), WriteError> {
    // `cap_std::fs::Dir` has no `sync_all` method, and its underlying
    // `std::fs::File` is opened with `O_PATH` on Linux (cap-std's dir-open
    // optimisation for `*at`-only handles). `O_PATH` fds cannot be fsync'd
    // — `sync_all` on one returns `EBADF`. Re-open `.` through the
    // capability with `read(true)` to get a regular RDONLY fd; cap-std's
    // path-resolver special-cases the trailing `.` and re-opens with the
    // user-supplied options, so the resulting handle supports `sync_all`.
    // (Confirmed against cap-primitives' `internal_open::follow_with_dot`
    // branch.) Stays cross-platform — Windows accepts `sync_all` on the
    // dir handle either way.
    //
    // Cost vs pre-migration: two extra syscalls per write (`open(".")` +
    // drop) on top of the `sync_all`. `atomic_write_no_follow` is the
    // markdown-write hot path; the overhead has been measured as invisible
    // here. Revisit only if a future profile flags it.
    let dot = dir
        .open(".")
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    dot.into_std()
        .sync_all()
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))
}

fn cleanup_temp(parent: &Dir, name: &OsStr) {
    let _ = parent.remove_file(name);
}

// Numeric errno constants used in path-error classification.
//
// AC2 of the cap-std migration spec ("`rustix::io::Errno` is also removed")
// is intentionally partial here, as an explicit reviewed trade-off rather
// than inherited rustix coupling. The reason: `std::io::ErrorKind` gates
// `NotADirectory`, `IsADirectory`, `FilesystemLoop`, and `InvalidFilename`
// behind the unstable `io_error_more` feature on Rust 1.91 (the project's
// MSRV), so the public `WriteErrorKind` discrimination callers depend on
// has to come from `raw_os_error()` matches. The three alternatives all
// have worse trade-offs:
//
//   1. Hand-rolled platform-cfg-gated constants — banned by the spec
//      ("No new `#[cfg(unix)]` / `#[cfg(windows)]` gates inside
//      `sandbox.rs`").
//   2. Add `libc` as a direct dep just for the constants — heavyweight
//      for a four-constant ask.
//   3. Wait for `io_error_more` to stabilise — open-ended, MSRV-bound.
//
// The spec's Risk Register entry on `classify_path_error` already
// anticipated this gap. `rustix::io::Errno`'s associated constants are
// stable, cross-platform names that resolve to the right raw errno per
// target, and rustix is already a workspace dep (kept for
// `process::kill_process` in tests and `fs::flock` in
// `src/app/daemon/server.rs`).
//
// Linux/macOS: ELOOP=Linux 40 / macOS 62, ENOTDIR=20, EISDIR=21, EINVAL=22.
// Windows: cap-std maps native errors into the rustix errno namespace via
// its translation layer; runtime verification is deferred to a later curd
// per the spec's Verification Plan.
const ELOOP: i32 = rustix::io::Errno::LOOP.raw_os_error();
const ENOTDIR: i32 = rustix::io::Errno::NOTDIR.raw_os_error();
const EISDIR: i32 = rustix::io::Errno::ISDIR.raw_os_error();
const EINVAL: i32 = rustix::io::Errno::INVAL.raw_os_error();

/// Classify an error from a non-create path operation (open_dir, open,
/// symlink_metadata on an intermediate component). Preserves the historical
/// `WriteErrorKind::Symlink` / `InvalidPath` / `Io` discrimination callers
/// depend on.
fn classify_path_io_error(err: std::io::Error) -> WriteError {
    if let Some(raw) = err.raw_os_error() {
        if raw == ELOOP {
            return WriteError::new(WriteErrorKind::Symlink, err);
        }
        if raw == ENOTDIR || raw == EINVAL {
            // `EINVAL` covers the NUL-in-path case: cap-std (via rustix)
            // surfaces a NUL byte in an `&OsStr` path argument as `EINVAL`
            // when building the underlying `&CStr`. Pre-migration code
            // caught NUL upstream via `cstring(name)` and returned
            // `InvalidPath` with a clear "path contains a NUL byte" message;
            // preserve the public `WriteErrorKind` contract for any caller
            // that bypasses `safe_relative_path`'s boundary NUL check.
            return WriteError::new(WriteErrorKind::InvalidPath, err);
        }
    }
    if err.kind() == std::io::ErrorKind::InvalidInput {
        return WriteError::new(WriteErrorKind::InvalidPath, err);
    }
    WriteError::new(WriteErrorKind::Io, err)
}

/// Classify an error from a create or rename operation. Adds the `EEXIST →
/// Exists` mapping on top of `classify_path_io_error`'s rules, plus a
/// `EISDIR → InvalidPath` mapping the old `Errno::ISDIR` branch had.
fn classify_create_io_error(err: std::io::Error) -> WriteError {
    if let Some(raw) = err.raw_os_error() {
        if raw == ELOOP {
            return WriteError::new(WriteErrorKind::Symlink, err);
        }
        if raw == ENOTDIR || raw == EISDIR || raw == EINVAL {
            return WriteError::new(WriteErrorKind::InvalidPath, err);
        }
    }
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        return WriteError::new(WriteErrorKind::Exists, err);
    }
    if err.kind() == std::io::ErrorKind::InvalidInput {
        return WriteError::new(WriteErrorKind::InvalidPath, err);
    }
    WriteError::new(WriteErrorKind::Io, err)
}

fn symlink_io_error() -> std::io::Error {
    // Construct a synthetic ELOOP-bearing error so callers that inspect
    // `WriteError.source.raw_os_error()` see the same value the rustix-era
    // code returned via `Errno::LOOP.into()`.
    std::io::Error::from_raw_os_error(ELOOP)
}

fn invalid_path_error(msg: &'static str) -> WriteError {
    WriteError::new(
        WriteErrorKind::InvalidPath,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, msg),
    )
}

#[cfg(test)]
mod tests {
    //! Single suite for the shared sandbox helpers. Replaces the two forked
    //! test suites that previously lived in `src/app/daemon/dispatch.rs` and
    //! `src/adapters/mcp/tools.rs`. Every rejection branch of
    //! `safe_relative_path`, every `pick_corpus` outcome, every
    //! `ensure_corpus_allows_file` decision, and every atomic-write symlink
    //! refusal stays anchored to a named test so the boundary contract is
    //! immediately legible.

    use super::*;

    fn cfg(name: &str, paths: Vec<&str>) -> CorpusConfig {
        CorpusConfig {
            name: name.to_string(),
            paths: paths.into_iter().map(String::from).collect(),
            globs: Vec::new(),
            exclude: Vec::new(),
            global: false,
        }
    }

    // ── safe_relative_path ────────────────────────────────────────────────

    #[test]
    fn safe_relative_path_rejects_empty_string() {
        let err = safe_relative_path("").expect_err("empty must fail");
        assert!(err.as_str().contains("non-empty"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_absolute_path() {
        let err = safe_relative_path("/etc/passwd").expect_err("absolute must fail");
        assert!(err.as_str().contains("non-empty relative"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_parent_dir_component() {
        let err = safe_relative_path("../escape.md").expect_err("parent dir must fail");
        assert!(err.as_str().contains("normal"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_embedded_parent_dir() {
        let err = safe_relative_path("docs/../../etc/passwd").expect_err("embedded `..` must fail");
        assert!(err.as_str().contains("normal"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_current_dir_only() {
        let err = safe_relative_path(".").expect_err("`.` must fail");
        assert!(err.as_str().contains("must name a file"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_trailing_slash() {
        let err = safe_relative_path("docs/").expect_err("trailing slash must fail");
        assert!(err.as_str().contains("must name a file"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_leading_dot_slash() {
        let err = safe_relative_path("./file.md").expect_err("`./` prefix must fail");
        assert!(err.as_str().contains("normal"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_embedded_dot_segment() {
        let err = safe_relative_path("docs/./file.md").expect_err("`/./` must fail");
        assert!(err.as_str().contains("normal"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_trailing_dot_segment() {
        let err = safe_relative_path("docs/.").expect_err("trailing `/.` must fail");
        assert!(err.as_str().contains("must name a file"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_rejects_nul_byte() {
        let err = safe_relative_path("file\0.md").expect_err("NUL byte must fail");
        assert!(err.as_str().contains("NUL byte"), "got: {err}");
    }

    #[test]
    fn safe_relative_path_accepts_simple_filename() {
        let p = safe_relative_path("file.md").expect("simple name must pass");
        assert_eq!(p, Path::new("file.md"));
    }

    #[test]
    fn safe_relative_path_accepts_nested_relative_path() {
        let p = safe_relative_path("wiki/concepts/attention.md").expect("nested must pass");
        assert_eq!(p, Path::new("wiki/concepts/attention.md"));
    }

    // ── pick_corpus ───────────────────────────────────────────────────────

    #[test]
    fn pick_corpus_returns_only_corpus_when_unspecified() {
        let docs = cfg("docs", vec!["/docs"]);
        let got = pick_corpus(std::slice::from_ref(&docs), None).expect("single must pass");
        assert_eq!(got.name, "docs");
    }

    #[test]
    fn pick_corpus_errors_when_unspecified_and_multiple_configured() {
        let a = cfg("a", vec!["/a"]);
        let b = cfg("b", vec!["/b"]);
        let err = pick_corpus(&[a, b], None).expect_err("ambiguous must fail");
        assert!(err.as_str().contains("corpus required"), "got: {err}");
    }

    #[test]
    fn pick_corpus_errors_when_unspecified_and_no_corpora_configured() {
        let err = pick_corpus(&[], None).expect_err("empty must fail");
        let msg = err.as_str();
        assert!(msg.contains("no corpora configured"), "got: {msg}");
        // Hint must point at BOTH canonical config keys so the user who
        // declared `[[repository]]` doesn't go adding a redundant `[[corpus]]`.
        assert!(msg.contains("[[corpus]]"), "got: {msg}");
        assert!(msg.contains("[[repository]]"), "got: {msg}");
    }

    #[test]
    fn pick_corpus_returns_named_corpus_when_present() {
        let a = cfg("a", vec!["/a"]);
        let b = cfg("b", vec!["/b"]);
        let got = pick_corpus(&[a, b], Some("b")).expect("named must pass");
        assert_eq!(got.name, "b");
    }

    #[test]
    fn pick_corpus_errors_when_named_corpus_missing_with_quoted_name() {
        let docs = cfg("docs", vec!["/docs"]);
        let err = pick_corpus(std::slice::from_ref(&docs), Some("missing"))
            .expect_err("unknown name must fail");
        let msg = err.as_str();
        assert!(msg.contains("\"missing\""), "got: {msg}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    // ── ensure_corpus_allows_file ─────────────────────────────────────────

    #[test]
    fn ensure_corpus_allows_file_passes_when_no_globs_or_excludes_configured() {
        let corpus = cfg("docs", vec!["/docs"]);
        ensure_corpus_allows_file(&corpus, Path::new("/docs/anything.md"))
            .expect("empty rules accept everything");
    }

    #[test]
    fn ensure_corpus_allows_file_rejects_non_matching_include_glob() {
        let mut corpus = cfg("docs", vec!["/docs"]);
        corpus.globs = vec!["**/*.md".to_string()];
        let err = ensure_corpus_allows_file(&corpus, Path::new("/docs/note.txt"))
            .expect_err("txt excluded");
        assert!(err.as_str().contains("not included"), "got: {err}");
    }

    #[test]
    fn ensure_corpus_allows_file_rejects_path_matching_exclude_glob() {
        let mut corpus = cfg("docs", vec!["/docs"]);
        corpus.globs = vec!["**/*.md".to_string()];
        corpus.exclude = vec!["**/drafts/**".to_string()];
        let err = ensure_corpus_allows_file(&corpus, Path::new("/docs/drafts/wip.md"))
            .expect_err("drafts must be excluded");
        assert!(err.as_str().contains("excluded"), "got: {err}");
    }

    #[test]
    fn ensure_corpus_allows_file_accepts_path_matching_include_and_not_exclude() {
        let mut corpus = cfg("docs", vec!["/docs"]);
        corpus.globs = vec!["**/*.md".to_string()];
        corpus.exclude = vec!["**/drafts/**".to_string()];
        ensure_corpus_allows_file(&corpus, Path::new("/docs/concepts/attention.md"))
            .expect("matching path must pass");
    }

    // ── first_corpus_root ────────────────────────────────────────────────

    #[test]
    fn first_corpus_root_returns_expanded_first_path() {
        let corpus = cfg("docs", vec!["/abs/cheese"]);
        let got = first_corpus_root(&corpus).expect("present must pass");
        assert_eq!(got, PathBuf::from("/abs/cheese"));
    }

    #[test]
    fn first_corpus_root_errors_when_paths_empty() {
        let corpus = cfg("docs", vec![]);
        let err = first_corpus_root(&corpus).expect_err("empty must fail");
        assert!(err.as_str().contains("has no paths"), "got: {err}");
    }

    // ── atomic_write_no_follow ──────────────────────────────────────────

    #[test]
    fn atomic_write_no_follow_creates_parent_dirs_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let written = atomic_write_no_follow(root, Path::new("a/b/c/leaf.md"), b"# leaf\n", false)
            .expect("first write");
        assert!(written.exists(), "file landed: {}", written.display());
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "# leaf\n");
    }

    #[test]
    fn atomic_write_no_follow_rejects_intermediate_symlink_without_creating_outside_dirs() {
        // The whole reason this helper exists: a symlinked intermediate dir
        // component must NOT redirect the write outside the corpus root.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let err = atomic_write_no_follow(&root, Path::new("escape/pwned.md"), b"x", false)
            .expect_err("symlinked intermediate dir must bounce");
        assert!(
            matches!(err.kind, WriteErrorKind::Symlink),
            "expected Symlink, got {err:?}"
        );
        assert!(
            !outside.join("pwned.md").exists(),
            "writer must not punch through the symlink"
        );
    }

    #[test]
    fn atomic_write_no_follow_rejects_final_component_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let outside = tmp.path().parent().unwrap().join("outside-leaf");
        let _ = std::fs::remove_file(&outside);
        std::fs::write(&outside, b"original").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.md")).unwrap();

        let err = atomic_write_no_follow(root, Path::new("link.md"), b"new", true)
            .expect_err("final symlink must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn atomic_write_no_follow_rejects_root_itself_as_symlink() {
        // The bug this guards: a symlinked corpus root (swapped in after a
        // caller's preflight check, e.g. `ensure_wiki_root_safe`) must not
        // let the capability bootstrap redirect writes outside the root.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let root = tmp.path().join("root");
        std::os::unix::fs::symlink(&outside, &root).unwrap();

        let err = atomic_write_no_follow(&root, Path::new("pwned.md"), b"x", false)
            .expect_err("symlinked corpus root must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
        assert!(
            !outside.join("pwned.md").exists(),
            "writer must not punch through the root symlink"
        );
    }

    #[test]
    fn atomic_write_no_follow_overwrite_preserves_destination_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("secret.md"), b"first", false).unwrap();
        std::fs::set_permissions(
            root.join("secret.md"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        atomic_write_no_follow(root, Path::new("secret.md"), b"second", true).unwrap();

        let mode = std::fs::metadata(root.join("secret.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "overwrite must preserve the destination's mode"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("secret.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn atomic_write_no_follow_new_file_gets_configured_safe_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("fresh.md"), b"data", false).unwrap();
        let mode = std::fs::metadata(root.join("fresh.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & !0o644,
            0,
            "new file must not be more permissive than the configured safe mode"
        );
        assert_eq!(
            mode & 0o600,
            0o600,
            "new file must be owner-readable and owner-writable"
        );
    }

    #[test]
    fn atomic_write_no_follow_refuses_existing_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("x.md"), b"first", false).unwrap();
        let err = atomic_write_no_follow(root, Path::new("x.md"), b"second", false)
            .expect_err("second write must EEXIST");
        assert!(matches!(err.kind, WriteErrorKind::Exists), "{err:?}");
    }

    #[test]
    fn atomic_write_no_follow_classifies_nul_byte_filename_as_invalid_path() {
        // Defense-in-depth: even if a caller bypasses `safe_relative_path`
        // and reaches the writer with a NUL-bearing component, the platform
        // `EINVAL` (sourced via `rustix::io::Errno::INVAL` — see the errno
        // constants block above for why) must classify as `InvalidPath` —
        // not `Io` — so the public `WriteErrorKind` contract matches the
        // pre-migration `cstring()`-based rejection.
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A filename literally containing a NUL byte. Bypasses
        // `safe_relative_path`'s string-level check by constructing
        // straight from raw bytes.
        let nul_name = OsString::from_vec(b"bad\0name.md".to_vec());
        let rel = PathBuf::from(nul_name);
        let err = atomic_write_no_follow(root, &rel, b"x", false)
            .expect_err("NUL in component must fail");
        assert!(
            matches!(err.kind, WriteErrorKind::InvalidPath),
            "expected InvalidPath, got {err:?}"
        );
    }

    #[test]
    fn atomic_write_no_follow_overwrites_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("x.md"), b"first", false).unwrap();
        atomic_write_no_follow(root, Path::new("x.md"), b"second", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("x.md")).unwrap(),
            "second"
        );
    }

    // ── cap-std migration hardening ──────────────────────────────────
    //
    // These tests lock behaviour that changed shape in the cap-std
    // migration: the symlink-refusal contract moved from kernel
    // `O_NOFOLLOW` to `Dir::symlink_metadata` pre-checks, and the
    // capability now also refuses outside-the-root escapes via its own
    // `EscapeAttempt`. Each test pins one cap-std-era property that would
    // regress if the pre-check were dropped or the capability shape
    // changed.

    #[test]
    fn read_no_follow_returns_contents_when_path_is_plain_file() {
        // Happy-path lock for `read_no_follow` — the cap-std migration
        // rewrote the leaf-open path; if the new `Dir::open` + `into_std`
        // chain ever returns the wrong handle (e.g. a re-opened symlink
        // target), the round-trip read will surface the regression.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/leaf.md"), b"# hardened\n").unwrap();
        let got = read_no_follow(root, Path::new("a/b/leaf.md")).expect("read");
        assert_eq!(got, b"# hardened\n");
    }

    #[test]
    fn read_no_follow_rejects_symlinked_leaf_with_symlink_kind() {
        // Mirrors `atomic_write_no_follow_rejects_final_component_symlink`
        // but on the read path. The cap-std migration relies on a
        // `Dir::symlink_metadata` pre-check (cap-std's own `Dir::open`
        // would silently follow an in-capability symlink); regress the
        // pre-check and a symlinked leaf would surface as `Io` (or worse,
        // succeed) instead of `Symlink`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let target = root.join("real.md");
        std::fs::write(&target, b"target\n").unwrap();
        std::os::unix::fs::symlink(&target, root.join("link.md")).unwrap();
        let err =
            read_no_follow(&root, Path::new("link.md")).expect_err("symlinked leaf must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
    }

    #[test]
    fn read_no_follow_rejects_symlinked_intermediate_dir() {
        // The pre-fix daemon path missed this exact case (leaf-only
        // `symlink_metadata` check). The cap-std migration must keep the
        // every-component pre-check — if `open_or_create_child_dir` skips
        // the `symlink_metadata` call, an attacker who can plant a
        // symlinked dir component inside the corpus can redirect reads.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), b"shh\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let err = read_no_follow(&root, Path::new("escape/secret.md"))
            .expect_err("intermediate symlink must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
    }

    #[test]
    fn read_no_follow_classifies_missing_file_as_invalid_path() {
        // Locks the classification path: a missing leaf goes through
        // `classify_path_io_error`. ENOENT → `Io` (not `InvalidPath`,
        // which is reserved for shape violations) — pins the contract so
        // a downstream caller can still distinguish "you gave me a bad
        // path" from "the file isn't there".
        let tmp = tempfile::tempdir().unwrap();
        let err = read_no_follow(tmp.path(), Path::new("missing.md"))
            .expect_err("missing file must fail");
        assert!(matches!(err.kind, WriteErrorKind::Io), "{err:?}");
        assert_eq!(
            err.source.kind(),
            std::io::ErrorKind::NotFound,
            "source kind: {err:?}"
        );
    }

    // ── resolve_read_root: multi-root read resolution (Curd B) ───────────

    #[test]
    fn resolve_read_root_single_root_returns_that_root_when_file_present() {
        // Single-root behaviour is unchanged: the one configured root is the
        // sole candidate and resolves when the file is present under it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("only");
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("notes/a.md"), b"# a\n").unwrap();
        let corpus = cfg("docs", vec![root.to_str().unwrap()]);
        let got =
            resolve_read_root(&corpus, Path::new("notes/a.md")).expect("present file resolves");
        assert_eq!(got, root);
    }

    #[test]
    fn resolve_read_root_finds_file_under_a_non_first_root() {
        // The whole point of Curd B: a file living under `paths[1]` (searchable
        // via the multi-root scan) must be reachable for reads, not just
        // `paths[0]`.
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("only-here.md"), b"# here\n").unwrap();
        let corpus = cfg(
            "docs",
            vec![first.to_str().unwrap(), second.to_str().unwrap()],
        );
        let got =
            resolve_read_root(&corpus, Path::new("only-here.md")).expect("file under paths[1]");
        assert_eq!(
            got, second,
            "must resolve to the root that actually holds the file"
        );
    }

    #[test]
    fn resolve_read_root_prefers_earlier_root_on_collision() {
        // When the same relative path exists under more than one root, the
        // first configured root wins — deterministic, order-stable resolution.
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("dup.md"), b"first\n").unwrap();
        std::fs::write(second.join("dup.md"), b"second\n").unwrap();
        let corpus = cfg(
            "docs",
            vec![first.to_str().unwrap(), second.to_str().unwrap()],
        );
        let got = resolve_read_root(&corpus, Path::new("dup.md")).expect("present");
        assert_eq!(got, first);
    }

    #[test]
    fn resolve_read_root_missing_in_all_roots_is_notfound_io() {
        // No root contains the file → the same `Io`/`NotFound` shape
        // `read_no_follow` returns for a missing leaf, so callers map it to a
        // single "does not exist" response.
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let corpus = cfg(
            "docs",
            vec![first.to_str().unwrap(), second.to_str().unwrap()],
        );
        let err = resolve_read_root(&corpus, Path::new("nope.md")).expect_err("absent everywhere");
        assert!(matches!(err.kind, WriteErrorKind::Io), "{err:?}");
        assert_eq!(err.source.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }

    #[test]
    fn resolve_read_root_symlinked_leaf_bounces_without_falling_through() {
        // A symlinked leaf under the first root is a security stop: it must
        // surface as `Symlink`, NOT be silently skipped so a same-named real
        // file under a later root gets read instead.
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, b"secret\n").unwrap();
        std::os::unix::fs::symlink(&outside, first.join("link.md")).unwrap();
        std::fs::write(second.join("link.md"), b"real\n").unwrap();
        let corpus = cfg(
            "docs",
            vec![first.to_str().unwrap(), second.to_str().unwrap()],
        );
        let err =
            resolve_read_root(&corpus, Path::new("link.md")).expect_err("symlinked leaf bounces");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
    }

    #[test]
    fn resolve_read_root_does_not_create_directories_while_probing() {
        // Non-mutating contract: probing a root that lacks the nested parent
        // dirs must not create them (unlike `read_no_follow`, which walks via
        // the create-on-missing parent opener).
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(second.join("deep/nested")).unwrap();
        std::fs::write(second.join("deep/nested/leaf.md"), b"x\n").unwrap();
        let corpus = cfg(
            "docs",
            vec![first.to_str().unwrap(), second.to_str().unwrap()],
        );
        resolve_read_root(&corpus, Path::new("deep/nested/leaf.md"))
            .expect("resolves under second");
        assert!(
            !first.join("deep").exists(),
            "probing the first root must not create the nested parent dirs"
        );
    }

    #[test]
    fn delete_no_follow_removes_existing_regular_file() {
        // Happy-path lock for the delete path. cap-std's
        // `Dir::remove_file` is the migration's replacement for
        // `rustix::fs::unlinkat`; this test would catch a regression
        // where the call is wired to the wrong cap-std method (e.g.
        // `remove_dir`, which would fail on a regular file).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let target = root.join("sub/doomed.md");
        std::fs::write(&target, b"bye\n").unwrap();
        delete_no_follow(root, Path::new("sub/doomed.md")).expect("delete");
        assert!(!target.exists(), "file must be gone");
    }

    #[test]
    fn delete_no_follow_rejects_symlinked_leaf_without_unlinking_target() {
        // Lock the security contract: a symlinked leaf must surface as
        // `Symlink` and the target must survive. Regress the pre-check
        // and cap-std's `Dir::remove_file` would unlink the SYMLINK
        // itself (POSIX unlink semantics) — which technically leaves the
        // target intact, but loses the explicit `Symlink` discrimination
        // every daemon and MCP caller maps to a user-visible "refused
        // symlink" error.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let target = tmp.path().join("target.md");
        std::fs::write(&target, b"keep me\n").unwrap();
        std::os::unix::fs::symlink(&target, root.join("link.md")).unwrap();
        let err =
            delete_no_follow(&root, Path::new("link.md")).expect_err("symlinked leaf must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
        assert!(target.exists(), "target must survive");
        assert!(root.join("link.md").exists(), "symlink must survive too");
    }

    #[test]
    fn delete_no_follow_rejects_symlinked_intermediate_dir() {
        // Same shape as the read test — covers the unlink path. The
        // existing MCP integration test
        // `mcp_delete_markdown_rejects_intermediate_symlinked_directory`
        // covers this at the transport level; this unit lock makes the
        // contract regression visible without booting the daemon.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("victim.md"), b"keep\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let err = delete_no_follow(&root, Path::new("escape/victim.md"))
            .expect_err("intermediate symlink must bounce");
        assert!(matches!(err.kind, WriteErrorKind::Symlink), "{err:?}");
        assert!(
            outside.join("victim.md").exists(),
            "victim file must survive the refused unlink"
        );
    }

    #[test]
    fn delete_no_follow_rejects_directory_target_as_invalid_path() {
        // The leaf-is-a-directory case maps to `InvalidPath` ("target is
        // not a regular file"), not `Io`. The cap-std migration's
        // pre-check inspects `meta.is_file()`; this test pins that
        // discrimination so a daemon caller asking for `delete_markdown
        // some-subdir` gets the right user-facing error.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("notafile")).unwrap();
        let err = delete_no_follow(root, Path::new("notafile")).expect_err("dir target must fail");
        assert!(matches!(err.kind, WriteErrorKind::InvalidPath), "{err:?}");
    }

    #[test]
    fn atomic_write_no_follow_rejects_regular_file_as_intermediate_dir_component() {
        // If a normal file sits where the writer expects a directory,
        // the failure must be `InvalidPath` — not `Io`. The cap-std
        // migration routes this through `open_or_create_child_dir`'s
        // post-`symlink_metadata` `meta.is_dir()` check; without that
        // check, cap-std would surface `NotADirectory` (errno mapped to
        // `Io` via the stable `ErrorKind`), losing the historical
        // discrimination.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a"), b"i am a file\n").unwrap();
        let err = atomic_write_no_follow(root, Path::new("a/b.md"), b"x", false)
            .expect_err("file-as-dir-component must fail");
        assert!(matches!(err.kind, WriteErrorKind::InvalidPath), "{err:?}");
    }

    /// Run `body` with the process umask forced to 0, holding a process-wide
    /// lock for the whole window so the global umask cannot race a sibling
    /// test, and restoring the inherited umask before releasing the lock.
    /// Needed because `cargo test` runs a binary's tests on multiple threads
    /// and `umask(2)` is per-process, not per-thread; the `Restore` guard also
    /// repairs the umask if `body` panics.
    #[cfg(unix)]
    fn with_permissive_umask<R>(body: impl FnOnce() -> R) -> R {
        use rustix::fs::Mode;
        use rustix::process::umask;
        use std::sync::Mutex;

        static UMASK_LOCK: Mutex<()> = Mutex::new(());

        struct Restore(Mode);
        impl Drop for Restore {
            fn drop(&mut self) {
                umask(self.0);
            }
        }

        // Mutual exclusion is all we need; a poisoned lock (a prior test
        // panicked mid-window) is still safe to take.
        let _serialize = UMASK_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Dropped before `_serialize` (LIFO), so the umask is restored while
        // the lock is still held.
        let _restore = Restore(umask(Mode::empty()));
        body()
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_no_follow_caps_intermediate_dir_mode_at_0o755_under_permissive_umask() {
        // The cap-std migration swapped `mkdirat(.., 0o755)` for a bare
        // `Dir::create_dir`, which requests `0o777`-before-umask. Under a
        // permissive umask the created intermediate dir is group/other
        // writable — a silent loosening the old `mkdirat(0o755)` never
        // allowed. We force umask 0 so the buggy path yields 0o777 and only
        // the explicit `DirBuilderExt::mode(0o755)` caps it; under the
        // default 0o022 umask both paths coincide at 0o755 and the bug is
        // invisible (Risk Register R1).
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        with_permissive_umask(|| {
            atomic_write_no_follow(root, Path::new("perm/leaf.md"), b"x", false)
                .expect("write through a fresh intermediate dir");
        });

        let mode = std::fs::metadata(root.join("perm"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "intermediate dir must be capped at 0o755, got {mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_no_follow_caps_every_nested_intermediate_dir_at_0o755() {
        // Boundary for the component walk (Risk Register R2): each level of
        // a deep path is created one-at-a-time through
        // `open_or_create_child_dir`, so the `0o755` cap must hold for EVERY
        // intermediate dir, not just the leaf's parent. A regression that
        // only moded the last component would still leak `0o777` on the
        // outer dirs under a permissive umask.
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        with_permissive_umask(|| {
            atomic_write_no_follow(root, Path::new("a/b/c/leaf.md"), b"x", false)
                .expect("write through three fresh intermediate dirs");
        });

        for rel in ["a", "a/b", "a/b/c"] {
            let mode = std::fs::metadata(root.join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "{rel} must be capped at 0o755, got {mode:o}");
        }
    }
    #[test]
    fn atomic_write_no_follow_durable_write_round_trips_through_fsync_path() {
        // Locks the `fsync_dir` workaround for cap-std's `O_PATH` `Dir`
        // handles on Linux: if the re-open of `.` regresses (e.g. the
        // `Dir::open(".")` call gets replaced with a `try_clone` that
        // returns the same O_PATH fd), `sync_all` would surface `EBADF`
        // and propagate as `WriteError { kind: Io, .. }`. A successful
        // round-trip means the workaround stayed effective.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("durable/leaf.md"), b"sync me\n", false)
            .expect("first write must survive the fsync_dir call");
        atomic_write_no_follow(root, Path::new("durable/leaf.md"), b"and again\n", true)
            .expect("overwrite must survive the second fsync_dir call");
        assert_eq!(
            std::fs::read(root.join("durable/leaf.md")).unwrap(),
            b"and again\n"
        );
    }

    // ── list_corpus_files ──────────────────────────────────────────────

    #[test]
    fn list_corpus_files_returns_sorted_relative_markdown_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("b.md"), "b").unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("sub/c.md"), "c").unwrap();
        std::fs::write(root.join("skip.txt"), "x").unwrap();
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let entries = list_corpus_files(&corpus).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "b.md", "sub/c.md"]);
    }

    #[test]
    fn canonicalize_or_log_falls_back_to_input_for_missing_path() {
        // A freshly-configured wiki corpus root may not yet exist on disk when
        // `list_corpus_files` / `build_corpus_tree` build their root list.
        // `canonicalize` errors for a non-existent path; the helper must fall
        // back to the (non-canonical) input rather than panic on `unwrap`.
        // WHY: the pre-fix `.unwrap_or(expanded)` swallowed the error silently
        // — this pins that the fallback is graceful AND returns the exact
        // input path so `absolute_path` is still well-formed (just resolvable
        // to the LanceDB `file_ref` only once the dir exists and is re-listed).
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("not-created-yet");
        assert!(!missing.exists(), "precondition: path must not exist");
        let got = canonicalize_or_log(missing.clone());
        assert_eq!(
            got, missing,
            "fallback must return the input path unchanged"
        );
    }

    #[test]
    fn canonicalize_or_log_returns_canonical_path_for_existing_dir() {
        // The success arm: an existing root must be canonicalized, NOT passed
        // through. WHY: `FileEntry.absolute_path` has to match the canonical
        // `file_ref` LanceDB stores; a regression that always returned the raw
        // input (collapsing the helper to just the fallback) would silently
        // de-canonicalize every path. On macOS `tempdir()` lives under the
        // `/var -> /private/var` symlink, so the canonical form genuinely
        // differs from the input and this assertion has teeth.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let expected = std::fs::canonicalize(&root).expect("existing dir canonicalizes");
        let got = canonicalize_or_log(root);
        assert_eq!(
            got, expected,
            "existing dir must resolve to its canonical path"
        );
    }

    #[test]
    fn list_corpus_files_returns_empty_for_freshly_created_empty_corpus() {
        // The real fresh-wiki path: `ensure_paths_exist` has created the root
        // dir but no markdown has been written yet. `canonicalize` succeeds
        // here, but the corpus must list cleanly (no panic, empty result)
        // rather than error — the regression guard for "first `list_files`
        // against a brand-new wiki".
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fresh-wiki");
        std::fs::create_dir_all(&root).unwrap();
        let corpus = CorpusConfig {
            name: "fresh".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let entries = list_corpus_files(&corpus).expect("empty fresh corpus must list cleanly");
        assert!(entries.is_empty(), "no markdown written yet");
    }

    // ── build_corpus_tree ──────────────────────────────────────────────

    #[test]
    fn build_corpus_tree_groups_files_by_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("adapters")).unwrap();
        std::fs::create_dir_all(root.join("adapters/nested")).unwrap();
        std::fs::write(root.join("top.md"), "# Top").unwrap();
        std::fs::write(root.join("adapters/index.md"), "# Adapters").unwrap();
        std::fs::write(root.join("adapters/lance.md"), "# Lance").unwrap();
        std::fs::write(root.join("adapters/nested/deep.md"), "# Deep").unwrap();
        let corpus = CorpusConfig {
            name: "docs".into(),
            paths: vec![root.to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let tree = build_corpus_tree(&corpus).unwrap();
        assert_eq!(tree.path, "");
        assert_eq!(tree.files.len(), 1, "one top-level file");
        assert_eq!(tree.files[0].path, "top.md");
        assert_eq!(tree.subdirs.len(), 1, "one subdir at root");
        let adapters = &tree.subdirs[0];
        assert_eq!(adapters.path, "adapters");
        assert_eq!(adapters.files.len(), 2);
        assert_eq!(adapters.subdirs.len(), 1);
        assert_eq!(adapters.subdirs[0].path, "adapters/nested");
        assert_eq!(adapters.subdirs[0].files[0].path, "adapters/nested/deep.md");
    }

    #[test]
    fn build_corpus_tree_returns_root_only_for_empty_corpus() {
        let tmp = tempfile::tempdir().unwrap();
        let corpus = CorpusConfig {
            name: "empty".into(),
            paths: vec![tmp.path().to_string_lossy().into_owned()],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let tree = build_corpus_tree(&corpus).unwrap();
        assert_eq!(tree.path, "");
        assert!(tree.files.is_empty());
        assert!(tree.subdirs.is_empty());
    }

    #[test]
    fn build_corpus_tree_drops_files_outside_first_root_for_multi_root_corpus() {
        // Regression for the multi-root filter bug: `list_corpus_files` strips
        // `path` against whichever root matched, so a file under `paths[1..]`
        // used to slip past the `absolute_path == path` filter and land in
        // the tree under a bogus subdir of `paths[0]`. With the strip-against-
        // `root_abs` filter, only files under the first root survive.
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("a.md"), "# A").unwrap();
        std::fs::write(second.join("b.md"), "# B").unwrap();
        let corpus = CorpusConfig {
            name: "multi".into(),
            paths: vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            globs: vec!["**/*.md".into()],
            exclude: vec![],
            global: false,
        };
        let tree = build_corpus_tree(&corpus).unwrap();
        assert_eq!(tree.path, "");
        assert!(
            tree.subdirs.is_empty(),
            "non-first-root file must not fan out"
        );
        assert_eq!(
            tree.files.len(),
            1,
            "only the first-root file lands in the tree"
        );
        assert_eq!(tree.files[0].path, "a.md");
    }
}
