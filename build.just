# `build` module — invoked as `just build::ci` / `just build::llm`.

# CI gate — checks only, no writes (mirrors .github/workflows/ci.yml).
ci: _fmt-check _clippy _build _test

# For agents/LLMs — auto-fix formatting + machine-applicable lints, then verify.
llm: _fix _clippy _build _test

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
