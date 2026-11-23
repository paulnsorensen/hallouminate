use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{NaiveDate, Utc};
use hallouminate_config::LoggingConfig;

use super::OperationRecord;

const SEGMENT_PREFIX: &str = "operations-";
const SEGMENT_SUFFIX: &str = ".jsonl";
const SEGMENT_DATE_FORMAT: &str = "%Y-%m-%d";
const LOCK_NAME: &str = "operations.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    Enabled,
    Disabled,
}

enum Retention {
    AppendOnly,
    EnforceCap,
}

/// Writer for the operations log segments in one directory.
///
/// A write failure never reaches the caller; the writer logs it with
/// `tracing` and returns. Clones share the cap cache and the failure latches.
#[derive(Debug, Clone)]
pub struct OpLog {
    dir: PathBuf,
    switch: Switch,
    max_total_bytes: u64,
    shared: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    older_segments: Mutex<Option<OlderSegments>>,
    append_failure: FailureLatch,
    cap_failure: FailureLatch,
}

/// Sizes of the segments older than `current` and of the segments dated
/// after it, from the last directory scan. Only the current segment grows,
/// so the sizes stay valid until the UTC date changes.
#[derive(Debug, Clone, Copy)]
struct OlderSegments {
    current: NaiveDate,
    bytes: u64,
    newer_bytes: u64,
}

/// Keeps a repeated failure to one warning until the next success.
#[derive(Debug, Default)]
struct FailureLatch(AtomicBool);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    First,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapCheck {
    Within,
    Scan,
}

impl FailureLatch {
    fn trip(&self) -> Failure {
        match self.0.swap(true, Ordering::Relaxed) {
            false => Failure::First,
            true => Failure::Repeat,
        }
    }

    fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

struct Segment {
    date: NaiveDate,
    path: PathBuf,
    len: u64,
}

impl OpLog {
    /// Builds the writer for the Hallouminate state directory from the
    /// startup `[logging]` configuration.
    pub fn from_config(config: &LoggingConfig) -> Self {
        Self::in_dir(hallouminate_config::xdg::state_dir(), config)
    }

    /// Builds a writer for the segments in `dir`.
    pub fn in_dir(dir: impl Into<PathBuf>, config: &LoggingConfig) -> Self {
        let LoggingConfig {
            max_file_bytes: _,
            max_total_bytes: _,
            operations,
            operations_max_total_bytes,
        } = *config;
        let switch = match operations {
            true => Switch::Enabled,
            false => Switch::Disabled,
        };
        Self {
            dir: dir.into(),
            switch,
            max_total_bytes: operations_max_total_bytes,
            shared: Arc::default(),
        }
    }

    /// Appends `record` as one line to the segment for the current UTC date.
    ///
    /// Does nothing when the log is disabled. Never deletes a segment.
    pub fn append(&self, record: &OperationRecord) {
        self.append_at(record, Retention::AppendOnly, || Utc::now().date_naive());
    }

    /// Appends `record` like [`OpLog::append`], then deletes the oldest
    /// segments while the total size is over the cap.
    ///
    /// Only the daemon calls this. The segment for the current UTC date is
    /// never deleted. The directory is scanned only on the first call, after
    /// the UTC date changes, or when the total passes the cap.
    pub fn append_and_enforce_cap(&self, record: &OperationRecord) {
        self.append_at(record, Retention::EnforceCap, || Utc::now().date_naive());
    }

    fn append_at(
        &self,
        record: &OperationRecord,
        retention: Retention,
        date: impl FnOnce() -> NaiveDate,
    ) {
        match self.switch {
            Switch::Enabled => {}
            Switch::Disabled => return,
        }
        let lock_path = self.dir.join(LOCK_NAME);
        let _lock = match self.lock_directory(&lock_path) {
            Ok(lock) => lock,
            Err(error) => {
                self.report_append_failure(&lock_path, &error);
                return;
            }
        };
        let date = date();
        match self.write_line(date, record) {
            Ok(()) => self.shared.append_failure.reset(),
            Err(error) => self.report_append_failure(&self.segment_path(date), &error),
        }
        match retention {
            Retention::AppendOnly => {}
            Retention::EnforceCap => self.enforce_cap(date),
        }
    }

