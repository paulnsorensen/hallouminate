# Wiki conventions for LLM authors

You — the LLM writing wiki entries via `add_markdown` — are bound by a
few conventions hallouminate counts on. They are echoed from
`SERVER_INSTRUCTIONS` in `src/adapters/mcp/tools.rs` plus practical
extensions.

## One topic per file

A wiki entry is a slice of knowledge with a clear scope. If you find
yourself drafting two unrelated topics in one file, split them. The
chunker breaks markdown by H1/H2/H3 headings — a file with two H1
sections will still chunk, but ground retrieval will rank both
sections together and that's almost never what you want.

## First non-blank line is the H1

The first non-blank line of every wiki entry must be `# Topic Name`.
The chunker uses the H1 as the breadcrumb root for every chunk in the
file. Skip the H1 and breadcrumbs degrade to just sub-headings, which
makes `ground` results less navigable.

## File stem matches the slug

Topic "Corpus walker" → file `corpus-walker.md`. Lowercase, kebab
case. No spaces, no capitals, no extensions other than `.md`. The file
stem is what other wiki pages link to and what shows up in `ground`
outline paths.

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

## What belongs here vs `.cheese/`

| Where | Indexed as | Lifecycle | Use for |
|---|---|---|---|
| `.hallouminate/wiki/` | `repo:hallouminate:wiki` | durable across sessions | architecture, conventions, protocols, gotchas, "why this design not that one" notes |
| `.cheese/` | `cheese-local` | transient per-task reports | output of `/cook`, `/age`, `/press`, `/cure`; per-spec artifacts |

Rule of thumb: if a future LLM landing in this repo would benefit from
the note even with no context about the task that produced it, write
it to the wiki. If it only makes sense in the context of a specific
task, leave it in `.cheese/`.

## The authoring loop

```text
1. list_files repo:hallouminate:wiki         (avoid duplication)
2. ground "<topic adjacent search>"           (find related entries)
3. read_markdown index.md                    (confirm naming + style)
4. draft the new page (H1 first line, kebab-case slug)
5. add_markdown { corpus: "repo:hallouminate:wiki",
                  path: "<slug>.md",
                  content: "<markdown>",
                  overwrite: false }
6. read_markdown index.md
7. add_markdown index.md with overwrite: true
   (append the new entry to the topic list, alphabetical)
```

## Style

- Lead with the conclusion. Don't bury what the file is about under preamble.
- Cite files and line ranges by path: `src/domain/corpus/walker.rs::root_is_gitignored`.
- Cite commits by SHA when behavior depends on history (e.g. `f5c5224` introduced gitignore-aware walking).
- Prefer concrete examples to abstract description.
- Keep entries short — a wiki page is not a tutorial. ~50-150 lines is the right band.
