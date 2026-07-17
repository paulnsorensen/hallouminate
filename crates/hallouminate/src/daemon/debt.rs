//! Maintenance debt accounting (ADR daemon-rework-001): real backlog
//! signals (fragment count, stale version count) that graduate the
//! maintenance loop's writer-backpressure ladder. This seed stubs the
//! level query at `Ok`; dispatch B and later curds wire the real
//! fragment/version thresholds that would report `Soft`/`Hard`.

/// Real debt signals behind a `DebtLevel` classification. Not read yet --
/// dispatch B wires the threshold logic that reads these fields.
#[allow(dead_code)]
pub(crate) struct MaintenanceDebt {
    pub(crate) fragments: u64,
    pub(crate) stale_versions: u64,
}

/// Graduated maintenance debt level (ADR daemon-rework-001): `Hard` forces
/// a maintenance run past the Active/IoPressure defer gates; `Soft` paces
/// per-mutation cost. Stubbed to always report `Ok` until debt thresholds
/// land, so `Soft`/`Hard` are declared but not yet reachable.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DebtLevel {
    Ok,
    Soft,
    Hard,
}

/// Always `Ok` until dispatch B wires real fragment/version thresholds.
pub(super) fn level() -> DebtLevel {
    DebtLevel::Ok
}