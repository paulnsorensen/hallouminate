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
//! Oversized records are dropped. New log files are owner-only on Unix.
//! `max_total_bytes` bounds all hallouminate log files in the directory
//! together, including legacy `hallouminate.log` files. At startup and after
//! each rotation, oversized removable archives go first, then other eligible
//! files oldest-first. A stable per-directory lock serializes retention passes.
//! Another live process's files are never deleted.
//!
//! Each writer holds an exclusive advisory lock on `<name>.log.lock` while it
//! runs. A cleaner that takes this lock without blocking has proof that the
//! owner is dead, so a reused pid cannot make a live file look dead. The file
//! and its lock appear on the first record, so a command that logs nothing
//! leaves no file. A second daemon that finds `daemon.log` locked writes
//! `daemon-<pid>.log` instead and switches to `daemon.log` at a later rotation
//! once the lock frees.
//!
//! After the subscriber installs, a panic hook records every panic as an
//! `ERROR` event with target `hallouminate::panic`, then runs the previous
//! hook so stderr output stays.
//!
//! The returned [`LogGuard`] must be held while logging is active. Dropping it
//! attempts a bounded flush, then closes the file and releases its lock.
//!
//! Filter precedence: `HALLOUMINATE_LOG` env var → `RUST_LOG` → default
//! `hallouminate=info`. Use the same syntax as `RUST_LOG`
//! (`hallouminate=debug,lance=warn`).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

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
const CLI_FAMILY_PREFIX: &str = "cli-";
const MAX_CLI_FAMILIES: usize = 32;
const OPEN_RETRY_DELAY: Duration = Duration::from_secs(5);
const RETENTION_LOCK: &str = ".log-retention";

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
/// Dropping the guard attempts a bounded flush of queued records, then closes
/// the log file and releases its owner lock.
pub struct LogGuard {
    worker: Option<WorkerGuard>,
    slot: Arc<Mutex<Slot>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        drop(self.worker.take());
        match self.slot.try_lock() {
            Ok(mut slot) => *slot = Slot::Closed,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                *poisoned.into_inner() = Slot::Closed;
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
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

/// Record every panic as an `ERROR` event with target `hallouminate::panic`,
/// then run the previous hook so stderr output stays.
///
/// The event passes through the subscriber filter. A `HALLOUMINATE_LOG` (or
/// `RUST_LOG`) filter without a `hallouminate` directive filters it out. The
/// backtrace is empty unless `RUST_BACKTRACE` enables it.
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
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!(
            target: "hallouminate::panic",
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
    if let Err(error) = reap_legacy_dated_logs(dir) {
        report_once("remove legacy log files", &error);
    }
    if let Err(error) = prune(dir, None, config) {
        report_once("prune log files", &error);
    }
    let slot = Arc::new(Mutex::new(Slot::Unopened));
    let sink = LogSink {
        dir: dir.to_path_buf(),
        role,
        pid,
        config: *config,
        slot: Arc::clone(&slot),
    };
    // Stay lossy so a full log buffer does not block a tokio worker.
    // WorkerGuard attempts a bounded flush when LogGuard drops.
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
    list_log_files_with(dir, |_| {})
}

fn list_log_files_with(
    dir: &Path,
    mut before_stat: impl FnMut(&LogFamily),
) -> io::Result<Vec<LogFile>> {
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
        before_stat(&family);
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            continue;
        }
        files.push(LogFile {
            path,
            family,
            kind,
            bytes: metadata.len(),
            modified: metadata.modified()?,
        });
    }
    Ok(files)
}

static OWNED_LOCKS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static REPORTED: AtomicBool = AtomicBool::new(false);

fn owned_locks() -> MutexGuard<'static, BTreeSet<PathBuf>> {
    match OWNED_LOCKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Print the first logging problem of this process on stderr and stay silent
/// about later ones.
fn report_once(context: &str, error: &io::Error) {
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!("hallouminate: {context}: {error}");
}

