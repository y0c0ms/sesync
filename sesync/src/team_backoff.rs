//! The poll interval for the daemon's team side. The local pass runs every SESYNC_POLL. The team
//! pull and push run on this interval instead: it starts at SESYNC_POLL, doubles after each team
//! step that found nothing new, is capped at SESYNC_TEAM_POLL_MAX, and goes back to the base as
//! soon as anything changes. Pure: no clock and no I/O, so the daemon's loop is the only place that
//! waits.

use std::time::Duration;

/// The team interval and the rule that moves it. `base` is SESYNC_POLL; `max` is never below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamBackoff {
    base: Duration,
    max: Duration,
    current: Duration,
}

impl TeamBackoff {
    /// Starts at `base`. A `max` below `base` is raised to `base`, so the interval never drops
    /// below the base.
    pub fn new(base: Duration, max: Duration) -> TeamBackoff {
        TeamBackoff { base, max: max.max(base), current: base }
    }

    /// How long to wait before the next team step.
    pub fn interval(&self) -> Duration {
        self.current
    }

    /// The interval after one team step. `activity` is true when a pull accepted a new record or a
    /// push succeeded: the interval returns to the base. Otherwise it doubles, up to the cap.
    pub fn next(self, activity: bool) -> TeamBackoff {
        let current = if activity {
            self.base
        } else {
            self.current
                .checked_mul(2)
                .unwrap_or(self.max)
                .min(self.max)
                .max(self.base)
        };
        TeamBackoff { current, ..self }
    }
}

/// Whether a team pull was idle: it accepted no new record, and the number of conversations it
/// showed or skipped as unchanged is the same as on the previous pass. `visible_before` is `None`
/// before the first pass, which is never idle.
pub fn pull_is_idle(accepted: usize, visible_now: usize, visible_before: Option<usize>) -> bool {
    accepted == 0 && visible_before == Some(visible_now)
}
