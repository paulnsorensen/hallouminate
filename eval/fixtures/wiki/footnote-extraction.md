---
status: reviewed
last_verified: 2026-07-20
confidence: high
sources:
  - crates/hallouminate-domain/src/footnotes.rs
  - https://github.com/paulnsorensen/hallouminate/issues/241
---
# Footnote extraction and resolution

Footnotes are parsed straight from raw markdown bytes, not from a rendered
tree, so `get_footnote` can resolve a citation without re-deriving the whole
page's structure. `extract_footnotes` (`crates/hallouminate-domain/src/footnotes.rs:28-76`)
walks the file once with `pulldown-cmark`'s offset iterator, collecting
ordered `(label, target_text)` pairs; link and image destinations inside a
definition are appended as `text (url)` so a citation written as
`[docs](https://example.com)` keeps its URL even after the surrounding markup
is stripped.[^1]

## Why extraction happens against the whole page, not a chunk

The indexer chunks a page by heading (`crates/hallouminate-domain/src/corpus/chunker.rs:68-100`),
and a long file's footnote block — conventionally the last few lines —
regularly lands in a different chunk than the paragraph that cites it. If
resolution walked chunk text, a claim near the top of a long page could cite
a `[^3]` whose definition sits in a chunk `ground` never returned for that
query, and the citation would silently dead-end.

`get_footnote` avoids that failure mode entirely by not depending on chunk
placement. The MCP handler (`crates/hallouminate/src/mcp/tools.rs:886-909`)
issues a `ReadMarkdown` request for the full page, then calls
`get_footnote_target` (`crates/hallouminate-domain/src/footnotes.rs:90-95`)
against the complete on-disk text. The lookup is a linear scan over
`extract_footnotes`'s ordered pairs, matched by exact label — 1 for `[^1]`,
note for `[^note]` — with no prefix matching, so a label search for 1 will
not accidentally return `[^10]`'s target.[^2]

## What Only and Exclude modes are for

`FootnoteMode` (`crates/hallouminate-domain/src/footnotes.rs:11-21`) has three
variants: Include (verbatim, the default), Exclude (strip both the inline
`[^label]` markers and the definition blocks), and Only (return just the
definition lines). `ground` and `read_markdown` both take a `footnotes` param
using this enum — Exclude is useful when a snippet's inline markers would
read as noise without the definitions to back them, while Only is a cheap way
to inspect a page's citation list without pulling its prose.[^3]

The indexer's own `search_text` column always excludes footnotes
(`crates/hallouminate-domain/src/indexer/format.rs:28-34`) — retrieval
ranking should not be skewed by citation boilerplate — while the stored
`text` column keeps them, since `read_markdown` and `get_footnote` need the
verbatim bytes.

## Parser-based, not regex-based

`exclude_footnotes` and `only_footnotes` collect byte ranges from
`pulldown-cmark`'s event stream rather than pattern-matching `[^...]`
directly. That is why a fake-looking `[^label]` string inside a fenced code
block or inline code span survives untouched instead of being mistaken for a
real footnote marker.[^4]

See [claim-provenance-marks](claim-provenance-marks.md), [wiki-conventions](wiki-conventions.md), and [mcp-surface](mcp-surface.md).

[^1]: `crates/hallouminate-domain/src/footnotes.rs:28-76`
[^2]: `crates/hallouminate/src/mcp/tools.rs:886-909`; `crates/hallouminate-domain/src/footnotes.rs:90-95`
[^3]: `crates/hallouminate-domain/src/footnotes.rs:11-21`
[^4]: `crates/hallouminate-domain/src/footnotes.rs:107-110`

_Source: `footnotes.rs` + `mcp/tools.rs` · Updated: 2026-07-20_
