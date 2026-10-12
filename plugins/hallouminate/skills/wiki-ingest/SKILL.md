---
name: wiki-ingest
description: Folds new source material or a recorded fact into an existing hallouminate wiki, merging each claim into the page it extends. Use when the user says "add this to the wiki", "ingest these docs", "update the wiki with what we learned", "remember this", "record this decision", or invokes /wiki-ingest <path|topic>. Do NOT use to bootstrap an empty wiki (/wiki-init) or to answer a question (/wiki-query).
argument-hint: "[path|topic]"
---

# wiki-ingest — incremental ingest & update

A wiki is a **compiled knowledge representation, not a retrieval dump.** New material
doesn't get appended blindly — it routes to the page it belongs on, merges in, and
only spawns a new page when nothing covers it. The failure mode to avoid: dumping
raw content that leaves the real pages stale. A smaller, curated wiki beats a larger
unvetted one.

Every hallouminate MCP tool call takes a required `cwd`: the absolute path of
your own active checkout. In a git worktree, this can differ from the
harness's original directory. Pass the same `cwd` to every call in this skill.

**Agent topology (required):**

- **Root = opus-tier** (the strongest model the harness offers). Splits source
  material into atomic claims, decides route vs.
  merge vs. overwrite vs. new, judges contradictions, and writes the final entries.
  Every judgment call lives here.
- **Fan-out = haiku-tier** (the cheapest model). One sub-agent per candidate
  claim/topic: runs `ground` to
  find the nearest existing page, reads it, and returns the match, its similarity
  score, and the relevant existing lines. Retrieval and reading are fanned out;
  decisions are not.

## Phase 1 — Hash and atomize (root / opus)

1. Take the source: the text after the skill name (a file path or topic). When that text is
   empty, use the pasted doc, conversation takeaway, or decision in the current turn.
2. Pick the corpus (`repo:{name}:wiki` or ask).
3. Run the Layer 1 hash check (Phase 3) on the whole source. A ledger hit ends the run for this
   source: write only its `skipped-duplicate-hash` log row (Phase 4), then report (Phase 5).
4. Split the source into **atomic claims** — one topic each, the same granularity as a wiki
   page section. Don't ingest a 10-page doc as one blob.
5. Read `wiki-conventions.md` (the wiki's constitution) for slug/voice/merge rules.
   If absent, fall back to hallouminate's authoring conventions (one topic per file,
   H1 first line, kebab slug).

Done when the source passed Layer 1, every claim is atomic, and every page that may change has
frozen probes (below).

### Source pages and frozen retrieval probes

When an atomic claim comes from durable external research, create or reuse one corpus-local
`sources/<slug>.md` page. Before creating it, search the corpus for both the canonical URL and
exact source title, then read likely matches; either match reuses the canonical page instead of
creating a duplicate. The source page keeps this indexed retrieval spine in normal body text,
with source-appropriate evidence sections after it:

```markdown
# <Exact source title>

<Publisher/author>'s <source type>, published or last verified <date>,
supports <one-sentence contribution>.
Canonical source: [<exact title>](<URL>)

## <Source-appropriate heading>
<Supported claims, project relevance, and limitations>

_Source: <canonical URL> · Updated: <date>_
```

The exact title, publisher or author, source type, date, contribution, and canonical URL must all
be indexed body text. Frontmatter and footnotes may repeat them but cannot be their only location.
Every dependent topic page must also name the source and relevant claim in indexed prose and link
the corpus-local source page.

During claim decomposition, before any drafting, freeze the retrieval probes for every page that
may change: the exact topic identity; two to four natural questions derived from the atomic claim
and source; the exact external-source title for a source page; and the source's central claim for
the dependent topic page. Record the exact query strings, expected corpus-relative page, and
required rank. Drafting and the repair pass must use these unchanged probes; do not generate them
from the finished prose.

## Phase 2 — Locate (haiku, parallel)

Dispatch one fresh-context, read-only, haiku-tier sub-agent per atomic claim, all **in a single
message** so they run in parallel (for example Claude `Agent(...)`, Codex `spawn_agent`, OMP
`task(...)`). When the host has no sub-agent tool, the root runs the same contract inline for
each claim. Each sub-agent follows this contract:

> Run `ground { query: "<claim topic>", corpus: "<corpus>", top_files: 3, chunks_per_file: 3, cwd }`.
> Return the best-matching existing page: its **corpus-relative path**, the file-level
> `mtime`, the file-level `score` and `z_score` (`DocFile.score`/`DocFile.z_score` — `z_score` is
> the Layer-2 banding signal; `None` unless the cross-encoder ran), and from the top chunk its
> `heading_path`, `line_range`, and
> `snippet`. (`ground` keys its `docs` by *absolute* path and `mtime` is file-level,
> not per-chunk — convert the key to the corpus-relative path, the same shape
> `read_markdown`/`add_markdown` take, since they reject absolute paths.) If the top
> score is low / nothing relevant, return `{ match: none }`. Do NOT edit anything —
> you only locate. If the match looks close, `read_markdown { corpus, path, cwd }` that
> page (relative path) and return the section that would be updated.

