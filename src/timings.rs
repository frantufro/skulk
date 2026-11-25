//! Tunable timing parameters threaded through `run()`.
//!
//! Extracted from `main.rs` because this is a dependency-injection shim for
//! tests (which build `Timings::zero()` to skip real sleeps) and production
//! (which uses `Timings::production()`), not a CLI concern.

use std::time::Duration;

/// Tunable timing parameters threaded through `run()`.
///
/// Kept as a struct so adding a new timing doesn't touch every call site.
pub(crate) struct Timings {
    pub send_verify_delay: Duration,
    pub wait_poll_interval: Duration,
}

impl Timings {
    pub fn production() -> Self {
        Self {
            send_verify_delay: Duration::from_millis(500),
            wait_poll_interval: Duration::from_millis(500),
        }
    }

    #[cfg(test)]
    pub fn zero() -> Self {
        Self {
            send_verify_delay: Duration::ZERO,
            wait_poll_interval: Duration::ZERO,
        }
    }
}

/// Time sources, injectable so stream timing is testable.
pub(crate) trait Clock {
    /// Monotonic time, for deadlines and intervals.
    fn monotonic(&self) -> std::time::Instant;
    /// Wall-clock milliseconds since the Unix epoch, for timestamps only.
    fn wall_ms(&self) -> u64;
}

/// The system clocks.
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn wall_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}
