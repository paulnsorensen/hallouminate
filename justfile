# hallouminate dev commands — run `just` to list recipes.
#
# rustup honors rust-toolchain.toml (the crate's MSRV). protoc must be
# installed locally for the lancedb build script:
#   macOS:  brew install protobuf
#   Debian: sudo apt-get install -y protobuf-compiler

default:
    @just --list

# CI gate — checks only, no writes (mirrors .github/workflows/ci.yml).
ci: _fmt-check _clippy _build _test

# For agents/LLMs — auto-fix formatting + machine-applicable lints, then verify.
llm: _fix _clippy _build _test

# Move v<version> to HEAD and push, retriggering release.yml + publish-crates.yml + release-skills.yml.
# Use to re-ship the SAME version after landing a release fix on main.
# Deleting the existing v<version> GitHub Release first makes the re-run idempotent:
# cargo-dist's host job runs a plain `gh release create`, which errors on an
# existing release. Re-created binaries replace it; crates.io publish self-skips
# (already published); the skill pack lives on the separate skills-v<version>
# release, so it is untouched here and clobbered by release-skills.yml anyway.
re-tag version:
    @git diff --quiet HEAD || { echo "working tree dirty — commit first"; exit 1; }
    -gh release delete v{{version}} --yes
    -git push origin :refs/tags/v{{version}}
    -git tag -d v{{version}}
    git tag v{{version}}
    git push origin v{{version}}

_fmt-check:
    cargo fmt --all --check

_fix:
    cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged
    cargo fmt --all

_clippy:
    cargo clippy --locked --all-targets --all-features -- -D warnings

_build:
    cargo build --locked --all-targets

_test:
    cargo test --locked
