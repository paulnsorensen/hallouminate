use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{NaiveDate, Utc};
use hallouminate_config::LoggingConfig;

use super::OperationRecord;

const SEGMENT_PREFIX: &str = "operations-";
const SEGMENT_SUFFIX: &str = ".jsonl";
const SEGMENT_DATE_FORMAT: &str = "%Y-%m-%d";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    Enabled,
    Disabled,
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

/// Total size of the segments older than `current`, from the last directory
/// scan. Only the current segment grows, so the total stays valid until the
/// UTC date changes.
#[derive(Debug, Clone, Copy)]
struct OlderSegments {
    current: NaiveDate,
    bytes: u64,
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
        self.append_row(record);
    }

    /// Appends `record` like [`OpLog::append`], then deletes the oldest
    /// segments while the total size is over the cap.
    ///
    /// Only the daemon calls this. The segment for the current UTC date is
    /// never deleted. The directory is scanned only on the first call, after
    /// the UTC date changes, or when the total passes the cap.
    pub fn append_and_enforce_cap(&self, record: &OperationRecord) {
        let Some(current) = self.append_row(record) else {
            return;
        };
        self.enforce_cap(current);
    }

    fn append_row(&self, record: &OperationRecord) -> Option<NaiveDate> {
        match self.switch {
            Switch::Enabled => {}
            Switch::Disabled => return None,
        }
        let date = Utc::now().date_naive();
        let path = self.segment_path(date);
        match self.write_line(&path, record) {
            Ok(()) => self.shared.append_failure.reset(),
            Err(error) => match self.shared.append_failure.trip() {
                Failure::First => {
                    tracing::warn!(path = %path.display(), %error, "operations log append failed");
                }
                Failure::Repeat => {
                    tracing::debug!(path = %path.display(), %error, "operations log append failed again");
                }
            },
        }
        Some(date)
    }

    fn write_line(&self, path: &Path, record: &OperationRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        std::fs::create_dir_all(&self.dir)?;
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
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
            Ok(CapCheck::Scan) => self.scan_and_delete(current).map(|bytes| {
                *older = Some(OlderSegments { current, bytes });
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
        match bytes.saturating_add(len) > self.max_total_bytes {
            true => Ok(CapCheck::Scan),
            false => Ok(CapCheck::Within),
        }
    }

    /// Deletes the oldest segments while the total is over the cap, and
    /// returns the total size of the remaining segments older than `current`.
    fn scan_and_delete(&self, current: NaiveDate) -> io::Result<u64> {
        let mut segments = Vec::new();
        let mut total: u64 = 0;
        let mut current_len: u64 = 0;
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
            if date == current {
                current_len = len;
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
            if date == current {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            total = total.saturating_sub(len);
        }
        Ok(total.saturating_sub(current_len))
    }
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
