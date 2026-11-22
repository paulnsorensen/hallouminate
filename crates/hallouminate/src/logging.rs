//! Tracing subscriber wiring.
//!
//! Each process writes its own log file in
//! `$XDG_STATE_HOME/hallouminate/` (fallback `~/.local/state/hallouminate/`).
//! The [`LogRole`] selects the file name: `daemon.log` for the daemon,
//! `serve-<pid>.log` for an MCP stdio server, and `cli-<pid>.log` for any other
//! command. Exactly one process writes each file, so size rotation cannot race.
//!
//! A file rotates when the next record would exceed `max_file_bytes`. Archives
//! use `<name>.log.1`, `.2`, and so on, with the highest number newest.
//! `max_total_bytes` bounds all hallouminate log files in the directory
//! together, including legacy `hallouminate.log` files. At startup and after
//! each rotation, the oldest files of dead processes go first, then the oldest
//! archives of the rotating process. The active file and archives of a live
//! process are never deleted by another process.
//!
//! Each writer holds an exclusive advisory lock on `<name>.log.lock` while it
//! runs. A cleaner that takes this lock without blocking has proof that the
//! owner is dead, so a reused pid cannot make a live file look dead. The file
//! and its lock appear on the first record, so a command that logs nothing
//! leaves no file. A second daemon that finds `daemon.log` locked writes
//! `daemon-<pid>.log` instead.
//!
//! After the subscriber installs, a panic hook records every panic as an
//! `ERROR` event with target `hallouminate::panic`, then runs the previous
//! hook so stderr output stays.
//!
//! The returned [`LogGuard`] must be held while logging is active. Dropping it
//! drains queued records, then closes the file and releases its lock.
//!
//! Filter precedence: `HALLOUMINATE_LOG` env var → `RUST_LOG` → default
//! `hallouminate=info`. Use the same syntax as `RUST_LOG`
//! (`hallouminate=debug,lance=warn`).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use chrono::NaiveDate;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER: &str = "hallouminate=info";
const LEGACY_LOG_PREFIX: &str = "hallouminate.";
const LEGACY_LOG_SUFFIX: &str = ".log";
const DAEMON_FAMILY: &str = "daemon";
const LEGACY_FAMILY: &str = "hallouminate";
const PID_FAMILY_PREFIXES: [&str; 3] = ["daemon-", "serve-", "cli-"];
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

/// The kind of hallouminate process, which selects its log file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRole {
    /// The daemon. Writes `daemon.log`, or `daemon-<pid>.log` when another
    /// live daemon holds `daemon.log`.
    Daemon,
    /// An MCP stdio server. Writes `serve-<pid>.log`.
    Serve,
    /// Any other CLI command. Writes `cli-<pid>.log`.
    Cli,
}

/// Keeps the background log writer alive.
///
/// Dropping the guard drains and flushes queued records, then closes the log
/// file and releases its owner lock.
pub struct LogGuard {
    worker: Option<WorkerGuard>,
    slot: Arc<Mutex<Slot>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        drop(self.worker.take());
        *lock_slot(&self.slot) = Slot::Closed;
    }
}

/// Install the global tracing subscriber and the panic hook for a process of
/// the given `role`.
///
/// # Errors
///
/// Returns an error if the log directory cannot be prepared or a global
/// subscriber is already installed.
pub fn init(
    role: LogRole,
    config: &hallouminate_config::LoggingConfig,
) -> anyhow::Result<LogGuard> {
    install(&state_dir(), role, std::process::id(), config)
}

