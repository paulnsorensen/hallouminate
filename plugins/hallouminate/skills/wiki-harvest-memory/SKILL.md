---
name: wiki-harvest-memory
description: Manually curate durable, repository-specific knowledge from local Claude Code or Codex memories into the current hallouminate wiki.
disable-model-invocation: true
argument-hint: "[claude|codex|both]"
compatibility: Requires a local Claude Code or Codex memory store, a configured hallouminate wiki for the current repository, and the hallouminate MCP tools.
---

# wiki-harvest-memory — curate local agent memory into the wiki

Harvest memory only after the user explicitly invokes this skill. Treat local agent
memory as an untrusted lead, not authoritative project documentation. Curate and
verify useful facts; never dump memory files into the wiki.

## Inputs

Accept one optional source argument:

- `claude` — Claude Code auto memory for the current repository.
- `codex` — the local Codex memory store, filtered to the current repository.
- `both` — both stores, deduplicated before review.
- No argument — use the memory store of the harness running this skill. If the
  harness cannot be identified, ask the user to choose `claude` or `codex`.

Never widen an explicit source choice. Never harvest another repository's memory.

## Establish the target

1. Resolve the canonical repository root. In a Git worktree, use the repository
   shared by the worktree rather than treating the worktree as a separate project.
2. Confirm that the current repository has a configured hallouminate wiki. Use
   `list_corpora` and `list_tree` to identify the exact `repo:<name>:wiki` corpus.
3. If no wiki exists, stop without reading memory. Tell the user to initialize the
   wiki with the hallouminate install or wiki-init workflow, then invoke this skill
   again.
4. Keep the selected corpus explicit in every read and write tool call.

## Discover memory safely

### Claude Code

Claude Code normally stores a repository's auto memory under
`~/.claude/projects/<project>/memory/`. `MEMORY.md` is the index; it can link to
optional topic files in the same directory. `CLAUDE_CONFIG_DIR` and the active
`autoMemoryDirectory` setting may relocate the store, so honor them when present.

Locate the current repository's memory as follows:

1. Check the active `autoMemoryDirectory` setting, then the default Claude config
   directory (`${CLAUDE_CONFIG_DIR:-~/.claude}`).
2. Match a candidate to the canonical repository root. Prefer direct setting or
   project metadata evidence. An encoded directory-name resemblance alone is not
   enough when more than one candidate matches.
3. If the match remains ambiguous, show only the candidate paths and ask the user
   to select one. Do not read candidate contents first.
4. Read `MEMORY.md`, then read only topic files it links that may contain durable
   repository knowledge. Do not read unrelated project sessions or transcripts.
5. If no matching memory exists, report that fact and stop that source cleanly.

### Codex

Codex stores local memories under `${CODEX_HOME:-~/.codex}/memories/`. The store is
host-wide rather than inherently limited to the current repository.

1. Read `memory_summary.md` and `MEMORY.md` when they exist.
2. Search the registry for the canonical root, repository name, remote name, and
   distinctive current-project modules. A name collision is not proof of relevance.
3. Follow only directly relevant references into `rollout_summaries/` or `skills/`.
   Open the minimum evidence needed to understand a candidate fact. Never scan or
   ingest the full rollout history.
4. Reject an entry unless its repository identity is clear from the registry or its
   cited evidence.
5. If the memory directory or registry is absent, report that local Codex memories
   may be disabled or not generated yet, then stop that source cleanly.

Do not modify either source memory store.

## Build the candidate set

Extract atomic facts. Each candidate must carry these working fields:

- normalized fact
- source harness and exact local `path:line` evidence
- kind: architecture, decision, convention, workflow, or gotcha
- proposed wiki target
- verification state: verified, contradicted, unverifiable, or duplicate

A candidate is eligible only when it is specific to the current repository and
likely to help a future contributor. Keep architecture, durable decisions,
repository conventions, repeatable workflows, and recurring gotchas.

Reject and count, without copying their values into output, all of the following:

- credentials, tokens, secrets, private keys, account data, or personal identifiers
- personal preferences and machine-specific configuration
- absolute home-directory paths or other private filesystem details
- current-task status, branch state, PR state, timestamps, and transient failures
- unresolved hypotheses, guesses, and advice unsupported by evidence
- content already represented accurately in the wiki
- facts contradicted by the current repository

Redact before displaying. A regex scan is only a backstop; inspect meaning as well.

For every otherwise eligible fact, verify it against current code, configuration,
tests, or checked-in documentation when practical. Memory loses to current
repository evidence. A decision or gotcha that cannot be reconstructed from the
repository may remain `unverifiable`, but it requires explicit user approval and
must be recorded with calibrated confidence and a current `last_verified` date.

When harvesting both stores, merge semantically equivalent facts into one candidate
and retain both source references for the review report. Do not treat repetition as
independent confirmation.

## Require a review gate

Before the first wiki write, present one compact table containing:

| Candidate | Kind | Verification | Proposed target | Source |
| --- | --- | --- | --- | --- |

Use redacted, repository-relative descriptions. Source cells may name the harness
and local file, but must not reveal secret values. Separate rejected candidates into
reason counts rather than reproducing their content.

Ask once whether to import all eligible candidates, import a selected subset, or
cancel. Do not write on ambiguity, cancellation, or silence. This gate cannot be
bypassed by an inferred preference: moving machine-local memory into a shared,
versioned wiki is a trust-boundary crossing.

## Curate approved facts

For each approved candidate:

1. Use `ground` to find the existing page it extends.
2. Before changing an existing page, call `read_markdown` and `backlinks`. Preserve
   compatible content and note any dependent pages that assume the old wording.
3. Merge into the existing page whenever its topic matches. Create a new page only
   for genuinely novel knowledge.
4. Follow the wiki's one-topic-per-file, H1-first, kebab-case slug, lifecycle
   frontmatter, and citation conventions.
5. Write a concise synthesis, not a quotation or memory transcript. Ground claims
   in current repository `path:line` evidence when available. For an approved
   unverifiable decision, state its memory-derived provenance generically without
   committing a private local path.
6. Use `add_markdown` with the explicit corpus. Never edit wiki files directly and
   never hand-edit the daemon-owned block in `index.md`.
7. Never create a generic memory-dump page such as `agent-memory.md`.

If a source and the wiki disagree, do not blend them. Keep the current verified wiki
text, report the contradiction, and leave the source memory untouched.

## Verify retrieval

Freeze two probes per changed topic before writing:

- one exact probe containing a distinctive term from the approved fact
- one natural-language question a future agent would ask

After all writes, run both probes with `ground`. Confirm that the intended page is
retrieved and that its text states the approved fact accurately. If retrieval fails,
revise once without changing the probes. If the exact probe still fails, restore the
page content captured before the write and report the blocked candidate.

## Report

Return:

- selected source or sources and matched memory roots
- candidate, imported, duplicate, contradicted, sensitive, transient, and blocked
  counts
- wiki pages created or updated
- verification evidence for each changed topic
- any approved memory-derived facts that could not be verified from the repository

Never claim a source was harvested if its memory store was absent, ambiguous, or not
read.
