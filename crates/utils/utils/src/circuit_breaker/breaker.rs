use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::config::CircuitBreakerConfig;
use super::state::CircuitState;
use super::{get_current_time_nanos, has_timeout_elapsed};

#[derive(Debug)]
pub(super) struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    last_failure_time_nanos: AtomicU64,
    last_access_time_nanos: AtomicU64,
    half_open_trial_count: AtomicU32,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub(super) fn new(config: &CircuitBreakerConfig) -> Self {
        let now_nanos = get_current_time_nanos();
        Self {
            state: AtomicU8::new(CircuitState::Closed as u8),
            failure_count: AtomicU32::new(0),
            last_failure_time_nanos: AtomicU64::new(0),
            last_access_time_nanos: AtomicU64::new(now_nanos),
            half_open_trial_count: AtomicU32::new(0),
            config: *config,
        }
    }

    #[inline]
    pub(super) fn record_success(&self) {
        let now_nanos = get_current_time_nanos();
        self.failure_count.store(0, Ordering::Relaxed);
        self.half_open_trial_count.store(0, Ordering::Relaxed);
        self.last_failure_time_nanos.store(0, Ordering::Relaxed);
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);
        self.state
            .store(CircuitState::Closed as u8, Ordering::Release);
    }

    #[inline]
    pub(super) fn record_failure(&self) {
        let now_nanos = get_current_time_nanos();
        let current_state = CircuitState::from_u8_failsafe(self.state.load(Ordering::Acquire));

        if current_state == CircuitState::HalfOpen {
            self.half_open_trial_count.store(0, Ordering::Relaxed);
            self.last_failure_time_nanos
                .store(now_nanos, Ordering::Relaxed);
            self.last_access_time_nanos
                .store(now_nanos, Ordering::Relaxed);
            self.state
                .store(CircuitState::Open as u8, Ordering::Release);
            return;
        }

        let new_count = self
            .failure_count
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.last_failure_time_nanos
            .store(now_nanos, Ordering::Relaxed);
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);

        if new_count >= self.config.failure_threshold.get() {
            self.state
                .store(CircuitState::Open as u8, Ordering::Release);
        }
    }

    #[inline]
    fn should_attempt_recovery(&self) -> bool {
        let last_failure_nanos = self.last_failure_time_nanos.load(Ordering::Relaxed);
        has_timeout_elapsed(last_failure_nanos, self.config.cooldown_duration)
    }

    #[inline]
    fn try_transition_to_half_open(&self) -> bool {
        let transition_succeeded = self
            .state
            .compare_exchange(
                CircuitState::Open as u8,
                CircuitState::HalfOpen as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();

        if transition_succeeded {
            let _ = self.increment_trial_count();
        }

        transition_succeeded
    }

    #[inline]
    fn try_consume_recovery_attempt(&self) -> bool {
        self.increment_trial_count().is_ok()
    }

    #[inline]
    fn increment_trial_count(&self) -> Result<u32, u32> {
        self.half_open_trial_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |x| {
                (x < self.config.recovery_attempts.get()).then_some(x + 1)
            })
    }

    #[must_use = "ignoring availability check defeats the purpose of circuit breaker"]
    pub(super) fn is_available(&self) -> bool {
        let now_nanos = get_current_time_nanos();
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);

        let current_state = CircuitState::from_u8_failsafe(self.state.load(Ordering::Acquire));

        match current_state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                self.should_attempt_recovery() && self.try_transition_to_half_open()
            }
            CircuitState::HalfOpen => self.try_consume_recovery_attempt(),
        }
    }

    #[inline]
    pub(super) fn is_stale(&self) -> bool {
        let last_access_nanos = self.last_access_time_nanos.load(Ordering::Relaxed);
        has_timeout_elapsed(last_access_nanos, self.config.stale_timeout)
    }

    #[inline]
    pub(super) fn state(&self) -> CircuitState {
        CircuitState::from_u8_failsafe(self.state.load(Ordering::Acquire))
    }

    #[cfg(test)]
    pub(super) fn failure_count(&self) -> u32 {
        self.failure_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_utils::TestConfigBuilder;
    use super::*;
    use serial_test::serial;
    use std::time::Duration;

    fn test_config_with_recovery_attempts(attempts: u32) -> CircuitBreakerConfig {
        TestConfigBuilder::default()
            .recovery_attempts(attempts)
            .build()
    }

    fn test_config_with_stale_timeout(timeout: Duration) -> CircuitBreakerConfig {
        TestConfigBuilder::default().stale_timeout(timeout).build()
    }

    #[test]
    #[serial]
    fn test_half_open_transition_after_cooldown() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);

        let config = TestConfigBuilder::default().build();
        let breaker = CircuitBreaker::new(&config);

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Open);

        advance_test_time(Duration::from_millis(150));

        assert!(breaker.is_available());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }

    #[test]
    #[serial]
    fn test_half_open_limits_recovery_attempts() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);

        let config = test_config_with_recovery_attempts(2);
        let breaker = CircuitBreaker::new(&config);

        for _ in 0..3 {
            breaker.record_failure();
        }

        advance_test_time(Duration::from_millis(150));

        assert!(breaker.is_available());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        assert!(breaker.is_available());

        assert!(!breaker.is_available());
    }

    #[test]
    #[serial]
    fn test_is_stale_after_timeout() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);

        let config = test_config_with_stale_timeout(Duration::from_millis(100));
        let breaker = CircuitBreaker::new(&config);

        assert!(!breaker.is_stale());

        advance_test_time(Duration::from_millis(150));

        assert!(breaker.is_stale());
    }

    #[test]
    #[serial]
    fn test_is_available_updates_access_time() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);

        let config = test_config_with_stale_timeout(Duration::from_millis(100));
        let breaker = CircuitBreaker::new(&config);

        advance_test_time(Duration::from_millis(60));

        let _ = breaker.is_available();

        assert!(!breaker.is_stale());
    }

    mod proptests {
        use super::super::super::test_utils::TestConfigBuilder;
        use super::*;
        use proptest::prelude::*;
        use serial_test::serial;

        fn proptest_config(failure_threshold: u32) -> CircuitBreakerConfig {
            TestConfigBuilder::default()
                .failure_threshold(failure_threshold)
                .cooldown_duration(Duration::from_millis(50))
                .stale_timeout(Duration::from_secs(10))
                .build()
        }

        fn proptest_config_with_recovery(
            failure_threshold: u32,
            recovery_attempts: u32,
        ) -> CircuitBreakerConfig {
            TestConfigBuilder::default()
                .failure_threshold(failure_threshold)
                .cooldown_duration(Duration::from_millis(50))
                .stale_timeout(Duration::from_secs(10))
                .recovery_attempts(recovery_attempts)
                .build()
        }

        proptest! {
            #[test]
            fn prop_success_always_resets_to_closed(
                failure_threshold in 1_u32..20,
                num_failures in 0_u32..50,
            ) {
                let config = proptest_config(failure_threshold);
                let breaker = CircuitBreaker::new(&config);

                for _ in 0..num_failures {
                    breaker.record_failure();
                }

                breaker.record_success();
                prop_assert_eq!(breaker.state(), CircuitState::Closed);
                prop_assert_eq!(breaker.failure_count(), 0);
            }

            #[test]
            fn prop_never_exceed_threshold_without_opening(
                failure_threshold in 2_u32..20,
                num_failures in 0_u32..50,
            ) {
                let config = proptest_config(failure_threshold);
                let breaker = CircuitBreaker::new(&config);

                for _ in 0..num_failures {
                    breaker.record_failure();
                    let state = breaker.state();
                    let count = breaker.failure_count();

                    // Invariant: If closed, count < threshold
                    if state == CircuitState::Closed {
                        prop_assert!(count < failure_threshold);
                    }
                }
            }

            #[test]
            #[serial]
            fn prop_half_open_single_failure_reopens(
                failure_threshold in 1_u32..20,
            ) {
                use super::super::super::test_utils::{advance_test_time, TestTimeGuard};

                let _guard = TestTimeGuard::new(1_000_000_000);

                let config = proptest_config(failure_threshold);
                let breaker = CircuitBreaker::new(&config);

                for _ in 0..failure_threshold {
                    breaker.record_failure();
                }
                prop_assert_eq!(breaker.state(), CircuitState::Open);

                advance_test_time(Duration::from_millis(60));

                let _ = breaker.is_available();
                prop_assert_eq!(breaker.state(), CircuitState::HalfOpen);

                breaker.record_failure();
                prop_assert_eq!(breaker.state(), CircuitState::Open);
            }

            #[test]
            #[serial]
            fn prop_recovery_attempts_bounded(
                failure_threshold in 1_u32..20,
                recovery_attempts in 1_u32..10,
                extra_attempts in 0_u32..20,
            ) {
                use super::super::super::test_utils::{advance_test_time, TestTimeGuard};

                let _guard = TestTimeGuard::new(1_000_000_000);

                let config = proptest_config_with_recovery(failure_threshold, recovery_attempts);
                let breaker = CircuitBreaker::new(&config);

                for _ in 0..failure_threshold {
                    breaker.record_failure();
                }

                advance_test_time(Duration::from_millis(60));
                let _ = breaker.is_available();
                prop_assert_eq!(breaker.state(), CircuitState::HalfOpen);

                let mut successful_attempts = 0;
                for _ in 0..(recovery_attempts + extra_attempts) {
                    if breaker.is_available() {
                        successful_attempts += 1;
                    }
                }
                prop_assert!(successful_attempts <= recovery_attempts);
            }

            #[test]
            fn prop_state_transitions_are_valid(
                operations in prop::collection::vec(
                    prop::bool::ANY,
                    0..100
                )
            ) {
                use super::super::super::config::{BreakerCapacity, FailureThreshold, RecoveryAttempts};
                use std::num::{NonZeroU32, NonZeroUsize};

                let config = CircuitBreakerConfig {
                    capacity: BreakerCapacity::new(NonZeroUsize::new(100).unwrap()),
                    failure_threshold: FailureThreshold::new(NonZeroU32::new(3).unwrap()),
                    cooldown_duration: Duration::from_millis(10),
                    stale_timeout: Duration::from_secs(10),
                    recovery_attempts: RecoveryAttempts::new(NonZeroU32::new(2).unwrap()),
                };
                let breaker = CircuitBreaker::new(&config);

                for is_success in operations {
                    let before_state = breaker.state();

                    if is_success {
                        breaker.record_success();
                        // Success always leads to Closed
                        prop_assert_eq!(breaker.state(), CircuitState::Closed);
                    } else {
                        breaker.record_failure();
                        let after_state = breaker.state();

                        // Valid transitions: Closed→Open, HalfOpen→Open, or stay same
                        match (before_state, after_state) {
                            (CircuitState::Closed, CircuitState::Closed) => {},
                            (CircuitState::Closed, CircuitState::Open) => {},
                            (CircuitState::Open, CircuitState::Open) => {},
                            (CircuitState::HalfOpen, CircuitState::Open) => {},
                            _ => prop_assert!(false, "Invalid state transition"),
                        }
                    }
                }
            }
        }
    }
}
