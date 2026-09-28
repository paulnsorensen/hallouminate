# Contributing to hallouminate

Thanks for your interest. Contributions of all sizes are welcome — a
typo fix is just as useful as a feature. This document describes how
to get from "I want to help" to "my change is merged".

## Filing issues

- Search [open issues](https://github.com/paulnsorensen/hallouminate/issues)
  before opening a new one.
- Use the bug-report or feature-request template.
- For security vulnerabilities, do **not** open a public issue — see
  [`SECURITY.md`](./SECURITY.md).

## Setting up locally

Development requires Rust, `protoc`, Python 3, and
[just](https://github.com/casey/just) 1.53 or newer. Clone the repository and
route the initial build through the verification lease:

```sh
git clone https://github.com/paulnsorensen/hallouminate.git
cd hallouminate
just verify cargo build --locked --all-targets
```

## Optional local compiler caching

Install `sccache` if you want local compiler caching:

```sh
brew install sccache
```

`just verify` detects `sccache` on `PATH` and uses its absolute path for
leased Cargo commands. It sets `CARGO_INCREMENTAL=0` only when it enables this
automatic cache. Run `sccache --show-stats` to inspect cache activity.

Set `RUSTC_WRAPPER=` to opt out. Any explicit Rust wrapper variable or
`CARGO_INCREMENTAL=1` also disables automatic setup. Direct `cargo` commands
remain unchanged. Wrappers configured only in Cargo config are not visible to
this check; export the wrapper variable to preserve one, or opt out explicitly.
By default, `just verify` uses the toolchain selected by
`rust-toolchain.toml`. PATH or `RUSTUP_TOOLCHAIN` overrides can select another
toolchain and change cache keys.

## Running tests

Run the full local gate before opening a PR:

```sh
just verify
```

Use the same entrypoint for a focused Cargo check:

```sh
just verify cargo test -p hallouminate --test it verification_gate
```

Do not run compiler/linker-heavy Cargo gates concurrently in linked worktrees.
The lease prevents duplicate heavy work for this repository only; host-wide
admission and resource isolation belong to the execution environment. See
[Development verification](./site/src/content/docs/development-verification.md) for the
lease, evidence log, compatibility routes, and fix mode.

## Submitting a pull request

1. Fork the repo and create a topic branch from `main`.
2. Make your change. Keep commits focused; one concern per commit is
   easier to review than a kitchen-sink commit.
3. Use [Conventional Commits](https://www.conventionalcommits.org)
   for the PR title (e.g. `feat: add X`, `fix: handle Y`,
   `docs: explain Z`). Squash-merge will use the PR title as the
   commit subject.
4. Fill out the PR template — the "why" matters more than the "what".
5. Wait for CI to go green and address review feedback.

## Code of Conduct

Participation in this project is governed by the
[Contributor Covenant](./CODE_OF_CONDUCT.md). By contributing you
agree to abide by it.

## Licensing

By submitting a contribution you agree that it will be licensed under
the same terms as the project itself (see [`LICENSE`](./LICENSE)).
