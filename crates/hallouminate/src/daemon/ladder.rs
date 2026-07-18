//! Backpressure ladder (ADR daemon-rework seed 4): pure evaluation of what
//! a rising count (e.g. consecutive maintenance defers) should trigger --
//! nothing, a warn, or an escalation action. Wired into `watch.rs`'s churn
//! ladder (`ForceMaintenance` on reindex churn) and `state.rs`'s supervisor
//! seed (`WatchdogTrip` on restart-intensity escalation).

use super::heartbeat::TaskName;

/// Escalating response a ladder can fire once its `act_at` threshold is
/// crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LadderAction {
    ForceMaintenance,
    #[allow(dead_code)]
    // not constructed in production yet; only WatchdogTrip/ForceMaintenance are seeded today
    RestartTask(TaskName),
    WatchdogTrip,
}

/// What a ladder evaluation determined for a given count: nothing, a warn,
/// or the ladder's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LadderOutcome {
    Nothing,
    Warn,
    Action(LadderAction),
}

/// A two-threshold ladder: below `warn_at` fires nothing, at/above `warn_at`
/// (but below `act_at`) fires a warn, at/above `act_at` fires `action`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ladder {
    pub(crate) warn_at: u32,
    pub(crate) act_at: u32,
    pub(crate) action: LadderAction,
}

impl Ladder {
    pub(crate) fn evaluate(&self, count: u32) -> LadderOutcome {
        if count >= self.act_at {
            LadderOutcome::Action(self.action)
        } else if count >= self.warn_at {
            LadderOutcome::Warn
        } else {
            LadderOutcome::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> Ladder {
        Ladder {
            warn_at: 5,
            act_at: 10,
            action: LadderAction::ForceMaintenance,
        }
    }

    #[test]
    fn evaluate_reports_nothing_below_warn_at() {
        assert_eq!(ladder().evaluate(4), LadderOutcome::Nothing);
    }

    #[test]
    fn evaluate_reports_warn_between_warn_at_and_act_at() {
        assert_eq!(ladder().evaluate(5), LadderOutcome::Warn);
        assert_eq!(ladder().evaluate(9), LadderOutcome::Warn);
    }

    #[test]
    fn evaluate_reports_action_at_and_above_act_at() {
        assert_eq!(
            ladder().evaluate(10),
            LadderOutcome::Action(LadderAction::ForceMaintenance)
        );
        assert_eq!(
            ladder().evaluate(100),
            LadderOutcome::Action(LadderAction::ForceMaintenance)
        );
    }

    #[test]
    fn restart_task_action_carries_the_task_name() {
        let l = Ladder {
            warn_at: 1,
            act_at: 2,
            action: LadderAction::RestartTask(TaskName::WatcherPump),
        };
        assert_eq!(
            l.evaluate(2),
            LadderOutcome::Action(LadderAction::RestartTask(TaskName::WatcherPump))
        );
    }
}
