# Spec — wiki-ingest: explicit 3-layer dedup, append-only `log.md`, `index.md` routing

**Issue:** #151 — Socratic wiki-bootstrapping agent (elicitation + incremental ingestion)
**Status:** approved-for-cook (autonomous milknado node; no interactive approval gate)
**Surface:** SKILL / markdown authoring under `plugins/hallouminate/skills/`. **No Rust / application code.**
**Date:** 2026-06-20

---

## Scope

Issue #151 describes a full bootstrapping agent. **~60% already ships** and is explicitly
**out of scope** — do not re-spec or rewrite it:

| Already implemented | Where |
|---|---|
| Socratic one-question-per-turn elicitation, ACTA sequence, internal slots | `wiki-init/SKILL.md` (Phase 1) |
| LLM-as-judge contradiction detection (newer-authoritative-wins / keep-both-flag) | `wiki-ingest/SKILL.md` (Phase 3a) |
| Provenance footers (`_Source: … · Updated: … · Supersedes: …_`) | `wiki-ingest/SKILL.md` (Phase 4), `wiki-init/SKILL.md` (Phase 2), `templates/wiki-entry.md` |
| Route-before-create, read-before-overwrite, haiku-locate / opus-decide topology | `wiki-ingest/SKILL.md` |

This spec covers **only the three gaps**:

1. **Explicit 3-layer deduplication** — a deterministic hash-identity layer plus
   **numeric** similarity bands, replacing the current qualitative "very high / high /
   low → skip / merge / new" score-band routing (`wiki-ingest/SKILL.md:50–63`).
2. **Append-only `log.md` ingest journal** — every ingest action recorded with source,
   date, and what changed.
3. **`index.md` routing-table interaction** — read page glosses to route; respect the
   daemon-owned link block; own the prose.

**Out of scope:** elicitation changes, contradiction-judge changes, the provenance-footer
format, any change to Rust (`ground`, `add_markdown`, the daemon, the chunker). New MCP
tools are explicitly deferred (see Open Questions).

---

## Grounding (verified, not assumed)

These facts were read from source and constrain the design. They are the reason the spec
diverges from the issue's literal prescription in one place.

- **`ground` does not expose raw cosine similarity.** `DocFile` returns
  `score: f64` — the file-level relevance score (best of its chunk scores after
  rollup), computed as an RRF rank-fusion value (`src/domain/ground/types.rs:47–48`)
  — and `z_score: Option<f64>` — *"A per-query RELATIVE score, not a calibrated
  0-1 probability"* (`src/domain/ground/types.rs:52`). `z_score` is std-devs above the
  query's candidate mean, `None` unless the cross-encoder ran (RRF scores are
  rank-derived and don't normalize), or for degenerate pools (n < 5, all-equal)
  (`src/domain/ground/types.rs:49–52`). **There is no MCP tool that returns a 0–1 cosine
  between two texts.** `<certain>`
- **Consequence for the issue's "cosine ≥ 0.95 skip / 0.80–0.95 merge".** Those literal
  thresholds are **not implementable in a skill** — the underlying signal is absent. The
  spec maps the *intent* (a numeric, ordered band that replaces vibes) onto the signals
  that **do** exist: a skill-computed content hash, then `z_score` (primary, when present)
  with `score`-percentile fallback. The numeric character of the decision is preserved;
  the specific units are not cosine and are declared tunable. `<certain>`
- **`add_markdown` supports targeted edits (#134).** Exactly one of `under_heading`
  (`position: append` splices before the next same-or-higher heading; `prepend` right
  after the heading), `replace_lines`, `replace_match`. All require the file to exist
  (`src/adapters/mcp/tools.rs:622–623`). → **`log.md` append needs no whole-file rewrite
  and no Rust:** `add_markdown { under_heading: "Log", position: "append", content: <row> }`.
  `<certain>`
- **`index.md` link block is daemon-owned.** The daemon refreshes the link list between
  `<!-- HALLOUMINATE:INDEX-START -->` / `<!-- HALLOUMINATE:INDEX-END -->` on every
  `add_markdown`/`delete_markdown`, walking ancestor dirs; **prose outside the markers is
  preserved verbatim** (`src/domain/corpus/index_md.rs:22–29`, `src/adapters/mcp/tools.rs:112–124`).
  → The skill must **never hand-edit inside the markers**; it reads page glosses for
  routing and may own a routing-summary prose block outside them. `<certain>`
- **`blake3` is a project dep**, but a skill cannot call it in-process; a skill hashes via
  a shell tool (`sha256sum`). The hash layer is a skill behavior, not app code. `<certain>`

---

## Gap 1 — Explicit 3-layer deduplication

Replace the qualitative table at `wiki-ingest/SKILL.md:50–63` with an **ordered,
short-circuiting** three-layer pipeline. Each layer runs only if the previous did not
decide. The bands are numeric and named; the units are stated honestly.

### Layer 1 — Hash identity (deterministic; skill-computed; no vector store)

Catches identical re-ingestion of a whole source before any embedding work.

- **Normalize** the incoming source text: strip leading/trailing whitespace, collapse
  internal whitespace runs to single spaces, drop a trailing newline. (Normalization is
  fixed so the same source always hashes identically; do **not** lowercase or strip
  markdown — keep it cheap and stable.)
- **Hash:** `sha256sum` of the normalized bytes; keep the first 16 hex chars as the
  source id.
- **Ledger:** `log.md` is the ledger (Gap 2). Before ingesting, scan `log.md` for the
  hash. **Hit → skip the entire source**, append a `skipped-duplicate-hash` log row,
  report it. No `ground`, no read, no write.
- Hash identity is **whole-source** (not per-claim) — it is the cheap exact-dup guard.
  Per-claim dedup is Layers 2–3.

### Layer 2 — Near-duplicate (numeric, primary signal `z_score`)

For each atomic claim that survived Layer 1, the haiku locate step (existing Phase 2)
runs `ground` and returns the top `DocFile`'s `score` and `z_score` (file-level,
`DocFile.z_score` — this drives the Layer 2 banding), plus from the top `DocChunk`
of that file: `heading_path`, `line_range`, `snippet`, plus the read-back section. The opus root decides:

| Condition | Band | Decision |
|---|---|---|
| `z_score` present **and** `z_score ≥ 2.0` | **near-duplicate** | **Skip** unless the read-back section is missing a concrete sub-fact the claim adds; then → Layer 3 merge. |
| `z_score` present **and** `1.0 ≤ z_score < 2.0` | **merge band** | **Merge** into the matched section. |
| `z_score` present **and** `z_score < 1.0` | **novel** | → Layer 3 routing (new page candidate). |
| `z_score` **absent** (`None`) | unnormalized | Fall back: see *Fallback* below. |

`z_score ≥ 2.0` means "≥2 std-devs above this query's candidate mean" — the most
confident match the corpus offers for that query. These cutoffs are the **explicit
replacement** for the old qualitative bands.

**Fallback when `z_score` is `None`** (RRF-only path / small corpus): the numeric skip is
not available, so **never skip on score alone**. Use the raw `score` *rank* (is this the
clear top hit, score gap to #2 large?) **plus a verbatim-overlap check**: read the matched
section and skip only if the claim's key sentence appears near-verbatim (≥ ~90% token
overlap). Otherwise treat as merge band. This keeps the "don't blend, don't silently lose
a fact" guarantee without a normalized number.

### Layer 3 — Route or create (numeric, signal `score` ordering)

For claims that reach here (novel / merge-band), route to the best-matching existing page
using `score` ordering + the `index.md` glosses (Gap 3):

- A **merge-band** claim folds into the matched page's section (existing Phase 4 merge
  loop: `read_markdown` → edit section → `add_markdown { overwrite: true }`).
- A **novel** claim with no page that owns its topic → **new page** (existing Phase 4
  new-page loop). New page is the **last resort**, unchanged from today.
- Contradiction handling is unchanged — if a merge-band/near-dup claim *conflicts* with
  the section, hand to the existing Phase 3a judge. (This layer routes; it does not
  re-implement contradiction detection.)

