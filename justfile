# hallouminate dev commands.
#
#   just build::ci    checks only, no writes (mirrors .github/workflows/ci.yml)
#   just build::llm   auto-fix formatting + lints, then verify
#
# (`just build ci` / `just build llm` also work.)
#
# rustup honors rust-toolchain.toml (the crate's MSRV). protoc must be
# installed locally for the lancedb build script:
#   macOS:  brew install protobuf
#   Debian: sudo apt-get install -y protobuf-compiler

mod build

default:
    @just --list
