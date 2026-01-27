use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::config::{BreakerCapacity, CircuitBreakerConfig, FailureThreshold, RecoveryAttempts};

pub(crate) static TEST_TIME_OVERRIDE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_test_time(nanos: u64) {
    TEST_TIME_OVERRIDE.store(nanos, Ordering::SeqCst);
}

pub(crate) fn advance_test_time(duration: Duration) {
    let current = TEST_TIME_OVERRIDE.load(Ordering::SeqCst);
    let new_time = current.saturating_add(duration.as_nanos() as u64);
    TEST_TIME_OVERRIDE.store(new_time, Ordering::SeqCst);
}

pub(crate) fn reset_test_time() {
    TEST_TIME_OVERRIDE.store(0, Ordering::SeqCst);
}

/// RAII guard that sets test time on creation and resets on drop.
/// Prevents test pollution when a test panics before manual reset.
#[must_use = "guard resets test time on drop; bind it to a variable"]
pub(crate) struct TestTimeGuard;

impl TestTimeGuard {
    pub(crate) fn new(initial_nanos: u64) -> Self {
        reset_test_time();
        set_test_time(initial_nanos);
        Self
    }
}

impl Drop for TestTimeGuard {
    fn drop(&mut self) {
        reset_test_time();
    }
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
    capacity: usize,
    failure_threshold: u32,
    cooldown_duration: Duration,
    stale_timeout: Duration,
    recovery_attempts: u32,
}

impl Default for TestConfigBuilder {
    fn default() -> Self {
        Self {
            capacity: 100,
            failure_threshold: 3,
            cooldown_duration: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(60),
            recovery_attempts: 3,
        }
    }
}

impl TestConfigBuilder {
    pub(crate) fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

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
            capacity: BreakerCapacity::new(
                NonZeroUsize::new(self.capacity).expect("capacity must be > 0"),
            ),
            failure_threshold: FailureThreshold::new(
                NonZeroU32::new(self.failure_threshold).expect("failure_threshold must be > 0"),
            ),
            cooldown_duration: self.cooldown_duration,
            stale_timeout: self.stale_timeout,
            recovery_attempts: RecoveryAttempts::new(
                NonZeroU32::new(self.recovery_attempts).expect("recovery_attempts must be > 0"),
            ),
        }
    }
}
