---
status: reviewed
last_verified: 2026-07-19
confidence: high
sources:
  - crates/hallouminate-domain/src/indexer/writer.rs
  - https://github.com/paulnsorensen/hallouminate/issues/227
---
# Frontmatter conventions

A wiki page may open with a YAML frontmatter block — a leading `---` fence,
key/value lines, then a closing `---` — carrying lifecycle and provenance
metadata: `status` (draft | reviewed | trusted | deprecated), `last_verified`
(a date), `confidence`, and `sources` (URLs or repo paths). Every field is
optional, and so is the block itself; most existing pages have none.[^1]

## Page-level, not enforced

Frontmatter describes the page as a whole — when it was last checked, how
much to trust it, what backs it — not any individual claim inside the page.
The indexer strips the block before chunking (`prepare_file` offsets line
numbers past it,
`crates/hallouminate-domain/src/indexer/writer.rs:201-250`) so it never
pollutes chunk text or `ground` snippets, and citation line numbers still
point at the real on-disk lines below it. Unknown keys are ignored rather
than rejected, and a malformed block — broken YAML between the fences — is
left in the body verbatim with a single advisory warning, the same
non-blocking posture `add_markdown` takes toward wikilink lint failures.

This is deliberate. hallouminate imposes no markdown content schema at all —
`add_markdown` stores content verbatim — so frontmatter is a convention the
author honors, not a shape the writer enforces. A stricter schema would let
the daemon validate metadata, but it would also mean a legitimate page with
an unusual shape gets rejected rather than indexed with a warning, which cuts
against treating the filesystem as the unconditional source of truth.

## How it differs from inline per-claim marks

Frontmatter cannot express that one sentence is stale while the rest of the
page is fine — it is a single block covering the entire file. Per-claim
confidence lives in inline claim marks instead, anchored to the specific
sentence they qualify and chunked along with it rather than stripped out.
The two systems answer different questions: frontmatter tells a reader
whether to trust the page as a starting point at all; a claim mark tells a
reader whether this specific sentence, mid-page, still holds. A page can
carry `status: reviewed` in its frontmatter while one paragraph inside it
carries a claim mark flagging that paragraph specifically as unverified —
the two are not in tension, because they operate at different granularity.

## Practical effect on authoring

Because frontmatter is optional and unenforced, adding it is a judgment call:
worth it when a page's freshness or trust level is genuinely in question (a
proposed design not yet implemented, a claim resting on a since-changed API),
and skippable for a stable, load-bearing page like this wiki's own
conventions file. Update `last_verified` when you re-confirm a page's claims
against the current code, not on every cosmetic edit.

See [claim-provenance-marks](claim-provenance-marks.md), [wiki-conventions](wiki-conventions.md), and [design-rationale](design-rationale.md).

[^1]: `crates/hallouminate-domain/src/indexer/writer.rs:201-250`

_Source: `indexer/writer.rs` + `wiki-conventions.md` · Updated: 2026-07-19_