fn keep_first(slot: &mut Option<io::Error>, error: io::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[cfg(unix)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &Metadata, _b: &Metadata) -> bool {
    true
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
    /// holds the lock, this process already owns it, or the lock file changed
    /// during the attempt.
    ///
    /// A family this process owns is never probed: on filesystems with
    /// per-process POSIX lock emulation, opening and closing a second handle
    /// would drop our own lock.
    fn try_claim(dir: &Path, family: &LogFamily) -> io::Result<Claim> {
        let path = family.lock_path(dir);
        if owned_locks().contains(&path) {
            return Ok(Claim::Held);
        }
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
        // The lock counts only when the path still names the locked file.
        let locked = file.metadata()?;
        let current = match std::fs::metadata(&path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Claim::Held),
            Err(error) => return Err(error),
        };
        if !same_file(&locked, &current) {
            return Ok(Claim::Held);
        }
        if !owned_locks().insert(path.clone()) {
            return Ok(Claim::Held);
        }
        Ok(Claim::Acquired(Self { path, _file: file }))
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
        let _ = owned_locks().remove(&self.path);
    }
}

enum Owner {
    Claimed { _lock: FamilyLock },
    Live,
}

fn remove_log_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Delete log files oldest-first until all of them fit in
/// `max_total_bytes - max_file_bytes`, which leaves room for one active file.
/// Only files of `own`'s archives and of families whose lock can be claimed
/// are deleted. Without `own` (startup), it also keeps only the newest
/// `MAX_CLI_FAMILIES` dead `cli-*` families and removes orphan lock files.
/// A failed removal does not stop the pass; the first error is returned.
fn prune(
    dir: &Path,
    own: Option<&LogFamily>,
    config: &hallouminate_config::LoggingConfig,
) -> io::Result<()> {
    if list_log_files(dir)?.is_empty() && list_lock_families(dir)?.is_empty() {
        return Ok(());
    }
    let retention = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(RETENTION_LOCK))?;
    retention.lock()?;

    let budget = config.max_total_bytes.saturating_sub(config.max_file_bytes);
    let mut owners = BTreeMap::new();
    let files = list_log_files_with(dir, |family| {
        if own != Some(family) {
            claim_owner(dir, family, &mut owners);
        }
    })?;
    let mut first_error = None;
    let mut eligible = Vec::new();
    for file in files {
        let archive = match file.kind {
            LogFileKind::Archive(_) => true,
            LogFileKind::Active => false,
        };
        let oversized_archive =
            file.bytes > config.max_file_bytes && archive && is_removable(own, &file, &owners);
        if oversized_archive {
            if let Err(error) = remove_log_file(&file.path) {
                keep_first(&mut first_error, error);
                eligible.push(file);
            }
        } else {
            eligible.push(file);
        }
    }
    let mut total: u64 = 0;
    for file in &eligible {
        total = total.saturating_add(file.bytes);
    }
    eligible.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut remaining = Vec::new();
    for file in eligible {
        if total <= budget || !is_removable(own, &file, &owners) {
            remaining.push(file);
            continue;
        }
        match remove_log_file(&file.path) {
            Ok(()) => total = total.saturating_sub(file.bytes),
            Err(error) => {
                keep_first(&mut first_error, error);
                remaining.push(file);
            }
        }
    }
    if own.is_none() {
        prune_startup_extras(dir, &remaining, &mut owners, &mut first_error)?;
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn prune_startup_extras(
    dir: &Path,
    remaining: &[LogFile],
    owners: &mut BTreeMap<LogFamily, Owner>,
    first_error: &mut Option<io::Error>,
) -> io::Result<()> {
    let mut newest: BTreeMap<LogFamily, SystemTime> = BTreeMap::new();
    for file in remaining {
        if !file.family.0.starts_with(CLI_FAMILY_PREFIX) {
            continue;
        }
        let modified = newest.entry(file.family.clone()).or_insert(file.modified);
        if file.modified > *modified {
            *modified = file.modified;
        }
    }
    let mut dead = Vec::new();
    for (family, modified) in newest {
        if owner_is_dead(&family, owners) {
            dead.push((modified, family));
        }
    }
    dead.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, family) in dead.into_iter().skip(MAX_CLI_FAMILIES) {
        for file in remaining {
            if file.family != family {
                continue;
            }
            if let Err(error) = remove_log_file(&file.path) {
                keep_first(first_error, error);
            }
        }
    }
    for family in list_lock_families(dir)? {
        claim_owner(dir, &family, owners);
    }
    Ok(())
}

