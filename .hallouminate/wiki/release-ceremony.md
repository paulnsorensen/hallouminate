# Release ceremony gotchas

The release flow is scripted (`just prepare-release <ver>` → merge bump PR → `just release <ver>` from green main; cargo-dist's release.yml does the rest). Things the scripts don't tell you, learned cutting v0.3.1 (2026-07-12):

## Stale `nightly` tag breaks both recipes

Both `prepare-release` and `release` run `git fetch origin main --tags`, which **rejects** when the local `nightly` tag differs from remote ("would clobber existing tag") — and nightly moves every night, so a checkout older than a day always fails. Run `git fetch origin --tags --force` first. Why not fix the recipe: a plain `--tags` fetch refusing to clobber is git's safety default; forcing inside the recipe would silently move *release* tags too if they ever diverged.

## npm publish rides release.yml as a cargo-dist custom publish job

Historical bug (fixed 2026-07-12): `publish-npm.yml` originally triggered on `release: published` and **never fired once** — release.yml publishes the GitHub Release with `GITHUB_TOKEN`, and GitHub suppresses workflow triggers from `GITHUB_TOKEN`-created events (v0.3.1 consequently never reached npm; only 0.3.0, published by hand, exists there).

Current design: `publish-jobs = ["./publish-npm"]` in dist-workspace.toml; publish-npm.yml is `on: workflow_call` (dist passes the `plan` manifest; the version check derives the tag from `github.ref_name` — the caller's context). Runs after the `host` phase, so release tarballs are live before npm postinstall needs them. Two things to know:

- **npm Trusted Publisher must name `release.yml` (the CALLER), not publish-npm.yml** — npm validates the calling workflow's filename for reusable workflows (docs.npmjs.com/trusted-publishers), exact and case-sensitive. Repointing it on npmjs.com was a manual one-time step; if npm publish ever fails `ENEEDAUTH`, check this first. Verified live by the v0.3.2 cut (2026-07-12, run 29208478197): tag → dist → GitHub Release → OIDC npm publish, all green — the pipeline's first end-to-end run.
- **The release.yml call-site job (`custom-publish-npm`) is hand-maintained** — `allow-dirty = ["ci"]` means dist never regenerates release.yml, so the publish-jobs config alone doesn't wire it. The job mirrors dist 0.32.0's generated shape (verified against j178/prek). A future `dist generate` regen would include it natively but clobber the hand-pinned action SHAs.

Full cited research: `.cheese/research/npm-publish-tagged-release/`.

## Reusable-workflow call sites zero unlisted permission scopes

A `permissions:` block on a job that `uses:` a reusable workflow REPLACES the workflow-level grant — every unlisted scope becomes `none`, and GitHub validates the callee's requested scopes against that at graph setup. `custom-publish-npm` originally granted only `id-token: write`; publish-npm.yml's jobs request `contents: read`, so every release.yml run died with `startup_failure` ("The nested job 'publish-npm' is requesting 'contents: read', but is only allowed 'contents: none'") — including tag pushes, which would have silently killed the release. Fixed 2026-07-12 by listing `contents: read` alongside `id-token: write` at the call site. Startup failures produce NO failing check on the PR — they only show in the Actions tab — so a green PR does not prove release.yml still parses; check `gh run list --workflow Release` after touching it.

## Nightly smoke test matches the binary's version line

`hallouminate --version` prints `hallouminate X.Y.Z` (clap's `name version` shape), and the wrapper package version (`0.0.0-experimental.N.M`) never appears in binary output. The smoke assertion greps for `^hallouminate <semver>` — the first cut anchored on bare `^<semver>` and failed all 10 attempts despite exit 0 (fixed 2026-07-12).

## macOS CI flake on the idle-exit daemon test (fixed)

`idle_exit_removes_socket_and_lockfile_and_refuses_new_connections` could fail on macOS with `WouldBlock` re-flocking the single-instance lockfile: the test relocked immediately after the serve future returned, racing the async drop of the lock-holding fd. Fixed in [PR #210](https://github.com/paulnsorensen/hallouminate/pull/210) ([issue #207](https://github.com/paulnsorensen/hallouminate/issues/207)) with a bounded 2s retry that only retries `EWOULDBLOCK`. If it reappears, the panic line is tests/daemon.rs ~1790.

## A failed Release run half-publishes the version

A `v*` tag starts three workflows, and they do not wait for each other. `publish-crates.yml` and `release-skills.yml` publish to crates.io and `skills-v<ver>`. `release.yml` builds the binaries, then creates the GitHub Release and calls npm. If one dist build fails, crates.io and skills still publish, but the GitHub Release and npm do not. A crates.io version cannot be reused, and a published tag must not move. So the only recovery is a new patch version.

This happened twice on 2026-11-21. v0.12.0 and v0.12.1 exist on crates.io and as skills releases, with no GitHub Release or npm package. v0.12.2 is the first complete 0.12 release.

Prove a release.yml change on a PR before you tag. release.yml runs on `pull_request`, but dist builds only `plan` there. On a throwaway branch, set `pr-run-mode = "upload"` in `[dist]`, and limit `targets` if you need to. A PR run has `publishing: false`, so it builds every artifact and publishes nothing. Revert the harness before merge. PR #780 used this method (run 44938778934).

## macOS dist build needs two pins

- **Runner.** dist 0.32 defaults `aarch64-apple-darwin` to `macos-14`, which GitHub no longer supports. The job ends in 1 s with "The configuration 'macos-14-arm64-us-default' is not supported". `dist-workspace.toml` pins `aarch64-apple-darwin = "macos-latest"`, the runner that nightly.yml uses (PR #777).
- **Clang runtime.** The prebuilt onnxruntime in `ort-sys` uses Objective-C `@available`, which calls `___isPlatformVersionAtLeast`. Rust std defines that symbol, but the `dist` profile's `lto = "thin"` drops it (rust-lang/rust#164915). rustc links with `-nodefaultlibs`, so clang does not add its runtime. The link then fails with "Undefined symbols … ___isPlatformVersionAtLeast". The "Expose Xcode compiler runtime" step in release.yml appends `-Clink-arg=<…>/libclang_rt.osx.a` to `RUSTFLAGS` (PR #780).
  - Use `RUSTFLAGS`, not `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS`. dist sets `RUSTFLAGS` from the environment, and that overrides the target-specific variable.
  - nightly.yml does not need this, because the `release` profile has no LTO. A green nightly therefore does not prove that the dist build links.

Related: [[daemon-and-cli]], [[worktree-dev-gotchas]]