    fn lock_directory(&self, path: &Path) -> io::Result<File> {
        std::fs::create_dir_all(&self.dir)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = match options.open(path) {
            Ok(lock) => lock,
            // A restrictive umask can leave the lock file read-only.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => File::open(path)?,
            Err(error) => return Err(error),
        };
        lock.lock()?;
        Ok(lock)
    }

    fn report_append_failure(&self, path: &Path, error: &io::Error) {
        match self.shared.append_failure.trip() {
            Failure::First => {
                tracing::warn!(path = %path.display(), %error, "operations log append failed");
            }
            Failure::Repeat => {
                tracing::debug!(path = %path.display(), %error, "operations log append failed again");
            }
        }
    }

    fn write_line(&self, date: NaiveDate, record: &OperationRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.segment_path(date))?;
        file.lock()?;
        recover_unterminated_row(&mut file)?;
        file.write_all(&line)
    }

    fn segment_path(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!(
            "{SEGMENT_PREFIX}{}{SEGMENT_SUFFIX}",
            date.format(SEGMENT_DATE_FORMAT)
        ))
    }

    fn enforce_cap(&self, current: NaiveDate) {
        let mut older = self
            .shared
            .older_segments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let result = match self.check_cached(*older, current) {
            Ok(CapCheck::Within) => Ok(()),
            Ok(CapCheck::Scan) => self.scan_and_delete(current).map(|(bytes, newer_bytes)| {
                *older = Some(OlderSegments {
                    current,
                    bytes,
                    newer_bytes,
                });
            }),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => self.shared.cap_failure.reset(),
            Err(error) => {
                *older = None;
                match self.shared.cap_failure.trip() {
                    Failure::First => {
                        tracing::warn!(dir = %self.dir.display(), %error, "operations log cap enforcement failed");
                    }
                    Failure::Repeat => {
                        tracing::debug!(dir = %self.dir.display(), %error, "operations log cap enforcement failed again");
                    }
                }
            }
        }
    }

    /// Decides from the cache and one stat of the current segment whether
    /// the directory needs a scan.
    fn check_cached(
        &self,
        older: Option<OlderSegments>,
        current: NaiveDate,
    ) -> io::Result<CapCheck> {
        let Some(OlderSegments {
            current: cached,
            bytes,
            newer_bytes,
        }) = older
        else {
            return Ok(CapCheck::Scan);
        };
        if cached != current {
            return Ok(CapCheck::Scan);
        }
        // No older segment is left to delete.
        if bytes == 0 {
            return Ok(CapCheck::Within);
        }
        let len = match std::fs::metadata(self.segment_path(current)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        let total = bytes.saturating_add(newer_bytes).saturating_add(len);
        match total > self.max_total_bytes {
            true => Ok(CapCheck::Scan),
            false => Ok(CapCheck::Within),
        }
    }

    /// Deletes the oldest segments while the total is over the cap. Returns
    /// the total size of the remaining segments older than `current` and the
    /// total size of the segments dated after `current`.
    fn scan_and_delete(&self, current: NaiveDate) -> io::Result<(u64, u64)> {
        let mut segments = Vec::new();
        let mut total: u64 = 0;
        let mut kept_len: u64 = 0;
        let mut newer_len: u64 = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(date) = segment_date(name) else {
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
            let len = metadata.len();
            total = total.saturating_add(len);
            if date >= current {
                kept_len = kept_len.saturating_add(len);
            }
            if date > current {
                newer_len = newer_len.saturating_add(len);
            }
            segments.push(Segment {
                date,
                path: entry.path(),
                len,
            });
        }
        segments.sort_by_key(|segment| segment.date);
        for Segment { date, path, len } in segments {
            if total <= self.max_total_bytes {
                break;
            }
            if date >= current {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            total = total.saturating_sub(len);
        }
        Ok((total.saturating_sub(kept_len), newer_len))
    }
}

fn recover_unterminated_row(file: &mut File) -> io::Result<()> {
    let end = file.seek(SeekFrom::End(0))?;
    let start = last_row_start(file, end)?;
    if start == end {
        return Ok(());
    }

    file.seek(SeekFrom::Start(start))?;
    let tail = BufReader::new((&mut *file).take(end - start));
    let row = serde_json::from_reader::<_, serde_json::Value>(tail);
    if file.seek(SeekFrom::End(0))? != end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "operation log changed during recovery",
        ));
    }
    match row {
        Ok(_value) => file.write_all(b"\n"),
        Err(error) if error.is_eof() => file.set_len(start),
        Err(error) => Err(error.into()),
    }
}