fn install(
    dir: &Path,
    role: LogRole,
    pid: u32,
    config: &hallouminate_config::LoggingConfig,
) -> anyhow::Result<LogGuard> {
    let (non_blocking, guard) = log_writer(dir, role, pid, config)?;

    let filter = EnvFilter::try_from_env("HALLOUMINATE_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init()
        .map_err(|e| anyhow::anyhow!("install tracing subscriber: {e}"))?;

    install_panic_hook();
    Ok(guard)
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let current = std::thread::current();
        let thread = current.name().unwrap_or("<unnamed>");
        let location = match info.location() {
            Some(location) => location.to_string(),
            None => "<unknown location>".to_owned(),
        };
        let payload = info.payload_as_str().unwrap_or("non-string panic payload");
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            target: "hallouminate::panic",
            thread,
            %location,
            %backtrace,
            "thread '{thread}' panicked at {location}: {payload}"
        );
        previous(info);
    }));
}

fn log_writer(
    dir: &Path,
    role: LogRole,
    pid: u32,
    config: &hallouminate_config::LoggingConfig,
) -> anyhow::Result<(NonBlocking, LogGuard)> {
    std::fs::create_dir_all(dir)?;
    reap_legacy_dated_logs(dir)?;
    prune(dir, None, config)?;
    let slot = Arc::new(Mutex::new(Slot::Unopened));
    let sink = LogSink {
        dir: dir.to_path_buf(),
        role,
        pid,
        config: *config,
        slot: Arc::clone(&slot),
    };
    // Stay lossy (the crate default): the async daemon must not block a tokio
    // worker on a full log buffer. Flush-on-exit is guaranteed by `LogGuard`
    // drop, not by non-lossy mode.
    let (writer, worker) = NonBlockingBuilder::default().finish(sink);
    let guard = LogGuard {
        worker: Some(worker),
        slot,
    };
    Ok((writer, guard))
}

/// Base name shared by one writer's active file, archives, and lock file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LogFamily(String);

impl LogFamily {
    fn candidates(role: LogRole, pid: u32) -> Vec<Self> {
        match role {
            LogRole::Daemon => vec![
                Self(DAEMON_FAMILY.to_owned()),
                Self(format!("{DAEMON_FAMILY}-{pid}")),
            ],
            LogRole::Serve => vec![Self(format!("serve-{pid}"))],
            LogRole::Cli => vec![Self(format!("cli-{pid}"))],
        }
    }

    fn parse(stem: &str) -> Option<Self> {
        if stem == DAEMON_FAMILY || stem == LEGACY_FAMILY {
            return Some(Self(stem.to_owned()));
        }
        for prefix in PID_FAMILY_PREFIXES {
            let Some(pid) = stem.strip_prefix(prefix) else {
                continue;
            };
            if !is_decimal(pid) {
                return None;
            }
            return Some(Self(stem.to_owned()));
        }
        None
    }

    fn active_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.log", self.0))
    }

    fn archive_path(&self, dir: &Path, index: u64) -> PathBuf {
        dir.join(format!("{}.log.{index}", self.0))
    }

    fn lock_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.log.lock", self.0))
    }
}

fn is_decimal(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFileKind {
    Active,
    Archive(u64),
}

fn parse_log_name(name: &str) -> Option<(LogFamily, LogFileKind)> {
    let (stem, rest) = name.split_once(".log")?;
    let family = LogFamily::parse(stem)?;
    if rest.is_empty() {
        return Some((family, LogFileKind::Active));
    }
    let index = rest.strip_prefix('.')?;
    if !is_decimal(index) {
        return None;
    }
    let index = index.parse().ok()?;
    Some((family, LogFileKind::Archive(index)))
}

struct LogFile {
    path: PathBuf,
    family: LogFamily,
    kind: LogFileKind,
    bytes: u64,
    modified: SystemTime,
}

fn list_log_files(dir: &Path) -> io::Result<Vec<LogFile>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((family, kind)) = parse_log_name(name) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            continue;
        }
        files.push(LogFile {
            path: entry.path(),
            family,
            kind,
            bytes: metadata.len(),
            modified: metadata.modified()?,
        });
    }
    Ok(files)
}

/// Exclusive advisory lock on a family's `<name>.log.lock` file. Dropping it
/// removes the lock file before the lock is released.
struct FamilyLock {
    path: PathBuf,
    _file: File,
}

