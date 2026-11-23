# Wiki-grounding agent benchmark (pilot)

This is a pilot, not a public benchmark. It measures whether giving a coding
agent access to the hallouminate wiki changes task outcomes on real questions
against real repositories, under a protocol disciplined enough that the
result can be trusted internally. Headline marketing claims are an explicit
non-goal — the sample sizes and repo count here are not built for that.

This pilot is independent of the retrieval evaluation in `eval/` (`eval/queries.json`,
`just eval`). That harness measures Ground's ranking quality against a frozen
Markdown corpus; this harness measures whether an agent with wiki access answers
real questions about real repos better than an agent without it. Neither touches
the other.

## Goal and claim under test

The claim under test: for a coding agent working against an external
repository, exposing hallouminate's wiki corpus for that repository changes
the agent's ability to answer questions about the repository — measured as
pass rate and tokens-to-correct-answer — relative to an otherwise identical
agent with the same native tools and no wiki access. The pilot does not test
whether hallouminate's semantic source search (`repo:<name>:corpus`) helps;
that is a different, already-measured claim (see `eval/README.md`). It does
not test wiki quality in the abstract, agent framework choice, or model
choice — those are held fixed so the wiki-presence delta is the only thing
that moves.

## Arms

Each question runs under two arms, `wiki` and `baseline`, backed by
`agent_bench::Arm`. Both arms give the agent the same prompt, the same task,
and the same native tools (file read, grep, shell). The sole delta between
arms is whether the hallouminate MCP server is attached and, if attached,
what corpus it exposes:

- `wiki` — one private Hallouminate MCP server. It exposes only the frozen
  `benchmark-wiki` corpus. The daemon mounts the wiki read-only.
- `baseline` — no Hallouminate MCP server, wiki files, or wiki socket.

### Measurement isolation

`bench-run` extracts the same pinned commit into two separate read-only
source views. Both arms see `/workspace`. Neither can read the host
checkout, its Git metadata, its `.hallouminate` directory, or the host home.
Both arms use fresh homes and ignore ambient Claude Code settings and MCP
servers. Only the wiki arm can reach the private daemon's Unix socket.

Before running either arm, the harness starts the daemon with one explicit
corpus. It calls `list_corpora` and `read_markdown` through the wiki arm's
MCP server. It checks the exact corpus list and the bytes of a known page.
A failure stops the run before it writes results.

This verifies the endpoint, not whether Claude Code attaches to it or
uses it on each trial. Confirm attachment separately before interpreting
pilot results.

### Preparing subject-repo checkouts

Live subject trials are blocked. The runner cannot isolate model-service
credentials from subject tools, so it stops before starting an agent or
writing results. This applies to both API keys and OAuth tokens.

For offline integration tests, run `bench-run --offline-agent PATH` with a
trusted local stand-in. Use Linux with Bubblewrap at `/usr/bin/bwrap` and
user namespaces enabled. Set `AGENT_BENCH_HALLOUMINATE_BIN` to a standalone
Hallouminate executable if the PATH entry is a package-manager wrapper.
Neither subject arm has network access or model-service credentials.

The private daemons can fetch the retrieval model into a temporary cache
shared by the sweep. Their startup can take up to ten minutes, and they do
not exit when idle. Neither the agent nor the daemon gets the host home or
socket directories.

Clone each subject repo to `<checkout_root>/<name>`. Check out the pinned
commit. Author the wiki in the checkout before measuring.

```sh
git clone <subject_repo.url> <checkout_root>/<subject_repo.name>
git -C <checkout_root>/<subject_repo.name> checkout <subject_repo.commit>
```

`bench-run` and `bench-author` verify every checkout before doing any work:
a missing directory, or a `HEAD` that does not match the manifest's pinned
`commit`, is a hard failure naming both SHAs. A drifted checkout silently
invalidates every number produced against it, so this is never a warning.

## Subject repos

