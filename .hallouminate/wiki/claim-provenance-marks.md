# Claim-level provenance marks

Claim marks tag individual claim *sentences* inside a wiki page with a provenance
status, as inline HTML comments parsed at index time. They are a separate
construct from page-level frontmatter (see [wiki-conventions](wiki-conventions.md)
§ Optional frontmatter) and from footnotes. Introduced for issue #88 (PR #126),
building on the page-level lifecycle work (#89). The vocabulary lives in
`src/domain/corpus/claim_marks.rs`, a deliberate twin of `frontmatter.rs`.

Syntax — an HTML comment at the end of the claim it annotates:

```
<!--claim:confirmed-->
<!--claim:superseded ref=path/to/page.md-->
<!--claim:contradicted ref=https://example.com/rfc note="repealed in v3"-->
<!--claim:qualified note="only on macOS"-->
```

`STATUS` ∈ `confirmed | qualified | superseded | contradicted`, case-insensitive.
An unrecognized status yields no structured mark and an advisory lint warning
(mirrors `LifecycleStatus::from_str_ci` returning `None`), but the comment is
still stripped from retrieval text because it is claim-shaped; only non-`claim:`
HTML comments survive in retrieval text. `ref=` is an opaque
pointer (page path, footnote label, or URL); `note=` an optional quoted rationale.

## Two provenance vocabularies, two columns

Page-level lifecycle (`draft/reviewed/trusted/deprecated`, from frontmatter) and
claim-level marks (`confirmed/qualified/superseded/contradicted`) are lexically
disjoint and stored in **separate** nullable Lance columns — `frontmatter` vs
`claim_marks`. No collision in storage or namespace. Any rollup ("many
`contradicted` claims should demote a `trusted` page") is deliberately *not*
built: claim marks are descriptive metadata, not a page-status driver.

## Per-chunk (positional), not per-file

This is the one structural difference from frontmatter. Frontmatter is
page-level — the same JSON is denormalized identically onto every chunk row of a
file. Claim marks are **positional**: each mark belongs to the single chunk whose
line range contains the mark's line. `prepare_file`
(`src/domain/indexer/writer.rs`) extracts all marks once per body, then buckets
them per chunk by line range; the per-chunk JSON payload is the `claim_marks`
column. On read, `decode_claim_marks` (`src/adapters/lance.rs`) parses the column
back into `Vec<ClaimMark>`, which flows through `bucket.rs` into
`ChunkProvenance.claim_marks` and out of `ground`. Malformed stored JSON degrades
to an empty vec with a `warn` log rather than failing the query.

## One strip cleans both embedding and snippet

`prepare_file` runs `strip_claim_marks(&c.text)` on each chunk's text in the
chunk loop. `PreparedChunk.text` is the single string used for **both** the
embedding input and the stored snippet, so one strip cleans both — embeddings and
`ground` snippets carry no raw `<!--claim:...-->` text. Strip preserves line count
(it removes the comment span without dropping newlines), so `line_start`/
`line_end` citations and the per-chunk line-range filter stay aligned.
`read_markdown` reads on-disk bytes directly and is unaffected — marks remain
verbatim on disk.

## Each mark belongs to exactly one chunk

The subtle correctness rule, and the easiest thing to break. `MarkdownSplitter`
will split a single over-budget line across multiple chunks; when it does,
adjacent chunks carry **overlapping inclusive** line ranges — chunk A's
`line_end` equals chunk B's `line_start`, both being the split line. A naive
per-chunk filter `m.line >= line_start && m.line <= line_end` then matches the
mark in *every* sub-chunk (in the regression test, a long line split into 44
sub-chunks put the mark in all 44, so it surfaced 44 times in `ground`). The fix:
assign each mark to the **last** chunk whose inclusive range contains its line
(`rposition`), keyed on `Chunk.ord` — which equals the chunk's index in the chunk
vector, since `chunker.rs` sets `ord: out.len()` on push. The mark's
`<!--claim:...-->` bytes sit at the line's end, so the final sub-chunk is the
faithful owner. If you touch the bucketing, keep the single-owner invariant — the
regression test forces a tiny chunk budget and asserts exactly one chunk carries
the mark.

## Schema, lint, read_markdown

- **Schema** — adding the column bumped the Lance schema version **2 → 3**
  (`default_schema_version` in `src/adapters/lance.rs`). A v2 store reindexes
  cleanly through the existing version-mismatch delete+reindex path.
- **Lint** — `lint_claim_marks` joins the existing `add_markdown` advisory chain
  (`src/app/daemon/dispatch.rs`) alongside `lint_frontmatter` / `lint_markdown`.
  It warns on malformed marks and on `superseded`/`contradicted` marks missing a
  `ref=`. Advisory only: it rides back in `AddMarkdownResult.warnings` and never
  blocks the write. There is no `lint` CLI/MCP command.
- **Malformed-input edge** — a well-formed HTML comment cannot contain `-->`, so
  `note="… --> …"` ends the comment at the first `-->`; the note is truncated
  there and the tail stays in the body as ordinary text. This is HTML-correct and
  pinned by a test, not "fixed" — deleting the tail would guess at author intent.