enum Claim {
    Acquired(FamilyLock),
    Held,
}

impl FamilyLock {
    /// One non-blocking attempt. `Held` means a live owner or another cleaner
    /// holds the lock, or the lock file changed during the attempt.
    fn try_claim(dir: &Path, family: &LogFamily) -> io::Result<Claim> {
        let path = family.lock_path(dir);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(Claim::Held),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        // A cleaner may unlink the lock file between our open and our lock.
        // A second handle on the path conflicts with our lock only when the
        // path still names the file we locked.
        let probe = match File::open(&path) {
            Ok(probe) => probe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Claim::Held),
            Err(error) => return Err(error),
        };
        match probe.try_lock() {
            Err(TryLockError::WouldBlock) => Ok(Claim::Acquired(Self { path, _file: file })),
            Ok(()) => Ok(Claim::Held),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    fn wait_for(dir: &Path, family: &LogFamily) -> io::Result<Option<Self>> {
        for _ in 0..LOCK_ATTEMPTS {
            match Self::try_claim(dir, family)? {
                Claim::Acquired(lock) => return Ok(Some(lock)),
                Claim::Held => std::thread::sleep(LOCK_RETRY_DELAY),
            }
        }
        Ok(None)
    }
}

impl Drop for FamilyLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum Owner {
    Claimed { _lock: FamilyLock },
    Live,
}

/// Delete log files oldest-first until all of them fit in
/// `max_total_bytes - max_file_bytes`, which leaves room for one active file.
/// Only files of `own`'s archives and of families whose lock can be claimed
/// are deleted.
fn prune(
    dir: &Path,
    own: Option<&LogFamily>,
    config: &hallouminate_config::LoggingConfig,
) -> io::Result<()> {
    let budget = config.max_total_bytes.saturating_sub(config.max_file_bytes);
    let mut files = list_log_files(dir)?;
    let mut total: u64 = 0;
    for file in &files {
        total = total.saturating_add(file.bytes);
    }
    if total <= budget {
        return Ok(());
    }
    files.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut owners = BTreeMap::new();
    for file in files {
        if total <= budget {
            break;
        }
        if !is_removable(dir, own, &file, &mut owners) {
            continue;
        }
        match std::fs::remove_file(&file.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        total = total.saturating_sub(file.bytes);
    }
    Ok(())
}

fn is_removable(
    dir: &Path,
    own: Option<&LogFamily>,
    file: &LogFile,
    owners: &mut BTreeMap<LogFamily, Owner>,
) -> bool {
    if own == Some(&file.family) {
        return match file.kind {
            LogFileKind::Active => false,
            LogFileKind::Archive(_) => true,
        };
    }
    let owner = match owners.entry(file.family.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let owner = match FamilyLock::try_claim(dir, &file.family) {
                Ok(Claim::Acquired(lock)) => Owner::Claimed { _lock: lock },
                Ok(Claim::Held) => Owner::Live,
                Err(_) => Owner::Live,
            };
            entry.insert(owner)
        }
    };
    match owner {
        Owner::Claimed { _lock: _ } => true,
        Owner::Live => false,
    }
}

enum Slot {
    Unopened,
    Open(ActiveLog),
    Closed,
}

fn lock_slot(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct LogSink {
    dir: PathBuf,
    role: LogRole,
    pid: u32,
    config: hallouminate_config::LoggingConfig,
    slot: Arc<Mutex<Slot>>,
}

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut slot = lock_slot(&self.slot);
        match *slot {
            Slot::Unopened => *slot = Slot::Open(ActiveLog::open(&self.dir, self.role, self.pid)?),
            Slot::Open(_) => {}
            Slot::Closed => {}
        }
        let active = match &mut *slot {
            Slot::Open(active) => active,
            Slot::Unopened => return Err(io::Error::other("log file is not open")),
            Slot::Closed => return Err(io::Error::other("log file is closed")),
        };
        active.write_record(&self.dir, &self.config, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *lock_slot(&self.slot) {
            Slot::Open(active) => active.file.flush(),
            Slot::Unopened => Ok(()),
            Slot::Closed => Ok(()),
        }
    }
}

struct ActiveLog {
    family: LogFamily,
    file: File,
    len: u64,
    next_archive: u64,
    _lock: FamilyLock,
}

impl ActiveLog {
    fn open(dir: &Path, role: LogRole, pid: u32) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        for family in LogFamily::candidates(role, pid) {
            let Some(lock) = FamilyLock::wait_for(dir, &family)? else {
                continue;
            };
            let file = open_append(&family.active_path(dir))?;
            let len = file.metadata()?.len();
            let mut next_archive = 1;
            for existing in list_log_files(dir)? {
                let LogFile {
                    family: existing_family,
                    kind,
                    ..
                } = existing;
                let LogFileKind::Archive(index) = kind else {
                    continue;
                };
                if existing_family == family && index >= next_archive {
                    next_archive = index.saturating_add(1);
                }
            }
            return Ok(Self {
                family,
                file,
                len,
                next_archive,
                _lock: lock,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another process holds every log file this process may use",
        ))
    }