Two external OSS repositories, each pinned to a full commit SHA recorded in
the manifest's `subject_repos` (`agent_bench::SubjectRepo`): one `small` and
one `large` per `agent_bench::SizeClass`. Selection criteria: rich issue/PR
history (so wiki-only and greppable questions both have real material to
draw on) and a license permitting snapshot redistribution inside this
repository's fixtures.

Concrete repo selection is an open human task. This document does not name
candidates — naming them here would freeze a choice nobody has evaluated
against the criteria above.

## Wiki authoring protocol

For each subject repo, an agent authors the wiki under a fixed, logged token
budget. The budget and the actual token spend are both recorded — wiki
authoring cost is itself a reportable number inside the benchmark, not a
sunk cost hidden from the results.

Freeze ordering: the wiki for a subject repo is frozen — no further edits —
before any question about that repo is authored. This ordering exists so
questions cannot be shaped, even unconsciously, around whatever the wiki
happens to already say. The freeze is recorded in the manifest's
`prompt_hashes` (`agent_bench::PromptRef`, one entry per frozen wiki-authoring
prompt) once the wiki-authoring session completes.

## Question authoring protocol

Questions and gold answers are human-curated, not agent-generated. Each
question carries a `agent_bench::QuestionTag`:

- `wiki-only` — answerable from the wiki, not reliably findable by grep alone
  in the time an agent would spend.
- `greppable` — answerable by reading or searching the repo directly; the
  wiki should not be necessary.
- `abstention` — the gold answer asserts the fact is *not* recorded in either
  the wiki or the repo, and the correct agent behavior is to say so rather
  than fabricate an answer.

Each subject repo has a minimum tag balance floor (a minimum count of each
of the three tags) so no repo's question set can be entirely one tag and
silently dodge the comparisons that matter.

Freeze ordering: the question set is frozen — no further edits — before any
measured run starts, and always after the wiki for every subject repo it
questions is already frozen. The frozen question set's blake3 hash is
recorded as `question_set_hash` in the manifest (`agent_bench::Manifest`).
Editing the question set after this hash is recorded invalidates every run
measured against it (see Invalidation rules).

## Runs and metrics

Each task-arm pair (one question, one arm) runs a minimum of 10 runs
(`agent_bench::SessionRecord`, one record per run, keyed by `run_index`).

- `pass@k` — probability that at least one of the first `k` runs for a
  task-arm passes the judge threshold.
- `pass^k` — probability that all of the first `k` runs for a task-arm pass
  the judge threshold.

Both are computed per arm and per tag, and reported per question and
aggregated (`agent_bench::ArmSummary.pass_at_k` / `.pass_pow_k`, keyed by
`k`). `tokens_to_correct_answer` is the mean total token usage
(`agent_bench::TokenUsage::total`) across only the runs that passed, `None`
when a task-arm has zero passes.

The between-arm difference on each headline metric is reported with a paired
bootstrap confidence interval (`agent_bench::PairedCi`), resampling
*questions* — not individual runs — as the paired unit, since `wiki` and
`baseline` share the same question set and the pairing is what removes
question-difficulty variance from the estimate.

## Token accounting

Every session's usage is the four fields Anthropic reports per API call,
named here verbatim because downstream code depends on the exact names:
`input_tokens`, `output_tokens`, `cache_read_input_tokens`, and
`cache_creation_input_tokens` (`agent_bench::TokenUsage`). These are sourced
from the session result object emitted by `claude -p --output-format json` —
not estimated, not parsed from a transcript.

`tokens_to_correct_answer` is computed from the *sum* of all four fields
(`TokenUsage::total`), because cache reads and cache writes are real cost and
real latency even though they are not "generated" tokens. A single opaque
total reported by a tool without the four-field breakdown is never accepted
as a substitute — if a session's usage object doesn't carry all four fields,
the session is not counted.

## Judging

