# Bounded lazy reranker admission

## Decision

Each canonical crossencoder model has one independent lazy slot.
The async state lookup validates the model name and returns a handle without native construction.
The existing Ground blocking task acquires the slot with `try_lock`.
That task constructs the model on first use and then runs native reranking.
The rerank deadline therefore includes native construction and slot admission.[^1]

A busy slot returns immediately instead of adding a waiting blocking task.
Ground keeps the original fusion order after busy, initialization, or timeout failures.
A timed-out native call can continue; Rust task cancellation does not stop native execution.
Its slot remains occupied until the call returns.
Requests for different models use different slots.[^2]

## Rationale

A whole-map guard couples independent models and lets later requests wait outside the deadline.
Eager startup construction also blocks an async runtime worker.
One lazy construction path avoids both problems without a second model cache.
This decision does not establish native memory reclamation or a whole-daemon resource ceiling.

## Wiki destination

This decision updates `blocking-inference-offload.md` and the reranker claim in `ort-arena-retention.md`.
The wiki mutation gate returns a Hard-debt timeout during this repair.
This tracked ADR preserves the decision until wiki writeback succeeds.

[^1]: crates/hallouminate-domain/src/ground/orchestrate.rs::rerank_with_timeout; crates/hallouminate-daemon/src/state.rs::CrossencoderGuard
[^2]: crates/hallouminate-daemon/src/state.rs::DaemonState::crossencoder; crates/hallouminate-domain/src/ground/orchestrate.rs::rerank_with_timeout