    fn write_record(
        &mut self,
        dir: &Path,
        config: &hallouminate_config::LoggingConfig,
        buf: &[u8],
    ) -> io::Result<()> {
        let incoming = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.len > 0 && self.len.saturating_add(incoming) > config.max_file_bytes {
            self.rotate(dir, config)?;
        }
        self.file.write_all(buf)?;
        self.len = self.len.saturating_add(incoming);
        Ok(())
    }

    fn rotate(
        &mut self,
        dir: &Path,
        config: &hallouminate_config::LoggingConfig,
    ) -> io::Result<()> {
        let active = self.family.active_path(dir);
        let archive = self.family.archive_path(dir, self.next_archive);
        match std::fs::rename(&active, &archive) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.next_archive = self.next_archive.saturating_add(1);
        self.file = open_append(&active)?;
        self.len = 0;
        prune(dir, Some(&self.family), config)
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn reap_legacy_dated_logs(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        reap_legacy_dated_log(entry)?;
    }
    Ok(())
}

fn reap_legacy_dated_log(entry: std::fs::DirEntry) -> std::io::Result<()> {
    let file_type = match entry.file_type() {
        Ok(file_type) => file_type,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !file_type.is_file() {
        return Ok(());
    }
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(());
    };
    let Some(date) = name.strip_prefix(LEGACY_LOG_PREFIX) else {
        return Ok(());
    };
    let Some(date) = date.strip_suffix(LEGACY_LOG_SUFFIX) else {
        return Ok(());
    };
    if date.len() != 10 || NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
        return Ok(());
    }
    match std::fs::remove_file(entry.path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn state_dir() -> PathBuf {
    hallouminate_config::xdg::xdg_path("XDG_STATE_HOME", "~/.local/state", &["hallouminate"])
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    use hallouminate_config::LoggingConfig;
    use tracing_subscriber::fmt::MakeWriter;

    fn regular_files(dir: &Path) -> anyhow::Result<Vec<(PathBuf, Vec<u8>)>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let path = entry.path();
                let bytes = std::fs::read(&path)?;
                files.push((path, bytes));
            }
        }
        Ok(files)
    }

    /// Lines of one family's archives in index order, then its active file.
    fn family_lines(dir: &Path, family: &str) -> anyhow::Result<(usize, Vec<String>)> {
        let mut archives = BTreeMap::new();
        let mut active = None;
        for file in list_log_files(dir)? {
            let LogFile {
                path,
                family: LogFamily(name),
                kind,
                ..
            } = file;
            if name != family {
                continue;
            }
            match kind {
                LogFileKind::Archive(index) => {
                    archives.insert(index, path);
                }
                LogFileKind::Active => active = Some(path),
            }
        }
        let mut paths = Vec::new();
        for (_, path) in archives {
            paths.push(path);
        }
        if let Some(path) = active {
            paths.push(path);
        }
        let mut lines = Vec::new();
        for path in &paths {
            for line in std::fs::read_to_string(path)?.lines() {
                lines.push(line.to_owned());
            }
        }
        Ok((paths.len(), lines))
    }

