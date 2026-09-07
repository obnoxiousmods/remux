//! Per-addon circuit breaker for stream resolution.
//!
//! A stream addon that is *down* — its host deleted, its manifest 404ing — is
//! indistinguishable from a slow one at the call site: both simply consume the
//! whole per-addon timeout. Repeated across every track a client resolves, one
//! dead provider makes the entire music library feel broken, and it keeps doing
//! so until an operator notices.
//!
//! This tracks consecutive failures per addon and, past a threshold, skips the
//! provider outright for a growing window. When the window lapses exactly one
//! request is let through (half-open); its outcome either closes the breaker or
//! re-opens it for longer. Recovery therefore needs no restart and no operator.
//!
//! Deliberately *not* driven by wall-clock time internally: [`AddonHealth`] is a
//! pure state machine over an injected `Instant`, so the transitions are unit
//! tested without sleeping.

use std::time::{Duration, Instant};

/// Failures before the breaker opens. Two is too twitchy for a provider that
/// occasionally times out under load; three means a real outage.
const FAILURES_TO_OPEN: u32 = 3;
/// First open window. Long enough to stop wasting the timeout on every track in
/// an album, short enough that a brief outage self-heals within a listening
/// session.
const BASE_OPEN: Duration = Duration::from_secs(60);
/// Ceiling on the doubling, so a provider that stays down is still re-probed
/// regularly rather than being written off for the process lifetime.
const MAX_OPEN: Duration = Duration::from_secs(15 * 60);

/// What one resolution attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Returned usable streams.
    Ok,
    /// Answered correctly with nothing to offer.
    ///
    /// **Not a failure.** Local library addons answer `Empty` for the vast
    /// majority of tracks; counting that as ill health would trip the breaker
    /// on exactly the providers that work.
    Empty,
    /// Returned an error.
    Failed,
    /// Exceeded its budget.
    TimedOut,
}

/// Whether a caller may contact this addon right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Breaker closed — call normally.
    Allow,
    /// Breaker half-open — this one call decides whether it closes or re-opens.
    Probe,
    /// Breaker open — skip the addon entirely.
    Skip { retry_in: Duration },
}

#[derive(Debug, Clone, Default)]
pub struct AddonHealth {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

impl AddonHealth {
    pub fn gate(&self, now: Instant) -> Gate {
        match self.open_until {
            Some(until) if until > now => Gate::Skip {
                retry_in: until - now,
            },
            Some(_) => Gate::Probe,
            None => Gate::Allow,
        }
    }

    pub fn record(&mut self, outcome: Outcome, now: Instant) {
        match outcome {
            Outcome::Ok | Outcome::Empty => {
                self.consecutive_failures = 0;
                self.open_until = None;
            }
            Outcome::Failed | Outcome::TimedOut => {
                self.consecutive_failures = self
                    .consecutive_failures
                    .saturating_add(1);
                if self.consecutive_failures >= FAILURES_TO_OPEN {
                    self.open_until = Some(now + self.open_window());
                }
            }
        }
    }

    /// Doubles per failure past the threshold, saturating at [`MAX_OPEN`].
    fn open_window(&self) -> Duration {
        let steps = self
            .consecutive_failures
            .saturating_sub(FAILURES_TO_OPEN);
        BASE_OPEN
            .checked_mul(
                1u32.checked_shl(steps)
                    .unwrap_or(u32::MAX),
            )
            .unwrap_or(MAX_OPEN)
            .min(MAX_OPEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_healthy_addon_is_always_allowed() {
        let mut health = AddonHealth::default();
        let now = t0();
        assert_eq!(health.gate(now), Gate::Allow);
        health.record(Outcome::Ok, now);
        assert_eq!(health.gate(now), Gate::Allow);
    }

    #[test]
    fn three_consecutive_failures_open_the_breaker() {
        let mut health = AddonHealth::default();
        let now = t0();
        health.record(Outcome::Failed, now);
        health.record(Outcome::TimedOut, now);
        assert_eq!(
            health.gate(now),
            Gate::Allow,
            "two failures is not an outage"
        );
        health.record(Outcome::TimedOut, now);
        assert!(matches!(health.gate(now), Gate::Skip { .. }));
    }

    #[test]
    fn an_open_breaker_reports_how_long_is_left() {
        let mut health = AddonHealth::default();
        let now = t0();
        for _ in 0..FAILURES_TO_OPEN {
            health.record(Outcome::TimedOut, now);
        }
        let Gate::Skip { retry_in } = health.gate(now) else {
            panic!("expected the breaker to be open");
        };
        assert_eq!(retry_in, BASE_OPEN);
    }

    #[test]
    fn the_window_lapsing_yields_a_single_probe_that_can_close_it() {
        let mut health = AddonHealth::default();
        let now = t0();
        for _ in 0..FAILURES_TO_OPEN {
            health.record(Outcome::Failed, now);
        }
        let later = now + BASE_OPEN + Duration::from_secs(1);
        assert_eq!(health.gate(later), Gate::Probe);

        health.record(Outcome::Ok, later);
        assert_eq!(health.gate(later), Gate::Allow);
        // The counter resets too, so the next outage needs a full three
        // failures again rather than tripping on the first.
        health.record(Outcome::Failed, later);
        assert_eq!(health.gate(later), Gate::Allow);
    }

    #[test]
    fn a_failed_probe_reopens_for_longer_and_saturates() {
        let mut health = AddonHealth::default();
        let now = t0();
        for _ in 0..FAILURES_TO_OPEN {
            health.record(Outcome::Failed, now);
        }
        assert_eq!(health.open_window(), BASE_OPEN);

        health.record(Outcome::Failed, now);
        assert_eq!(health.open_window(), BASE_OPEN * 2);
        health.record(Outcome::Failed, now);
        assert_eq!(health.open_window(), BASE_OPEN * 4);

        for _ in 0..64 {
            health.record(Outcome::Failed, now);
        }
        assert_eq!(
            health.open_window(),
            MAX_OPEN,
            "a long outage must still be re-probed, and must not overflow"
        );
    }

    #[test]
    fn empty_results_never_open_the_breaker() {
        // The local opendal addons answer Empty for nearly every streaming
        // track. Treating that as a failure would disable the providers that
        // actually work.
        let mut health = AddonHealth::default();
        let now = t0();
        for _ in 0..50 {
            health.record(Outcome::Empty, now);
        }
        assert_eq!(health.gate(now), Gate::Allow);
    }

    #[test]
    fn an_empty_answer_clears_an_accumulating_failure_streak() {
        let mut health = AddonHealth::default();
        let now = t0();
        health.record(Outcome::Failed, now);
        health.record(Outcome::Failed, now);
        health.record(Outcome::Empty, now);
        health.record(Outcome::Failed, now);
        health.record(Outcome::Failed, now);
        assert_eq!(health.gate(now), Gate::Allow);
    }
}