**Read `index.md` glosses to route, never rewrite them.** Before or within locate, read the
relevant `index.md`'s link list for page **glosses** — each link's gloss is the target page's H1
only (`ground` returns a richer `DocFile.summary`, H1 + lead). Use `list_tree` only to enumerate
the bare page inventory; it carries no glosses. Routing is `ground` `score` ordering
**cross-checked** against the gloss list, not
gloss-matching alone — vague glosses cause misrouting. The link list between
`<!-- HALLOUMINATE:INDEX-START -->` / `<!-- HALLOUMINATE:INDEX-END -->` is daemon-maintained;
the skill must **never edit inside those markers**. A short human-routing prose paragraph may be
kept *above* the start marker (outside it, so the daemon leaves it alone) and refreshed via a
normal `add_markdown` of the prose region; it must not duplicate the auto link list. **Exclude
`log.md` from routing** — it is a journal, never a merge target.

Done when every claim has a located page (with its read-back section) or `{ match: none }`.

## Phase 3 — Decide: 3-layer dedup (root / opus)

Run an **ordered, short-circuiting** three-layer pipeline. Layer 1 runs once per source in
Phase 1, before atomize; Layers 2–3 run here per claim. Each layer runs only if the previous one
did not decide. The bands are **numeric and named**; the units are hallouminate
`z_score`/`score`, **not raw cosine** — `ground` exposes no cosine between two texts.

### Layer 1 — Hash identity (deterministic; bundled CLI; runs in Phase 1)

Catches identical re-ingestion of a whole source before any embedding work. Run the bundled
command instead of hashing by hand. Its path is relative to this `SKILL.md` directory:

```bash
python3 <this-skill-dir>/scripts/ingest-ledger check <source-file> --ledger <corpus-root>/log.md
```

- `<source-file>` holds the whole source. Save pasted or conversation material to a temporary
  file first. Omit `--ledger` when the corpus root is not on disk, and save the `read_markdown`
  content of `log.md` to a file when it exists.
- Output is one JSON object: `source_id` (16 hex chars), `ledger` (`hit` / `miss` / `absent`),
  `matches`, and `first_match` (the first matching log row, or `null`). A row matches when its
  second ` · `-separated field equals `source_id`.
- The command collapses every whitespace run to one space and trims both ends before it hashes.
  It keeps case and markdown, so the same source always gets the same id.
- **`hit` → skip the entire source**, append a `skipped-duplicate-hash` row, report it. No
  `ground`, no page read/merge — the only write is the `skipped-duplicate-hash` log row.
  **`miss` or `absent`** → continue (Phase 4 scaffolds `log.md` on first write).
- Exit 2 means an empty source or a bad argument; fix the call. Exit 3 with a `cannot read`
  error means an unreadable input file; fix the path. Any other failure means the launcher cannot
  fetch, verify, or run its archive (it needs Python 3.11+, and network on first run). Then
  report the error, treat Layer 1 as a miss, and write `—` as the source hash in the log row.
  Layers 2–3 still dedup each claim.
- Hash identity is **whole-source**, not per-claim — the cheap exact-dup guard. Per-claim dedup
  is Layers 2–3.

### Layer 2 — Near-duplicate (numeric, primary signal `z_score`)

For each atomic claim that survived Layer 1, use the locate step's top `DocFile` `score` and
`z_score` (file-level — `z_score` drives the banding) plus the read-back section:

| Condition | Band | Decision |
|---|---|---|
| `z_score` present **and** `z_score ≥ 2.0` | **near-duplicate** | **Skip** unless the read-back section is missing a concrete sub-fact the claim adds; then → Layer 3 merge. |
| `z_score` present **and** `1.0 ≤ z_score < 2.0` | **merge band** | **Merge** into the matched section. |
| `z_score` present **and** `z_score < 1.0` | **novel** | → Layer 3 routing (new-page candidate). |
| `z_score` **absent** (`None`) | unnormalized | Fall back — see below. |

`z_score ≥ 2.0` means "≥2 std-devs above this query's candidate mean" — the most confident match
the corpus offers for that query.

