//! Internal churn accounting. Stub -- a later curd (10) owns the real
//! per-corpus reindex-churn logic; this seed only reserves the call shape.

/// No-op reindex-churn recorder. Curd 10 replaces this with real
/// accounting.
#[allow(dead_code)]
pub(crate) fn record_reindex() {}