**Every threshold above is declared tunable and domain-dependent** (issue acceptance
criterion #7). The skill must carry a short calibration note stating: the units are
hallouminate `z_score`/`score`, **not** raw cosine; the cutoffs (`2.0`, `1.0`) are a
starting point pending a sample-and-review calibration pass; and the issue's literal
`0.95`/`0.80` cosine figures are **not used** because no tool exposes cosine.

---

## Gap 2 — Append-only `log.md` ingest journal

A single `log.md` at the corpus root, append-only, one row per ingest action.

- **Location & shape:** `log.md` with a stable `## Log` heading. Newest rows **appended**
  at the bottom (chronological journal). Created on first ingest if absent
  (`add_markdown { overwrite: false }` with the heading scaffold), thereafter appended.
- **Append mechanism (no rewrite):**
  `add_markdown { corpus, path: "log.md", under_heading: "Log", position: "append", content: <row> }`.
  Whole-file rewrites of `log.md` are **forbidden** — it is append-only; the only writes
  are `under_heading: append` splices (and the one-time scaffold create).
- **Row format** (one markdown table row or list item; pick one and keep it consistent):
  `<date> · <source-hash> · <action> · <target path|—> · <summary>` where `action ∈
  {skipped-duplicate-hash, skipped-near-duplicate, merged, new-page, conflict-flagged}`.
- **What gets logged:** *every* claim/source decision from the dedup pipeline, including
  skips (the skip record is what makes Layer 1's hash ledger work) and **every flagged
  contradiction** (issue: conflicts flagged in `log.md`, not silently resolved).
- `log.md` is **excluded from routing** — it is a journal, never a merge target. The
  locate step must not route a claim into `log.md`.

---

## Gap 3 — `index.md` routing-table interaction

- **Read for routing, don't rewrite.** Before/within the locate step, read the relevant
  `index.md` (or `list_tree`) to get page **glosses** (each `DocFile.summary` = the
  page's H1 + lead). The gloss is the routing signal; vague glosses cause misrouting
  (issue). Routing = `ground` `score` ordering **cross-checked** against the gloss list,
  not gloss-matching alone.
- **Never touch the marker block.** The link list between
  `<!-- HALLOUMINATE:INDEX-START -->` / `…INDEX-END -->` is daemon-maintained and refreshes
  automatically on every `add_markdown`/`delete_markdown` (including the merges/new-pages
  this skill does). The skill must not edit inside the markers.
- **Routing-summary prose (optional, outside markers).** The skill *may* maintain a short
  human-routing paragraph **above** the marker block summarizing what lives where — this
  is the "routing table for subsequent ingestion." If written, it goes outside the markers
  so the daemon leaves it alone, and is refreshed via a normal `add_markdown` of the
  `index.md` prose region (or `replace_match` on the prose). It must **not** duplicate the
  auto link list.
- **No new index after a no-op pass.** If a whole source is Layer-1-skipped, no page is
  written, so no index refresh fires — that is correct; only `log.md` records the skip.

---

## Acceptance criteria (maps to issue #151)

- [ ] `wiki-ingest/SKILL.md` describes a **three-layer, ordered, short-circuiting** dedup:
      Layer 1 hash identity, Layer 2 `z_score` numeric band (with documented `None`
      fallback), Layer 3 route/create — **replacing** the qualitative score-band table.
- [ ] The numeric bands cite **`z_score`/`score`**, state they are **not raw cosine**, and
      carry a tunable-calibration note (issue criterion #7). The `0.95`/`0.80` cosine
      figures from the issue are **not** presented as implementable cutoffs.
- [ ] Layer 1 computes a `sha256sum`-based source id over normalized text and skips on a
      ledger hit recorded in `log.md`.
- [ ] `log.md` is specified as append-only at corpus root, written **only** via
      `add_markdown … under_heading:"Log", position:"append"` (plus one-time scaffold), with
      a fixed row format and an enumerated `action` set including `conflict-flagged`.
- [ ] Contradiction flags land in `log.md` (existing judge unchanged; this spec wires the
      journal sink).
- [ ] `index.md` routing reads glosses, **never** edits inside the daemon marker block, and
      any optional routing-summary prose lives outside the markers.
- [ ] Every page the merge/new-page path writes still carries the existing provenance
      footer (unchanged — verify, don't rewrite).
- [ ] No Rust file is modified. `git diff --stat` touches only files under
      `plugins/hallouminate/skills/` (and this spec).

---

## Implementation path (for `/cook`)

Primary edits, all markdown:

1. **`plugins/hallouminate/skills/wiki-ingest/SKILL.md`**
   - Replace Phase 3's qualitative table (`:50–63`) with the Gap-1 three-layer pipeline.
   - Add the Gap-2 `log.md` append step to Phase 4 (and a Phase-2 hint to return
     `z_score` alongside `score` — the locate contract already returns `score`; add
     `z_score`).
   - Add the Gap-3 `index.md` read-for-routing / don't-touch-markers rule to Phase 2/3.
   - Add the calibration note + a Rules bullet ("log every decision; never rewrite
     `log.md`; never edit inside index markers").
2. **`plugins/hallouminate/skills/wiki-init/SKILL.md`** (small)
   - In Phase 2 conventions, declare `log.md` (append-only journal, `## Log` heading) and
     the `sha256sum` source-hash ledger convention in the generated `wiki-conventions.md`,
     so `wiki-ingest` reads and writes one shape. Scaffold an empty `log.md` at bootstrap.

Verification (no app tests exist for skills): self-review the two SKILL.md files against
the acceptance checklist; confirm `git diff --stat` shows only skill files; lint-read each
edited skill for internal consistency (the numeric bands, the log row format, and the
conventions declaration must agree across both files).

---

## Open questions / deferred (not blocking)

- **True cosine bands need a Rust MCP tool.** Implementing the issue's literal
  `cosine ≥ 0.95 / 0.80–0.95` would require a new MCP tool returning a calibrated 0–1
  similarity (e.g. a `similarity { a, b }` or exposing per-candidate cosine on `ground`).
  That is application code, **out of scope** for this skill-only node. **Recommended
  follow-up node:** "Expose calibrated embedding similarity (raw cosine) via MCP so wiki
  dedup can use absolute thresholds." *(The milknado follow-up tracking tool is not
  available in this session, so this is recorded here instead of registered as a graph
  node — promote it when running interactively.)*
- **`z_score` is `None` on small/RRF-only corpora** — the fallback (rank + verbatim
  overlap) is the documented path; calibrate once the cross-encoder is the norm.
- **Merge-band cutoffs (`2.0`, `1.0`) are guesses** pending a sample-and-human-review
  calibration pass before first production deploy (issue open question). `<speculative>`
- **Stopping criterion for elicitation** and **cold-start question generation** remain the
  issue's unresolved literature gaps — untouched here (elicitation is out of scope).
