---
name: install
description: Install hallouminate and bootstrap the user's first LLM-authored per-repo wiki. Use when the user runs /install from the hallouminate skill pack, or asks to "install hallouminate", "set up hallouminate", or "start a hallouminate wiki". Installs the cargo binary, registers the MCP server, then uses Socratic questioning to decide where and how the wiki lives, scaffolds it under .hallouminate/, indexes it, and commits the result with git.
argument-hint: "[target repo path]"
allowed-tools: AskUserQuestion, Read, Write, Edit, Bash(cargo:*), Bash(rustup:*), Bash(hallouminate:*), Bash(git:*), Bash(claude:*), Bash(which:*), Bash(command:*), Bash(protoc:*)
---

# Install hallouminate & start your first wiki

You are guiding the user from zero to a working hallouminate install with a
first, indexed wiki page — committed to git. Work through the phases in order.
Narrate briefly; do **not** dump this whole document back to the user.

`hallouminate` is a markdown-corpus indexer: an LLM authors per-repo wikis
through an MCP surface (`add_markdown` / `read_markdown` / `delete_markdown` /
`ground`) and queries them semantically. The filesystem is the source of truth;
wikis live under `<repo>/.hallouminate/wiki/`.

If the user passed a target repo path as an argument, treat that as the wiki's
home and skip the "where" question in Phase 5.

## Phase 1 — Preflight

1. Confirm the toolchain: `cargo --version`. If cargo is missing, point the user
   at https://rustup.rs to install Rust, then stop until it's available.
2. `protoc` is required to build the `lancedb` dependency. Check
   `protoc --version`. If it's missing, tell the user to install it
   (`brew install protobuf` on macOS, `sudo apt-get install -y protobuf-compiler`
   on Debian/Ubuntu) and stop until resolved — the build fails without it.

## Phase 2 — Install the binary

Run `cargo install hallouminate --locked`. This pulls from crates.io and can
take a few minutes — it compiles native dependencies. Verify with
`hallouminate --version`. If `hallouminate` isn't on PATH afterward, remind the
user that cargo installs to `~/.cargo/bin`, which must be on their PATH.

## Phase 3 — Register the MCP server

hallouminate exposes its tools over stdio via `hallouminate serve`. Register it
with this agent so the wiki tools (`ground`, `add_markdown`, …) become
available. Ask the user whether they want it at **project** scope (this repo
only, written to `.mcp.json`) or **user** scope (all their projects), then run:

```sh
claude mcp add hallouminate --scope project -- hallouminate serve
# or: claude mcp add hallouminate --scope user -- hallouminate serve
```

After adding, the tools surface as `mcp__hallouminate__*`. They may only load
after the session reloads its MCP servers — if they aren't callable yet this
session, fall back to the CLI (`hallouminate index` / `hallouminate ground`) in
the later phases and tell the user to re-run later to pick up the tools.

## Phase 4 — Initialize config

Run `hallouminate config init` to write the XDG baseline config
(`~/.config/hallouminate/config.toml`), then `hallouminate config validate` to
confirm it parses. Don't hand-edit the baseline here — the per-repo wiki is
declared by a repo-layer config in Phase 6.

## Phase 5 — Socratic discovery (where & how)

Before writing anything, work out *where* the wiki should live and *what* it
should first capture. Ask the user a short, focused sequence of questions — one
decision at a time, Socratic style, each building on the last. Use the
`AskUserQuestion` tool. Cover at least:

- **Where**: Which repository/directory gets the wiki? Default to the current
  working directory if it's a git repo — confirm with
  `git rev-parse --show-toplevel`.
- **What first**: The single most valuable thing to capture first — an
  architecture overview, onboarding/setup, a key domain concept, or a runbook?
  Resist breadth; one genuinely useful page beats ten stubs.
- **Audience**: Who reads this — future-you, new contributors, or other agents?
  That sets the tone and depth.
- **Git**: Confirm you may create a branch and commit. Ask for a branch name or
  propose one (e.g. `hallouminate-wiki`).

Keep it to 3–4 crisp questions. Reflect each answer back in one line before
moving on, so the user can course-correct early.

## Phase 6 — Scaffold the repo-layer config

In the target repo, create `.hallouminate/config.toml` declaring the repo as a
tenant, so its wiki is searchable as the `repo:<name>:wiki` corpus:

```toml
[[repository]]
name = "<repo-name>"
path = "."
```

`path = "."` resolves against the repo root (the parent of `.hallouminate/`), so
it works from any checkout or worktree with no per-machine config — the daemon
reads the repo layer fresh on every request. Choose a unique `<repo-name>`, and
do **not** also declare the same name in the XDG baseline, or the two layers
collide on the duplicate-name check. Create the `.hallouminate/wiki/` directory.

## Phase 7 — Author the first wiki page

Using what you learned in Phase 5, write the first page. Convention: one topic
per file, first line is `# Title`, and the file stem matches the slug
(e.g. `architecture.md` → `# Architecture`).

Prefer the MCP tool `mcp__hallouminate__add_markdown` — it writes under the
corpus' first root atomically, auto-reindexes just that file, and returns
advisory lint `warnings` (empty links, heading-level jumps, empty mermaid
blocks). If the MCP tools haven't loaded yet this session, write the file
directly to `.hallouminate/wiki/<slug>.md` instead and rely on Phase 8 to index.

Make the first page genuinely useful and grounded in the *actual* repo — read
real code and configs to get it right; don't invent. Resolve any lint warnings
`add_markdown` reports.

## Phase 8 — Index & verify

If you wrote files directly (no `add_markdown`), run `hallouminate index` to
build the index. Then prove it end-to-end:

```sh
hallouminate ground "<a question the page answers>"
```

Show the user the hit. A real result confirms the daemon, index, and corpus
wiring all work.

## Phase 9 — Commit with git

On the branch agreed in Phase 5, stage and commit the scaffolding plus the first
page:

```sh
git checkout -b <branch>
git add .hallouminate/
git commit -m "Add hallouminate wiki scaffolding and first page"
```

Show the diff and summarize what landed. Then offer next steps:

- Add more pages with `add_markdown` (or the CLI).
- Optionally install the auto-reindex git hooks: `hallouminate hook install`
  adds `post-commit` / `post-merge` hooks that re-index in the background.
- Query anytime with `hallouminate ground "<query>"` or the `ground` MCP tool.

## Done

Recap for the user: binary installed, MCP server registered, config
initialized, first wiki page authored + indexed + committed, and how to grow it
from here.
