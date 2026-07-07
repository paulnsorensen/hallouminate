//! Ripgrep-backed exact-match retrieval.
//!
//! Covers the gap LanceDB's BM25 tokenizer misses: identifiers with
//! embedded punctuation and raw substrings inside code fences. Matching
//! is case-insensitive (`--ignore-case`), matching BM25's folded
//! tokens. Returns hits in the order `rg` emits them so the caller can
//! treat first-occurrence position as the rank for RRF fusion.

use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::domain::common::{HallouminateError, Result, canonicalize_or_passthrough};

#[derive(Debug, Clone)]
pub struct RipgrepHit {
    /// Absolute path as `rg` reports it — kept as-is so the value lines
    /// up byte-for-byte with `SearchHit.file_ref` (also absolute via the
    /// indexer's `canonicalize_or_passthrough` step).
    pub file_ref: String,
    pub line: u64,
    pub snippet: String,
}

/// Run `rg` over each `path`, treating `query` as a literal (`-F`)
/// pattern restricted to markdown files. Returns at most `limit`
/// matches; rg's own `--max-count` would cap per-FILE not per-run, so
/// we truncate after collecting.
///
/// Failure modes:
/// - `rg` missing on PATH → `HallouminateError::Io` (`io::ErrorKind::NotFound`)
/// - `rg` exits 1 with no matches → `Ok(vec![])`; this is rg's normal
///   "nothing found" signal, not an error.
/// - `rg` exits >=2 (a real error) AND nothing was emitted on stdout →
///   `HallouminateError::Search`; non-zero with matches already collected
///   (e.g. one path vanished while another matched) is tolerated.
pub async fn run(paths: &[String], query: &str, limit: usize) -> Result<Vec<RipgrepHit>> {
    if paths.is_empty() || query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new("rg");
    cmd.arg("--json")
        .arg("--no-heading")
        .arg("--fixed-strings")
        .arg("--type")
        .arg("md")
        .arg("--ignore-case")
        .arg("--max-columns")
        .arg("512")
        .arg("--")
        .arg(query);
    for p in paths {
        cmd.arg(p);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(HallouminateError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HallouminateError::Embed("rg child missing stdout".into()))?;
    let mut reader = BufReader::new(stdout).lines();

    let mut hits: Vec<RipgrepHit> = Vec::new();
    while let Some(line) = reader.next_line().await.map_err(HallouminateError::Io)? {
        if hits.len() >= limit {
            break;
        }
        if let Some(mut hit) = parse_match_line(&line) {
            // Indexer stores file_ref as `canonicalize_or_passthrough`'d
            // path; mirror that here so the fusion key (file_ref string
            // equality) actually lines up.
            let canon = canonicalize_or_passthrough(Path::new(&hit.file_ref));
            hit.file_ref = canon.as_path().to_string_lossy().into_owned();
            hits.push(hit);
        }
    }

    // Wait for the child so it exits cleanly (kill_on_drop catches the
    // worst case, but a clean wait is cheaper) AND so we can inspect its
    // exit status instead of masking real failures.
    let status = child.wait().await.map_err(HallouminateError::Io)?;
    // rg exit codes: 0 = matches found, 1 = no matches, >=2 = real error
    // (bad pattern, IO failure, …). Exit 1 with no hits is rg's normal
    // "nothing found" signal, not a failure — return an empty result. A
    // non-zero exit with hits already collected is also tolerated — e.g.
    // one path vanished while another matched. Only a real error (exit
    // >=2) with NO hits is a genuine failure: surface it (with stderr)
    // rather than returning an empty success that hides the error.
    let exit_code = status.code();
    if !status.success() && hits.is_empty() && exit_code != Some(1) {
        let mut stderr_buf = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr_buf).await;
        }
        return Err(HallouminateError::Search(format!(
            "rg failed ({status}): {}",
            stderr_buf.trim()
        )));
    }
    Ok(hits)
}

