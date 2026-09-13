//! Shared helpers for the daemon crate's unit tests.

use std::time::Duration;

/// Polls `pred` every 10 ms until it returns `true`.
///
/// Panics with `what` when `timeout` elapses first. Sleeping between polls
/// parks the task instead of spinning on `yield_now`, so a slow catch-up pass
/// does not burn a worker while the test waits.
///
/// `pred` returns a future so both sync conditions (`|| async { x == y }`)
/// and async lookups (a store query) share one helper.
pub(crate) async fn wait_until<F, Fut>(timeout: Duration, what: &str, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while !pred().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
