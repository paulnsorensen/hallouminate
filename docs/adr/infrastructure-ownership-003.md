# ADR — infrastructure-ownership-003

### ADR-003: Do not adopt ractor-supervisor for restart policy ownership  [status: accepted]

- **Context:** Spike #478 asks whether `ractor-supervisor` can replace the custom restart-window and failure accounting in `supervisor.rs`, `backoff.rs`, and `ladder.rs`. The accepted policy (wiki page `supervisor-restart-ladder`) restarts a task without limit, keeps sticky escalation strikes, and does not change the heartbeat on restart.
- **Decision:** Reject adoption. The custom `Supervisor` and `Ladder<A>` stay.
- **Evidence:**
  - Meltdown stops the supervisor. When restarts exceed `max_restarts` within `max_window`, the supervisor returns `SupervisorError::Meltdown` and shuts down. The daemon policy continues to restart and reports `RestartTask(name)`. An outer loop that restarts the supervisor after meltdown builds the removed loop again.
  - The crate has no ladder with a warning threshold and an action threshold. `backoff_fn` returns only `Option<Duration>`, so strike reports need a captured counter and manual log calls.
  - `restart_count` is per child, resets with `reset_after`, and is readable only through an async `InspectState` call. `daemon status` reads a lifetime count synchronously.
  - Native matches exist for two behaviors: the backoff curve through `backoff_fn`, and reset after a healthy run through `reset_after`.
  - The estimate is about 150 lines removable against 155 to 235 adapter lines. The five supervised loops are not actors, so `ractor` adds a second concurrency substrate.
  - `ractor` is well maintained. `ractor-supervisor` has one main author, about 10,600 downloads, and an 18-month interval between releases 0.1.9 and 0.2.0.
- **Policy items:** Persistent trip history and status reporting stay in `watchdog.rs` and `status.rs`. External liveness also relies on `supervisor.rs` preserving heartbeat epochs across task restarts; `watchdog.rs` observes the unchanged epoch. `ractor-supervisor` does not provide this application policy.
- **Tests not run:** The spike compiles no prototype for isolated child failure, restart-window exhaustion, reset-after behavior, or shutdown races. The crate source documents the meltdown and counter semantics, and those semantics decide the result. One claim about how `ractor` captures a child panic has a secondary source only.
- **Alternatives:** Structured shutdown (#476, record -001) owns shutdown propagation and has no restart primitive. OS-managed recovery (#477, record -002) owns whole-process restart and cannot restart one task. Neither owns the mechanism in this record.
- **Consequences:** No actor migration and no code removal. The three infrastructure-ownership spikes each end in rejection, so the current lifecycle, restart, and recovery code stays in application ownership. Full claim table: `.cheese/research/spike-478-ractor-supervisor/spike-478-ractor-supervisor.md`.
