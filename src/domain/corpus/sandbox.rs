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
//! 2. **`atomic_write_no_follow`** — the symlink-safe `openat`-based writer
//!    the MCP path already used. The daemon's previous
//!    `tokio::fs::create_dir_all` + `tokio::fs::write` path only checked the
//!    leaf for symlinks and missed symlinked intermediate directory
//!    components; this shared implementation walks every path component with
//!    `O_NOFOLLOW | O_DIRECTORY` so a symlinked dir-component bounces with
//!    `WriteError { kind: Symlink, .. }`.
//!
//! 3. **`list_corpus_files`** — scan-and-format helper both transports
//!    expose via `list_files` and the daemon's add-markdown ack.
//!
//! 4. **`read_no_follow` / `delete_no_follow`** — symlink-safe read + unlink
//!    counterparts to `atomic_write_no_follow`. The pre-refactor daemon used
//!    `tokio::fs::read` / `tokio::fs::remove_file` after a leaf-only
//!    `symlink_metadata` check; that left intermediate-symlink escapes wide
//!    open. These helpers walk every parent component with `O_NOFOLLOW` so a
//!    symlinked dir component bounces with `WriteError { kind: Symlink, .. }`
//!    instead of leaking files outside the corpus root.

use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::domain::common::{CorpusConfig, expand_tilde};
use crate::domain::corpus::scan;

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
            std::fs::canonicalize(&expanded).unwrap_or(expanded)
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

// ── atomic_write_no_follow ──────────────────────────────────────────────
//
// `openat`-based atomic writer that walks every path component with
// `O_NOFOLLOW | O_DIRECTORY` and refuses to traverse a symlinked component.
// Previously a fork of this lived only in the MCP transport while the
// daemon used `tokio::fs::write` + a leaf `symlink_metadata` check — which
// missed symlinked intermediate directories. Sharing the implementation
// closes that asymmetry and gives both transports the same write contract.

#[derive(Debug)]
pub enum WriteErrorKind {
    /// File exists and `overwrite=false`.
    Exists,
    /// A path component was a symlink while walking with `O_NOFOLLOW`.
    Symlink,
    /// The relative path names a non-directory parent or non-file target.
    InvalidPath,
    /// Any other I/O failure.
    Io,
}

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

pub fn atomic_write_no_follow(
    root: &Path,
    relative: &Path,
    content: &[u8],
    overwrite: bool,
) -> Result<PathBuf, WriteError> {
    std::fs::create_dir_all(root).map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
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
/// component with `O_NOFOLLOW`. The leaf is opened `O_RDONLY | O_NOFOLLOW`,
/// so a symlinked leaf bounces with `WriteErrorKind::Symlink`. Mirrors
/// `atomic_write_no_follow`'s component-walk so a single change to the
/// no-follow logic stays in one place.
pub fn read_no_follow(root: &Path, relative: &Path) -> Result<Vec<u8>, WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;
    let c_name = cstring(file_name.as_os_str())?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags) };
    let owned = fd_to_owned(fd, WriteErrorKind::Io).map_err(|err| {
        // Match `open_child_dir_no_follow`'s classification: ELOOP on Linux,
        // ENOTDIR / EACCES on macOS for symlinked leaves opened with
        // `O_NOFOLLOW`. Promote whichever to `Symlink` if `lstat` confirms
        // the entry is a symlink, so the caller sees one consistent kind.
        let kind = classify_path_error(err.source);
        if matches!(
            kind.kind,
            WriteErrorKind::InvalidPath | WriteErrorKind::Io
        ) && is_symlink_at(parent.as_raw_fd(), file_name.as_os_str())
        {
            return WriteError::new(
                WriteErrorKind::Symlink,
                std::io::Error::from_raw_os_error(libc::ELOOP),
            );
        }
        kind
    })?;
    let mut file = unsafe { std::fs::File::from_raw_fd(owned.into_raw_fd()) };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| WriteError::new(WriteErrorKind::Io, e))?;
    Ok(buf)
}