    #[test]
    fn state_dir_ends_under_hallouminate_subdir() {
        let dir = state_dir();
        assert!(
            dir.ends_with("hallouminate"),
            "state dir must terminate in the app subdir: {dir:?}"
        );
        assert!(dir.is_absolute(), "state dir must be absolute: {dir:?}");
    }

    #[test]
    fn log_names_match_only_hallouminate_process_logs() {
        let family = |name: &str| LogFamily(name.to_owned());
        assert_eq!(
            parse_log_name("daemon.log"),
            Some((family("daemon"), LogFileKind::Active))
        );
        assert_eq!(
            parse_log_name("serve-42.log.7"),
            Some((family("serve-42"), LogFileKind::Archive(7)))
        );
        assert_eq!(
            parse_log_name("hallouminate.log.3"),
            Some((family("hallouminate"), LogFileKind::Archive(3)))
        );
        assert_eq!(
            parse_log_name("cli-9.log"),
            Some((family("cli-9"), LogFileKind::Active))
        );
        for name in [
            "daemon-bootstrap.log",
            "daemon.log.lock",
            "serve-.log",
            "cli-12x.log",
            "serve-1.log.+1",
            "hallouminate.2026-07-04.log",
            "unrelated.log",
        ] {
            assert_eq!(parse_log_name(name), None, "{name} must not parse");
        }
    }

