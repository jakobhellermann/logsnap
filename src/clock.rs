//! A tiny clock abstraction so the `commit --wait-for` poll loop is testable without
//! real sleeping: the binary uses [`OsClock`] (wall clock + `thread::sleep`); tests
//! drive a virtual clock that advances on `sleep` and mutates the in-memory fs between
//! ticks.

use std::time::{Duration, Instant};

pub trait Clock {
    /// Time elapsed since this clock was created.
    fn elapsed(&self) -> Duration;
    /// Block for `dt` (advancing `elapsed` by `dt`).
    fn sleep(&self, dt: Duration);
}

/// Real time backing for the binary.
pub struct OsClock {
    start: Instant,
}

impl OsClock {
    pub fn new() -> Self {
        OsClock {
            start: Instant::now(),
        }
    }
}

impl Default for OsClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for OsClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&self, dt: Duration) {
        std::thread::sleep(dt);
    }
}
