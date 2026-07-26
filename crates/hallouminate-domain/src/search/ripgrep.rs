//! Ripgrep-backed exact-match retrieval.
//!
//! Covers the gap LanceDB's BM25 tokenizer misses: identifiers with
//! embedded punctuation and raw substrings inside code fences. Matching
//! is case-insensitive (`--ignore-case`), matching BM25's folded
//! tokens. That coverage only reaches chunks the FTS/vector pool already
//! retrieved (see `search_with_ripgrep`), so a chunk BM25 misses entirely
//! is still invisible to this pass.
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

/// Outcome of one [`run`] invocation: the parsed hits plus the counters
/// the call site needs to tell an honest "no literal matches" apart from
/// a signal that ran and produced nothing usable (truncated, timed out,
/// or hit an rg output shape this parser doesn't recognise).
#[derive(Debug, Clone)]
pub struct RipgrepRun {
    pub hits: Vec<RipgrepHit>,
    /// `true` when collection stopped at `max_hits` before rg finished
    /// walking the corpus.
    pub truncated: bool,
    /// Wall-clock duration of the `rg` subprocess, spawn to exit.
    pub elapsed_ms: u64,
    /// Count of `"type":"match"` events whose fields didn't parse — the
    /// detectable signature of an rg version bump renaming a JSON field
    /// (e.g. `submatches`/`match`/`text`). Excludes ordinary non-match
    /// events (begin/end/summary/context), which are expected.
    pub unparseable: usize,
}

/// Number of files the run-wide budget is spread across before any single
/// file may exhaust it. Under `--sort path` the walk is alphabetical, so
/// without a per-file cap one term-dense early file consumes the whole
/// budget and every later-sorting file gets zero evidence no matter how
/// well it would have matched.
const MIN_FILES_SPREAD: usize = 10;