    #[test]
    fn log_writer_removes_only_legacy_dated_logs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = [
            dir.path().join("hallouminate.2026-07-04.log"),
            dir.path().join("hallouminate.2026-07-12.log"),
        ];
        let preserved = [
            dir.path().join("hallouminate.2026-02-30.log"),
            dir.path().join("hallouminate.not-a-date.log"),
            dir.path().join("unrelated.log"),
        ];
        for path in &legacy {
            std::fs::write(path, b"log bytes")?;
        }
        for path in &preserved {
            std::fs::write(path, b"log bytes")?;
        }
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 192,
        };

        let (_writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        drop(guard);

        for path in legacy {
            assert!(!path.exists(), "legacy dated log survived: {path:?}");
        }
        for path in preserved {
            assert!(path.exists(), "non-legacy file was removed: {path:?}");
        }
        Ok(())
    }

    #[test]
    fn reaping_a_legacy_entry_already_removed_by_another_startup() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("hallouminate.2026-07-04.log");
        std::fs::write(&path, b"log bytes")?;
        let entry = std::fs::read_dir(dir.path())?
            .next()
            .expect("legacy entry")?;
        std::fs::remove_file(path)?;

        reap_legacy_dated_log(entry)?;

        Ok(())
    }

    #[test]
    fn writers_of_two_processes_keep_their_own_lines() -> anyhow::Result<()> {
        const LINES: usize = 60;
        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1 << 20,
        };
        let (writer_a, guard_a) = log_writer(dir.path(), LogRole::Serve, 1001, &config)?;
        let (writer_b, guard_b) = log_writer(dir.path(), LogRole::Serve, 1002, &config)?;
        let mut writer_a = writer_a.make_writer();
        let mut writer_b = writer_b.make_writer();
        let mut expected_a = Vec::new();
        let mut expected_b = Vec::new();
        for index in 0..LINES {
            let line_a = format!("a-{index:04}");
            let line_b = format!("b-{index:04}");
            writer_a.write_all(format!("{line_a}\n").as_bytes())?;
            writer_b.write_all(format!("{line_b}\n").as_bytes())?;
            expected_a.push(line_a);
            expected_b.push(line_b);
        }
        drop(writer_a);
        drop(writer_b);
        drop(guard_a);
        drop(guard_b);

        let (files_a, lines_a) = family_lines(dir.path(), "serve-1001")?;
        let (files_b, lines_b) = family_lines(dir.path(), "serve-1002")?;
        assert!(files_a > 1, "writer a must rotate, found {files_a} files");
        assert!(files_b > 1, "writer b must rotate, found {files_b} files");
        assert_eq!(
            lines_a, expected_a,
            "serve-1001 files hold exactly its lines, in order"
        );
        assert_eq!(
            lines_b, expected_b,
            "serve-1002 files hold exactly its lines, in order"
        );
        assert!(
            !dir.path().join("hallouminate.log").exists(),
            "no process may write the shared legacy log"
        );
        Ok(())
    }

    #[test]
    fn total_cap_prunes_dead_owners_oldest_first_and_keeps_live_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let live = LogFamily("serve-2".to_owned());
        let Claim::Acquired(live_lock) = FamilyLock::try_claim(dir.path(), &live)? else {
            panic!("test must hold the live family's lock");
        };
        let files = [
            ("serve-2.log", 1),
            ("hallouminate.log.2", 2),
            ("serve-1.log.1", 3),
            ("serve-1.log", 4),
            ("cli-3.log", 5),
        ];
        for (name, age) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, [b'x'; 100])?;
            let file = File::options().write(true).open(&path)?;
            file.set_modified(UNIX_EPOCH + Duration::from_secs(age * 1000))?;
        }
        let config = LoggingConfig {
            max_file_bytes: 100,
            max_total_bytes: 300,
        };

        let (_writer, guard) = log_writer(dir.path(), LogRole::Cli, 99, &config)?;
        drop(guard);

        for kept in ["serve-2.log", "cli-3.log", "serve-2.log.lock"] {
            assert!(
                dir.path().join(kept).exists(),
                "{kept} must survive pruning"
            );
        }
        for pruned in [
            "hallouminate.log.2",
            "serve-1.log.1",
            "serve-1.log",
            "serve-1.log.lock",
            "hallouminate.log.lock",
            "cli-3.log.lock",
        ] {
            assert!(!dir.path().join(pruned).exists(), "{pruned} must be pruned");
        }
        drop(live_lock);
        Ok(())
    }

    #[test]
    fn cli_writer_without_records_leaves_no_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 192,
        };

        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 77, &config)?;
        drop(writer);
        drop(guard);

        let files = regular_files(dir.path())?;
        assert!(files.is_empty(), "a silent command left files: {files:?}");
        Ok(())
    }

    #[test]
    fn second_daemon_writes_a_pid_file_when_daemon_log_is_locked() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let daemon = LogFamily(DAEMON_FAMILY.to_owned());
        let Claim::Acquired(daemon_lock) = FamilyLock::try_claim(dir.path(), &daemon)? else {
            panic!("test must hold the daemon family's lock");
        };
        let config = LoggingConfig {
            max_file_bytes: 1024,
            max_total_bytes: 4096,
        };

        let (writer, guard) = log_writer(dir.path(), LogRole::Daemon, 555, &config)?;
        writer.make_writer().write_all(b"second daemon\n")?;
        drop(writer);
        drop(guard);

        let fallback = std::fs::read_to_string(dir.path().join("daemon-555.log"))?;
        assert_eq!(fallback, "second daemon\n");
        assert!(
            !dir.path().join("daemon.log").exists(),
            "locked daemon.log gained a writer"
        );
        drop(daemon_lock);
        Ok(())
    }

    #[test]
    fn panic_after_init_is_recorded_in_the_log_file() -> anyhow::Result<()> {
        const PAYLOAD: &str = "panic-probe-payload-7c1e";
        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 1 << 20,
            max_total_bytes: 4 << 20,
        };

        let guard = install(dir.path(), LogRole::Cli, 4242, &config)?;
        let joined = std::thread::Builder::new()
            .name("panic-probe".to_owned())
            .spawn(|| panic!("{PAYLOAD}"))?
            .join();
        assert!(joined.is_err(), "probe thread must panic");
        drop(guard);

        let log = std::fs::read_to_string(dir.path().join("cli-4242.log"))?;
        assert!(
            log.contains("thread 'panic-probe' panicked at"),
            "panic line missing from log:\n{log}"
        );
        assert!(
            log.contains(PAYLOAD),
            "panic payload missing from log:\n{log}"
        );
        assert!(
            log.contains("hallouminate::panic"),
            "panic target missing:\n{log}"
        );
        Ok(())
    }

    #[test]
    fn size_rotation_bounds_active_and_total_bytes() -> anyhow::Result<()> {
        const MAX_FILE_BYTES: u64 = 64;
        const MAX_TOTAL_BYTES: u64 = 192;
        const BLOCK: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
        const SENTINEL: &[u8] = b"drain-complete\n";

        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        let mut writer = writer.make_writer();
        for _ in 0..10 {
            writer.write_all(BLOCK)?;
        }
        writer.write_all(SENTINEL)?;
        drop(writer);
        drop(guard);

        let files = regular_files(dir.path())?;
        assert!(!files.is_empty(), "writer must create a log file");

        let mut active_index = None;
        for (index, (_, bytes)) in files.iter().enumerate() {
            let mut contains_sentinel = false;
            for window in bytes.windows(SENTINEL.len()) {
                if window == SENTINEL {
                    contains_sentinel = true;
                    break;
                }
            }
            if contains_sentinel {
                assert!(
                    active_index.is_none(),
                    "sentinel must occur in one log file"
                );
                active_index = Some(index);
            }
        }
        let Some(active_index) = active_index else {
            panic!("final sentinel must be drained before the guard returns");
        };

        let active_size = u64::try_from(files[active_index].1.len())?;
        assert!(
            active_size <= MAX_FILE_BYTES,
            "active log is {active_size} bytes, above {MAX_FILE_BYTES}"
        );

        let mut rotated_files = 0;
        let mut total_bytes = 0;
        for (index, (path, bytes)) in files.iter().enumerate() {
            let size = u64::try_from(bytes.len())?;
            total_bytes += size;
            if index != active_index {
                rotated_files += 1;
                assert!(
                    size <= MAX_FILE_BYTES,
                    "rotated log {path:?} is {size} bytes, above {MAX_FILE_BYTES}"
                );
            }
        }
        assert!(rotated_files > 0, "fixed writes must trigger size rotation");
        assert!(
            total_bytes <= MAX_TOTAL_BYTES,
            "retained logs total {total_bytes} bytes, above {MAX_TOTAL_BYTES}"
        );

        Ok(())
    }

    /// With `max_total_bytes == max_file_bytes`, no archives are kept: only
    /// the active file, itself still capped at `max_file_bytes`, and total
    /// bytes on disk never exceed the configured limit.
    #[test]
    fn size_rotation_with_equal_limits_keeps_no_archives() -> anyhow::Result<()> {
        const MAX_FILE_BYTES: u64 = 64;
        const BLOCK: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_FILE_BYTES,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        let mut writer = writer.make_writer();
        for _ in 0..10 {
            writer.write_all(BLOCK)?;
        }
        drop(writer);
        drop(guard);

        let files = regular_files(dir.path())?;
        let mut total_bytes: u64 = 0;
        for (_, bytes) in &files {
            total_bytes += u64::try_from(bytes.len())?;
        }
        assert_eq!(
            files.len(),
            1,
            "equal limits must retain only the active file"
        );
        assert!(
            total_bytes <= MAX_FILE_BYTES,
            "retained logs total {total_bytes} bytes, above {MAX_FILE_BYTES}"
        );

        Ok(())
    }
}
