# ADR — daemon-idle-exit-001

### ADR-001: Idle-exit replaces session-eviction  [status: accepted]

- **Context:** #161's idle eviction drops the ONNX `Embedder` after `idle_evict_secs` and logs "ORT BFCArena memory released", but upstream ONNX Runtime's CPU BFCArena retains its extents after `ReleaseSession` (onnxruntime #26831, #11627, #23339). Each evict→reload cycle stacks a fresh arena: a live daemon measured 12.1GB footprint (peak 18.8GB) after ~15 cycles, confirmed by `vmmap` minutes after an eviction fired. Eviction is net-negative — worse than either keep-alive or process exit.
- **Decision:** Delete the eviction machinery (timer task `state.rs:303-340`, `evict_if_idle_at`, its tests) and exit the whole process when idle: activity clock quiet for `[daemon].idle_exit_secs` and zero active connections, via `shutdown_token.cancel()` — the same clean path as SIGTERM. `embeddings.idle_evict_secs` stays parse-compatible as a warned no-op.
- **Alternatives:** Keep eviction (rejected: demonstrably releases nothing and multiplies the leak); coexist dormant (rejected: dead code invites re-trusting #161's premise); do nothing (rejected: unbounded swap bloat); disable the ORT arena (rejected: fastembed exposes no session-options passthrough — upstream ask filed separately).
- **Consequences:** Memory genuinely returns to the OS. Costs a ~4s cold start on next use — identical to what eviction's model reload already paid — plus a client-side respawn path (ADR-002). Escape hatch: `idle_exit_secs = 0`.
