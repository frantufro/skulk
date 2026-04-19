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
