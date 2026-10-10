//! Lexical fallback for files with no indexed rows.
//!
//! Ripgrep matches files the index does not know yet, for example on a
//! fresh worktree. This module turns those matches into line-window hits
//! so that ground returns something useful before the first catch-up ends.

use std::collections::BTreeMap;

use rustix::fs::OFlags;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::common::{CorpusKey, epoch_millis, file_name};
use crate::indexer::SearchHit;

use super::ripgrep::RipgrepHit;

/// Lines kept on each side of the first matching line.
const WINDOW_RADIUS: usize = 3;

/// Bytes read from the start of one file. A match beyond this offset gets no hit.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Characters kept from one window line.
const MAX_LINE_CHARS: usize = 512;

struct FileMatch {
    terms: Vec<String>,
    first_line: u64,
}

/// Builds one line-window hit per matched file.
///
/// Files with more distinct matched terms come first. Ties break on path.
/// A file that cannot be read as UTF-8 text is skipped. The returned hits
/// carry `score` `0.0`; the caller assigns the final score.
pub(super) async fn line_window_hits(
    corpus_key: &CorpusKey,
    rg_hits: &[RipgrepHit],
    limit: usize,
) -> Vec<SearchHit> {
    let mut by_file: BTreeMap<&str, FileMatch> = BTreeMap::new();
    for rg_hit in rg_hits {
        let entry = by_file
            .entry(rg_hit.file_ref.as_str())
            .or_insert_with(|| FileMatch {
                terms: Vec::new(),
                first_line: rg_hit.line,
            });
        entry.first_line = entry.first_line.min(rg_hit.line);
        for term in &rg_hit.matched {
            if !entry.terms.contains(term) {
                entry.terms.push(term.clone());
            }
        }
    }
    let mut ordered: Vec<(&str, FileMatch)> = by_file.into_iter().collect();
    ordered.sort_by_key(|(_, found)| std::cmp::Reverse(found.terms.len()));

    let mut hits = Vec::new();
    for (file_ref, found) in ordered {
        if hits.len() == limit {
            break;
        }
        let Some(hit) = window_hit(corpus_key, file_ref, found.first_line).await else {
            continue;
        };
        hits.push(hit);
    }
    hits
}

fn truncate_line(text: &str) -> &str {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Opens `file_ref` for reading and refuses a symlink at the final path
/// component, so a link swapped in after the scan cannot lead outside the root.
async fn open_no_follow(file_ref: &str) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(OFlags::NOFOLLOW.bits().cast_signed());
    options.open(file_ref).await
}

async fn window_hit(corpus_key: &CorpusKey, file_ref: &str, line: u64) -> Option<SearchHit> {
    let line = usize::try_from(line).ok()?;
    if line == 0 {
        return None;
    }
    let start = line.saturating_sub(WINDOW_RADIUS).max(1);
    let wanted_end = line + WINDOW_RADIUS;
    let file = open_no_follow(file_ref).await.ok()?;
    let mtime_ms = match file.metadata().await {
        Ok(metadata) => metadata.modified().ok().and_then(epoch_millis).unwrap_or(0),
        Err(error) => {
            tracing::debug!(
                target: "hallouminate::search",
                err = %error,
                file_ref,
                "fallback file metadata unavailable"
            );
            0
        }
    };
    let mut lines = BufReader::new(file.take(MAX_READ_BYTES)).lines();
    let mut window: Vec<String> = Vec::new();
    let mut number = 0;
    while number < wanted_end {
        let Ok(next) = lines.next_line().await else {
            return None;
        };
        let Some(text) = next else {
            break;
        };
        number += 1;
        if number >= start {
            window.push(truncate_line(&text).to_string());
        }
    }
    if number < line {
        return None;
    }
    let text = window.join("\n");
    Some(SearchHit {
        chunk_id: format!("lexical-fallback:{file_ref}:{start}"),
        corpus_key: corpus_key.clone(),
        file_ref: file_ref.to_string(),
        heading_path: Vec::new(),
        line_start: start,
        line_end: number,
        search_text: text.clone(),
        text,
        summary: file_name(file_ref),
        keywords: Vec::new(),
        score: 0.0,
        mtime_ms,
        claim_marks: Vec::new(),
        structure: None,
        overlap_bytes: 0,
        z_score: None,
    })
}
