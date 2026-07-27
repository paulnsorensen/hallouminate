---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - Cargo.toml
  - npm/package.json
  - npm-nightly/package.json
  - npm/install.js
  - dist-workspace.toml
  - https://github.com/paulnsorensen/hallouminate/issues/198
---
# Release and packaging

All workspace crates share one version, and neither npm package ships source — both wrap a prebuilt binary fetched from a GitHub release. This page covers how the version is set, why two npm packages exist, and what a release before 1.0 does not promise.

## One version, workspace-wide

`Cargo.toml`'s `[workspace.package]` declares `version = "0.5.0"`, and every member crate (`hallouminate`, `hallouminate-domain`, `hallouminate-adapters`, `hallouminate-config`, `hallouminate-daemon`) inherits it via `version.workspace = true` rather than pinning its own.[^1] A release is therefore one version bump across the whole workspace, not a per-crate negotiation — `just prepare-release <version>` validates the requested string is bare SemVer, requires a clean tree on `main` synced with `origin/main`, and cuts a `release/v<version>` branch from there.[^2] Cross-crate version drift is not a thing this workspace has to reconcile; the workspace inheritance mechanism rules it out structurally.

## Distributed, not built from source

Consumers install `hallouminate` from npm, and the npm package does not compile anything — its `postinstall` script downloads a `cargo-dist`-built archive matching the platform. `install.js` maps `${process.platform}-${process.arch}` to one of three Rust target triples (`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`) and fetches `hallouminate-<target>.tar.xz` from the tagged GitHub release; an unsupported platform gets a clear error and a `cargo install hallouminate` fallback rather than a silent failure.[^3] `dist-workspace.toml` is the source of that target list — note macOS Intel is dropped entirely because `ort` (the ONNX Runtime binding fastembed uses) ships no prebuilt for it, and building it from source is out of scope.[^4]

The reason to distribute a binary rather than let npm build from source is the same `ort`/`fastembed` dependency: it needs a matched, prebuilt ONNX Runtime per platform, pinned CI runners to link against the right glibc version, and a build environment far heavier than an npm `postinstall` should assume. Shipping the artifact `cargo-dist` already built for CI removes all of that from the install path.

## Two npm packages: stable and nightly

`npm/` publishes the `hallouminate` package at the tagged release version, downloading from that version's tag. `npm-nightly/` publishes `@paulnsorensen/hallouminate-nightly`, whose `package.json` describes itself as an "EXPERIMENTAL rolling build of paulnsorensen/hallouminate off main — NOT the official `hallouminate` package."[^5] Its installer fetches from the fixed `nightly` prerelease tag rather than a version-specific one, and always re-downloads because "each npm version maps to a fresh nightly build" — there is no cached-binary short-circuit the stable installer has.[^6] The two packages exist so a user can track `main` deliberately (nightly) without that ever becoming the default install path for `hallouminate` itself.

## What a release does not promise before 1.0

The workspace version sits at 0.5.0. A 0.x version under SemVer carries no compatibility guarantee across a minor bump — nothing pins CLI flags, MCP tool schemas, or on-disk config/index formats as stable across releases yet. Schema migrations for the Lance store, for instance, already trigger a full rebuild on an older store rather than an in-place upgrade path; that is acceptable pre-1.0 precisely because no promise has been made that an old store keeps working forever. Treat every 0.x release as free to break format or interface compatibility until the workspace crosses 1.0, at which point that latitude goes away.

See [architecture](architecture.md), [testing-strategy](testing-strategy.md), [worktree-dev-gotchas](worktree-dev-gotchas.md).

[^1]: `Cargo.toml:5-6`; e.g. `crates/hallouminate-domain/Cargo.toml:3`
[^2]: `justfile:34-55`
[^3]: `npm/install.js:11-35`
[^4]: `dist-workspace.toml:1-23`
[^5]: `npm-nightly/package.json`
[^6]: `npm-nightly/install.js:35-46`

_Source: Cargo.toml, dist-workspace.toml, npm/ and npm-nightly/ · Updated: 2026-07-18_
