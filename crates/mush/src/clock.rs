//! The clock seam: what time it is, and how to wait.
//!
//! Two things read a clock: a wait loop's sleep, and a deadline compared
//! against "now". Both are tested by advancing a fake and asserting how far it
//! moved — a test that proves a ten-minute deadline by waiting ten minutes is a
//! test that costs the suite the time it is proving.
//!
//! Nothing here is async, and nothing here is a timer. `sleep` is blocking on
//! purpose: the agent actors are threads, and one of them sleeping is exactly
//! what a pause in a turn is.

use std::time::{Duration, Instant};

/// What a wait loop needs: the time, and a way to pause that it can be handed a
/// different answer to.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

/// The system clock: the only impl production code uses.
pub struct System;

impl Clock for System {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The system clock, for the two places that are not handed one: `http.rs`
/// reads a socket on whatever thread called it, and the request's own read
/// timeout is the socket's, not a policy.
pub fn system() -> &'static System {
    &SYSTEM
}

static SYSTEM: System = System;

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::Clock;

    /// A clock that only moves when it is told to.
    ///
    /// `sleep` returns immediately and advances the clock instead, so a wait
    /// loop that sleeps ten milliseconds at a time reaches a ten-minute
    /// deadline as fast as the CPU can run it. A test asserts *that* the clock
    /// took that long, rather than waiting to see.
    pub struct Advanceable {
        base: Instant,
        now: Mutex<Instant>,
    }

    impl Advanceable {
        pub fn new() -> Self {
            let base = Instant::now();
            Self {
                base,
                now: Mutex::new(base),
            }
        }

        /// Move the clock forward, as if `d` had passed.
        pub fn advance(&self, d: Duration) {
            *self.now.lock().unwrap() += d;
        }

        /// How far the clock has moved since it was made.
        pub fn elapsed(&self) -> Duration {
            self.now().saturating_duration_since(self.base)
        }
    }

    impl Default for Advanceable {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Clock for Advanceable {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, d: Duration) {
            self.advance(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::fake::Advanceable;
    use super::{Clock, System};

    /// Sleeping is how a wait loop spends its time; on the fake it costs
    /// nothing and the clock moves instead. That is the whole seam: every
    /// deadline below is reached by moving this number.
    #[test]
    fn a_fake_sleep_advances_the_clock_instead_of_waiting() {
        let clock = Advanceable::new();
        let started = clock.now();
        clock.sleep(Duration::from_secs(600));
        assert_eq!(
            clock.now().duration_since(started),
            Duration::from_secs(600),
            "the clock took the time the sleep asked for"
        );
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.elapsed(), Duration::from_millis(600_500));
    }

    /// The real clock is the real clock: a sleep of nothing still returns, and
    /// `now` moves forward.
    #[test]
    fn the_system_clock_is_the_wall_clock() {
        let clock = System;
        let before = clock.now();
        clock.sleep(Duration::from_millis(1));
        assert!(clock.now() > before);
    }
}
