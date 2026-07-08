---
status: reviewed
last_verified: 2026-07-07
confidence: high
sources:
  - https://github.com/microsoft/onnxruntime/issues/26831
  - https://github.com/microsoft/onnxruntime/issues/11627
  - https://github.com/microsoft/onnxruntime/issues/23339
---
# ORT BFCArena retention — why session eviction never reclaimed memory

Dropping the `Embedder` does **not** return memory to the OS. The idle
session-eviction feature (#161) logged "ORT BFCArena memory released"
and released nothing — do not re-trust that log line or the feature's
premise. It is replaced by daemon idle-exit (spec `daemon-idle-exit`;
rationale in `docs/adr/daemon-idle-exit-001..003.md`).

## The mechanism

- During an embed run, ONNX Runtime's CPU BFCArena grows in ~128MB
  extents to fit the inference-scratch high-water mark (attention
  scores dominate: batch × heads × seq² × 4 bytes). It is a
  high-water-mark allocator: it never shrinks afterwards.
- fastembed's internal defaults — 256 sequences × 512 max tokens per
  ONNX run, memory-pattern preallocation on, no session options
  exposed — set that high-water mark in the multi-GB range for a
  128MB-on-disk model (`src/adapters/embedder.rs`, fastembed
  `common.rs::init_session_builder`).
- The upstream bug: releasing the session does not return the arena's
  extents either. ort's `Drop` correctly calls `ReleaseSession`; the
  retention is inside ONNX Runtime (issues #26831, #11627, #23339;
  maintainer guidance: "For CPU we recommend disabling the arena all
  together" — which fastembed gives no way to do).
- Net effect of evict→reload cycling: each cycle abandons the old
  arena and grows a fresh one. Measured on a live daemon (v0.2.4,
  Jul 2026): 15 logged evictions ≈ 12.1GB footprint, peak 18.8GB, 118
  live `MALLOC_LARGE` regions (many exactly 128MB), still present in
  `vmmap` minutes after an eviction fired. Eviction is strictly worse
  than doing nothing.

## What actually works

- **Process exit.** The OS reclaims everything; the next start pays
  the same ~4s model load eviction already paid. This is the
  `daemon-idle-exit` design: exit after `[daemon].idle_exit_secs` of
  inactivity with zero active connections; clients respawn via
  `client_for(None)` → `ensure_daemon_running()`.
- **Smaller high-water mark.** Capping the embed batch
  (`embed(texts, Some(32))` instead of `None`) shrinks the arena
  peak roughly proportionally — a separate mitigation, not a fix.

## For future agents

- `embeddings.idle_evict_secs` is a deprecated no-op after
  `daemon-idle-exit` lands; the config warns and does nothing.
- Full cited diagnosis: `.cheese/research/fastembed-ort-arena-leak/`.
