//! Generic circuit breaker implementation for failure handling.
//!
//! Prevents cascading failures by detecting and temporarily blocking requests to failing resources.

use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::Ordering;

mod breaker;
mod config;
mod manager;
mod metrics;
mod state;
#[cfg(test)]
mod test_utils;

pub use config::{CircuitBreakerConfig, FailureThreshold, RecoveryAttempts};
pub use manager::CircuitBreakerManager;
pub use metrics::CircuitBreakerMetrics;
pub use state::CircuitState;

// Time utilities for circuit breaker operations
static BASE_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[cfg(test)]
#[inline]
fn get_current_time_nanos() -> u64 {
    let override_time = test_utils::TEST_TIME_OVERRIDE.load(Ordering::Relaxed);
    if override_time > 0 {
        override_time
    } else {
        let base = BASE_TIME.get_or_init(Instant::now);
        handle_instant_before_base(Instant::now(), *base)
    }
}

#[cfg(not(test))]
#[inline]
fn get_current_time_nanos() -> u64 {
    let base = BASE_TIME.get_or_init(Instant::now);
    handle_instant_before_base(Instant::now(), *base)
}

#[inline]
fn handle_instant_before_base(instant: Instant, base: Instant) -> u64 {
    match instant.checked_duration_since(base) {
        Some(duration) => duration.as_nanos() as u64,
        None => 0,
    }
}

#[inline]
#[must_use = "timeout check result determines circuit breaker behavior"]
fn has_timeout_elapsed(stored_nanos: u64, timeout: Duration) -> bool {
    if stored_nanos == 0 {
        return false;
    }
    let current_nanos = get_current_time_nanos();
    let elapsed_nanos = current_nanos.saturating_sub(stored_nanos);
    elapsed_nanos >= timeout.as_nanos() as u64
}

#[cfg(test)]
mod time_tests {
    use super::*;
    use crate::circuit_breaker::test_utils::{instant_to_nanos, reset_test_time};
    use proptest::prelude::*;

    proptest! {
        /// Invariant: Instant ↔ nanos conversion is lossless (< 1ns precision).
        /// Failures indicate BASE_TIME race conditions or arithmetic overflow.
        #[test]
        fn prop_instant_nanos_roundtrip_lossless(
            offset_nanos in 0_u64..1_000_000_000_000  // 0 to ~16 minutes
        ) {
            reset_test_time();
            let base = Instant::now();
            let instant = base + Duration::from_nanos(offset_nanos);

            let nanos = instant_to_nanos(instant);
            let recovered = crate::circuit_breaker::test_utils::nanos_to_instant(nanos);

            prop_assert!(recovered >= instant);
            prop_assert!(recovered.duration_since(instant) < Duration::from_nanos(1));
        }

        /// Invariant: Zero timestamp always returns false (safety fallback).
        /// Zero = uninitialized or instant < BASE_TIME race condition.
        #[test]
        fn prop_zero_nanos_never_times_out(timeout_ms in 1_u64..10000) {
            reset_test_time();
            prop_assert!(!has_timeout_elapsed(0, Duration::from_millis(timeout_ms)));
        }
    }

    #[test]
    fn test_base_time_initialization_is_consistent() {
        reset_test_time();
        let nanos1 = instant_to_nanos(Instant::now());
        std::thread::sleep(Duration::from_millis(10));
        let nanos2 = instant_to_nanos(Instant::now());

        assert!(nanos2 > nanos1);
        assert!(nanos2 - nanos1 >= 10_000_000);
    }

    #[test]
    fn test_instant_before_base_returns_zero() {
        let past_instant = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let future_base = Instant::now();

        let result = handle_instant_before_base(past_instant, future_base);

        assert_eq!(result, 0);
    }
}