fn list_lock_families(dir: &Path) -> io::Result<Vec<LogFamily>> {
    let mut families = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".log.lock") else {
            continue;
        };
        if let Some(family) = LogFamily::parse(stem) {
            families.push(family);
        }
    }
    Ok(families)
}

fn is_removable(
    own: Option<&LogFamily>,
    file: &LogFile,
    owners: &BTreeMap<LogFamily, Owner>,
) -> bool {
    if own == Some(&file.family) {
        return match file.kind {
            LogFileKind::Active => false,
            LogFileKind::Archive(_) => true,
        };
    }
    owner_is_dead(&file.family, owners)
}

/// Claim a lock before reading file metadata. Keep live families out of
/// this pass and hold each claimed lock until all removals finish.
fn claim_owner(dir: &Path, family: &LogFamily, owners: &mut BTreeMap<LogFamily, Owner>) {
    if let Entry::Vacant(entry) = owners.entry(family.clone()) {
        let owner = match FamilyLock::try_claim(dir, family) {
            Ok(Claim::Acquired(lock)) => Owner::Claimed { _lock: lock },
            Ok(Claim::Held) | Err(_) => Owner::Live,
        };
        entry.insert(owner);
    }
}

fn owner_is_dead(family: &LogFamily, owners: &BTreeMap<LogFamily, Owner>) -> bool {
    match owners.get(family) {
        Some(Owner::Claimed { _lock: _ }) => true,
        Some(Owner::Live) | None => false,
    }
}

enum Slot {
    Unopened,
    Open(ActiveLog),
    Failed { retry_at: Instant },
    Closed,
}

