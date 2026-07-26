//! Ripgrep-backed exact-match retrieval.
//!
//! Covers the gap LanceDB's BM25 tokenizer misses: identifiers with
//! embedded punctuation and raw substrings inside code fences. Matching
//! is case-insensitive (`--ignore-case`), matching BM25's folded
//! tokens.
//!
//! Each query term is passed as its own literal pattern, and every hit
//! reports which terms matched it, so the caller can rank chunks by how
//! many distinct terms they cover. Output order is forced deterministic
//! (`--sort path`) because that ranking must be reproducible.

use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::common::{HallouminateError, Result, canonicalize_or_passthrough};

#[derive(Debug, Clone)]
pub struct RipgrepHit {
    /// Absolute path as `rg` reports it — kept as-is so the value lines
    /// up byte-for-byte with `SearchHit.file_ref` (also absolute via the
    /// indexer's `canonicalize_or_passthrough` step).
    pub file_ref: String,
    pub line: u64,
    pub snippet: String,
    /// Distinct lowercased terms matched at this line, from rg's
    /// `submatches[].match.text` — the only reliable way to know which
    /// `-e` term hit, since substring-searching the snippet can't
    /// disambiguate overlapping terms.
    pub matched: Vec<String>,
}

/// Run `rg` over each `path`, matching each of `terms` as its own literal
/// (`-e`) pattern in one invocation — a multi-word query needs every term a
/// chance to match, not just the literal whole-query string. Returns at
/// most `max_hits` matches; rg's own `--max-count` would cap per-FILE not
/// per-run, so we truncate after collecting.
///
/// Failure modes:
/// - `rg` missing on PATH → `HallouminateError::Io` (`io::ErrorKind::NotFound`)
/// - `rg` exits 1 with no matches → `Ok(vec![])`; this is rg's normal
///   "nothing found" signal, not an error.
/// - `rg` exits >=2 (a real error), or terminates abnormally/by signal,
///   AND nothing was emitted on stdout → `HallouminateError::Search`;
///   non-zero with matches already collected (e.g. one path vanished
///   while another matched) is tolerated.
pub async fn run(paths: &[String], terms: &[String], max_hits: usize) -> Result<Vec<RipgrepHit>> {
    if paths.is_empty() || terms.is_empty() || max_hits == 0 {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new("rg");
    cmd.arg("--json")
        .arg("--no-heading")
        .arg("--fixed-strings")
        // Deterministic output order, and therefore a deterministic
        // truncation at `max_hits`. Without it rg walks in parallel and
        // emits matches in whatever order threads finish, so a run that
        // stops early keeps a different subset each time and the ranked
        // list this feeds is not reproducible. Measured: five identical
        // invocations produced five different outputs unsorted, and the
        // evaluation moved by four queries between identical runs.
        // `--sort path` forces a single traversal thread; the corpora this
        // searches are wiki-sized, so the ordering guarantee is worth more
        // than the parallelism.
        .arg("--sort")
        .arg("path")
        .arg("--type")
        .arg("md")
        .arg("--ignore-case")
        .arg("--max-columns")
        .arg("512");
    for term in terms {
        cmd.arg("-e").arg(term);
    }
    cmd.arg("--");
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
    let mut limit_reached = false;
    while let Some(line) = reader.next_line().await.map_err(HallouminateError::Io)? {
        if let Some(mut hit) = parse_match_line(&line) {
            // Indexer stores file_ref as `canonicalize_or_passthrough`'d
            // path; mirror that here so the fusion key (file_ref string
            // equality) actually lines up.
            let canon = canonicalize_or_passthrough(Path::new(&hit.file_ref));
            hit.file_ref = canon.as_path().to_string_lossy().into_owned();
            hits.push(hit);
        }
        // Break the instant the cap is satisfied — checking after the push
        // (rather than at the top of the next iteration) means we don't
        // await one more `next_line()`, which could block until rg emits a
        // later match. `max_hits >= 1` here (max_hits == 0 short-circuits above).
        if hits.len() >= max_hits {
            limit_reached = true;
            break;
        }
    }

    if limit_reached {
        // We stopped draining stdout before rg finished writing. rg may
        // be blocked on a full stdout pipe, so a plain wait() here can
        // deadlock — kill it instead of waiting for a graceful exit.
        let _ = child.kill().await;
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
    let mut matched = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for sub in data.submatches.unwrap_or_default() {
        let Some(text) = sub.m.and_then(|m| m.text) else {
            continue;
        };
        let lower = text.to_lowercase();
        if seen.insert(lower.clone()) {
            matched.push(lower);
        }
    }
    Some(RipgrepHit {
        file_ref: path,
        line: line_no,
        snippet,
        matched,
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
    submatches: Option<Vec<RgSubmatch>>,
}

#[derive(Debug, Deserialize)]
struct RgSubmatch {
    #[serde(rename = "match")]
    m: Option<RgText>,
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
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"hello world\n"},"line_number":42,"absolute_offset":0,"submatches":[{"match":{"text":"Hello"},"start":0,"end":5}]}}"#;
        let hit = parse_match_line(line).expect("match event yields hit");
        assert_eq!(hit.file_ref, "/tmp/a.md");
        assert_eq!(hit.line, 42);
        assert_eq!(hit.snippet, "hello world\n");
        assert_eq!(hit.matched, vec!["hello".to_string()]);
    }

    #[test]
    fn parse_match_line_dedups_matched_terms() {
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"foo foo bar\n"},"line_number":1,"submatches":[{"match":{"text":"foo"}},{"match":{"text":"FOO"}},{"match":{"text":"bar"}}]}}"#;
        let hit = parse_match_line(line).expect("match event yields hit");
        assert_eq!(hit.matched, vec!["foo".to_string(), "bar".to_string()]);
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
        assert!(run(&[], &["q".to_string()], 5).await.unwrap().is_empty());
        assert!(run(&["/tmp".into()], &[], 5).await.unwrap().is_empty());
        assert!(
            run(&["/tmp".into()], &["q".to_string()], 0)
                .await
                .unwrap()
                .is_empty()
        );
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
            &["caerbannog".to_string()],
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
        assert_eq!(hits[0].matched, vec!["caerbannog".to_string()]);
    }

    /// A multi-word natural-language question must produce hits when its
    /// individual terms appear in the corpus, even though the question
    /// itself appears nowhere.
    ///
    /// This is the defect the per-term pass exists to fix. Passing the raw
    /// query as one literal returned zero files for every one of the
    /// original twelve evaluation queries, so the signal cost a subprocess
    /// spawn on every request and contributed nothing. The assertion on the
    /// whole-query form is what makes this test meaningful: without it, a
    /// regression to whole-query matching would still find the fixture via
    /// some other term and the test would pass.
    #[tokio::test]
    async fn natural_language_query_matches_per_term_though_the_phrase_does_not() {
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("melange.md"),
            "# Navigation\n\nGuild navigators fold space using melange.\n",
        )
        .expect("write fixture");
        let roots = [dir.path().to_string_lossy().into_owned()];
        let question = "why is the spice melange important to navigation";

        let terms = crate::search::terms::split_terms(question);
        let hits = run(&roots, &terms, 10).await.expect("per-term run");
        assert!(
            !hits.is_empty(),
            "per-term matching must find the fixture for a natural-language question"
        );
        assert!(
            hits.iter().any(|h| h.matched.contains(&"melange".into())),
            "the matching term must be reported so the caller can rank by term coverage"
        );

        // The whole question as a single literal matches nothing — which is
        // exactly the behaviour that made this signal dead weight.
        let whole = run(&roots, &[question.to_string()], 10)
            .await
            .expect("whole-query run");
        assert!(
            whole.is_empty(),
            "the raw question appears nowhere in the corpus; got {} hits",
            whole.len()
        );
    }

    /// Repeated runs over the same tree must return the same hits in the
    /// same order, including when `max_hits` truncates.
    ///
    /// Ranking downstream is derived from this sequence, so a
    /// nondeterministic order makes search results irreproducible. rg walks
    /// in parallel by default and emits matches in whatever order its
    /// threads finish; truncating that mid-stream keeps a different subset
    /// each run. This caught a real regression — the evaluation moved by
    /// four queries between two identical runs before `--sort path` was
    /// added.
    #[tokio::test]
    async fn repeated_runs_return_identical_hits_even_when_truncated() {
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        // Enough files that a parallel walk has room to reorder them, and
        // enough matches that `max_hits` truncates well short of the total.
        for i in 0..24 {
            let path = dir.path().join(format!("page{i:02}.md"));
            std::fs::write(
                &path,
                "# Page\n\nshrubbery and swallow here.\nswallow again.\n",
            )
            .expect("write fixture");
        }
        let roots = [dir.path().to_string_lossy().into_owned()];
        let terms = ["shrubbery".to_string(), "swallow".to_string()];

        let first = run(&roots, &terms, 10).await.expect("first run");
        assert_eq!(first.len(), 10, "max_hits must actually truncate here");
        for _ in 0..4 {
            let again = run(&roots, &terms, 10).await.expect("repeat run");
            let a: Vec<_> = first
                .iter()
                .map(|h| (&h.file_ref, h.line, &h.matched))
                .collect();
            let b: Vec<_> = again
                .iter()
                .map(|h| (&h.file_ref, h.line, &h.matched))
                .collect();
            assert_eq!(a, b, "truncated rg output must be reproducible");
        }
    }

    #[tokio::test]
    async fn multiple_terms_match_different_files() {
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.md"), "# A\n\ncaerbannog beast.\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "# B\n\nknight of ni.\n").unwrap();
        let hits = run(
            &[dir.path().to_string_lossy().into_owned()],
            &["caerbannog".to_string(), "ni".to_string()],
            10,
        )
        .await
        .expect("rg run");
        let files: std::collections::HashSet<&str> = hits
            .iter()
            .map(|h| h.file_ref.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            files,
            std::collections::HashSet::from(["a.md", "b.md"]),
            "both files must be matched by their own term"
        );
    }

    #[tokio::test]
    async fn only_some_terms_hit() {
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.md"), "# A\n\ncaerbannog beast.\n").unwrap();
        let hits = run(
            &[dir.path().to_string_lossy().into_owned()],
            &["caerbannog".to_string(), "nonexistentterm".to_string()],
            10,
        )
        .await
        .expect("rg run");
        assert_eq!(hits.len(), 1, "only the matching term produces a hit");
        assert_eq!(hits[0].matched, vec!["caerbannog".to_string()]);
    }

    #[tokio::test]
    async fn limit_reached_returns_promptly_without_draining_full_output() {
        // Regression test: reaching `max_hits` used to `break` the stdout-draining
        // loop and then `child.wait()`, which can deadlock if rg is still
        // blocked writing the rest of its matches into a full pipe. Generate
        // enough matching lines to overflow the OS pipe buffer (well over the
        // 64KiB typical max) so the pre-fix code would hang forever here.
        if which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.md");
        let mut content = String::from("# Big\n\n");
        for _ in 0..20_000 {
            content.push_str("caerbannog beast appears again in the text\n");
        }
        std::fs::write(&path, content).unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run(
                &[dir.path().to_string_lossy().into_owned()],
                &["caerbannog".to_string()],
                1,
            ),
        )
        .await
        .expect("run() must return promptly instead of deadlocking on a full pipe")
        .expect("rg run");

        assert_eq!(result.len(), 1, "limit of 1 must cap the returned hits");
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
            &["caerbannog".to_string()],
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
        let err = run(
            &["/no/such/path/hallouminate-test".into()],
            &["q".to_string()],
            5,
        )
        .await
        .expect_err("exit >= 2 must surface as an error");
        assert!(
            matches!(err, HallouminateError::Search(_)),
            "expected Search variant, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("/no/such/path/hallouminate-test"),
            "expected stderr to include the failing path, got: {msg}"
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
