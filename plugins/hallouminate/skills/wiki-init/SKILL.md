---
name: wiki-init
description: Bootstrap a hallouminate wiki from scratch by interviewing the user with Socratic questioning, then writing the first entries. Use when a repo has no wiki yet or the corpus is near-empty — "start a wiki", "bootstrap the knowledge base", "interview me about this project", "set up the wiki", "/wiki-init". An opus root runs a semi-structured interview (one question per turn, behavior-first probes) and plans the page taxonomy; haiku sub-agents fan out to draft the captured topics into one-topic-per-file entries in parallel via `add_markdown`. Do NOT use to answer questions (use wiki-query) or to fold new source docs into an existing wiki (use wiki-ingest).
---

# wiki-init — Socratic bootstrap of a hallouminate wiki

Turn what's in the user's head into a structured, searchable wiki. The hard part is
**elicitation** — experts can't introspect tacit knowledge on demand, so you ask
about *behavior in concrete situations* and extract the model from their answers.

**Agent topology (required):**

- **Root = opus.** Runs the interview, holds the dialogue history, decides the page
  taxonomy, and extracts answers into structured slots. All reasoning lives here.
- **Fan-out = haiku.** Once topics are captured, one sub-agent per planned page drafts
  the entry and calls `add_markdown`. Drafting is mechanical formatting of slots the
  root already gathered — cheap, parallel, isolated.

The root interviews; it does not draft pages itself. Haiku drafts; it does not
interview. Never invert.

## Phase 1 — Interview (root / opus)

Semi-structured beats both a rigid script and open rambling. Move
**unstructured → semi-structured → structured**: open wide to learn the domain's
vocabulary, then tighten to fill gaps.

**Rules of the interview:**

- **One question per turn.** Multiple questions → the user answers the easiest and
  drops the rest.
- **Behavior-first, not knowledge-first.** "What do you know about X?" fails. Ask
  "walk me through what you do when X happens" and extract the model from the story.
- **Internal slots, open phrasing.** Track slots (`purpose`, `components`,
  `constraints`, `gotchas`, `decisions/why`, `edge-cases`) silently. Never say the
  slot name — ask "what would break this?" not "what are the constraints?".
- **Don't repeat.** Keep dialogue history; suppress questions overlapping an
  already-filled slot.
- **No leading questions.** Don't smuggle the answer into the question.

**ACTA sequence (the practical spine):**

1. **Task diagram** — first, scope it: *"Break this project into more than three but
   fewer than six major areas — what are they?"* This sets the page list without
   premature detail.
2. **Knowledge audit** — per area, probe with the high-yield question types:

   | Probe | Asks for | Example |
   |---|---|---|
   | Tour | the mental model | "Walk me through how X works here." |
   | Taxonomic | categories | "What kinds of Y are there?" |
   | Reason-seeking | rationale | "Why this way and not the obvious alternative?" |
   | Constraint | non-obvious limits | "What can go wrong? What breaks it?" |
   | Counterfactual | edge cases | "What if [current condition] weren't true?" |
   | Consistency | contradictions | "Earlier you said X — does that fit with Y?" |
   | Elaboration | depth | "Tell me more about that." |

3. **Simulation** — for tricky areas: *"Imagine situation Z lands — what do you do,
   and why?"* Surfaces decision logic direct questions miss.

**Chain the questions.** Derive each next question from the prior answer — pick one
thread to deepen, park the others, come back. Stop an area when probes stop
yielding new slots; stop the session when the user signals done or the task diagram's
areas are all covered.

After each exchange, extract what was said into a structured slot record (keep it in
your working notes), separating *what was said* from *how it'll be written*.

## Phase 2 — Plan the taxonomy (root / opus)

- One **topic per file**, kebab-case slug, first line `# Title` (the chunker uses the
  H1 as the breadcrumb root and the index gloss).
- Top-level files for foundational topics (`architecture.md`, `mcp-surface.md`);
  subdirectories for clusters (`adapters/lance.md`). The daemon creates dirs and
  maintains each `index.md` link list for you.
- Shape every page on the pack's formal entry template
  (`../../templates/wiki-entry.md` from this skill's directory): lifecycle
  frontmatter, H1 first, footnote citations, provenance footer.
- Write a **`wiki-conventions.md`** first (the wiki's constitution): slug rules, the
  H1 rule, one-topic-per-file, the voice, and a provenance-footer convention
  (`_Source: <how we know this> · Updated: <date> · Supersedes: <if any>_`). Declare
  the `Supersedes:` field here so `wiki-ingest` writes and reads one key. Also declare
  the two ingest-ledger conventions so `wiki-ingest` reads and writes one shape:
  - **`log.md`** — an append-only ingest journal at the corpus root under a stable
    `## Log` heading. `wiki-ingest` appends one row per decision via
    `add_markdown { under_heading: "Log", position: "append" }`; it is **never**
    rewritten and **never** a routing/merge target.
  - **`sha256sum` source-hash ledger** — `wiki-ingest`'s Layer-1 dedup hashes each
    normalized source (`sha256sum`, first 16 hex chars) and records the id in `log.md`;
    a ledger hit skips the whole source. Note the hash convention here so the id format
    is stable across ingests.
  Later skills (`wiki-ingest`) read all of this to stay consistent.
- Pick the corpus: `repo:{name}:wiki` for the repo, or ask if ambiguous
  (`list_corpora`). Confirm the page list with the user before fanning out.

## Phase 3 — Fan out drafts (haiku, parallel)

Spawn one haiku sub-agent per planned page, **in a single message**, each with:

- the slug + path, the H1 title, and the slot record for that topic,
- the `wiki-conventions.md` rules,
- this contract:

> Draft a one-topic markdown entry from these slots. First non-blank line is
> `# <Title>`. Lead with the conclusion, ~50–150 lines, concrete over abstract,
> cite code as `path:line` where the slots name files. Add the provenance footer.
> Then call `add_markdown { corpus, path, content, overwrite: false }` — the target
> corpus must be single-root (`add_markdown` rejects multi-root corpora);
> `repo:{name}:wiki` is single-root. Return the path written and any lint `warnings`
> from the response. Do NOT interview the user.
> Do NOT invent facts beyond the slots — if a slot is thin, write only what's there
> and note the gap.

The daemon reindexes each file and refreshes ancestor `index.md` link lists
automatically on write.

## Phase 4 — Stitch (root / opus)

- Collect the written paths and lint warnings; fix any flagged entry (heading jumps,
  empty links) with an `overwrite: true` redraft.
- `list_tree` to confirm the shape; write or refine the top-level `index.md` prose
  (outside the `<!-- HALLOUMINATE:INDEX-START -->` / `<!-- HALLOUMINATE:INDEX-END -->`
  markers — the daemon owns the link list between them).
- **Scaffold an empty `log.md`** at the corpus root so `wiki-ingest` has a ledger to
  append to from its first run: `add_markdown { corpus, path: "log.md",
  content: "# Ingest Log\n\n## Log\n", overwrite: false }`. It stays append-only thereafter.
- Report the page list to the user and name the gaps the interview didn't reach
  (hand-off candidates for a later `wiki-init` continuation or `wiki-ingest`).

## Rules

- Root interviews and reasons; haiku drafts. One question per turn.
- Ask about behavior in concrete situations, not abstract knowledge.
- Track slots internally; phrase every question open-ended.
- One topic per file, H1 first line, kebab slug — non-negotiable for retrieval.
- Confirm the page taxonomy with the user before writing anything.
- Fan out page drafts in one message so they run in parallel.
- Never write a fact the interview didn't establish; note thin spots as gaps.
