//! Injected clock so all time-dependent logic (idle timers, status heuristics, debounces)
//! is deterministic under test — no ambient `Utc::now()` sprinkled through the code.
//! See `docs/DESIGN.md` §12.

use chrono::{DateTime, Duration, Utc};
use std::sync::Mutex;

/// A source of the current time. Inject this everywhere time is read.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Real wall-clock, for production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A manually-advanced clock, for deterministic tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Jump the clock forward by `delta`.
    pub fn advance(&self, delta: Duration) {
        let mut now = self.now.lock().expect("clock mutex poisoned");
        *now += delta;
    }

    /// Set the clock to an absolute instant.
    pub fn set(&self, instant: DateTime<Utc>) {
        *self.now.lock().expect("clock mutex poisoned") = instant;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("clock mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_and_sets() {
        let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);

        clock.advance(Duration::seconds(90));
        assert_eq!(clock.now(), start + Duration::seconds(90));

        let later = start + Duration::hours(2);
        clock.set(later);
        assert_eq!(clock.now(), later);
    }
}
