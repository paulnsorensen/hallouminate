---
name: wiki-roadmap
description: Author or extend a wiki roadmap in milknado's importable format — .hallouminate/wiki/roadmaps/<slug>/ with an index.md plus one goal file per goal. Use when the user wants execution-ready planning in the wiki — "create a roadmap", "add a roadmap", "add a goal to the roadmap", "plan the next milestone in the wiki", "/wiki-roadmap". Writes from the pack's roadmap templates (templates/roadmap/) so an installed milknado seeds the graph with `milknado roadmap import <slug>`, zero rework. Do NOT use to run or decompose the roadmap (milknado's load-roadmap and planner own execution) or to write outcomes back into goal files (milknado's harvest owns the Outcome block).
---

# wiki-roadmap — author milknado-importable roadmaps

The wiki owns roadmap and goal **intent**; milknado owns task **execution
state**. This skill writes the intent side in exactly the shape
`milknado roadmap import <slug>` consumes, so a later milknado install loads
the roadmap as-is.

Templates ship in this pack at `templates/roadmap/` (from this skill's base
directory: `../../templates/roadmap/`): `index.md` for the roadmap, `goal.md`
for each goal. Their skeletons are inlined below in case only the skill file
was copied to your harness.

## Layout

```text
.hallouminate/wiki/roadmaps/<roadmap-slug>/
├── index.md          # the roadmap node — title + created
├── <goal-a>.md       # one file per goal; the stem is the goal slug
└── <goal-b>.md
```

## The format contract

What milknado's importer reads — get these right and everything else is prose
for humans:

| Element | Rule |
|---|---|
| Frontmatter block | Required on every file. Import stamps a missing `created` and errors when there is no `---` block to stamp into. |
| `created: <YYYY-MM-DD>` | Keys the deterministic identity (`wiki_ref = uuid5(roadmap/goal@created)`). Stamp once at authoring; never change it after import — a changed date is a brand-new node to milknado. |
| First `# H1` | Becomes the milknado node description. |
| `prereqs: [a, b]` | Goal slugs in the same roadmap that must land first (edges in the graph). Every slug must name a sibling `<slug>.md` or import fails. The wikilink form `down: ["[[a]]"]` also works. |
| `## Intent` / `## Acceptance` | Human-owned prose. milknado preserves them byte-for-byte on every sync. |
| Outcome + harvest markers | Never pre-add. `milknado roadmap export` appends and owns them, plus the `status:` / `last_synced:` frontmatter keys. |

## Flow

1. **Resolve the slug.** Kebab-case directory name under
   `.hallouminate/wiki/roadmaps/`. Renaming the directory or a goal file
   later changes its identity — pick names that last.
2. **Write `index.md`** from the template.
3. **Write one goal file per goal** from the template. Fill `Intent` (why,
   and what done looks like) and `Acceptance` (verifiable criteria — these
   become the bar the executing agent is held to).
4. **Wire `prereqs`.** Check every listed slug names a sibling file.
5. **Verify.** Confirm no unfilled `<placeholder>` text survives — an
   unfilled `created` would key the node's permanent identity to the literal
   placeholder, and import succeeds silently on it. Prefer `add_markdown` for
   the writes (auto-reindex + ancestor link lists); otherwise run
   `hallouminate index`. If milknado is installed,
   `milknado roadmap import <slug>` now seeds the graph; goals decompose into
   tasks with `milknado_plan_batches`, and outcomes flow back with harvest.

### index.md skeleton

```markdown
---
created: <YYYY-MM-DD>
---
# <Roadmap Title>

<What this roadmap delivers, and roughly in what order.>

<!-- HALLOUMINATE:INDEX-START -->
<!-- HALLOUMINATE:INDEX-END -->
```

### goal skeleton

```markdown
---
kind: goal
slug: <goal-slug>
roadmap: <roadmap-slug>
created: <YYYY-MM-DD>
prereqs: []
---
# <Goal title>

## Intent

<Why this goal exists and what done looks like.>

## Acceptance

- <verifiable criterion>
```

## Rules

- `created` and file/directory names are identity — fill the placeholders at
  authoring, then never churn them.
- Every `prereqs` slug must name a sibling goal file in the same roadmap.
- Never write an Outcome section, harvest markers, `status:`, or
  `last_synced:` — those are milknado's half of the membrane.
- Acceptance criteria must be verifiable; vague goals execute vaguely.
- Links in `Intent`/`Acceptance` must resolve inside the corpus — copy a local
  doc into the wiki (e.g. `sources/`) and link the corpus-relative copy; web
  URLs and `path:line` code citations pass through as text.
- One goal per file; the file stem is the goal slug.
