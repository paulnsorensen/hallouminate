---
status: reviewed
last_verified: 2026-07-20
confidence: high
sources:
  - crates/hallouminate-domain/src/corpus/validate.rs
  - https://github.com/paulnsorensen/hallouminate/issues/233
---
# Backlinks and [[wikilink]] maintenance

`[[wikilink]]` is plain text to `pulldown-cmark` — no markdown extension
renders double-bracket syntax as a link — so wikilinks are extracted with a
dedicated pass rather than pulled off the parser's event stream. `find_wikilinks`
(`crates/hallouminate-domain/src/corpus/validate.rs:89-110`) walks the raw
text looking for `[[...]]` spans while skipping anything inside a fenced code
block, since a wikilink written as an example in a code fence is not a real
link to follow.[^1]

## From link text to a resolved target

A wikilink can name a bare stem (`[[architecture]]`) or a path
(`[[adapters/lance]]`). `normalize_slug` makes both forms comparable to the
corpus' known paths, and `lint_wikilinks`
(`crates/hallouminate-domain/src/corpus/validate.rs:236-250`) flags a target
as broken when it matches no page, or ambiguous when a bare stem matches more
than one page under different directories. `add_markdown` runs this lint on
every write and returns the results as advisory warnings — it does not block
the write, matching the no-enforced-schema stance the wiki takes elsewhere.[^2]

## Why ancestor index.md files refresh on every write

`add_markdown` and `delete_markdown` against a `repo:*:wiki` corpus walk from
the corpus root down to the touched file's parent directory after the write
lands, rewriting the link list between the `HALLOUMINATE:INDEX-START` /
`-END` markers in each ancestor's `index.md`. A missing `index.md` is
scaffolded; prose outside the markers is preserved verbatim; an `index.md`
with no markers is left untouched (the author opted out of auto-maintenance
for that directory).

The reason this runs on every write rather than as a periodic sweep is that a
wiki's tree view is the primary way an agent orients before it knows what to
`ground` for. A link list that only caught up on the next full reindex would
show a directory missing its newest page for however long elapses between
writes — exactly when a fresh page is most likely to be the thing worth
finding.

## What backlinks are for

`backlinks` (`crates/hallouminate/src/mcp/tools.rs:915-944`) returns the
corpus-relative paths of every page that links to a given page via
`[[wikilink]]`, resolved with the same bare-stem matching `lint_wikilinks`
uses. Unlike `ground`, which ranks by semantic similarity to a query,
backlinks answer a structural question: what already treats this page as
relevant. That is useful precisely when an agent lands on one page — often
via `ground` — and needs to know what else in the wiki assumes or builds on
it before editing it, without having to re-derive the link graph by reading
every sibling page.[^3]

See [wiki-conventions](wiki-conventions.md), [mcp-surface](mcp-surface.md), and [corpus-walker](corpus-walker.md).

[^1]: `crates/hallouminate-domain/src/corpus/validate.rs:89-110`
[^2]: `crates/hallouminate-domain/src/corpus/validate.rs:150-250`
[^3]: `crates/hallouminate/src/mcp/tools.rs:915-944`

_Source: `corpus/validate.rs` + `mcp/tools.rs` · Updated: 2026-07-20_