Each session is graded 0–5 against rubric anchors tied to the question's
gold answer and `rubric_notes` (`agent_bench::Question`):

- 0 — no relevant content, or a fabricated answer where the gold answer is a
  known fact.
- 1–2 — attempts the question but is materially wrong or missing the key
  fact.
- 3 — partially correct; the key fact is present but incomplete or hedged
  incorrectly.
- 4 — correct and complete against the gold answer's substance.
- 5 — correct, complete, and additionally reflects the abstention/precision
  behavior the question tag demands (e.g. an `abstention` question that
  correctly declines to guess).

`pass` is derived from `score >= threshold` (`agent_bench::GradeRecord::grade`,
`GradeRecord::passes`). Judging is arm-blind: the judge sees the question and
the answer text, never which arm produced it. A human-graded subset is used
to calibrate the automated judge. Exact-score agreement and pass-threshold
agreement are both reported, but the gate (`--min-kappa`, default 0.60) is on
Cohen's kappa. The threshold must be finite. Class skew can inflate raw
agreement. If a judge always says "pass", its raw agreement equals the
pass rate, but its kappa is zero when the labels vary.
A labelled subset with no variance (every item passes, or every item fails,
under both raters) leaves kappa undefined and fails calibration — such a
subset cannot certify a judge at all.

## Pinning and reproduction

Every run is reproducible from its manifest (`agent_bench::Manifest`):

- `model_ids` — subject and judge model identifiers (`ModelIds.subject`,
  `ModelIds.judge`).
- `claude_code_version` — the exact Claude Code build used for subject
  sessions.
- `subject_repos` — name, URL, pinned commit SHA, and size class for every
  subject repo (`SubjectRepo`).
- `prompt_hashes` — path and blake3 hash for every frozen prompt, including
  the wiki-authoring prompts (`PromptRef`).
- `question_set_hash` — blake3 hash of the frozen question set.
- `container_image_refs` — digests of every container image the run executed
  in.
- `results_dir` — the directory for session records, grades, and reports.
  The sweep, judge, calibration, and report recipes use this path relative to
  the benchmark repository root.

Raw traces (`SessionRecord.transcript_path`, one full transcript per run) are
retained under `results_dir` for every run, not just failures, so a
surprising result can be re-read rather than re-run.

## Invalidation rules

Any of the following forces a re-freeze — the affected artifact must be
re-hashed and every run measured against the old hash is no longer valid
evidence for the claim under test:

- Editing a subject repo's wiki after question authoring for that repo has
  begun.
- Changing a prompt's text without rotating its recorded hash in
  `prompt_hashes`.
- Changing either model ID in `model_ids` (subject or judge).
- Editing the question set after `question_set_hash` has been recorded.
- Repointing `checkout_root`, or moving a checkout to a different commit,
  between runs that are compared to each other.

`run-meta.json` binds the session ledger to the manifest and the actual
question set, source snapshots, frozen wikis, binaries, and isolation
configuration. A mismatch or missing metadata on an existing ledger
requires a fresh results directory. `--force` replaces trials only under
the same fingerprint and never overwrites an earlier transcript.

Automated grades bind the complete session record and exact grading inputs.
`bench-judge` and `bench-report` reject stale or legacy grades.
After replacing a session, use `bench-judge --force` to regenerate grades.
An interrupted forced regrade keeps completed replacements and untouched
valid grades. A normal rerun grades any sessions still missing a valid grade.
For a custom `--pass-threshold`, pass the same value to `bench-report`.

## Known confounds

- Wiki quality is agent-authored, not human-authored. A weak wiki-authoring
  session understates the wiki arm's ceiling; this pilot does not separate
  "wikis don't help" from "this particular wiki was authored poorly."
- `greppable` questions may show a small or negative effect from wiki
  presence (extra tool surface, extra context to sift through) — that is
  reported rather than hidden, since discriminating `wiki-only` benefit from
  `greppable` cost is the tag taxonomy's purpose.