/// Unlink `<root>/<relative>` after walking every parent component with
/// `O_NOFOLLOW`. Refuses to delete when the leaf is a symlink (the pre-fix
/// daemon path silently followed parent symlinks via `tokio::fs::remove_file`,
/// letting a corpus-controlled directory symlink redirect the unlink outside
/// the corpus root).
pub fn delete_no_follow(root: &Path, relative: &Path) -> Result<(), WriteError> {
    let names = normal_components(relative)?;
    let file_name = names
        .last()
        .ok_or_else(|| invalid_path_error("path must name a file"))?;
    let parent = open_parent_dir(root, &names[..names.len() - 1])?;
    let c_name = cstring(file_name.as_os_str())?;

    // Pre-flight: `fstatat` with `AT_SYMLINK_NOFOLLOW` so the symlink-leaf
    // case surfaces as `Symlink` (and not-a-regular-file surfaces as
    // `InvalidPath`) before we call `unlinkat`. `unlinkat` without
    // `AT_REMOVEDIR` also doesn't follow symlinks by itself, but the daemon's
    // "deleted X" success message would otherwise be misleading.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        return Err(classify_create_error(std::io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        return Err(WriteError::new(
            WriteErrorKind::Symlink,
            std::io::Error::from_raw_os_error(libc::ELOOP),
        ));
    }
    if file_type != libc::S_IFREG {
        return Err(invalid_path_error("target is not a regular file"));
    }

    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc == -1 {
        return Err(WriteError::new(
            WriteErrorKind::Io,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
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

fn open_parent_dir(root: &Path, dirs: &[OsString]) -> Result<OwnedFd, WriteError> {
    let mut current = open_root_dir(root)?;
    for dir in dirs {
        current = open_or_create_child_dir(current.as_raw_fd(), dir.as_os_str())?;
    }
    Ok(current)
}

fn open_root_dir(root: &Path) -> Result<OwnedFd, WriteError> {
    let c_root = cstring(root.as_os_str())?;
    let fd = unsafe {
        libc::open(
            c_root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    fd_to_owned(fd, WriteErrorKind::Io)
}

fn open_or_create_child_dir(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, WriteError> {
    match open_child_dir_no_follow(parent_fd, name) {
        Ok(fd) => Ok(fd),
        Err(err) if err.source.raw_os_error() == Some(libc::ENOENT) => {
            let c_name = cstring(name)?;
            let made = unsafe { libc::mkdirat(parent_fd, c_name.as_ptr(), 0o755) };
            if made == -1 {
                let source = std::io::Error::last_os_error();
                if source.raw_os_error() != Some(libc::EEXIST) {
                    return Err(classify_path_error(source));
                }
            }
            open_child_dir_no_follow(parent_fd, name)
        }
        Err(err) => Err(err),
    }
}

fn open_child_dir_no_follow(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, WriteError> {
    let c_name = cstring(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    fd_to_owned(fd, WriteErrorKind::Io).map_err(|err| {
        // Linux: opening a symlinked component with `O_NOFOLLOW | O_DIRECTORY`
        // returns `ELOOP`. macOS: same syscall returns `ENOTDIR` because the
        // dir-open refusal happens before the symlink check. To classify the
        // refusal consistently across platforms (the security guarantee is
        // identical — the writer bounces either way), `lstat` the entry and
        // promote `ENOTDIR` to `Symlink` when the entry is actually a symlink.
        let kind = classify_path_error(err.source);
        if matches!(kind.kind, WriteErrorKind::InvalidPath) && is_symlink_at(parent_fd, name) {
            return WriteError::new(
                WriteErrorKind::Symlink,
                std::io::Error::from_raw_os_error(libc::ELOOP),
            );
        }
        kind
    })
}

fn is_symlink_at(parent_fd: i32, name: &OsStr) -> bool {
    let Ok(c_name) = cstring(name) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

fn write_new_file(parent: &OwnedFd, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    let c_name = cstring(name)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags, 0o644) };
    if fd == -1 {
        return Err(classify_create_error(std::io::Error::last_os_error()));
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    write_and_sync(&mut file, content)?;
    drop(file);
    fsync_dir(parent)
}

fn atomic_replace(parent: &OwnedFd, name: &OsStr, content: &[u8]) -> Result<(), WriteError> {
    validate_replace_target(parent.as_raw_fd(), name)?;
    let (temp_name, mut file) = create_temp_file(parent.as_raw_fd(), name)?;
    if let Err(err) = write_and_sync(&mut file, content) {
        cleanup_temp(parent.as_raw_fd(), &temp_name);
        return Err(err);
    }
    drop(file);

    let c_name = cstring(name)?;
    let renamed = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            temp_name.as_ptr(),
            parent.as_raw_fd(),
            c_name.as_ptr(),
        )
    };
    if renamed == -1 {
        let source = std::io::Error::last_os_error();
        cleanup_temp(parent.as_raw_fd(), &temp_name);
        return Err(classify_create_error(source));
    }
    fsync_dir(parent)
}

fn validate_replace_target(parent_fd: i32, name: &OsStr) -> Result<(), WriteError> {
    let c_name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(classify_create_error(source));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        return Err(WriteError::new(
            WriteErrorKind::Symlink,
            std::io::Error::from_raw_os_error(libc::ELOOP),
        ));
    }
    if file_type != libc::S_IFREG {
        return Err(invalid_path_error("target is not a regular file"));
    }
    Ok(())
}

fn create_temp_file(parent_fd: i32, name: &OsStr) -> Result<(CString, std::fs::File), WriteError> {
    for attempt in 0..100 {
        let mut temp = OsString::from(".");
        temp.push(name);
        temp.push(format!(
            ".hallouminate-{}-{attempt}.tmp",
            std::process::id()
        ));
        let c_temp = cstring(temp.as_os_str())?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let fd = unsafe { libc::openat(parent_fd, c_temp.as_ptr(), flags, 0o644) };
        if fd != -1 {
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            return Ok((c_temp, file));
        }
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::EEXIST) {
            return Err(classify_create_error(source));
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

fn fsync_dir(dir: &OwnedFd) -> Result<(), WriteError> {
    let rc = unsafe { libc::fsync(dir.as_raw_fd()) };
    if rc == -1 {
        Err(WriteError::new(
            WriteErrorKind::Io,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

fn cleanup_temp(parent_fd: i32, name: &CString) {
    unsafe {
        libc::unlinkat(parent_fd, name.as_ptr(), 0);
    }
}

fn fd_to_owned(fd: i32, default: WriteErrorKind) -> Result<OwnedFd, WriteError> {
    if fd == -1 {
        Err(WriteError::new(default, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn cstring(name: &OsStr) -> Result<CString, WriteError> {
    CString::new(name.as_bytes()).map_err(|_| invalid_path_error("path contains a NUL byte"))
}

fn classify_path_error(source: std::io::Error) -> WriteError {
    match source.raw_os_error() {
        Some(errno) if errno == libc::ELOOP => WriteError::new(WriteErrorKind::Symlink, source),
        Some(errno) if errno == libc::ENOTDIR => {
            WriteError::new(WriteErrorKind::InvalidPath, source)
        }
        _ => WriteError::new(WriteErrorKind::Io, source),
    }
}

fn classify_create_error(source: std::io::Error) -> WriteError {
    match source.raw_os_error() {
        Some(errno) if errno == libc::ELOOP => WriteError::new(WriteErrorKind::Symlink, source),
        Some(errno) if errno == libc::EEXIST => WriteError::new(WriteErrorKind::Exists, source),
        Some(errno) if errno == libc::ENOTDIR || errno == libc::EISDIR => {
            WriteError::new(WriteErrorKind::InvalidPath, source)
        }
        _ => WriteError::new(WriteErrorKind::Io, source),
    }
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
    fn atomic_write_no_follow_refuses_existing_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_write_no_follow(root, Path::new("x.md"), b"first", false).unwrap();
        let err = atomic_write_no_follow(root, Path::new("x.md"), b"second", false)
            .expect_err("second write must EEXIST");
        assert!(matches!(err.kind, WriteErrorKind::Exists), "{err:?}");
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
        };
        let entries = list_corpus_files(&corpus).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "b.md", "sub/c.md"]);
    }
}
