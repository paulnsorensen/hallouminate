---
status: reviewed
last_verified: 2026-07-29
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/pull/314
---
# Eval harness gotchas

Operational traps in the ground-retrieval eval harness
(`crates/hallouminate/tests/eval_ground_recall.rs`, `.github/workflows/eval.yml`,
`eval/baseline.json`) that cost real debugging time and are not visible from
reading the harness alone. This page is about *running* the harness; the
forward-looking design for the evaluation itself lives in
[ground-search-evaluation](ground-search-evaluation.md).

## ripgrep is a hard runtime dependency of the eval job

The eval harness measures a fused retrieval signal that includes a ripgrep
lexical pass, so `rg` must be installed wherever the eval runs. Production
resolves it with a bare `Command::new("rg")` (`crates/hallouminate-domain/src/search/ripgrep.rs:111`)
— PATH-resolved and unversioned.

`.github/workflows/eval.yml` never installed it. The `ubuntu-24.04` hosted image
does not ship `rg`, and unlike `ci.yml` — which has installed it since the
ripgrep signal landed and documents it as a hard dependency — the eval workflow
had no install step. Every CI run of the weekly job before PR #314 therefore
measured with a **silently degraded (empty) ripgrep signal**, while still
reporting a number.

`<certain>` — the first scratch harvest run failed with `rg --version: No such
file or directory`, which is how the gap was found at all.

**Consequence:** any eval measurement taken before PR #314 is suspect, including
the divergence that triggered the whole investigation. When adding a new eval
runner or workflow, install `rg` explicitly; nothing in the harness fails at
setup time if you forget.

## The degraded-signal gate denies by default over a vocabulary three crates own

`ensure_signals_intact` decides whether a sweep is measurable at all by
inspecting `GroundResponse.warnings`. The trap is that `Warning`
(`crates/hallouminate-domain/src/ground/types.rs:126`) carries only a `String`
code — no kind, no severity — while the codes themselves are emitted from three
different crates:

| Class | Codes | Emitted at |
| --- | --- | --- |
| Degraded signal (must fail the eval) | `ripgrep-unresolved`, `ripgrep-unparseable`, `ripgrep-failed`, `ripgrep-timeout` | `hallouminate-domain/src/search.rs:171,187,372,386` |
| Rerank fallback (judged by `rerank_completion`) | `rerank-timeout`, `crossencoder-unavailable` | `hallouminate-domain/src/ground/orchestrate.rs:204`, `hallouminate-daemon/src/dispatch.rs:431` |
| Advisory (must **not** fail the eval) | `code-repos-empty`, `cross-repo-union`, `index-coverage` | `hallouminate-domain/src/ground/format.rs:334,368`, `hallouminate-daemon/src/dispatch.rs` (`handle_ground`: cross-repo-union + #427 coverage-warning blocks) |

The gate originally allowed only the two rerank codes and failed on everything
else, so the two advisory codes would have aborted a run. They were inert only
because the eval pins `corpus: Some(CORPUS_NAME)`, which forces `union = false`.

PR #314 inverted it to fail on the degraded set explicitly, which is safe only
because `producer_warning_codes_match_the_workspace_sources` scans
`crates/*/src` for every `code: "<literal>"` and fails when one is classified by
no set. **If you add a warning code anywhere in the domain or daemon crates,
that test tells you to classify it** — do not "fix" it by widening the advisory
list without deciding whether the code means a broken signal.

`<speculative>` the scan matches the literal `code: "` prefix, so a code built
via `format!` or held in a constant would evade it. Every construction site uses
a string literal today.

## `run_arm` runs the sweep twice; only the second one is a measurement

`run_arm` calls `run_sweep` twice — a discarded warmup pass to absorb cold-start
cost, then the measured pass whose output becomes the artifact. Anything that
hard-fails inside `run_sweep` therefore fails on a pass whose results nobody
reads. `RIPGREP_TIMEOUT` is a flat 1 s (`hallouminate-domain/src/search.rs:115`),
so a cold, contended runner can trip it on an early warmup query and red the
weekly cron while the measured pass is entirely healthy.

The signal gate is now scoped to `SweepKind::Measured` for this reason.
`rerank_completion` still judges both sweeps deliberately: a rerank fallback
means the arm is misconfigured, which warming up does not fix.

## Latency is recorded but never compared

`compare_against_baseline` reads only `query_set_digest`, `quality.recall_at_5`,
`quality.mrr`, and each query's `top_chunk_pass`. The `latency` block in
`eval/baseline.json` is provenance, not a gate — the committed baseline's
`cold_load_ms` moved 191 → 2823 across a re-baseline with nothing failing. The
only latency assertion anywhere is the intra-artifact invariant
`warm_p50_ms <= warm_p95_ms`. Do not read a latency change in a baseline diff as
a regression the gate caught; it did not.

`ripgrep_version` (added in schema 4) is likewise recorded and never compared —
see ADR-002 in
[eval-baseline-rg-provenance-adrs](eval-baseline-rg-provenance-adrs.md).

## Tests that exec a freshly written script hit ETXTBSY under the parallel harness

A test that writes a stub executable and immediately runs it fails
intermittently with `Text file busy (os error 26)`. Rust's test harness runs
tests on parallel threads; a sibling thread's `fork` inherits the still-open
write handle to the new file, and the `execve` fails even though the script is
complete and `chmod 0755`.

`<certain>` — reproduced at roughly 1-in-7 full-suite runs while passing 5/5 in
isolation, which is exactly the shape that reads as "works on my machine".

**Workaround:** a `static EXEC_LOCK: Mutex<()>` held across write-and-exec in
*every* test in the binary that spawns a process. Guarding only the tests that
write stubs is not enough — a test that execs a deliberately missing binary
still forks first, and that fork is enough to trip a sibling. Take the guard
before the write, not just before the exec.

_Source: PR #314 and the eval re-baseline investigation · Updated: 2026-07-29 · Supersedes: —_