fn last_row_start(file: &mut File, end: u64) -> io::Result<u64> {
    let mut block = [0; 4096];
    let mut offset = end;
    while offset > 0 {
        let len = offset.min(block.len() as u64) as usize;
        offset -= len as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut block[..len])?;
        if let Some(newline) = block[..len].iter().rposition(|&byte| byte == b'\n') {
            return Ok(offset + newline as u64 + 1);
        }
    }
    Ok(0)
}

fn segment_date(name: &str) -> Option<NaiveDate> {
    let date = name.strip_prefix(SEGMENT_PREFIX)?;
    let date = date.strip_suffix(SEGMENT_SUFFIX)?;
    if date.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(date, SEGMENT_DATE_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::{DaemonOpFields, Millis, Outcome, RecordKind, RequestId, SCHEMA_VERSION};

    fn record() -> OperationRecord {
        OperationRecord {
            schema_version: SCHEMA_VERSION,
            ts: Utc::now(),
            request_id: RequestId::generate(),
            op: "ping".to_owned(),
            outcome: Outcome::Ok,
            duration_ms: Millis::default(),
            corpus: None,
            version: "test".to_owned(),
            kind: RecordKind::DaemonOp(DaemonOpFields {
                stages: Default::default(),
                warning_count: 0,
                warning_codes: Vec::new(),
            }),
        }
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, day).expect("valid date")
    }

    fn capped_log(dir: &Path, cap: u64) -> OpLog {
        let config = LoggingConfig {
            operations_max_total_bytes: cap,
            ..LoggingConfig::default()
        };
        OpLog::in_dir(dir, &config)
    }

    fn write_segment(log: &OpLog, day: u32, bytes: usize) -> PathBuf {
        let path = log.segment_path(date(day));
        std::fs::write(&path, vec![b'x'; bytes]).expect("write segment");
        path
    }

    #[cfg(unix)]
    mod rollover {
        use super::*;
        use std::fs::TryLockError;
        use std::process::{Child, Command};
        use std::time::{Duration, Instant};

        const CHILD_DIR: &str = "HALLOUMINATE_TEST_OPLOG_ROLLOVER_DIR";
        const CHILD_OP: &str = "HALLOUMINATE_TEST_OPLOG_ROLLOVER_OP";
        const READY: &str = "peer-ready";
        const NEXT_DAY: &str = "next-day";

        struct Peer(Child);

        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn wait_for(mut condition: impl FnMut() -> bool, reason: &str) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !condition() {
                assert!(Instant::now() < deadline, "{reason}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn start_peer(dir: &Path, op: &str) -> Peer {
            let mut peer = Peer(
                Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "oplog::writer::tests::rollover::rollover_peer",
                        "--exact",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env(CHILD_DIR, dir)
                    .env(CHILD_OP, op)
                    .spawn()
                    .expect("spawn rollover peer"),
            );
            wait_for(
                || {
                    if dir.join(READY).exists() {
                        return true;
                    }
                    assert_eq!(peer.0.try_wait().expect("poll rollover peer"), None);
                    false
                },
                "peer did not reach the rollover barrier",
            );
            peer
        }

        fn finish_peer(mut peer: Peer) {
            wait_for(
                || {
                    let Some(status) = peer.0.try_wait().expect("poll rollover peer") else {
                        return false;
                    };
                    assert!(status.success(), "rollover peer failed: {status}");
                    true
                },
                "peer did not finish after the lock holder returned",
            );
        }

        fn row(op: &str) -> OperationRecord {
            OperationRecord {
                op: op.to_owned(),
                ..record()
            }
        }

        fn ops(contents: &str) -> Vec<String> {
            let mut ops = Vec::new();
            for line in contents.lines() {
                let OperationRecord { op, .. } =
                    serde_json::from_str(line).expect("complete operation row");
                ops.push(op);
            }
            ops
        }

        #[test]
        #[ignore = "child process of rollover tests"]
        fn rollover_peer() {
            let Some(dir) = std::env::var_os(CHILD_DIR) else {
                return;
            };
            let dir = Path::new(&dir);
            let op = std::env::var(CHILD_OP).expect("peer operation");
            let lock = File::open(dir.join(LOCK_NAME)).expect("open rollover lock");
            let blocked = match lock.try_lock() {
                Ok(()) => false,
                Err(TryLockError::WouldBlock) => true,
                Err(TryLockError::Error(error)) => panic!("probe rollover lock: {error}"),
            };
            drop(lock);
            if blocked {
                std::fs::write(dir.join(READY), []).expect("signal peer is blocked");
            }

            let log = capped_log(dir, 1);
            if op == "retention" {
                log.append_at(&row(&op), Retention::EnforceCap, || {
                    if blocked {
                        let contents = std::fs::read_to_string(log.segment_path(date(15)))
                            .expect("read yesterday before retention");
                        assert_eq!(ops(&contents), ["seed", "writer"]);
                    }
                    date(16)
                });
            } else {
                assert_eq!(op, "writer");
                log.append_at(&row(&op), Retention::AppendOnly, || {
                    if dir.join(NEXT_DAY).exists() {
                        date(16)
                    } else {
                        date(15)
                    }
                });
            }
            if !blocked {
                std::fs::write(dir.join(READY), []).expect("signal peer completed without waiting");
            }
        }

        #[test]
        fn rollover_writer_finishes_before_retention_unlinks_the_segment() {
            let dir = tempfile::tempdir().expect("tempdir");
            let log = capped_log(dir.path(), 1);
            File::create_new(dir.path().join(LOCK_NAME)).expect("seed coordination file");
            log.append_at(&row("seed"), Retention::AppendOnly, || date(15));
            let yesterday = log.segment_path(date(15));
            let mut inode = File::open(&yesterday).expect("keep the original inode open");
            let mut peer = None;
            log.append_at(&row("writer"), Retention::AppendOnly, || {
                let current = date(15);
                peer = Some(start_peer(dir.path(), "retention"));
                current
            });
            finish_peer(peer.expect("retention peer"));

            let mut contents = String::new();
            inode
                .read_to_string(&mut contents)
                .expect("read unlinked inode");
            assert_eq!(ops(&contents), ["seed", "writer"]);
            assert!(
                !yesterday.exists(),
                "retention must remove the completed segment"
            );
            let today = std::fs::read_to_string(log.segment_path(date(16))).expect("read today");
            assert_eq!(ops(&today), ["retention"]);
        }

        #[test]
        fn rollover_retention_finishes_before_the_waiting_writer_selects_its_date() {
            let dir = tempfile::tempdir().expect("tempdir");
            let log = capped_log(dir.path(), 1);
            File::create_new(dir.path().join(LOCK_NAME)).expect("seed coordination file");
            log.append_at(&row("seed"), Retention::AppendOnly, || date(15));
            let mut peer = None;
            log.append_at(&row("retention"), Retention::EnforceCap, || {
                peer = Some(start_peer(dir.path(), "writer"));
                std::fs::write(dir.path().join(NEXT_DAY), []).expect("advance UTC date");
                date(16)
            });
            finish_peer(peer.expect("writer peer"));

            let today = std::fs::read_to_string(log.segment_path(date(16))).expect("read today");
            assert_eq!(ops(&today), ["retention", "writer"]);
            assert!(
                !log.segment_path(date(15)).exists(),
                "retention must remove yesterday"
            );
        }
    }

    #[test]
    fn size_cap_deletes_oldest_segments_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = record();
        let row_bytes = u64::try_from(serde_json::to_vec(&first).expect("serialize").len() + 1)
            .expect("row length");
        let log = capped_log(dir.path(), 2000 + row_bytes);
        let oldest = write_segment(&log, 13, 1000);
        let middle = write_segment(&log, 14, 1000);
        let newest = write_segment(&log, 15, 1000);
        let unrelated = dir.path().join("hallouminate.log");
        std::fs::write(&unrelated, vec![b'l'; 10_000]).expect("write unrelated log");

        log.append_at(&first, Retention::EnforceCap, || date(16));
        assert!(!oldest.exists(), "oldest segment must go first");
        assert!(middle.exists(), "cap reached: middle segment must stay");
        assert!(newest.exists(), "cap reached: newest old segment must stay");
        let current = log.segment_path(date(16));
        let first_line = serde_json::to_string(&first).expect("serialize first row");
        assert_eq!(
            std::fs::read_to_string(&current).expect("read current segment"),
            format!("{first_line}\n")
        );

        let log = capped_log(dir.path(), 1);
        let second = record();
        log.append_at(&second, Retention::EnforceCap, || date(16));
        assert!(!middle.exists(), "over the cap: older segments go");
        assert!(!newest.exists(), "over the cap: older segments go");
        assert!(unrelated.exists(), "non-segment files are outside the cap");
        assert!(
            dir.path().join(LOCK_NAME).exists(),
            "the directory lock stays"
        );

        let third = record();
        log.append_at(&third, Retention::AppendOnly, || date(16));
        let second_line = serde_json::to_string(&second).expect("serialize second row");
        let third_line = serde_json::to_string(&third).expect("serialize third row");
        assert_eq!(
            std::fs::read_to_string(&current).expect("read current segment"),
            format!("{first_line}\n{second_line}\n{third_line}\n")
        );
    }

    #[test]
    fn append_recovers_a_partial_row_without_losing_complete_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before = record();
        let after = [record(), record()];
        let writer = capped_log(dir.path(), u64::MAX);
        let path = writer.segment_path(date(16));
        writer
            .write_line(date(16), &before)
            .expect("write first row");
        let interrupted = OperationRecord {
            corpus: Some("é".repeat(8192)),
            ..record()
        };
        let mut fragment = serde_json::to_vec(&interrupted).expect("serialize interrupted row");
        fragment.truncate(fragment.len() / 2);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open segment");
        file.write_all(&fragment).expect("leave partial row");
        drop(file);
        drop(writer);

        std::thread::scope(|scope| {
            for row in &after {
                let path = &path;
                scope.spawn(move || {
                    let dir = path.parent().expect("segment parent");
                    let reopened = capped_log(dir, u64::MAX);
                    reopened
                        .write_line(date(16), row)
                        .expect("write recovery row");
                });
            }
        });

        let contents = std::fs::read_to_string(&path).expect("read recovered segment");
        assert!(contents.ends_with('\n'), "segment must end with a newline");
        let mut rows = Vec::new();
        for line in contents.lines() {
            rows.push(serde_json::from_str::<OperationRecord>(line).expect("read complete row"));
        }
        assert_eq!(rows.first(), Some(&before));
        assert_eq!(rows.len(), 3);
        for row in &after {
            assert!(
                rows.contains(row),
                "missing recovery row: {:?}",
                row.request_id
            );
        }
    }

    #[test]
    fn append_preserves_a_complete_row_without_its_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = capped_log(dir.path(), u64::MAX);
        let path = writer.segment_path(date(16));
        let before = serde_json::to_string(&record()).expect("serialize first row");
        std::fs::write(&path, &before).expect("write first row without newline");
        let row = record();
        writer.write_line(date(16), &row).expect("write second row");
        let after = serde_json::to_string(&row).expect("serialize second row");
        let contents = std::fs::read_to_string(&path).expect("read recovered segment");
        assert_eq!(contents, format!("{before}\n{after}\n"));
    }

    #[test]
    fn size_cap_rescans_when_current_segment_grows_after_cached_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = capped_log(dir.path(), 150);
        let older = write_segment(&log, 15, 100);
        let current = write_segment(&log, 16, 10);
        log.enforce_cap(date(16));
        assert!(older.exists(), "under the cap: nothing is deleted");

        let unseen = write_segment(&log, 1, 1000);
        log.enforce_cap(date(16));
        assert!(
            unseen.exists(),
            "under the cached total, an append stats only the current segment"
        );

        let mut file = OpenOptions::new()
            .append(true)
            .open(&current)
            .expect("open current");
        file.write_all(&[b'x'; 100]).expect("grow current");
        log.enforce_cap(date(16));
        assert!(
            !unseen.exists(),
            "over the cap: the rescan deletes oldest first"
        );
        assert!(!older.exists(), "still over the cap: the next oldest goes");
        assert!(current.exists(), "the current segment is never deleted");
    }

    #[test]
    fn size_cap_rescans_after_date_rollover() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = capped_log(dir.path(), 150);
        let yesterday = write_segment(&log, 15, 100);
        log.enforce_cap(date(15));
        assert!(yesterday.exists(), "the current segment is never deleted");

        let today = write_segment(&log, 16, 100);
        log.enforce_cap(date(16));
        assert!(
            !yesterday.exists(),
            "after the date changes, the old current segment counts and goes"
        );
        assert!(today.exists(), "the current segment is never deleted");
    }

    #[test]
    fn size_cap_never_deletes_segments_dated_after_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = capped_log(dir.path(), 150);
        let older = write_segment(&log, 15, 100);
        let current = write_segment(&log, 16, 10);
        let newer = write_segment(&log, 17, 1000);
        let (remaining, newer_bytes) = log.scan_and_delete(date(16)).expect("scan");
        assert!(!older.exists(), "over the cap: the older segment goes");
        assert!(current.exists(), "the current segment is never deleted");
        assert!(newer.exists(), "a segment after the current date stays");
        assert_eq!(remaining, 0, "newer bytes are not deletable older bytes");
        assert_eq!(
            newer_bytes, 1000,
            "newer bytes still count toward the total"
        );
    }

    #[test]
    fn size_cap_rescans_when_newer_segments_push_total_over_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = capped_log(dir.path(), 250);
        let older = write_segment(&log, 15, 100);
        let current = write_segment(&log, 16, 10);
        write_segment(&log, 17, 100);
        log.enforce_cap(date(16));
        assert!(older.exists(), "under the cap: nothing is deleted");

        let mut file = OpenOptions::new()
            .append(true)
            .open(&current)
            .expect("open current");
        file.write_all(&[b'x'; 50]).expect("grow current");
        log.enforce_cap(date(16));
        assert!(
            !older.exists(),
            "older plus newer plus current is over the cap: the rescan deletes"
        );
    }

    #[test]
    fn failure_latch_warns_once_per_error_state() {
        let latch = FailureLatch::default();
        assert_eq!(latch.trip(), Failure::First);
        assert_eq!(latch.trip(), Failure::Repeat);
        latch.reset();
        assert_eq!(latch.trip(), Failure::First);
    }

    #[test]
    fn disabled_log_creates_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("state");
        let config = LoggingConfig {
            operations: false,
            ..LoggingConfig::default()
        };
        let log = OpLog::in_dir(&missing, &config);
        log.append(&record());
        log.append_and_enforce_cap(&record());
        assert!(
            !missing.exists(),
            "disabled log must not touch the filesystem"
        );
    }

    #[test]
    fn segment_date_accepts_only_dated_names() {
        assert_eq!(
            segment_date("operations-2026-11-16.jsonl"),
            NaiveDate::from_ymd_opt(2026, 11, 16)
        );
        for name in [
            "operations.jsonl",
            "operations-2026-11-16.jsonl.tmp",
            "operations-2026-1-16.jsonl",
            "operations-notes.jsonl",
            "hallouminate.log",
        ] {
            assert_eq!(segment_date(name), None, "{name}");
        }
    }
}