fn lock_slot(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_stderr(buf: &[u8]) -> io::Result<usize> {
    io::stderr().write_all(buf)?;
    Ok(buf.len())
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
        if u64::try_from(buf.len()).unwrap_or(u64::MAX) > self.config.max_file_bytes {
            return Ok(buf.len());
        }
        let mut slot = lock_slot(&self.slot);
        match *slot {
            Slot::Failed { retry_at } if Instant::now() < retry_at => {
                drop(slot);
                return write_stderr(buf);
            }
            Slot::Failed { retry_at: _ } => *slot = Slot::Unopened,
            Slot::Unopened | Slot::Open(_) | Slot::Closed => {}
        }
        match *slot {
            Slot::Unopened => match ActiveLog::open(&self.dir, self.role, self.pid) {
                Ok(active) => *slot = Slot::Open(active),
                Err(error) => {
                    *slot = Slot::Failed {
                        retry_at: Instant::now() + OPEN_RETRY_DELAY,
                    };
                    drop(slot);
                    report_once("open log file", &error);
                    return write_stderr(buf);
                }
            },
            Slot::Failed { retry_at: _ } | Slot::Open(_) | Slot::Closed => {}
        }
        let active = match &mut *slot {
            Slot::Open(active) => active,
            Slot::Unopened | Slot::Failed { retry_at: _ } => {
                return Err(io::Error::other("log file is not open"));
            }
            Slot::Closed => return Err(io::Error::other("log file is closed")),
        };
        let record = active.write_record(&self.dir, &self.config, buf);
        if let Err(error) = record {
            *slot = Slot::Failed {
                retry_at: Instant::now() + OPEN_RETRY_DELAY,
            };
            drop(slot);
            report_once("write log file", &error);
            return write_stderr(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *lock_slot(&self.slot) {
            Slot::Open(active) => active.file.flush(),
            Slot::Unopened | Slot::Failed { retry_at: _ } | Slot::Closed => Ok(()),
        }
    }
}

struct ActiveLog {
    family: LogFamily,
    /// The preferred family when `family` is a fallback for it.
    promotable: Option<LogFamily>,
    file: File,
    len: u64,
    next_archive: u64,
    _lock: FamilyLock,
}

fn next_archive_index(dir: &Path, family: &LogFamily) -> io::Result<u64> {
    let mut next = 1;
    for existing in list_log_files(dir)? {
        let LogFile {
            family: existing_family,
            kind,
            ..
        } = existing;
        let LogFileKind::Archive(index) = kind else {
            continue;
        };
        if existing_family == *family && index >= next {
            next = index.saturating_add(1);
        }
    }
    Ok(next)
}

impl ActiveLog {
    fn open(dir: &Path, role: LogRole, pid: u32) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let candidates = LogFamily::candidates(role, pid);
        let primary = candidates.first().cloned();
        for (index, family) in candidates.into_iter().enumerate() {
            let Some(lock) = FamilyLock::wait_for(dir, &family)? else {
                continue;
            };
            let file = open_append(&family.active_path(dir))?;
            let len = file.metadata()?.len();
            let next_archive = next_archive_index(dir, &family)?;
            let promotable = if index == 0 { None } else { primary };
            return Ok(Self {
                family,
                promotable,
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

    fn is_full(&self, incoming: u64, config: &hallouminate_config::LoggingConfig) -> bool {
        self.len > 0 && self.len.saturating_add(incoming) > config.max_file_bytes
    }

    fn write_record(
        &mut self,
        dir: &Path,
        config: &hallouminate_config::LoggingConfig,
        buf: &[u8],
    ) -> io::Result<()> {
        let incoming = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.is_full(incoming, config) {
            self.try_promote(dir);
        }
        let rotated = self.is_full(incoming, config);
        if rotated && let Err(error) = self.rotate(dir) {
            report_once("reopen log file", &error);
            return Err(error);
        }
        if rotated && let Err(error) = prune(dir, Some(&self.family), config) {
            report_once("prune log files", &error);
        }
        self.file.write_all(buf)?;
        self.len = self.len.saturating_add(incoming);
        Ok(())
    }

    /// Switch from a fallback family to the preferred one when its owner is gone.
    fn try_promote(&mut self, dir: &Path) {
        let Some(primary) = self.promotable.clone() else {
            return;
        };
        let Ok(Claim::Acquired(lock)) = FamilyLock::try_claim(dir, &primary) else {
            return;
        };
        let Ok(file) = open_append(&primary.active_path(dir)) else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };
        let Ok(next_archive) = next_archive_index(dir, &primary) else {
            return;
        };
        self.family = primary;
        self.promotable = None;
        self.file = file;
        self.len = metadata.len();
        self.next_archive = next_archive;
        self._lock = lock;
    }

    fn rotate(&mut self, dir: &Path) -> io::Result<()> {
        let active = self.family.active_path(dir);
        let archive = self.family.archive_path(dir, self.next_archive);
        match std::fs::rename(&active, &archive) {
            Ok(()) => {
                self.next_archive = self.next_archive.saturating_add(1);
                self.file = open_append(&active)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.file = open_append(&active)?;
            }
            Err(error) => {
                report_once("rotate log file", &error);
                self.file.set_len(0)?;
            }
        }
        self.len = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn reap_legacy_dated_logs(dir: &Path) -> std::io::Result<()> {
    let mut first_error = None;
    for entry in std::fs::read_dir(dir)? {
        if let Err(error) = entry.and_then(reap_legacy_dated_log) {
            keep_first(&mut first_error, error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
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
    hallouminate_config::xdg::state_dir()
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
            if entry.file_type()?.is_file() && entry.file_name().to_str() != Some(RETENTION_LOCK) {
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
    fn rotation_retains_a_full_archive_and_active_file_at_the_budget() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 128,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        let mut writer = writer.make_writer();
        let first = "a".repeat(63);
        let second = "b".repeat(63);
        writer.write_all(format!("{first}\n").as_bytes())?;
        writer.write_all(format!("{second}\n").as_bytes())?;
        drop(writer);
        drop(guard);
        assert_eq!(family_lines(dir.path(), "cli-1")?, (2, vec![first, second]));
        Ok(())
    }

    #[test]
    fn oversized_record_does_not_enter_the_log_or_close_the_sink() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 128,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        let mut writer = writer.make_writer();
        writer.write_all(b"before\n")?;
        writer.write_all(format!("{}\n", "x".repeat(64)).as_bytes())?;
        writer.write_all(b"after\n")?;
        drop(writer);
        drop(guard);
        let (files, lines) = family_lines(dir.path(), "cli-1")?;
        assert_eq!(files, 1);
        assert_eq!(lines, ["before", "after"]);
        Ok(())
    }

    #[test]
    fn lowered_file_limit_removes_oversized_dead_archives() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let archive = dir.path().join("cli-1.log.1");
        std::fs::write(&archive, [b'x'; 128])?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1024,
        };
        prune(dir.path(), None, &config)?;
        assert!(
            !archive.exists(),
            "oversized dead archive survived the lower cap"
        );
        Ok(())
    }

    #[test]
    fn oversized_archive_is_removed_before_older_valid_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let valid = dir.path().join("cli-1.log");
        let oversized = dir.path().join("cli-2.log.1");
        let newer = dir.path().join("cli-3.log");
        for (path, bytes, age) in [(&valid, 60, 1), (&oversized, 128, 2), (&newer, 60, 3)] {
            std::fs::write(path, vec![b'x'; bytes])?;
            File::options()
                .write(true)
                .open(path)?
                .set_modified(UNIX_EPOCH + Duration::from_secs(age))?;
        }
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 184,
        };
        prune(dir.path(), None, &config)?;
        assert!(!oversized.exists());
        assert!(valid.exists(), "valid oldest file was over-deleted");
        assert!(newer.exists(), "valid newest file was over-deleted");
        Ok(())
    }

    #[test]
    fn lowered_limit_preserves_another_live_family_archive() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let family = LogFamily("serve-7".to_owned());
        let Claim::Acquired(lock) = FamilyLock::try_claim(dir.path(), &family)? else {
            panic!("live family lock was not acquired");
        };
        let archive = family.archive_path(dir.path(), 1);
        std::fs::write(&archive, [b'x'; 128])?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1024,
        };
        prune(dir.path(), None, &config)?;
        assert!(archive.exists(), "another live archive was removed");
        drop(lock);
        Ok(())
    }

    #[test]
    fn lowered_limit_rotates_existing_oversized_active_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let active = dir.path().join("cli-1.log");
        std::fs::write(&active, [b'x'; 128])?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1024,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        writer.make_writer().write_all(b"valid\n")?;
        drop(writer);
        drop(guard);
        assert_eq!(std::fs::read_to_string(&active)?, "valid\n");
        assert!(!dir.path().join("cli-1.log.1").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn new_process_log_is_owner_only() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 128,
        };
        let (writer, guard) = log_writer(dir.path(), LogRole::Cli, 1, &config)?;
        writer.make_writer().write_all(b"private\n")?;
        drop(writer);
        drop(guard);
        let mode = std::fs::metadata(dir.path().join("cli-1.log"))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "process log permits group or other access");
        Ok(())
    }

    #[test]
    fn log_scan_reads_fresh_metadata_after_the_family_claim() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cli-1.log");
        std::fs::write(&path, b"old")?;
        let mut owners = BTreeMap::new();
        let files = list_log_files_with(dir.path(), |family| {
            claim_owner(dir.path(), family, &mut owners);
            assert!(owner_is_dead(family, &owners));
            std::fs::write(&path, b"replacement").expect("write replacement log");
        })?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].bytes, 11);
        Ok(())
    }

    #[test]
    fn rotate_keeps_cap_when_archive_rename_fails() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut active = ActiveLog::open(dir.path(), LogRole::Cli, 1)?;
        std::fs::create_dir(active.family.archive_path(dir.path(), active.next_archive))?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1 << 20,
        };
        let record = [b'x'; 40];
        for _ in 0..10 {
            active.write_record(dir.path(), &config, &record)?;
        }
        let len = std::fs::metadata(active.family.active_path(dir.path()))?.len();
        assert!(
            len <= config.max_file_bytes,
            "active file grew to {len} bytes past the {} byte cap",
            config.max_file_bytes
        );
        Ok(())
    }

    #[test]
    fn state_dir_ends_under_hallouminate_subdir() {
        let dir = state_dir();
        assert!(
            dir.ends_with(hallouminate_config::channel::BUILD_CHANNEL.app_dir()),
            "state dir must terminate in the channel app subdir: {dir:?}"
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

    const CHILD_MODE_VAR: &str = "HALLOUMINATE_LOG_TEST_CHILD";
    const CHILD_DIR_VAR: &str = "HALLOUMINATE_LOG_TEST_DIR";
    const CHILD_LINES: usize = 150;
    const HELD_FAMILY: &str = "serve-7";

    /// Body of the child processes that the multi-process tests spawn. It does
    /// nothing in a normal run, where `CHILD_MODE_VAR` is unset.
    #[test]
    fn child_entry() -> anyhow::Result<()> {
        let Ok(mode) = std::env::var(CHILD_MODE_VAR) else {
            return Ok(());
        };
        let dir = PathBuf::from(std::env::var(CHILD_DIR_VAR)?);
        match mode.as_str() {
            "write" => {
                let config = LoggingConfig {
                    max_file_bytes: 64,
                    max_total_bytes: 1 << 20,
                };
                let pid = std::process::id();
                let (writer, guard) = log_writer(&dir, LogRole::Serve, pid, &config)?;
                let mut writer = writer.make_writer();
                for index in 0..CHILD_LINES {
                    writer.write_all(format!("p{pid}-{index:04}\n").as_bytes())?;
                }
                drop(writer);
                drop(guard);
                Ok(())
            }
            "hold" => {
                let family = LogFamily(HELD_FAMILY.to_owned());
                let Claim::Acquired(lock) = FamilyLock::try_claim(&dir, &family)? else {
                    anyhow::bail!("child could not claim the family lock");
                };
                println!("held");
                io::stdout().flush()?;
                let mut sink = String::new();
                io::stdin().read_line(&mut sink)?;
                drop(lock);
                Ok(())
            }
            "oversize_burst" => {
                let config = LoggingConfig {
                    max_file_bytes: 256,
                    max_total_bytes: 768,
                };
                let guard = install(&dir, LogRole::Daemon, std::process::id(), &config)?;
                let payload = "x".repeat(128 * 1024);
                for _ in 0..32 {
                    tracing::info!(target: "hallouminate", "{payload}");
                }
                tracing::info!(target: "hallouminate", "valid-marker");
                drop(guard);
                Ok(())
            }
            #[cfg(target_os = "linux")]
            "failed_write" => {
                std::os::unix::fs::symlink(
                    "/dev/full",
                    dir.join(format!("cli-{}.log", std::process::id())),
                )?;
                let config = LoggingConfig {
                    max_file_bytes: 2 << 20,
                    max_total_bytes: 4 << 20,
                };
                let (writer, guard) = log_writer(&dir, LogRole::Cli, std::process::id(), &config)?;
                writer.make_writer().write_all(&vec![b'x'; 1 << 20])?;
                drop(writer);
                drop(guard);
                Ok(())
            }
            "prune" => {
                std::fs::write(dir.join(format!("ready-{}", std::process::id())), b"")?;
                let config = LoggingConfig {
                    max_file_bytes: 64,
                    max_total_bytes: 192,
                };
                prune(&dir, None, &config)?;
                Ok(())
            }
            other => anyhow::bail!("unknown child mode {other}"),
        }
    }

    fn child_command(mode: &str, dir: &Path) -> anyhow::Result<std::process::Command> {
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "logging::tests::child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MODE_VAR, mode)
            .env(CHILD_DIR_VAR, dir)
            .env("HALLOUMINATE_LOG", "hallouminate=info");
        Ok(command)
    }

    struct OwnedChildren(Vec<std::process::Child>);

    impl Drop for OwnedChildren {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn wait_child(child: &mut std::process::Child) -> anyhow::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                anyhow::bail!("child did not exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn oversized_daemon_burst_does_not_fill_bootstrap_stderr() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stderr = dir.path().join("stderr-capture");
        let mut child = child_command("oversize_burst", dir.path())?
            .stdout(std::process::Stdio::null())
            .stderr(File::create(&stderr)?)
            .spawn()?;
        assert!(
            wait_child(&mut child)?.success(),
            "daemon-role child failed"
        );
        let captured = std::fs::metadata(&stderr)?.len();
        assert!(
            captured < 4096,
            "oversized payload filled stderr: {captured} bytes"
        );
        let log = std::fs::read_to_string(dir.path().join("daemon.log"))?;
        assert!(log.contains("valid-marker"), "valid event was not logged");
        assert!(
            !log.contains(&"x".repeat(128 * 1024)),
            "oversized payload entered the log"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_write_with_unread_stderr_pipe_has_bounded_shutdown() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut child = child_command("failed_write", dir.path())?
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        assert!(
            wait_child(&mut child)?.success(),
            "failed-write child did not shut down cleanly"
        );
        Ok(())
    }

    #[test]
    fn concurrent_retention_waits_for_shared_lock_and_keeps_exact_budget() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let paths = ["cli-101.log", "cli-102.log", "cli-103.log"];
        for (index, name) in paths.iter().enumerate() {
            let path = dir.path().join(name);
            std::fs::write(&path, [b'x'; 64])?;
            File::options()
                .write(true)
                .open(&path)?
                .set_modified(UNIX_EPOCH + Duration::from_secs(u64::try_from(index + 1)?))?;
        }
        let retention = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join(RETENTION_LOCK))?;
        retention.lock()?;
        let mut children = OwnedChildren(Vec::new());
        for _ in 0..2 {
            children.0.push(
                child_command("prune", dir.path())?
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?,
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for child in &mut children.0 {
            let marker = dir.path().join(format!("ready-{}", child.id()));
            while !marker.exists() {
                assert!(
                    child.try_wait()?.is_none(),
                    "pruner exited before readiness"
                );
                assert!(Instant::now() < deadline, "pruner did not reach cleanup");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        for child in &mut children.0 {
            assert!(
                child.try_wait()?.is_none(),
                "pruner ignored the retention lock"
            );
        }
        for name in paths {
            assert!(
                dir.path().join(name).exists(),
                "{name} was removed while locked"
            );
        }
        drop(retention);
        for child in &mut children.0 {
            assert!(wait_child(child)?.success(), "pruner child failed");
        }
        assert!(!dir.path().join("cli-101.log").exists());
        assert!(dir.path().join("cli-102.log").exists());
        assert!(dir.path().join("cli-103.log").exists());
        Ok(())
    }

    #[test]
    fn concurrent_writer_processes_keep_complete_ordered_files() -> anyhow::Result<()> {
        const CHILDREN: usize = 4;
        let dir = tempfile::tempdir()?;
        let mut children = Vec::new();
        for _ in 0..CHILDREN {
            let child = child_command("write", dir.path())?
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            children.push(child);
        }
        let mut pids = Vec::new();
        for mut child in children {
            pids.push(child.id());
            assert!(child.wait()?.success(), "child writer failed");
        }

        for pid in pids {
            let mut expected = Vec::new();
            for index in 0..CHILD_LINES {
                expected.push(format!("p{pid}-{index:04}"));
            }
            let (files, lines) = family_lines(dir.path(), &format!("serve-{pid}"))?;
            assert!(files > 1, "child {pid} must rotate, found {files} files");
            assert_eq!(lines, expected, "serve-{pid} files hold exactly its lines");
        }
        for (path, _) in regular_files(dir.path())? {
            let name = path.to_string_lossy();
            assert!(
                !name.ends_with(".lock"),
                "lock file outlived its owner: {name}"
            );
            assert!(
                !name.ends_with("hallouminate.log"),
                "shared log written: {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn family_lock_is_held_during_another_process_and_acquired_after_it_exits() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let mut child = child_command("hold", dir.path())?
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            anyhow::bail!("child pipes missing");
        };
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if std::io::BufRead::read_line(&mut reader, &mut line)? == 0 {
                anyhow::bail!("child exited before it held the lock");
            }
            if line.trim().ends_with("held") {
                break;
            }
        }

        let family = LogFamily(HELD_FAMILY.to_owned());
        let Claim::Held = FamilyLock::try_claim(dir.path(), &family)? else {
            panic!("a family locked by a live child process must be held");
        };
        drop(stdin);
        assert!(child.wait()?.success(), "child holder failed");
        let Claim::Acquired(lock) = FamilyLock::try_claim(dir.path(), &family)? else {
            panic!("a family whose owner exited must be acquirable");
        };
        drop(lock);
        Ok(())
    }

    #[test]
    fn claiming_a_family_this_process_owns_reports_held_without_dropping_the_lock()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let family = LogFamily("serve-8".to_owned());
        let Claim::Acquired(lock) = FamilyLock::try_claim(dir.path(), &family)? else {
            panic!("first claim must succeed");
        };
        for _ in 0..2 {
            let Claim::Held = FamilyLock::try_claim(dir.path(), &family)? else {
                panic!("an owned family must never be probed as free");
            };
        }
        assert!(family.lock_path(dir.path()).exists());
        drop(lock);
        assert!(!family.lock_path(dir.path()).exists());
        Ok(())
    }

    #[test]
    fn startup_removes_orphan_lock_files_but_keeps_live_ones() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("serve-5.log.lock"), b"")?;
        let live = LogFamily("serve-6".to_owned());
        let Claim::Acquired(live_lock) = FamilyLock::try_claim(dir.path(), &live)? else {
            panic!("test must hold the live family's lock");
        };
        let config = LoggingConfig {
            max_file_bytes: 1024,
            max_total_bytes: 1 << 20,
        };

        let (_writer, guard) = log_writer(dir.path(), LogRole::Cli, 99, &config)?;
        drop(guard);

        assert!(
            !dir.path().join("serve-5.log.lock").exists(),
            "orphan lock file must be removed"
        );
        assert!(live.lock_path(dir.path()).exists());
        drop(live_lock);
        Ok(())
    }

    #[test]
    fn startup_keeps_only_the_newest_dead_cli_families() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let total = MAX_CLI_FAMILIES + 8;
        for index in 0..total {
            let path = dir.path().join(format!("cli-{}.log", index + 100));
            std::fs::write(&path, b"x")?;
            let age = u64::try_from(index + 1)?;
            File::options()
                .write(true)
                .open(&path)?
                .set_modified(UNIX_EPOCH + Duration::from_secs(age * 1000))?;
        }
        let config = LoggingConfig {
            max_file_bytes: 1024,
            max_total_bytes: 1 << 20,
        };

        let (_writer, guard) = log_writer(dir.path(), LogRole::Cli, 99_999, &config)?;
        drop(guard);

        let mut kept = 0;
        for index in 0..total {
            let exists = dir.path().join(format!("cli-{}.log", index + 100)).exists();
            if exists {
                kept += 1;
                assert!(
                    index >= total - MAX_CLI_FAMILIES,
                    "old family {index} survived"
                );
            }
        }
        assert_eq!(kept, MAX_CLI_FAMILIES);
        Ok(())
    }

    #[test]
    fn restarted_daemon_returns_to_daemon_log_once_it_is_free() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let daemon = LogFamily(DAEMON_FAMILY.to_owned());
        let Claim::Acquired(daemon_lock) = FamilyLock::try_claim(dir.path(), &daemon)? else {
            panic!("test must hold the daemon family's lock");
        };
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 1 << 20,
        };
        let first = format!("{}\n", "1".repeat(40));
        let second = format!("{}\n", "2".repeat(40));

        let (writer, guard) = log_writer(dir.path(), LogRole::Daemon, 555, &config)?;
        let mut writer = writer.make_writer();
        writer.write_all(first.as_bytes())?;
        let fallback = dir.path().join("daemon-555.log");
        for _ in 0..500 {
            if fallback.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fallback.exists(), "fallback file never appeared");
        drop(daemon_lock);
        writer.write_all(second.as_bytes())?;
        drop(writer);
        drop(guard);

        assert_eq!(std::fs::read_to_string(&fallback)?, first);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemon.log"))?,
            second
        );
        assert!(!dir.path().join("daemon-555.log.lock").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn startup_housekeeping_failure_does_not_fail_log_writer() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("hallouminate.2026-07-04.log"), b"old")?;
        let config = LoggingConfig {
            max_file_bytes: 64,
            max_total_bytes: 192,
        };
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))?;
        let result = log_writer(dir.path(), LogRole::Cli, 1, &config);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))?;

        assert!(result.is_ok(), "housekeeping errors must not fail startup");
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
