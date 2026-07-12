# Release ceremony gotchas

The release flow is scripted (`just prepare-release <ver>` → merge bump PR → `just release <ver>` from green main; cargo-dist's release.yml does the rest). Three things the scripts don't tell you, learned cutting v0.3.1 (2026-07-12):

## Stale `nightly` tag breaks both recipes

Both `prepare-release` and `release` run `git fetch origin main --tags`, which **rejects** when the local `nightly` tag differs from remote ("would clobber existing tag") — and nightly moves every night, so a checkout older than a day always fails. Run `git fetch origin --tags --force` first. Why not fix the recipe: a plain `--tags` fetch refusing to clobber is git's safety default; forcing inside the recipe would silently move *release* tags too if they ever diverged.

## `publish-npm.yml` has never fired

It triggers on `release: published`, but the release is published by release.yml using `GITHUB_TOKEN`, and GitHub suppresses workflow triggers from `GITHUB_TOKEN`-created events. Zero runs in history (checked v0.3.0 and v0.3.1). If npm publishing is ever wanted, the trigger needs a PAT/app token on the release step or a `workflow_run` chain.

## macOS CI flake on the idle-exit daemon test

`idle_exit_removes_socket_and_lockfile_and_refuses_new_connections` can fail on macOS with `WouldBlock` re-flocking the single-instance lockfile: the test relocks immediately after the serve future returns, racing the async drop of the lock-holding fd. Rerun passes. Tracked in [issue #207](https://github.com/paulnsorensen/hallouminate/issues/207) with a bounded-retry fix direction. Don't burn time bisecting a red macOS leg on this test — check the panic line (tests/daemon.rs ~1780) first.

Related: [[daemon-and-cli]], [[worktree-dev-gotchas]]