**Fallback when `z_score` is `None`** (RRF-only / small corpus): the numeric skip is unavailable,
so **never skip on `score` alone**. Use the raw `score` *rank* (clear top hit? large gap to #2?)
**plus a verbatim-overlap check** — read the matched section and skip only if the claim's key
sentence appears near-verbatim (≥ ~90% token overlap). Otherwise treat as merge band. This keeps
the "don't blend, don't silently lose a fact" guarantee without a normalized number.

### Layer 3 — Route or create (numeric, signal `score` ordering)

Route claims that reach here (novel / merge-band) using `score` ordering cross-checked against the
`index.md` glosses (Phase 2):

- A **merge-band** claim folds into the matched page's section (Phase 4 merge loop).
- A **novel** claim with no page owning its topic → **new page** (Phase 4 new-page loop). A new
  page is the **last resort**.
- If a merge-band/near-dup claim *conflicts* with the section, hand to the Phase 3a judge — this
  layer routes; it does not re-implement contradiction detection.

**Calibration note (tunable, domain-dependent).** `ground` returns an RRF-fused `score` and a
per-query relative `z_score`. The cutoffs (`2.0`, `1.0`) are a starting point; adjust them when
sampled decisions look mis-banded.

**Phase 3a — contradiction (LLM-as-judge, root):** When the new claim conflicts with
an existing page, do NOT average them — blending produces confident wrong answers.
Judge: is the new source more authoritative or more recent (compare `mtime`, source
provenance)?

- **Newer + authoritative** → overwrite the stale assertion, and record what
  superseded what in the provenance footer's `Supersedes:` field (`Supersedes:
  <what> · <date>`).
- **Unclear** → keep both, mark the conflict inline (`> ⚠️ Conflicts with <other>:
  <summary> — needs human resolution`), and flag it to the user. Never silently pick.

Done when every claim has exactly one decision: skip, merge, overwrite, new page, or
conflict-flagged.

## Phase 4 — Write (root / opus)

Apply each decision through the safe update loop:

- **Merge/overwrite:** `read_markdown` the page → `backlinks { corpus, path, cwd }` →
  edit the section → `add_markdown { overwrite: true, cwd }`. (Read-before-clobber is
  mandatory; it's your rollback point. `backlinks` returns the pages that
  `[[wikilink]]` to this one — the pages that assume or build on it. If the edit
  changes a claim a backlink relies on, queue that page into the same ingest pass
  instead of leaving it silently stale.)
- **New page:** draft one-topic entry (H1 first line, kebab slug, lead-first,
  ~50–150 lines, code cited as `path:line`, shaped on the pack's
  `../../templates/wiki-entry.md`) → `add_markdown { overwrite: false, cwd }`.
- **Chunk context:** Every H2/H3 section must open self-contained: give enough subject and purpose for the section to remain clear when retrieved without surrounding sections. Name the domain concept in the opening sentence; do not make a heading, pronoun, or parent page carry all context. Breadcrumbs and file summaries may supplement this authored context, but do not generate index-time or per-chunk LLM context.
- **Local links:** if merged or new content links a local file outside the corpus
  (absolute path, `~`, or a relative path escaping the corpus root — the ingest
  source itself is the common case), copy that file into the corpus first
  (`add_markdown`, e.g. `sources/<slug>.md` — or a config-declared corpus it
  belongs to) and link the corpus-relative copy. Web URLs pass through; code
  stays a `path:line` citation, never a copied file.
- **Provenance footer on every touched page:**
  `_Source: <where this came from> · Updated: <date> · Supersedes: <if any>_`
  Freshness is a first-class signal — stale pages produce confident-wrong answers.

Before journaling, verify the complete topic-and-source write set with the frozen probes:

1. After every `add_markdown` write has reindexed, run every unchanged probe with `ground`.
   Exact topic-identity and source-title probes require the intended page at rank 1. Natural
   questions and central-claim probes require it within the top 3.
2. If any probe fails, revise only the H1, lead, headings, or section opening once. Do not add
   keyword lists. Rerun the identical frozen probes after that single bounded revision.
3. If an exact probe still fails, restore every overwritten page from its preimage, delete every
   new page from the write set, return `blocked`, and do not append a journal row for the rolled-back
   writes.
4. If only natural or central-claim probes still fail, preserve the valid pages and return
   `written-with-retrieval-warning`. Report each failed query, expected page, observed top three,
   and actual rank (including absent). Journal the final warning disposition only after reporting
   data is complete.

Then **journal the decision** in `log.md` (append-only, never rewritten):

> `add_markdown { corpus, path: "log.md", under_heading: "Log", position: "append", content: <row>, cwd }`

where `<row>` is one log row `<date> · <source-hash> · <action> · <target path|—> · <summary>`
and `action ∈ {skipped-duplicate-hash, skipped-near-duplicate, merged, new-page, conflict-flagged, retrieval-warning}`.
Log **every** decision — including Layer-1 hash skips (the row *is* the ledger Layer 1 scans) and
**every** flagged contradiction. If `log.md` is absent, scaffold it once with
`add_markdown { corpus, path: "log.md", content: "# Ingest Log\n\n## Log\n", overwrite: false, cwd }`, then append.
Whole-file rewrites of `log.md` are forbidden — the only writes are `under_heading: append` splices.

The daemon reindexes each written file and refreshes ancestor `index.md` link lists
automatically; the marker rule in Phase 2 still applies. For edits made **outside** these tools,
run `index` to re-embed.

Done when every probe has a final disposition and every decision has one log row, except
writes rolled back to `blocked`.

## Phase 5 — Report (root / opus)

Summarize per claim in one table, then list every flagged contradiction by name:

```markdown
| Claim | Outcome | Page | Notes |
|---|---|---|---|
```

`Outcome` is one of: skipped / merged / new / conflict-flagged / written-with-retrieval-warning /
blocked. For retrieval warnings, `Notes` gives the query, expected page, observed top three, and
actual rank. Note any page that's now large enough to split (one-topic-per-file drift).