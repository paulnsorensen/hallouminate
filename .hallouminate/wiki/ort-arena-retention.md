---
status: reviewed
last_verified: 2026-09-12
confidence: high
sources:
  - https://github.com/microsoft/onnxruntime/issues/29538
  - https://github.com/microsoft/onnxruntime/issues/29538#issuecomment-5643228246
  - https://github.com/Anush008/fastembed-rs/issues/291
  - https://github.com/Anush008/fastembed-rs/pull/292
  - https://github.com/microsoft/onnxruntime/issues/26831
  - https://github.com/microsoft/onnxruntime/issues/11627
  - https://github.com/microsoft/onnxruntime/issues/23339
  - https://github.com/paulnsorensen/hallouminate/issues/288
  - https://github.com/paulnsorensen/hallouminate/issues/221
  - https://github.com/paulnsorensen/hallouminate/issues/285
---
# ORT memory retention — arena, KleidiAI, and the fix

The daemon retained gigabytes after one embedding pass. Two ONNX Runtime
mechanisms cause this. Both are fixed in the adapters as of 2026-09-12.
The fix is measured, not assumed. Do not re-trust the earlier claim
that "ORT BFCArena memory released" on session drop. It released nothing.

## Mechanism 1: BFCArena (all platforms)

- During an embed run, the CPU BFCArena grows in 128 MB extents to the
  inference-scratch high-water mark. It never shrinks. Dropping the
  session does not return the extents (ORT #26831, #11627, #23339).
- Fix: disable the arena. `cpu_no_arena()` in
  `crates/hallouminate-adapters/src/embedder.rs` passes
  `ort::ep::CPU::default().with_arena_allocator(false)` through
  fastembed's `with_execution_providers`. Both the embedder and the
  crossencoder use it. ORT then uses plain malloc/free for scratch.

## Mechanism 2: KleidiAI thread-local GEMM buffers (Apple SME only)

- ORT 1.28.0 routes fp32 `MatMul` through KleidiAI when
  `HasArm_SME()` is true. That is M4-generation Apple Silicon and later.
  M1–M3 never enter this path.
- `sgemm_kleidiai.cpp` keeps packed operands in a
  `static thread_local KaiTlsBuffers`. The vectors `resize()` up and never
  shrink. One set exists per intra-op thread (fastembed uses
  `available_parallelism`). `malloc_history` on a live daemon showed three
  live allocations of 250 MB, 230 MB, 180 MB, zero frees, all from
  `MatMul<float>::Compute → ArmKleidiAI::MlasGemmBatch → operator new`.
- Upstream: onnxruntime#29538, stale-closed 2026-09-11 without a fix.
  Our SGEMM trace is posted there. No env or global switch exists; the
  cmake flag `onnxruntime_USE_KLEIDIAI` and the per-session config entry
  `mlas.disable_kleidiai` are the only gates. All macOS-arm64 prebuilts
  bake KleidiAI in.
- Fix: `DISABLE_KLEIDIAI` in `embedder.rs` sets
  `with_session_config("mlas.disable_kleidiai", "1")` on both
  `TextInitOptions` and `RerankInitOptions`.

## Why fastembed is patched

fastembed 6.0.3 builds the `SessionBuilder` in a `pub(crate)` function and
exposes no session config entries. `ort` gives no public way to wrap a
custom execution provider. The root `Cargo.toml` therefore pins
`[patch.crates-io] fastembed` to `paulnsorensen/fastembed-rs` at the commit
that adds `with_session_config`. Upstream: Anush008/fastembed-rs#291 (issue)
and #292 (PR). Remove the patch when a release ships the method.

## Measured (240 markdown files, boot catch-up embed, arctic-embed-s fp32, batch 32, max_length 416)

| Build | Settled footprint | Peak | Embed pass |
|---|---|---|---|
| 0.10.0 as shipped | 4843 MB | 5041 MB | — |
| arena off + max_length 416 | 2189 MB | 3860 MB | 10 s |
| + `mlas.disable_kleidiai` | **217 MB** | 2067 MB | 14–23 s |
| embeddings disabled (floor) | 170 MB | — | — |

Arena-off alone is not a fix on SME hardware. KleidiAI-off costs about 40 %
on the embed pass. The peak is transient scratch and returns to the OS.

## How to measure

Use `footprint <pid>` and `vmmap <pid> | grep MALLOC_LARGE`. Do not use RSS.
macOS compresses idle pages, so RSS read 120 MB while the footprint was
2.9 GB. To find an owner, start the daemon with `MallocStackLogging=1`,
then run `malloc_history <pid> <region-start-address>`. The bench scripts
are throwaway files under `/tmp/hm-bench/` (config with an isolated
`ground_dir`, `HALLOUMINATE_SOCKET` override, 240-file corpus).

## Still true

- Process exit reclaims everything. `daemon-idle-exit` remains the
  backstop, but it is starved under multi-instance fleets (#222): with
  ten MCP `serve` clients attached, a 900 s global gap is rare.
- `EMBED_BATCH_SIZE = 32` and `RERANK_BATCH_SIZE = 32` bound the transient
  peak. `EMBED_MAX_LENGTH = 416` (chunk budget 384 + 32 headroom) bounds
  the seq² term.
- `embeddings.idle_evict_secs` is a deprecated no-op.
- Prior diagnosis: `.cheese/research/fastembed-ort-arena-leak/`. This
  round: `.cheese/research/ort-kleidiai-gemm-retention/`,
  `ort-kleidiai-fix-status/`, `fastembed-session-options-issue/`,
  `rust-embedding-alternatives/`.

_Source: live-daemon footprint/vmmap/malloc_history investigation, 2026-09-12 · Supersedes: the arena-only explanation and the eviction-era claims_
