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

/// One tracing event recorded by [`EventCapture`].
#[derive(Debug, Clone)]
pub(crate) struct CapturedEvent {
    pub(crate) level: tracing::Level,
    pub(crate) target: String,
    pub(crate) fields: std::collections::BTreeMap<String, String>,
}

/// Records every tracing event that this thread emits while the guard lives.
#[derive(Clone, Default)]
pub(crate) struct EventCapture(std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>);

impl EventCapture {
    /// Installs the capture as this thread's default subscriber.
    pub(crate) fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt;

        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        });
        let capture = Self::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        (capture, tracing::subscriber::set_default(subscriber))
    }

    /// Returns the recorded events whose target equals `target`.
    pub(crate) fn events_for(&self, target: &str) -> Vec<CapturedEvent> {
        let mut events = Vec::new();
        for event in self.0.lock().expect("capture lock").iter() {
            if event.target == target {
                events.push(event.clone());
            }
        }
        events
    }
}

struct FieldVisitor<'a>(&'a mut std::collections::BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = std::collections::BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        let metadata = event.metadata();
        self.0.lock().expect("capture lock").push(CapturedEvent {
            level: *metadata.level(),
            target: metadata.target().to_string(),
            fields,
        });
    }
}
