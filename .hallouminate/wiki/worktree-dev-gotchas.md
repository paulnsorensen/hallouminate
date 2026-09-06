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


## An abandoned worktree branch can hold the only copy of a real fix

Added 2026-07-29. A `/pasteurize` investigation running in an isolated worktree
diagnosed a genuine gate blind spot, wrote the fix, committed it to the
worktree's own branch (`worktree-agent-adeb6eee59a2309c1`, commit `a6f1ba2`),
and recorded the diagnosis in its `.cheese/pasteurize/` report — but the branch
was never merged and the worktree was abandoned. The report read as though the
fix had landed. It had not: `git merge-base --is-ancestor a6f1ba2 main` returned
false weeks later, and the blind spot it closed (a degraded ripgrep signal being
silently measured as a valid baseline) was still wide open in `main`.

`<certain>` the mechanism — a session report describing a fix is evidence the
fix was *written*, never evidence it was *merged*. Nothing in the pipeline
reconciles an abandoned worktree branch against `main`.

**Rules that follow:**
- Before trusting a prior session's report that a fix exists, verify it against
  the trunk: `git merge-base --is-ancestor <sha> main`, or
  `git branch --contains <sha>`.
- When closing out a worktree, either merge its branch or state plainly in the
  handoff that the work is unlanded — "fixed in `<sha>`" without a merge is a
  claim the next reader will misread.
- Periodically sweep `git branch --list 'worktree-agent-*'` for branches holding
  commits that are not ancestors of `main`.

## Wiki index rows stomp across worktrees

Added 2026-07-13. Indexing or `add_markdown`-ing the repo wiki from one
worktree **deletes the sibling worktrees' rows** for that corpus from the
shared LanceDB table — on-disk files are untouched, but `ground` stops
finding pages written from the other checkout. If wiki pages you wrote
"disappear" from search after working in another worktree, this is why.
Mechanism, symptoms, and the agreed fix direction (#215):
[worktree-corpus-identity](worktree-corpus-identity.md). Recover the view
you want with `hallouminate index` from the checkout you care about.


## Mise shims can override the verification toolchain

Mise shims can restore a toolchain override after `env -u RUSTUP_TOOLCHAIN`.
The measured #453 session selects Rust 1.98 through both `cargo` and `just` shims,
although `rust-toolchain.toml` pins 1.97.[^mise]

Put rustup proxies first in a clean `PATH`.
Invoke the installed `just` binary directly.
Keep every compiler-heavy command inside the repository verification lease.

```sh
env PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  RUSTUP_TOOLCHAIN=1.97 /opt/homebrew/bin/just verify
```

Read the current repository pin before copying this version.

The `env -u RUSTUP_TOOLCHAIN` workaround is **conditional, not superseded**.
It selects the pinned toolchain when the rustup proxies precede the mise shim in `PATH`.
It fails when the mise shim comes first.
A 2026-09-05 measurement in the `macau-v1` worktree resolves `cargo` to
`/opt/homebrew/opt/rustup/bin/cargo` before `~/.local/share/mise/shims/cargo`,
and `env -u RUSTUP_TOOLCHAIN rustc --version` reports 1.97.1 correctly.[^pathorder]
The explicit-`PATH` form above works in both orders, so prefer it in an agent brief.
Always confirm the selection before you trust a gate result:

```sh
env -u RUSTUP_TOOLCHAIN rustc --version   # must report the rust-toolchain.toml pin
```
A silent gate can also wait for another worktree's verification lease.
Check the lease records and owning process before treating that wait as a compiler hang.[^lease]

[^mise]: Issue #453 closeout, 2026-09-05: `type -a cargo just`, `rustup show active-toolchain`, and `env -u RUSTUP_TOOLCHAIN rustc --version` select the mise override; `rustup run 1.97 rustc --version` reports 1.97.1. Repository pin: rust-toolchain.toml:4-5.
[^lease]: scripts/verify.py::run_leased; AGENTS.md::Local verification

## `mcp_serve` tests time out under host load, not from a code fault

Added 2026-09-05. The `it` integration binary fails with
`response within timeout: Elapsed(())` at `crates/hallouminate/tests/it/mcp_serve.rs`
when the host runs many worktrees at once.
Each `mcp_serve` test spawns a real daemon subprocess and waits on a 15-second
`READ_TIMEOUT` for the JSON-RPC reply.
The deadline is wall-clock, so host contention alone exhausts it.

`<certain>` this is contention and not a logic fault.
One tree measured on the same commit, varying only parallelism:[^mcpload]

| Invocation | Result |
| --- | --- |
| `just verify` (default parallelism) | 20 failed, exit 101 |
| `cargo test -p hallouminate --test it` | 11 failed |
| `cargo test -p hallouminate --test it -- --test-threads=4` | 182 passed, 0 failed, exit 0 |
| `--test it mcp_serve` filter alone | 25 passed, 0 failed |

The discriminator is that **every** failure is the same read deadline with
**zero** assertion failures.
A real regression changes values; it does not produce uniform timeouts.

PR #459 grew `mcp_serve` from 22 to 27 daemon-spawning tests, which lowers the
load needed to trip the deadline.

**Rules that follow:**
- Check `uptime` before you trust an `it`-suite failure. Load average above ~20 on
  this host makes the default parallelism unreliable.
- Re-run with `-- --test-threads=4` before you attribute the failure to your change.
- Do not "fix" `mcp_serve` and do not raise `READ_TIMEOUT` in response to this.
- Report any `mcp_serve` failure that is **not** this exact signature — that one is real.

## Squash merges leave residue that the melt detector misses

Added 2026-09-05. This repository squash-merges pull requests.
A branch stacked on another branch therefore keeps a base commit whose content
already reached `main` under a different SHA and a different commit message.

`/melt`'s `detect-squash-residue` compares whole trees.
It returned `not-detected` for a branch stacked on `paulnsorensen/gh-issue-453`
after that branch merged as PR #459, because the squash commit contained **more**
than the stacked base commit did, so no tree matched.[^squash]

`<certain>` the reliable offline check is blob comparison, not tree comparison:

```sh
git rev-parse <base-commit>:<path>    # compare against
git rev-parse origin/main:<path>      # identical blobs => content already landed
```

Then drop the superseded commit with `git rebase --onto origin/main <base-commit>`.
Git's rename detection carries upstream edits across a file that the branch
renamed, but verify that rather than assume it: diff the upstream-added and
upstream-removed lines against the renamed file before you continue.

[^mcpload]: Issue #427 slice-3 resume, 2026-09-05. Host at load average 25-33 with ~12 concurrent worktrees. `READ_TIMEOUT` is `Duration::from_secs(15)` at mcp_serve.rs:30.
[^squash]: Issue #427 rebase, 2026-09-05. Base `a1219fd` versus squash `e745955`; `walker.rs` and `sandbox.rs` blobs identical, detector verdict `not-detected`.
[^pathorder]: Issue #427 resume, 2026-09-05: `type -a cargo` in the `macau-v1` worktree lists the rustup proxy before the mise shim; `env -u RUSTUP_TOOLCHAIN cargo --version` reports 1.97.1.

_Source: issue #453 verification diagnosis; issue #427 slice-3 resume · Updated: 2026-09-05 · Supersedes: unset-only toolchain selection for mise shims_