/// Parse one line of `rg --json` output. Returns `Some` only for
/// `"type":"match"` events; ignores begin/end/summary/context lines so
/// the caller doesn't have to know rg's event taxonomy.
///
/// Every nested field is `Option<…>` so an unexpected shape (newer rg
/// version, future event variants) returns `None` instead of failing
/// the whole stream.
fn parse_match_line(line: &str) -> Option<RipgrepHit> {
    let evt: RgEvent = serde_json::from_str(line).ok()?;
    if evt.kind != "match" {
        return None;
    }
    let data = evt.data?;
    let path = data.path?.text?;
    let line_no = data.line_number?;
    let snippet = data.lines.and_then(|l| l.text).unwrap_or_default();
    Some(RipgrepHit {
        file_ref: path,
        line: line_no,
        snippet,
    })
}

#[derive(Debug, Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RgMatchData>,
}

#[derive(Debug, Deserialize)]
struct RgMatchData {
    path: Option<RgText>,
    lines: Option<RgText>,
    line_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RgText {
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_match_line_extracts_path_and_line() {
        // Synthetic but matches `rg --json` shape for a match event.
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"hello world\n"},"line_number":42,"absolute_offset":0,"submatches":[]}}"#;
        let hit = parse_match_line(line).expect("match event yields hit");
        assert_eq!(hit.file_ref, "/tmp/a.md");
        assert_eq!(hit.line, 42);
        assert_eq!(hit.snippet, "hello world\n");
    }

    #[test]
    fn parse_match_line_ignores_non_match_events() {
        for kind in ["begin", "end", "summary", "context"] {
            let line = format!(r#"{{"type":"{kind}","data":{{"path":{{"text":"/tmp/a.md"}}}}}}"#);
            assert!(
                parse_match_line(&line).is_none(),
                "{kind} events must not produce hits"
            );
        }
    }

    #[test]
    fn parse_match_line_returns_none_on_garbage() {
        assert!(parse_match_line("not json").is_none());
        assert!(parse_match_line("").is_none());
    }

    #[tokio::test]
    async fn empty_inputs_short_circuit() {
        assert!(run(&[], "q", 5).await.unwrap().is_empty());
        assert!(run(&["/tmp".into()], "", 5).await.unwrap().is_empty());
        assert!(run(&["/tmp".into()], "q", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn finds_literal_match_in_markdown_file() {
        // rg is a hard dep for the binary; the e2e suite already
        // installs it. Skip silently if it's missing locally so this
        // doesn't break dev machines without it.
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Notes\n\nsome caerbannog beast here.\n").unwrap();
        let hits = run(
            &[dir.path().to_string_lossy().into_owned()],
            "caerbannog",
            5,
        )
        .await
        .expect("rg run");
        assert_eq!(hits.len(), 1, "exactly one match in fixture");
        assert!(
            hits[0].file_ref.ends_with("notes.md"),
            "expected notes.md, got {}",
            hits[0].file_ref
        );
        assert_eq!(hits[0].line, 3);
    }

    #[tokio::test]
    async fn no_lexical_match_returns_empty_ok_not_error() {
        // rg exit 1 (no matches) must be a normal empty result, not an
        // error — exit 1 is rg's documented "nothing found" signal.
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Notes\n\nnothing relevant here.\n").unwrap();
        let hits = run(
            &[dir.path().to_string_lossy().into_owned()],
            "caerbannog",
            5,
        )
        .await
        .expect("exit 1 with no hits must be Ok, not Err");
        assert!(hits.is_empty(), "expected no hits, got {hits:?}");
    }

    #[tokio::test]
    async fn real_rg_failure_errors_with_stderr() {
        // A nonexistent search path makes rg exit 2 (real error), not 1.
        if which("rg").is_err() {
            return;
        }
        let err = run(&["/no/such/path/hallouminate-test".into()], "q", 5)
            .await
            .expect_err("exit >= 2 must surface as an error");
        assert!(
            matches!(err, HallouminateError::Search(_)),
            "expected Search variant, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("no such file"),
            "expected stderr detail in error message, got: {msg}"
        );
    }

    fn which(bin: &str) -> std::io::Result<std::path::PathBuf> {
        let path = std::env::var_os("PATH")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PATH not set"))?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{bin} not on PATH"),
        ))
    }
}