/// Run `rg` over each `path`, matching each of `terms` as its own literal
/// (`-e`) pattern in one invocation — a multi-word query needs every term a
/// chance to match, not just the literal whole-query string. Returns at
/// most `max_hits` matches in total, and bounds how many of those any one
/// file may contribute via `--max-count`, so the budget reaches at least
/// [`MIN_FILES_SPREAD`] files. The post-collection `hits.len() >= max_hits`
/// check below stays as a run-wide backstop so the stream still terminates
/// promptly.
///
/// Failure modes:
/// - `rg` missing on PATH → `HallouminateError::Io` (`io::ErrorKind::NotFound`)
/// - `rg` exits 1 with no matches → `Ok(vec![])`; this is rg's normal
///   "nothing found" signal, not an error.
/// - `rg` exits >=2 (a real error), or terminates abnormally/by signal,
///   AND nothing was emitted on stdout → `HallouminateError::Search`;
///   non-zero with matches already collected (e.g. one path vanished
///   while another matched) is tolerated.
pub async fn run(paths: &[String], terms: &[String], max_hits: usize) -> Result<RipgrepRun> {
    if paths.is_empty() || terms.is_empty() || max_hits == 0 {
        return Ok(RipgrepRun {
            hits: Vec::new(),
            truncated: false,
            elapsed_ms: 0,
            unparseable: 0,
        });
    }
    // Spread the budget across files instead of letting one hot file
    // consume it all. Dividing by `terms.len()` would be useless for the
    // case that motivates this — a single common term, where the divisor
    // is 1 — so the spread is a fixed file count.
    //
    // The floor of `terms.len()` guarantees any one file can still report
    // at least one hit per term, so a file never loses a term purely to
    // the cap. Known limit: the cap is per *file* while ranking is per
    // *chunk*, so a file with more matching chunks than `per_file_cap`
    // leaves its later chunks without ripgrep evidence. That is the
    // accepted trade for not starving the rest of the corpus.
    let per_file_cap = max_hits.div_ceil(MIN_FILES_SPREAD).max(terms.len());
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
        .arg("512")
        .arg("--max-count")
        .arg(per_file_cap.to_string());
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

    let started = std::time::Instant::now();
    let mut child = cmd.spawn().map_err(HallouminateError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HallouminateError::Embed("rg child missing stdout".into()))?;
    let mut reader = BufReader::new(stdout).lines();

    let mut hits: Vec<RipgrepHit> = Vec::new();
    let mut limit_reached = false;
    let mut unparseable = 0usize;
    while let Some(line) = reader.next_line().await.map_err(HallouminateError::Io)? {
        match classify_line(&line) {
            ParsedLine::Hit(mut hit) => {
                // Indexer stores file_ref as `canonicalize_or_passthrough`'d
                // path; mirror that here so the fusion key (file_ref string
                // equality) actually lines up.
                let canon = canonicalize_or_passthrough(Path::new(&hit.file_ref));
                hit.file_ref = canon.as_path().to_string_lossy().into_owned();
                hits.push(hit);
            }
            ParsedLine::Malformed => unparseable += 1,
            ParsedLine::NotMatch => {}
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
    let elapsed_ms = started.elapsed().as_millis() as u64;
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
    Ok(RipgrepRun {
        hits,
        truncated: limit_reached,
        elapsed_ms,
        unparseable,
    })
}

/// Classification of one `rg --json` stdout line, distinguishing an
/// ordinary non-match event from a `"type":"match"` event whose fields
/// didn't parse — the latter is what [`RipgrepRun::unparseable`] counts.
enum ParsedLine {
    Hit(RipgrepHit),
    NotMatch,
    Malformed,
}

fn classify_line(line: &str) -> ParsedLine {
    let Ok(evt) = serde_json::from_str::<RgEvent>(line) else {
        return ParsedLine::Malformed;
    };
    if evt.kind != "match" {
        return ParsedLine::NotMatch;
    }
    match parse_match_hit(evt.data) {
        Some(hit) => ParsedLine::Hit(hit),
        None => ParsedLine::Malformed,
    }
}

/// Extract a [`RipgrepHit`] from a `"type":"match"` event's `data`.
/// Every nested field is `Option<…>`, deliberately so: an unexpected
/// shape (newer rg version, future event variants) returns `None`
/// instead of panicking on the whole stream.
fn parse_match_hit(data: Option<RgMatchData>) -> Option<RipgrepHit> {
    let data = data?;
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

    fn expect_hit(line: &str) -> RipgrepHit {
        match classify_line(line) {
            ParsedLine::Hit(hit) => hit,
            ParsedLine::NotMatch => panic!("match event classified as NotMatch"),
            ParsedLine::Malformed => panic!("match event classified as Malformed"),
        }
    }

    #[test]
    fn classify_line_extracts_path_and_line() {
        // Synthetic but matches `rg --json` shape for a match event.
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"hello world\n"},"line_number":42,"absolute_offset":0,"submatches":[{"match":{"text":"Hello"},"start":0,"end":5}]}}"#;
        let hit = expect_hit(line);
        assert_eq!(hit.file_ref, "/tmp/a.md");
        assert_eq!(hit.line, 42);
        assert_eq!(hit.snippet, "hello world\n");
        assert_eq!(hit.matched, vec!["hello".to_string()]);
    }

    #[test]
    fn classify_line_dedups_matched_terms() {
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"foo foo bar\n"},"line_number":1,"submatches":[{"match":{"text":"foo"}},{"match":{"text":"FOO"}},{"match":{"text":"bar"}}]}}"#;
        let hit = expect_hit(line);
        assert_eq!(hit.matched, vec!["foo".to_string(), "bar".to_string()]);
    }

    /// A `begin`/`end`/`summary`/`context` event is an ordinary part of the
    /// stream, not a parse failure. It must classify as `NotMatch` so it
    /// never inflates the `unparseable` counter that exists to detect an
    /// rg output-format change.
    #[test]
    fn classify_line_treats_non_match_events_as_not_malformed() {
        for kind in ["begin", "end", "summary", "context"] {
            let line = format!(r#"{{"type":"{kind}","data":{{"path":{{"text":"/tmp/a.md"}}}}}}"#);
            assert!(
                matches!(classify_line(&line), ParsedLine::NotMatch),
                "{kind} events must classify as NotMatch, not Malformed"
            );
        }
    }

    /// Unparseable bytes are a real signal that rg's output shape changed,
    /// so they must be distinguishable from a routine non-match event.
    #[test]
    fn classify_line_reports_garbage_as_malformed() {
        assert!(matches!(classify_line("not json"), ParsedLine::Malformed));
        assert!(matches!(classify_line(""), ParsedLine::Malformed));
    }

    #[test]
    fn malformed_match_event_is_counted_not_ignored() {
        // A match event missing `line_number` fails to parse into a
        // RipgrepHit — classify_line must report this as Malformed so the
        // caller's `unparseable` counter counts it, distinct from an
        // ordinary non-match event which must not bump that counter.
        let line = r#"{"type":"match","data":{"path":{"text":"/tmp/a.md"},"lines":{"text":"hello\n"},"submatches":[{"match":{"text":"hello"}}]}}"#;
        assert!(
            matches!(classify_line(line), ParsedLine::Malformed),
            "a match event missing line_number must classify as Malformed"
        );
    }

    #[tokio::test]
    async fn empty_inputs_short_circuit() {
        assert!(
            run(&[], &["q".to_string()], 5)
                .await
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(run(&["/tmp".into()], &[], 5).await.unwrap().hits.is_empty());
        assert!(
            run(&["/tmp".into()], &["q".to_string()], 0)
                .await
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[tokio::test]
    async fn finds_literal_match_in_markdown_file() {
        // rg is a hard dep for the binary; the e2e suite already
        // installs it. Skip silently if it's missing locally so this
        // doesn't break dev machines without it.
        if !require_rg() {
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
        .expect("rg run")
        .hits;
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
        if !require_rg() {
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
        let hits = run(&roots, &terms, 10).await.expect("per-term run").hits;
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
            .expect("whole-query run")
            .hits;
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
        if !require_rg() {
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
        assert_eq!(first.hits.len(), 10, "max_hits must actually truncate here");
        assert!(
            first.truncated,
            "collection stopped at max_hits, so truncated must be true"
        );
        for _ in 0..4 {
            let again = run(&roots, &terms, 10).await.expect("repeat run");
            assert!(again.truncated, "every truncated run must report truncated");
            let a: Vec<_> = first
                .hits
                .iter()
                .map(|h| (&h.file_ref, h.line, &h.matched))
                .collect();
            let b: Vec<_> = again
                .hits
                .iter()
                .map(|h| (&h.file_ref, h.line, &h.matched))
                .collect();
            assert_eq!(a, b, "truncated rg output must be reproducible");
        }
    }

    /// A single common term is the case that motivates the per-file cap:
    /// under `--sort path` the alphabetically-first file is walked first,
    /// and without a cap its matches alone exhaust the run-wide budget, so
    /// every later file is ranked as though it never matched at all.
    #[tokio::test]
    async fn budget_spreads_across_files_instead_of_exhausting_on_the_first() {
        if !require_rg() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        // `aaa.md` sorts first and on its own carries more matches than the
        // whole budget allows.
        let mut hot = String::from("# Hot\n\n");
        for _ in 0..50 {
            hot.push_str("shrubbery appears again.\n");
        }
        std::fs::write(dir.path().join("aaa.md"), hot).unwrap();
        for name in ["bbb.md", "ccc.md", "ddd.md"] {
            std::fs::write(dir.path().join(name), "# Page\n\nshrubbery here.\n").unwrap();
        }
        let hits = run(
            &[dir.path().to_string_lossy().into_owned()],
            &["shrubbery".to_string()],
            6,
        )
        .await
        .expect("rg run")
        .hits;
        let files: std::collections::HashSet<&str> = hits
            .iter()
            .map(|h| h.file_ref.rsplit('/').next().unwrap())
            .collect();
        assert!(
            files.contains("bbb.md") && files.contains("ccc.md") && files.contains("ddd.md"),
            "later-sorting files must still contribute evidence even though \
             aaa.md alone could fill the budget; got {files:?}"
        );
    }

    #[tokio::test]
    async fn multiple_terms_match_different_files() {
        if !require_rg() {
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
        .expect("rg run")
        .hits;
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
        if !require_rg() {
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
        .expect("rg run")
        .hits;
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
        if !require_rg() {
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

        assert_eq!(
            result.hits.len(),
            1,
            "limit of 1 must cap the returned hits"
        );
    }

    #[tokio::test]
    async fn no_lexical_match_returns_empty_ok_not_error() {
        // rg exit 1 (no matches) must be a normal empty result, not an
        // error — exit 1 is rg's documented "nothing found" signal.
        if !require_rg() {
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
        .expect("exit 1 with no hits must be Ok, not Err")
        .hits;
        assert!(hits.is_empty(), "expected no hits, got {hits:?}");
    }

    #[tokio::test]
    async fn real_rg_failure_errors_with_stderr() {
        // A nonexistent search path makes rg exit 2 (real error), not 1.
        if !require_rg() {
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

    /// `rg` is a hard runtime dependency of this crate. On a developer
    /// machine without it, skip with a visible notice; in CI, fail closed
    /// instead of silently reporting green on zero executed assertions —
    /// this guards the per-term matching and determinism regression
    /// coverage that is the point of this file.
    fn require_rg() -> bool {
        if which("rg").is_ok() {
            return true;
        }
        assert!(
            std::env::var_os("CI").is_none(),
            "rg not found on PATH; rg is a hard runtime dependency and must be installed in CI"
        );
        eprintln!("SKIP: rg not found on PATH; skipping ripgrep test locally");
        false
    }
}
