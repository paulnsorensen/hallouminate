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
> `mtime`, the file-level `score` and `z_score` (`DocFile.score`/`DocFile.z_score` — `z_score` is
> the Layer-2 banding signal; `None` unless the cross-encoder ran), and from the top chunk its
> `heading_path`, `line_range`, and
> `snippet`. (`ground` keys its `docs` by *absolute* path and `mtime` is file-level,
> not per-chunk — convert the key to the corpus-relative path, the same shape
> `read_markdown`/`add_markdown` take, since they reject absolute paths.) If the top
> score is low / nothing relevant, return `{ match: none }`. Do NOT edit anything —
> you only locate. If the match looks close, `read_markdown { corpus, path }` that
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

## Phase 3 — Decide: 3-layer dedup (root / opus)

Run an **ordered, short-circuiting** three-layer pipeline per source/claim. Each layer runs
only if the previous one did not decide. The bands are **numeric and named**; the units are
hallouminate `z_score`/`score`, **not raw cosine** — `ground` exposes no cosine between two texts.

### Layer 1 — Hash identity (deterministic; skill-computed; no vector store)

Catches identical re-ingestion of a whole source before any embedding work.

- **Normalize** the source text: strip leading/trailing whitespace, collapse internal whitespace
  runs to single spaces, drop a trailing newline. (Don't lowercase or strip markdown — keep it
  cheap and stable so the same source always hashes identically.)
- **Hash:** `sha256sum` of the normalized bytes (via the shell — a skill can't call `blake3`
  in-process); keep the first 16 hex chars as the source id.
- **Ledger:** `log.md` is the ledger (Phase 4). Scan it for the hash. If `log.md` is absent, treat
  as no ledger hit and continue (Phase 4 scaffolds it on first write). **Hit → skip the entire
  source**, append a `skipped-duplicate-hash` row, report it. No `ground`, no page read/merge — the only write is the `skipped-duplicate-hash` log row.
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
the corpus offers for that query. These cutoffs **replace** the old qualitative bands.

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
  page is the **last resort**, unchanged from today.
- If a merge-band/near-dup claim *conflicts* with the section, hand to the Phase 3a judge — this
  layer routes; it does not re-implement contradiction detection.

**Calibration note (tunable, domain-dependent).** The units are hallouminate `z_score`/`score`,
**not** raw cosine. The cutoffs (`2.0`, `1.0`) are a starting point pending a sample-and-review
calibration pass before first production deploy. The issue's literal `0.95`/`0.80` *cosine*
figures are **not used** — no tool exposes a 0–1 cosine between two texts (`ground` returns an
RRF-fused `score` and a per-query relative `z_score`, not cosine).

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

Then **journal the decision** in `log.md` (append-only, never rewritten):

> `add_markdown { corpus, path: "log.md", under_heading: "Log", position: "append", content: <row> }`

where `<row>` is one log row `<date> · <source-hash> · <action> · <target path|—> · <summary>`
and `action ∈ {skipped-duplicate-hash, skipped-near-duplicate, merged, new-page, conflict-flagged}`.
Log **every** decision — including Layer-1 hash skips (the row *is* the ledger Layer 1 scans) and
**every** flagged contradiction. If `log.md` is absent, scaffold it once with
`add_markdown { corpus, path: "log.md", content: "# Ingest Log\n\n## Log\n", overwrite: false }`, then append.
Whole-file rewrites of `log.md` are forbidden — the only writes are `under_heading: append` splices.

The daemon reindexes each written file and refreshes ancestor `index.md` link lists
automatically. **Never hand-edit inside the `index.md` `<!-- HALLOUMINATE:INDEX-START -->` /
`<!-- HALLOUMINATE:INDEX-END -->` markers** — the daemon maintains that link block on every
`add_markdown`/`delete_markdown`; an optional human-routing prose paragraph may live *outside* the
markers (see Phase 2). For edits made **outside** these tools, run `index` to re-embed.

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
- Dedup is three ordered layers: hash identity (Layer 1) → `z_score` band (Layer 2) → route/create (Layer 3). Each runs only if the prior didn't decide.
- Log every dedup decision in `log.md` — skips included. The Layer-1 hash ledger only works if skips are recorded.
- Never rewrite `log.md`; it is append-only (`under_heading: "Log", position: "append"`), and never a routing target.
- Never hand-edit inside the `index.md` `<!-- HALLOUMINATE:INDEX-START -->` / `INDEX-END` markers — the daemon owns that block.
