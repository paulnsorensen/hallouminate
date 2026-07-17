# ADR daemon-rework-006: Contain lance #223 in-process, fix upstream  [status: accepted]

- **Context:** lance 8.0.0's `filtered_read.rs:426` unwraps a `JoinError`, panicking on `JoinError::Cancelled` when a scan task is cancelled (observed at shutdown when `abort_all` fires after the drain deadline). Confirmed byte-identical since lance 6.0.0; no upstream issue tracks it. The panic lives in lance's own spawned task — our supervision boundary can contain it but not remove it.
- **Decision:** Wrap lance scan calls in supervised spawns whose `JoinError` is observed and mapped to a failed RPC (`is_panic`/`is_cancelled` — never unwrapped); file the one-line fix upstream (map the JoinError instead of unwrapping).
- **Alternatives rejected:** vendor-patching lance via `[patch]` (immediate certainty, ongoing maintenance across every lance upgrade); contain-only (every upgrade re-rolls the dice, shutdown logs keep the panic noise, upstream never learns).
- **Consequences:** shutdown logs stay noisy until upstream merges; the containment boundary must be tested with a deliberately cancelled scan.
