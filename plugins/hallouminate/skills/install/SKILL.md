---
name: install
description: Install hallouminate and bootstrap the user's first LLM-authored per-repo wiki. Use when the user runs /install from the hallouminate skill pack, or asks to "install hallouminate", "set up hallouminate", or "start a hallouminate wiki". Installs a prebuilt binary (no Rust toolchain needed on supported targets), makes the MCP server available for the current harness, then uses Socratic questioning to decide where and how the wiki lives, seeds it with `hallouminate init-repo`, indexes it, and commits the result with git.
argument-hint: "[target repo path]"
allowed-tools: AskUserQuestion, Read, Write, Edit, Bash(curl:*), Bash(sh:*), Bash(uname:*), Bash(cargo:*), Bash(rustup:*), Bash(hallouminate:*), Bash(git:*), Bash(claude:*), Bash(codex:*), Bash(which:*), Bash(command:*), Bash(protoc:*)
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

## Phase 1 — Platform check

Run `uname -sm`. Prebuilt binaries exist for:

| `uname -sm` | target |
| --- | --- |
| `Darwin arm64` | aarch64-apple-darwin |
| `Linux aarch64` | aarch64-unknown-linux-gnu |
| `Linux x86_64` | x86_64-unknown-linux-gnu |

Two platforms need special handling — tell the user plainly, don't silently
fall through:

- **Intel macOS (`Darwin x86_64`)**: no prebuilt — the `ort` (ONNX Runtime)
  dependency ships no x86_64-darwin binary. Say so loudly and go straight to
  the source build (Phase 2, step 3), which needs Rust and `protoc`.
- **Windows**: unsupported entirely (prebuilt *and* source) — the daemon is
  Unix-only (Unix domain socket + `flock`). Point at
  <https://github.com/paulnsorensen/hallouminate/issues/48> and stop.

## Phase 2 — Install the binary (cascade)

Try each step in order and stop at the first that leaves `hallouminate
--version` working. No Rust toolchain or `protoc` is needed unless you reach
step 3.

1. **Prebuilt installer (default):**

   ```sh
   curl --proto '=https' --tlsv1.2 -LsSf \
     https://github.com/paulnsorensen/hallouminate/releases/latest/download/hallouminate-installer.sh | sh
   ```

   The installer downloads the prebuilt for the detected platform and prints
   where it installed (typically `~/.cargo/bin` if you have one, else
   `~/.local/bin`). Make sure that directory is on PATH.

2. **cargo binstall (if already installed):** `cargo binstall hallouminate`
   — fetches the same prebuilt via the Release's `dist-manifest.json`.

3. **Source build (fallback; required on Intel macOS):** needs `cargo`
   (<https://rustup.rs>) and `protoc` (`brew install protobuf` on macOS,
   `sudo apt-get install -y protobuf-compiler` on Debian/Ubuntu). Then:

   ```sh
   cargo install hallouminate --locked
   ```

   This compiles native dependencies and takes a few minutes. Cargo installs
   to `~/.cargo/bin`, which must be on PATH.

## Phase 3 — Make the MCP tools available

The MCP server is `hallouminate serve` (stdio). How it gets registered depends
on the harness you are running in:

- **Claude Code (this plugin)**: nothing to do for the current project — the
  plugin bundles a `.mcp.json` that registers `hallouminate serve`
  declaratively when the plugin is installed. If the user wants the server in
  **every** project (user scope), or the bundled registration hasn't loaded,
  fall back to the imperative form:

  ```sh
  claude mcp add hallouminate --scope user -- hallouminate serve
  ```

- **Codex**: the plugin payload's `.mcp.json` registers the server when the
  plugin is installed (`codex plugin marketplace add paulnsorensen/hallouminate`,
  restart, then `codex plugin add hallouminate@hallouminate` or install from
  `/plugins`).

- **opencode**: opencode loads the MCP server and skills directly (no
  plugin manifest). Add the MCP server to the active `opencode.json`
  (project or home directory):

  ```json
  {
    "mcp": {
      "hallouminate": {
        "type": "local",
        "command": ["hallouminate", "serve"]
      }
    }
  }
  ```

  Copy the skills to the opencode skills directory:

  ```sh
  cp -r plugins/hallouminate/skills/ ~/.config/opencode/skills/
  ```

  Or, if this repo is cloned locally, symlink them:

  ```sh
  ln -sf "$PWD/plugins/hallouminate/skills/"* ~/.config/opencode/skills/
  ```

  The hallouminate MCP tools (`ground`, `add_markdown`, `read_markdown`,
  etc.) surface after an MCP server reload. If they haven't loaded this
  session, fall back to the CLI for the remaining phases.

- **Anything else**: any MCP client can launch `hallouminate serve` over stdio.

After registering, the tools surface as `mcp__hallouminate__*`. They may only
load after the session reloads its MCP servers — if they aren't callable yet
this session, fall back to the CLI (`hallouminate index` / `hallouminate
ground`) in the later phases and tell the user to re-run later to pick up the
tools.

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

## Phase 6 — Seed the repo

In the target repo, run the seeding subcommand (identical on every harness):

```sh
hallouminate init-repo <repo-name>          # seeds the current directory
# or: hallouminate init-repo <repo-name> --path <repo-root>
```

It writes `.hallouminate/config.toml` declaring the repo as a tenant
(`[[repository]]` with `path = "."`, which resolves against the repo root so it
works from any checkout or worktree) and a `.hallouminate/wiki/` skeleton. The
wiki becomes searchable as the `repo:<repo-name>:wiki` corpus. Choose a unique
`<repo-name>`, and do **not** also declare the same name in the XDG baseline,
or the two layers collide on the duplicate-name check. If a repo config already
exists, `init-repo` refuses; `--force` overwrites the config but never touches
an existing wiki.

**Narrate the first daemon spawn.** The first `hallouminate index` / `ground` /
`serve` after install auto-spawns a background daemon — the single owner of the
LanceDB ground directory; CLI and MCP talk to it over a Unix socket. Tell the
user this once so the background process isn't a surprise, and where to look:
`hallouminate daemon status` / `stop` / `restart`, with early-startup errors
landing in `~/.local/state/hallouminate/daemon-bootstrap.log`. The README's
"How the daemon works" section has the full lifecycle.

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

Recap for the user: binary installed (prebuilt, no toolchain), MCP server
available for their harness, config initialized, repo seeded with `init-repo`,
first wiki page authored + indexed + committed, and how to grow it from here.
