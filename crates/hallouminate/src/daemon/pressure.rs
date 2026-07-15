//! Host I/O pressure signal for maintenance deferral (ADR-003).
//!
//! On Linux, [`PsiProbe`] parses `/proc/pressure/io`'s "some avg10" field and
//! reports elevated pressure above a fixed threshold. Everywhere else (and on
//! any parse failure) pressure is treated as not elevated — fail-open, since
//! pressure can never block maintenance where it cannot be measured.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Threshold (percent) above which PSI `some avg10` counts as elevated I/O
/// pressure. Starting value, uncalibrated against production data.
const PSI_ELEVATED_THRESHOLD: f64 = 10.0;

/// Kernel PSI file the Linux probe reads.
const PSI_PATH: &str = "/proc/pressure/io";

/// Guards the fail-open debug log so an absent or unparseable PSI file is
/// reported once, not on every maintenance cycle.
static PSI_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

/// Host I/O pressure signal, injected into the maintenance loop so tests can
/// swap in a double instead of reading the real `/proc/pressure/io`.
pub(crate) trait IoPressureProbe: Send + Sync {
    /// True when host I/O pressure is elevated enough that a maintenance
    /// pass should defer. Fail-open: no signal available means `false`.
    fn elevated(&self) -> bool;
}

/// Linux PSI probe: parses `/proc/pressure/io`'s "some avg10" field.
pub(crate) struct PsiProbe;

impl IoPressureProbe for PsiProbe {
    #[cfg(target_os = "linux")]
    fn elevated(&self) -> bool {
        psi_elevated_at(Path::new(PSI_PATH))
    }

    #[cfg(not(target_os = "linux"))]
    fn elevated(&self) -> bool {
        false
    }
}

/// Reads `path` as a PSI file and reports whether `some avg10` exceeds the
/// threshold. Fail-open: a missing or unparseable file is not elevated, and the
/// condition is logged once at debug. Path-parameterized so tests can point at
/// a synthetic or absent file.
fn psi_elevated_at(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(contents) => match parse_some_avg10(&contents) {
            Some(avg10) => avg10 > PSI_ELEVATED_THRESHOLD,
            None => {
                log_fail_open("PSI io pressure line unparseable; treating as not elevated");
                false
            }
        },
        Err(_) => {
            log_fail_open("/proc/pressure/io unavailable; treating pressure as not elevated");
            false
        }
    }
}

/// Emits the fail-open condition at debug exactly once per process, so a
/// persistently absent/unparseable PSI file does not log every cycle.
fn log_fail_open(message: &'static str) {
    if !PSI_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
        tracing::debug!(target: "hallouminate::daemon", "{message}");
    }
}

/// Parses the "some avg10=<value>" field out of `/proc/pressure/io`'s
/// contents, e.g. `some avg10=12.34 avg60=8.01 avg300=3.20 total=123456`.
fn parse_some_avg10(contents: &str) -> Option<f64> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
}

/// Portable fallback when PSI is unavailable (non-Linux): never elevated.
/// Deferral there is activity-only.
#[cfg(not(target_os = "linux"))]
pub(crate) struct NoPressureSignal;

#[cfg(not(target_os = "linux"))]
impl IoPressureProbe for NoPressureSignal {
    fn elevated(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_some_avg10_extracts_the_value() {
        let contents = "some avg10=12.34 avg60=8.01 avg300=3.20 total=123456\nfull avg10=1.00 avg60=0.50 avg300=0.10 total=999\n";
        assert_eq!(parse_some_avg10(contents), Some(12.34));
    }

    #[test]
    fn parse_some_avg10_returns_none_when_line_missing() {
        assert_eq!(parse_some_avg10("full avg10=1.00\n"), None);
    }

    #[test]
    fn parse_some_avg10_returns_none_on_garbage() {
        assert_eq!(parse_some_avg10("some avg10=not-a-number\n"), None);
    }

    #[test]
    fn psi_elevated_at_fails_open_when_path_absent() {
        // The required fail-open-on-absent acceptance: a missing PSI file must
        // never block maintenance.
        assert!(!psi_elevated_at(Path::new(
            "/nonexistent/hallouminate/proc/pressure/io"
        )));
    }

    #[test]
    fn psi_elevated_at_fails_open_on_unparseable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("io");
        std::fs::write(&path, "garbage without a some line\n").expect("write");
        assert!(!psi_elevated_at(&path));
    }

    #[test]
    fn psi_elevated_at_reports_elevated_above_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("io");
        std::fs::write(
            &path,
            "some avg10=42.00 avg60=1.00 avg300=1.00 total=1\nfull avg10=1.00\n",
        )
        .expect("write");
        assert!(psi_elevated_at(&path));
    }

    #[test]
    fn psi_elevated_at_not_elevated_at_or_below_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("io");
        std::fs::write(&path, "some avg10=1.23 avg60=1.00 avg300=1.00 total=1\n").expect("write");
        assert!(!psi_elevated_at(&path));
    }

    #[test]
    fn psi_elevated_at_not_elevated_exactly_at_threshold() {
        // 10.0 is not > 10.0 -- the boundary value must not count as elevated.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("io");
        std::fs::write(&path, "some avg10=10.00 avg60=1.00 avg300=1.00 total=1\n").expect("write");
        assert!(!psi_elevated_at(&path));
    }

    #[test]
    fn psi_elevated_at_elevated_just_above_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("io");
        std::fs::write(&path, "some avg10=10.01 avg60=1.00 avg300=1.00 total=1\n").expect("write");
        assert!(psi_elevated_at(&path));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn no_pressure_signal_is_never_elevated() {
        assert!(!NoPressureSignal.elevated());
    }
}
