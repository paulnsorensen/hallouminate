//! Writer backpressure on maintenance debt (ADR daemon-rework-001). Today
//! `acquire` is a pass-through -- a later curd adds the Soft-delay/Hard-block
//! logic keyed on `debt::level()`.

use super::state::{DaemonState, MutationGuard};

/// Pass-through today: does exactly what `DaemonState::acquire_mutation_guard`
/// used to inline directly -- corpus lock, then write-lane permit, in that
/// documented order. A later curd adds the Soft-delay/Hard-block logic here,
/// keyed on `debt::level()`, without touching `state.rs` or any
/// `acquire_mutation_guard` call site in `dispatch.rs`/`watch.rs`.
pub(super) async fn acquire(
    state: &DaemonState,
    corpus: &str,
) -> Result<MutationGuard, &'static str> {
    let corpus_guard = state.lock_corpus(corpus).await;
    let permit = state
        .write_lane()
        .acquire_owned()
        .await
        .map_err(|_| "write lane closed")?;
    Ok(MutationGuard::new(permit, corpus_guard))
}
