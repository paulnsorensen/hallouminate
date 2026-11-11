---
status: reviewed
last_verified: 2026-11-11
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/pull/724
  - https://github.com/paulnsorensen/hallouminate/pull/725
  - https://github.com/pykeio/ort/issues/666
  - https://github.com/Anush008/fastembed-rs/pull/348
---
# ORT device policy — why every session sets PreferCPU

Every ONNX session must set `AutoDevicePolicy::PreferCPU`. Without it,
`ort` rc.15 on macOS runs the embedder on CoreML. The daemon footprint
then grows to ~21 GB, and a 156-file index does not finish in 15 min.
`cpu_no_arena()` does not prevent this.

## Mechanism

- `ort` sets `AutoDevicePolicy::MaxEfficiency` on every `SessionBuilder`
  (`ort-2.0.0-rc.15/src/session/builder/mod.rs:123`). On macOS this policy
  selects CoreML.
- ONNX Runtime applies the policy only when no execution provider is
  appended. `ort::ep::CPU::register` only toggles the arena. It appends no
  provider, so the policy always applies.
- ONNX Runtime 1.28 (`ort` rc.13) had no selectable CoreML device, so the
  work stayed on the CPU. ONNX Runtime 1.31 (`ort` rc.15) added CoreML to
  device-based EP selection. #632 (`ort` rc.13 → rc.15) caused the regression.
- The `ort` maintainer plans `PreferCPU` as the macOS default "in future
  versions" (pykeio/ort#666). No release has it as of 2026-11-11.

## Fix

- `CPU_DEVICE_POLICY` in `crates/hallouminate-adapters/src/embedder.rs`
  is `AutoDevicePolicy::PreferCPU`. The embedder and the crossencoder pass
  it through fastembed's `with_auto_device`. fastembed 7.6.0 added that
  method (Anush008/fastembed-rs#348). The adapters require `fastembed = "7.7"`.
- The arena and KleidiAI fixes still apply under `PreferCPU`. The verbose
  log shows `enable_cpu_mem_arena:0` and `mlas.disable_kleidiai: 1` on
  both sessions. See [[ort-arena-retention]].

## Guard

- `crates/hallouminate/tests/it/session_device.rs` runs the real
  `hallouminate config download` with `ORT_LOG=verbose`. It requires two
  `All nodes placed on [CPUExecutionProvider]` lines and no
  `CoreMLExecutionProvider` line. With `with_auto_device` removed from the
  embedder, it fails with 77 CoreML lines.
- The test is `#[ignore]` (model download), so CI does not run it. For
  that reason Renovate opens `ort` and `ort-sys` updates only after
  Dependency Dashboard approval, and Dependabot ignores them. Run the test
  on macOS before you approve an `ort` bump:
  `just verify cargo test -p hallouminate --test it session_device -- --ignored`.

## How to check a build

Run `ORT_LOG=verbose hallouminate config download` and read stderr. CPU
placement prints `All nodes placed on [CPUExecutionProvider]` once per
session. CoreML placement prints `CoreMLExecutionProvider` partition lines.
On a live daemon, `sample <pid> 2` shows `CoreMLExecutionProvider` and
`MLNeuralNetworkEngine` frames when CoreML runs.

## Measured (156 markdown files, isolated daemon, arctic-embed-s fp32)

| Build | Index | Peak | After index | `ground` | CoreML frames |
|---|---|---|---|---|---|
| rc.15, default policy | not done in 900 s | 21–22 GB | ~21 GB | 33–49 s | 371 |
| rc.13 pin (#724) | 31–61 s | 1.42–2.08 GB | ~210 MB | 11–23 ms | 0 |
| rc.15 + `PreferCPU` (#725) | 27–53 s | 1.59–1.76 GB | ~207 MB | 10–25 ms | 0 |

Index time varied with machine load. The two CPU builds performed the same.

_Source: CoreML regression investigation and fix, 2026-11-05 to 2026-11-11 · Bench scripts and research: `.context/bench/`, `.cheese/research/coreml-ep-ort-rc15/`_
