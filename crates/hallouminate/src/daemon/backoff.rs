//! Shared exponential backoff curve for the daemon's supervision layer.
//! The task supervisor (restart-on-panic, `supervisor.rs`) and the crash-loop
//! watchdog (boot backoff, `watchdog.rs`) both escalate `floor` doubling per
//! step, saturating at `cap`. This is the single source for that curve.

/// `floor` doubling per step past the first, saturating at `cap`.
///
/// `steps == 0` ⇒ no backoff (`0`); `steps == 1` ⇒ `floor` (clamped to `cap`).
/// The exponent is clamped to 63 so the shift never overflows; the following
/// `saturating_mul` then `.min(cap)` pins any larger step count to `cap`.
pub(super) fn exponential_backoff_secs(floor: u64, cap: u64, steps: u32) -> u64 {
    if steps == 0 {
        return 0;
    }
    let exponent = (steps - 1).min(63);
    floor.saturating_mul(1u64 << exponent).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_steps_means_no_backoff() {
        assert_eq!(exponential_backoff_secs(10, 300, 0), 0);
    }

    #[test]
    fn curve_is_floor_doubling_capped() {
        let expected = [10, 20, 40, 80, 160, 300, 300];
        for (i, want) in expected.iter().enumerate() {
            let steps = u32::try_from(i).unwrap() + 1;
            assert_eq!(
                exponential_backoff_secs(10, 300, steps),
                *want,
                "step {steps}"
            );
        }
    }

    #[test]
    fn saturates_at_cap_for_huge_step_counts() {
        assert_eq!(exponential_backoff_secs(10, 300, u32::MAX), 300);
        assert_eq!(
            exponential_backoff_secs(u64::MAX, u64::MAX, u32::MAX),
            u64::MAX
        );
    }

    #[test]
    fn floor_above_cap_clamps_to_cap() {
        assert_eq!(exponential_backoff_secs(100, 60, 1), 60);
    }

    #[test]
    fn zero_floor_imposes_no_wait() {
        assert_eq!(exponential_backoff_secs(0, 300, 5), 0);
    }
}
