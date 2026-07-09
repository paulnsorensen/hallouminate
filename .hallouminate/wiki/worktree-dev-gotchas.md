---
status: reviewed
last_verified: 2026-06-11
confidence: high
---
# Worktree dev-environment gotchas

Two environment traps that bite coding agents and sub-agents working on
this repo inside isolated git worktrees. Both were hit repeatedly in a
single session across three sub-agents, and each cost a compile cycle
before the cause was found. Neither is a code defect — they are
harness/host quirks worth recording so the next agent doesn't re-learn
them the hard way.

## tilth edits land in the parent repo, not the worktree

When a sub-agent runs in an isolated git worktree but edits code through
the **tilth MCP server**, the edits land in the parent checkout
(for example, `$HOME/Dev/hallouminate`) instead of the worktree. The reported
cause is that the tilth server process's working directory is the parent
checkout, so tilth-relative paths resolve there rather than in the worktree.
The mechanism is `<speculative>`; the symptom is `<certain>` — three
separate agents observed it (issues #101, #92, and the affinage PR runs).

**Symptom:** the first `cargo build` / `cargo test` in the worktree fails
with missing symbols, because the edits the agent believes it made are
not in the worktree tree at all — they are sitting uncommitted in the
parent checkout.

**Workaround:**
- After editing, run `git status` / `git diff` *in the worktree* and
  confirm the changes are actually present before committing.
- Prefer the host `Edit` / `Write` tools for worktree edits, or otherwise
  confirm the tilth write hit the worktree path.
- If edits already leaked to the parent: they appear as uncommitted
  changes under `/home/paul/Dev/hallouminate`. Copy them into the
  worktree, then `git stash` + drop the stray parent changes so the
  parent checkout is left clean.

The same class of issue applies to hallouminate's own `add_markdown` when
run from a worktree — see [wiki-conventions](wiki-conventions.md) ("Where this wiki lives"):
pass an explicit `corpus`, or author from the main checkout.

## /tmp scratch builds fail: disk quota + cargo wrapper (exit 134)

Building or testing in a `/tmp` scratch worktree fails in two compounding
ways:

- The default `cargo` shell wrapper swallows stdout, and foreground
  `cargo` invocations abort with **exit 134 (SIGABRT)** and no output —
  so the failure looks silent.
- `/tmp` is over disk quota, so linking the heavy binaries (`ort`,
  `fastembed`, the image codecs) OOM-kills the linker. It surfaces as
  `error: linking with cc failed` even though compilation itself
  succeeded.

`<certain>` exit 134 is SIGABRT (128 + 6). `<certain>` the link failure
is environmental, not a missing symbol — the tell is that
`cargo build --all-targets` resolves every symbol (exit 0) while a
follow-up `cargo test` then fails to *link* one test binary.

**Workaround that worked:**
- Point `CARGO_TARGET_DIR` and `TMPDIR` under `$HOME`, not `/tmp`.
- Pin `RUSTUP_TOOLCHAIN` to the repo's pinned toolchain (1.91 at time of
  writing) and call the **absolute** cargo binary, not the shell wrapper.
- A mixed-toolchain target dir can also throw spurious `E0514` errors; a
  `cargo clean` + pinned rebuild clears them.

**Consequence:** do not trust a local `/tmp` build to verify a merged
tree. Full-suite verification of a merge belongs on **CI**, which runs in
a clean, resourced environment. Use the local build only for the cheap,
reliable checks — text-level merge-conflict probes and single-target
compiles.

## Parallel agents in one shared workspace: never `git stash`

Added 2026-07-08. Two coder agents ran concurrently in one shared
Conductor workspace with uncommitted work from three writers in the
tree. One agent ran a `git stash` / `stash pop` round-trip to "isolate
its diff". The stash swept up **every** writer's uncommitted edits; the
pop conflicted on a file another agent was mid-edit on, and that agent
watched its files "silently revert", burned its context budget
re-deriving lost state, and handed off unfinished.

`<certain>` the mechanism: `git stash` operates on the whole worktree,
never on one agent's edits. The same goes for any tree-wide destructive
git operation: `checkout -- .`, `restore`, `reset --hard`.

**Rules that follow:**
- An agent that needs an isolated diff must get a real isolation
  worktree at dispatch time — never fake isolation with `stash` in a
  shared tree.
- Orchestrators dispatching parallel coders into one workspace should
  state it in the brief: "sibling agents are editing other files; do
  not run destructive git commands (stash / checkout -- . / restore /
  reset)".
- If files appear to revert mid-session, suspect a sibling's tree-wide
  git operation first, then the tilth parent-checkout leak above.
