use hallouminate_domain::indexer::{ApplyStats, SkipReason, SkippedFile};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub corpora: Vec<CorpusReport>,
    /// Corpora skipped during the run, one human-readable line each (e.g. a
    /// missing root). Empty in the common all-healthy case, so it is omitted
    /// from the JSON rather than serialized as `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Reply to a background `IndexRequest`: the corpora the daemon newly
/// registered and queued for catch-up. Corpora that were already registered
/// unchanged are not listed. The catch-up itself has not finished.
///
/// Wire compatibility: a daemon that predates background indexing replies
/// with an [`IndexReport`] and no `queued` field. That reply decodes here with
/// an empty `queued` list and `acknowledged` false. Only a daemon that queued
/// the pass sets `acknowledged`, so a client must fail on its absence instead
/// of treating an empty list as proof of a queued pass.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct QueuedIndexReport {
    /// True when the daemon honoured `background` and queued the catch-up.
    #[serde(default)]
    pub acknowledged: bool,
    #[serde(default)]
    pub queued: Vec<String>,
    /// True when the watcher registration limit left a corpus unwatched.
    #[serde(default)]
    pub registration_limit_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusReport {
    pub name: String,
    pub files_upserted: usize,
    pub files_touched: usize,
    pub files_deleted: usize,
    pub files_skipped_empty: usize,
    #[serde(default)]
    pub files_skipped_unreadable: usize,
    /// The files counted in `files_skipped_unreadable`, capped at
    /// `MAX_REPORTED_SKIPS`. Omitted from the JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_unreadable: Vec<SkippedFileReport>,
    pub chunks_inserted: usize,
    pub embeddings_inserted: usize,
}

impl CorpusReport {
    /// Builds the report for corpus `name` from the tallies of its run.
    pub fn from_stats(name: String, stats: ApplyStats) -> Self {
        let ApplyStats {
            files_upserted,
            files_touched,
            files_deleted,
            files_skipped_empty,
            files_skipped_unreadable,
            skipped_unreadable,
            chunks_inserted,
            embeddings_inserted,
        } = stats;
        let mut skipped = Vec::with_capacity(skipped_unreadable.len());
        for file in skipped_unreadable {
            skipped.push(SkippedFileReport::from(file));
        }
        Self {
            name,
            files_upserted,
            files_touched,
            files_deleted,
            files_skipped_empty,
            files_skipped_unreadable,
            skipped_unreadable: skipped,
            chunks_inserted,
            embeddings_inserted,
        }
    }
}

/// One file that an index run skipped without indexing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFileReport {
    /// Absolute path of the skipped file.
    pub path: String,
    /// Why the run skipped the file, with the extraction error when one exists.
    #[serde(flatten)]
    pub reason: SkippedFileReason,
}

/// Why an index run skipped a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkippedFileReason {
    /// No format handler accepts the file type.
    UnsupportedFormat,
    /// The format handler failed to extract content.
    ExtractionFailed {
        /// The extraction error text.
        error: String,
    },
}

impl From<SkippedFile> for SkippedFileReport {
    fn from(skipped: SkippedFile) -> Self {
        let SkippedFile { file, reason } = skipped;
        let path = file.as_path().display().to_string();
        let reason = match reason {
            SkipReason::UnsupportedFormat => SkippedFileReason::UnsupportedFormat,
            SkipReason::ExtractionFailed(error) => SkippedFileReason::ExtractionFailed { error },
        };
        Self { path, reason }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hallouminate_domain::common::FileRef;

    use super::*;

    #[test]
    fn corpus_report_names_skipped_files_on_the_wire() {
        let mut stats = ApplyStats::default();
        stats.record_unreadable(SkippedFile {
            file: FileRef::new(PathBuf::from("/r/brief.html")),
            reason: SkipReason::UnsupportedFormat,
        });
        stats.record_unreadable(SkippedFile {
            file: FileRef::new(PathBuf::from("/r/scan.pdf")),
            reason: SkipReason::ExtractionFailed("no text layer".into()),
        });
        let report = CorpusReport::from_stats("docs".into(), stats);
        let wire = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(wire["files_skipped_unreadable"], 2);
        assert_eq!(
            wire["skipped_unreadable"],
            serde_json::json!([
                {"path": "/r/brief.html", "reason": "unsupported_format"},
                {"path": "/r/scan.pdf", "reason": "extraction_failed", "error": "no text layer"},
            ])
        );
    }

    #[test]
    fn skipped_file_report_round_trips_through_json() {
        let reports = vec![
            SkippedFileReport {
                path: "/r/a.html".into(),
                reason: SkippedFileReason::UnsupportedFormat,
            },
            SkippedFileReport {
                path: "/r/b.pdf".into(),
                reason: SkippedFileReason::ExtractionFailed {
                    error: "no text layer".into(),
                },
            },
        ];
        let wire = serde_json::to_string(&reports).expect("serialize reports");
        let back: Vec<SkippedFileReport> =
            serde_json::from_str(&wire).expect("deserialize reports");
        assert_eq!(back, reports);
    }

    #[test]
    fn corpus_report_omits_an_empty_skip_list() {
        let report = CorpusReport::from_stats("docs".into(), ApplyStats::default());
        let wire = serde_json::to_value(&report).expect("serialize report");
        assert!(wire.get("skipped_unreadable").is_none(), "{wire}");
    }

    #[test]
    fn queued_report_decodes_an_older_daemons_index_reply_as_nothing_queued() {
        let old_reply = serde_json::json!({ "corpora": [], "warnings": ["x"] });
        let decoded: QueuedIndexReport =
            serde_json::from_value(old_reply).expect("older reply decodes");
        assert!(decoded.queued.is_empty());
        assert!(!decoded.acknowledged);
        assert!(!decoded.registration_limit_reached);
    }
}
