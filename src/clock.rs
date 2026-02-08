use std::time::{Duration, Instant};

/// A clock abstraction for deterministic testing and wrapper layers.
///
/// The core limiter APIs accept `now: Duration` directly, so this trait is purely optional.
pub trait Clock {
    /// Returns a monotonic timestamp expressed as a [`Duration`].
    fn now(&self) -> Duration;
}

/// A clock backed by [`Instant::elapsed`].
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Constructs a new system clock.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A manual clock for deterministic tests.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Duration,
}

impl ManualClock {
    /// Constructs a manual clock starting at `now`.
    pub fn new(now: Duration) -> Self {
        Self { now }
    }

    /// Advances time by `delta`.
    pub fn advance(&mut self, delta: Duration) {
        self.now += delta;
    }

    /// Sets time to an absolute value.
    pub fn set(&mut self, now: Duration) {
        self.now = now;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.now
    }
}
