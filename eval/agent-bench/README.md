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

- `wiki` — hallouminate MCP server attached, `ground` and read tools scoped
  to the repository's `repo:<name>:wiki` corpus only. The agent never sees a
  `repo:<name>:corpus` (semantic source search) corpus in this arm. Wiki-only
  scoping is what keeps the measured effect "the wiki helped" rather than
  "semantic source search helped" — mixing the two corpora would confound the
  claim under test.
- `baseline` — hallouminate MCP server not attached. The agent has only its
  native tools against the checked-out repository at the pinned commit.

Tasks are paired: the same question, the same subject repo and commit, and
the same non-hallouminate tool set run once per arm. Any difference in
outcome is attributable to wiki presence, not to a different task or a
different toolset.

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
to calibrate the automated judge, and agreement between human and automated
grades is reported at the pass threshold (fraction of the calibration subset
where the automated `pass` bool matches the human `pass` bool).

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
- `results_dir` — where this run's raw session and grade records live.

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

## Known confounds

- Wiki quality is agent-authored, not human-authored. A weak wiki-authoring
  session understates the wiki arm's ceiling; this pilot does not separate
  "wikis don't help" from "this particular wiki was authored poorly."
- `greppable` questions may show a small or negative effect from wiki
  presence (extra tool surface, extra context to sift through) — that is
  reported rather than hidden, since discriminating `wiki-only` benefit from
  `greppable` cost is the tag taxonomy's purpose.
