# Release ceremony gotchas

The release flow is scripted (`just prepare-release <ver>` → merge bump PR → `just release <ver>` from green main; cargo-dist's release.yml does the rest). Things the scripts don't tell you, learned cutting v0.3.1 (2026-07-12):

## Stale `nightly` tag breaks both recipes

Both `prepare-release` and `release` run `git fetch origin main --tags`, which **rejects** when the local `nightly` tag differs from remote ("would clobber existing tag") — and nightly moves every night, so a checkout older than a day always fails. Run `git fetch origin --tags --force` first. Why not fix the recipe: a plain `--tags` fetch refusing to clobber is git's safety default; forcing inside the recipe would silently move *release* tags too if they ever diverged.

## npm publish rides release.yml as a cargo-dist custom publish job

Historical bug (fixed 2026-07-12): `publish-npm.yml` originally triggered on `release: published` and **never fired once** — release.yml publishes the GitHub Release with `GITHUB_TOKEN`, and GitHub suppresses workflow triggers from `GITHUB_TOKEN`-created events (v0.3.1 consequently never reached npm; only 0.3.0, published by hand, exists there).

Current design: `publish-jobs = ["./publish-npm"]` in dist-workspace.toml; publish-npm.yml is `on: workflow_call` (dist passes the `plan` manifest; the version check derives the tag from `github.ref_name` — the caller's context). Runs after the `host` phase, so release tarballs are live before npm postinstall needs them. Two things to know:

- **npm Trusted Publisher must name `release.yml` (the CALLER), not publish-npm.yml** — npm validates the calling workflow's filename for reusable workflows (docs.npmjs.com/trusted-publishers), exact and case-sensitive. Repointing it on npmjs.com was a manual one-time step; if npm publish ever fails `ENEEDAUTH`, check this first.
- **The release.yml call-site job (`custom-publish-npm`) is hand-maintained** — `allow-dirty = ["ci"]` means dist never regenerates release.yml, so the publish-jobs config alone doesn't wire it. The job mirrors dist 0.32.0's generated shape (verified against j178/prek). A future `dist generate` regen would include it natively but clobber the hand-pinned action SHAs.

Full cited research: `.cheese/research/npm-publish-tagged-release/`.

## macOS CI flake on the idle-exit daemon test

`idle_exit_removes_socket_and_lockfile_and_refuses_new_connections` can fail on macOS with `WouldBlock` re-flocking the single-instance lockfile: the test relocks immediately after the serve future returns, racing the async drop of the lock-holding fd. Rerun passes. Tracked in [issue #207](https://github.com/paulnsorensen/hallouminate/issues/207) with a bounded-retry fix direction. Don't burn time bisecting a red macOS leg on this test — check the panic line (tests/daemon.rs ~1780) first.

Related: [[daemon-and-cli]], [[worktree-dev-gotchas]]
