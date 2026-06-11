---
# Frontmatter is optional — keep the keys you'll maintain, or delete the block.
status: draft                # draft | reviewed | trusted | deprecated
owner: <team-or-person>
last_verified: <YYYY-MM-DD>
confidence: <high | medium | low>
sources:
  - <url-or-path>
---
# <Topic Name>

<Lead with the conclusion: one or two sentences stating what this page tells
you. The file stem matches the title, kebab-cased: "Corpus walker" →
`corpus-walker.md`.>

## <First distinct point>

<H2/H3 headings are the retrieval unit — one per distinct point. Prefer
concrete examples to abstract description. Cite code by path
(`src/domain/corpus/walker.rs:42`) and back non-obvious claims with a
footnote.[^1]>

## <Second distinct point>

<Keep the whole entry to ~50–150 lines — a wiki page is not a tutorial. If a
second topic creeps in, split the file: one topic per file.>

[^1]: <file:line, doc URL, or commit SHA backing the claim>

_Source: <how we know this> · Updated: <YYYY-MM-DD> · Supersedes: <if any>_
