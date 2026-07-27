---
status: reviewed
last_verified: 2026-07-18
confidence: high
sources:
  - crates/hallouminate/tests/it/daemon.rs
  - crates/hallouminate/tests/it/cli_index.rs
  - justfile
  - scripts/verify.py
  - https://github.com/paulnsorensen/hallouminate/issues/241
---
# Testing strategy: cheap gate, expensive recipes

The default `cargo test` run never touches an embedding model. Tests that need one are `#[ignore]`d and only run through explicit `just` recipes, so the everyday gate stays fast and the model-backed suite stays honest about what it actually exercises.

## Why the split exists

`hallouminate` embeds every ground query and every indexed chunk through `fastembed`/`ort`, which means loading a real ONNX model file. Doing that inside the default test binary would make `cargo test` slow on every run and flaky on any machine without the model cached or without a working ONNX Runtime build. The daemon integration suite states the reasoning directly: the end-to-end `add_markdown`-through-daemon test "downloads the embedding model on first run and is gated `#[ignore]` to keep CI fast", mirroring the same gate on `tests/cli_index.rs`.[^1] The tokenizer-download integration test carries the identical justification for the same reason — a real HTTP fetch of a tokenizer file has no place in a fast default gate.[^2]

`scripts/verify.py`, which every `just verify` invocation runs, drives `cargo test --locked` with no `--ignored` flag as one of its `HEAVY_COMMANDS`.[^3] An `#[ignore]`d test is therefore invisible to `just verify`, `just ci`, and `just llm` unless a recipe explicitly asks for it.

## Explicit recipes, not a blanket flag

The `justfile` wires named recipes that invoke a specific `#[ignore]`d test target directly — `cargo test -p hallouminate --test <name> -- --ignored --nocapture --exact`, routed through the same `just verify` wrapper the fast gate uses, so the heavy run still goes through the project's single verification lock rather than racing a concurrent `cargo test`.[^4] The pattern is deliberate: a contributor who wants the model-backed suite runs a named recipe, not a stray `--ignored` flag tacked onto an ordinary `just test` invocation. Running the expensive suite is an explicit act with a name attached to it, never something triggered by accident.

## What ordinary tests cover without loading a model

The bulk of the suite — corpus walking, sandbox path validation, config parsing, chunking, the daemon's request routing, markdown lint warnings — runs against fixed inputs and deterministic logic, with no embedder in the loop. Where a test needs an embedding-shaped value without paying for the real model, it constructs one directly rather than routing through `fastembed`. That keeps `cargo test`'s default run bounded by compile time and disk I/O, not by inference latency, while still exercising every code path that doesn't depend on model behavior itself.

## The principle behind the gate

A test earns its place by being able to fail when the behavior it guards changes. An `#[ignore]`d model test is not exempt from that bar — it still asserts on real embedding output and real download behavior — it is exempt from running by default. The gate is about cost and determinism (model downloads, hardware variance, network access), never about weakening what the test checks once it does run. A test that would pass whether or not the guarded behavior worked has no business existing in either bucket; the `#[ignore]` attribute controls *when* a test runs, not *whether* it is allowed to be shallow.

See [architecture](architecture.md), [worktree-dev-gotchas](worktree-dev-gotchas.md), [design-rationale](design-rationale.md).

[^1]: `crates/hallouminate/tests/it/daemon.rs:1-13`
[^2]: `crates/hallouminate/tests/it/real_tokenizer.rs:1-3`
[^3]: `scripts/verify.py:16-19`
[^4]: `justfile:25-31`

_Source: justfile, scripts/verify.py, tests/it/daemon.rs · Updated: 2026-07-18_
