use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::config::{CircuitBreakerConfig, FailureThreshold, RecoveryAttempts};

pub(crate) static TEST_TIME_OVERRIDE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_test_time(nanos: u64) {
    TEST_TIME_OVERRIDE.store(nanos, Ordering::Relaxed);
}

pub(crate) fn advance_test_time(duration: Duration) {
    let current = TEST_TIME_OVERRIDE.load(Ordering::Relaxed);
    let new_time = current.saturating_add(duration.as_nanos() as u64);
    TEST_TIME_OVERRIDE.store(new_time, Ordering::Relaxed);
}

pub(crate) fn reset_test_time() {
    TEST_TIME_OVERRIDE.store(0, Ordering::Relaxed);
}

#[must_use = "time conversion result must be used"]
pub(crate) fn instant_to_nanos(instant: Instant) -> u64 {
    let base = super::BASE_TIME.get_or_init(Instant::now);
    super::handle_instant_before_base(instant, *base)
}

#[must_use = "time conversion result must be used"]
pub(crate) fn nanos_to_instant(nanos: u64) -> Instant {
    let base = super::BASE_TIME.get_or_init(Instant::now);
    *base + Duration::from_nanos(nanos)
}

pub(crate) struct TestConfigBuilder {
    failure_threshold: u32,
    cooldown_duration: Duration,
    stale_timeout: Duration,
    recovery_attempts: u32,
}

impl Default for TestConfigBuilder {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown_duration: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(60),
            recovery_attempts: 3,
        }
    }
}

impl TestConfigBuilder {
    pub(crate) fn failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    pub(crate) fn cooldown_duration(mut self, duration: Duration) -> Self {
        self.cooldown_duration = duration;
        self
    }

    pub(crate) fn stale_timeout(mut self, timeout: Duration) -> Self {
        self.stale_timeout = timeout;
        self
    }

    pub(crate) fn recovery_attempts(mut self, attempts: u32) -> Self {
        self.recovery_attempts = attempts;
        self
    }

    pub(crate) fn build(self) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: FailureThreshold::new(
                NonZeroU32::new(self.failure_threshold).unwrap(),
            ),
            cooldown_duration: self.cooldown_duration,
            stale_timeout: self.stale_timeout,
            recovery_attempts: RecoveryAttempts::new(
                NonZeroU32::new(self.recovery_attempts).unwrap(),
            ),
        }
    }
}
