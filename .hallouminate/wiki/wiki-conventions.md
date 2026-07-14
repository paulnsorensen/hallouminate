# Wiki conventions for LLM authors

You — the LLM writing wiki entries via `add_markdown` — are bound by a
few conventions hallouminate counts on. They are echoed from
`SERVER_INSTRUCTIONS` in `src/app/mcp/tools.rs` plus practical
extensions.

## One topic per file

A wiki entry is a slice of knowledge with a clear scope. If you find
yourself drafting two unrelated topics in one file, split them. The
chunker breaks markdown by H1/H2/H3 headings — a file with two H1
sections will still chunk, but ground retrieval will rank both
sections together and that's almost never what you want.

## First non-blank line is the H1

The first non-blank line of every wiki entry — or, when an optional
frontmatter block is present, the first non-blank line after its closing
`---` fence — must be `# Topic Name`.
The chunker uses the H1 as the breadcrumb root for every chunk in the
file. Skip the H1 and breadcrumbs degrade to just sub-headings, which
makes `ground` results less navigable. The auto-index also reads each
file's first H1 to use as the trailing gloss on its link entry, so a
missing H1 leaves the file's row in the parent index ungloss'd.

## File stem matches the slug

Topic "Corpus walker" → file `corpus-walker.md`. Lowercase, kebab
case. No spaces, no capitals, no extensions other than `.md`. The file
stem is what other wiki pages link to and what shows up in `ground`
outline paths.

## Optional frontmatter (lifecycle + provenance)

A page **may** open with a YAML frontmatter block: a leading `---`
fence on line 1, key/value lines, then a closing `---` fence. It
carries lifecycle and provenance metadata and is entirely optional —
most pages have none, and every field inside is optional too.

```text
---
status: reviewed        # draft | reviewed | trusted | deprecated
owner: cheese-team
last_verified: 2026-01-02
confidence: high
sources:
  - https://example.com/source
---
# Topic Name
```

The four lifecycle states are `draft`, `reviewed`, `trusted`, and
`deprecated`, parsed case-insensitively. The block is **stripped
before indexing**, so it never pollutes chunk text, summaries, or
`ground` results, and citation line numbers still point at the real
on-disk lines below it. Unknown keys are ignored (the file on disk
stays the source of truth). A malformed block — broken YAML between
the fences — is left in the body verbatim and `add_markdown` returns a
single advisory warning so you can fix or remove it.

## Idempotent writes

`add_markdown` rejects existing files by default. To update:

1. `read_markdown` to inspect current content.
2. Decide what changes.
3. `add_markdown` with `overwrite: true`.

This is intentional — it forces a look at current state before
clobbering, so concurrent authors don't silently lose each other's
edits.

## Where this wiki lives

`.hallouminate/wiki/` inside this repo. Indexed as the
`repo:hallouminate:wiki` corpus, derived from the `[[repository]]
name = "hallouminate"` entry in the XDG baseline. Writes go to the
first (and only) configured root.

## Tree layout & progressive disclosure

Use subdirectories to group related entries. `add_markdown` accepts
nested paths (`adapters/lance.md`) and creates parent directories
on demand. Recommended shape:

- Top-level files for foundational topics (`architecture.md`,
  `mcp-surface.md`, `wiki-conventions.md`).
- Subdirectories per concern (`adapters/lance.md`,
  `adapters/mcp.md`, …).

Every directory carries an `index.md`. The first H1 names the
subtopic; the body holds curated prose plus a link list to siblings
and children. Consumers either navigate via `list_tree` (which exposes
the structure machine-readably) or browse `index.md` files top-down.

### Auto-maintained link list

The daemon scaffolds and maintains the LINK LIST inside each
`index.md` between markers:

```markdown
# Topic — index

Curated prose lives outside the markers and is preserved verbatim.

<!-- HALLOUMINATE:INDEX-START -->
- [some-page](./some-page.md) — H1 of some-page.md
- [subdir/](./subdir/index.md) — H1 of subdir/index.md
<!-- HALLOUMINATE:INDEX-END -->

More prose down here also survives.
```

After every `add_markdown` / `delete_markdown` against a
`repo:*:wiki` corpus, the daemon walks from the corpus root down to
the new file's parent. For each ancestor dir:

- If `index.md` is missing, scaffold one with the H1 + empty marker
  block, then populate it.
- If `index.md` exists and has both markers, rewrite the link list
  between them. Everything outside the markers is preserved.
- If `index.md` exists but has no markers, leave it alone — the author
  opted out.

To opt out per file, remove the marker pair from your `index.md`. To
opt out per directory entirely, just don't create an `index.md` and
remove the markers from any parent dir's index that might point at it
(or leave them — the parent's link list will still show the subdir if
it contains markdown).

### Link convention

- `[stem](./stem.md)` for files in the same directory.
- `[subdir/](./subdir/index.md)` for child directories.
- Relative paths only — the wiki should survive moves of the whole
  directory.

## What belongs here vs `.cheese/`

| Where | Indexed as | Lifecycle | Use for |
|---|---|---|---|
| `.hallouminate/wiki/` | `repo:hallouminate:wiki` | durable across sessions | architecture, conventions, protocols, gotchas, "why this design not that one" notes |
| `.cheese/` | `cheese-local` | transient per-task reports | output of `/cook`, `/age`, `/press`, `/cure`; per-spec artifacts |

Rule of thumb: if a future LLM landing in this repo would benefit from
the note even with no context about the task that produced it, write
it to the wiki. If it only makes sense in the context of a specific
task, leave it in `.cheese/`.

## When to update — the post-land cadence

`AGENTS.md` at the repo root instructs every coding agent to refresh
this wiki **after a change lands on `main`**, but only when the change
altered durable knowledge — architecture, conventions, protocols, the
MCP tool surface, or a "why this design not that one" decision. Routine
bug fixes and transient per-task output stay out (see the table above).

Do the update through the MCP (`read_markdown` → `add_markdown` with
`overwrite: true`), not raw file edits, so the LanceDB index and the
ancestor `index.md` link lists stay in sync.

## The authoring loop

```text
1. list_tree                                 (see the existing shape)
2. ground "<topic adjacent search>"          (find related entries)
3. read_markdown index.md                    (confirm naming + style)
4. draft the new page (H1 first line, kebab-case slug, link siblings)
5. add_markdown { corpus: "repo:hallouminate:wiki",
                  path: "<slug>.md" or "<dir>/<slug>.md",
                  content: "<markdown>",
                  overwrite: false }
6. (the daemon rewrites ancestor index.md link lists for you)
```

For curated prose in `index.md` itself, manage it the same way:
`read_markdown` → edit → `add_markdown` with `overwrite: true`. The
auto-index only touches the marker block.

## Style

- Lead with the conclusion. Don't bury what the file is about under preamble.
- Cite files and line ranges by path: `src/domain/corpus/walker.rs::root_is_gitignored`.
- Cite commits by SHA when behavior depends on history (e.g. `f5c5224` introduced gitignore-aware walking).
- Prefer concrete examples to abstract description.
- Keep entries short — a wiki page is not a tutorial. ~50-150 lines is the right band.
