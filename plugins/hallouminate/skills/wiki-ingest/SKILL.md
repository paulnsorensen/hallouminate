---
name: wiki-ingest
description: Fold new knowledge into an existing hallouminate wiki — route each new fact to the page it extends, merge it in, create a page only when genuinely novel, and never blend contradictions. Use when there's source material to absorb or a fact to record — "add this to the wiki", "ingest these docs", "update the wiki with what we learned", "remember this", "record this decision", "/wiki-ingest <path|topic>". An opus root plans dedup/route/merge/contradiction decisions; haiku sub-agents fan out to `ground` each candidate against the corpus and read the target pages. Do NOT use to bootstrap an empty wiki (use wiki-init) or to answer a question (use wiki-query).
---

# wiki-ingest — incremental ingest & update

A wiki is a **compiled knowledge representation, not a retrieval dump.** New material
doesn't get appended blindly — it routes to the page it belongs on, merges in, and
only spawns a new page when nothing covers it. The failure mode to avoid: dumping
raw content that leaves the real pages stale. A smaller, curated wiki beats a larger
unvetted one.

**Agent topology (required):**

- **Root = opus.** Splits source material into atomic claims, decides route vs.
  merge vs. overwrite vs. new, judges contradictions, and writes the final entries.
  Every judgment call lives here.
- **Fan-out = haiku.** One sub-agent per candidate claim/topic: runs `ground` to
  find the nearest existing page, reads it, and returns the match, its similarity
  score, and the relevant existing lines. Retrieval and reading are fanned out;
  decisions are not.

## Phase 1 — Atomize (root / opus)

- Take the source (a file path, pasted doc, a conversation takeaway, a decision) and
  split it into **atomic claims** — one topic each, the same granularity as a wiki
  page section. Don't ingest a 10-page doc as one blob.
- Read `wiki-conventions.md` (the wiki's constitution) for slug/voice/merge rules.
  If absent, fall back to hallouminate's authoring conventions (one topic per file,
  H1 first line, kebab slug).
- Pick the corpus (`repo:{name}:wiki` or ask).

## Phase 2 — Locate (haiku, parallel)

Spawn one haiku sub-agent per atomic claim, **in a single message**, each with this
contract:

> Run `ground { query: "<claim topic>", corpus: "<corpus>", top_files: 3, chunks_per_file: 3 }`.
> Return the best-matching existing page: its **corpus-relative path**, the file-level
> `mtime`, and from the top chunk its `score`, `heading_path`, `line_range`, and
> `snippet`. (`ground` keys its `docs` by *absolute* path and `mtime` is file-level,
> not per-chunk — convert the key to the corpus-relative path, the same shape
> `read_markdown`/`add_markdown` take, since they reject absolute paths.) If the top
> score is low / nothing relevant, return `{ match: none }`. Do NOT edit anything —
> you only locate. If the match looks close, `read_markdown { corpus, path }` that
> page (relative path) and return the section that would be updated.

## Phase 3 — Decide (root / opus)

For each claim, use the `ground` score and the read-back section as the dedup signal
(hallouminate's hybrid score stands in for raw cosine similarity):

| Signal | Decision |
|---|---|
| Page already states this claim (near-identical) | **Skip** — no write. Note "already covered". |
| Strong topical match, page is missing/partial on this | **Merge** — update the page, fold the claim into the right section. |
| New claim **contradicts** the page | **Do not blend.** Judge it (Phase 3a). |
| Weak / no match | **New page** — only when no existing page owns the topic. |

Map the literature's bands onto hallouminate's `ground` score: very high → skip,
high-but-incomplete → merge, low → new page. Treat the band edges as a judgment
call, not a hard cutoff — read the page before deciding.

**Phase 3a — contradiction (LLM-as-judge, root):** When the new claim conflicts with
an existing page, do NOT average them — blending produces confident wrong answers.
Judge: is the new source more authoritative or more recent (compare `mtime`, source
provenance)?

- **Newer + authoritative** → overwrite the stale assertion, and record what
  superseded what in the provenance footer's `Supersedes:` field (`Supersedes:
  <what> · <date>`).
- **Unclear** → keep both, mark the conflict inline (`> ⚠️ Conflicts with <other>:
  <summary> — needs human resolution`), and flag it to the user. Never silently pick.

## Phase 4 — Write (root / opus)

Apply each decision through the safe update loop:

- **Merge/overwrite:** `read_markdown` the page → edit the section → `add_markdown
  { overwrite: true }`. (Read-before-clobber is mandatory; it's your rollback point.)
- **New page:** draft one-topic entry (H1 first line, kebab slug, lead-first,
  ~50–150 lines, code cited as `path:line`, shaped on the pack's
  `../../templates/wiki-entry.md`) → `add_markdown { overwrite: false }`.
- **Provenance footer on every touched page:**
  `_Source: <where this came from> · Updated: <date> · Supersedes: <if any>_`
  Freshness is a first-class signal — stale pages produce confident-wrong answers.

The daemon reindexes each written file and refreshes ancestor `index.md` link lists
automatically. For edits made **outside** these tools, run `index` to re-embed.

## Phase 5 — Report (root / opus)

Summarize per claim: **skipped / merged into `path` / new `path` / conflict flagged**.
Surface every flagged contradiction to the user by name — those are the ones that
need a human call. Note any page that's now large enough to split (one-topic-per-file
drift).

## Rules

- Route and merge before you create — a new page is the last resort, not the default.
- Read the target page before overwriting it. Always.
- Never blend contradictory claims; newer-authoritative wins with recorded
  provenance, otherwise keep both and flag for human resolution.
- Root decides route/merge/contradiction; haiku only locates and reads. Don't invert.
- Fan out the locate step in one message so searches run in parallel.
- Stamp a provenance/updated footer on every page you touch.
- Curate, don't accumulate — skip duplicates, split bloated pages, retire the stale.
