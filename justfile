# hallouminate dev commands — run `just` to list recipes.
#
# rustup honors rust-toolchain.toml (the crate's MSRV). protoc must be
# installed locally for the lancedb build script:
#   macOS:  brew install protobuf
#   Debian: sudo apt-get install -y protobuf-compiler

default:
    @just --list

# Full CI gate, locally — mirrors .github/workflows/ci.yml.
ci: fmt-check clippy build test

# Auto-fix everything: clippy machine-applicable fixes, then format.
fix:
    cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged
    cargo fmt --all

# Format all code in place.
fmt:
    cargo fmt --all

# Check formatting without writing (CI mode).
fmt-check:
    cargo fmt --all --check

# Lint with clippy, warnings-as-errors (matches CI).
clippy:
    cargo clippy --locked --all-targets --all-features -- -D warnings

# Fast type-check, no binaries.
check:
    cargo check --locked --all-targets

# Build all targets.
build:
    cargo build --locked --all-targets

# Run the test suite.
test:
    cargo test --locked
