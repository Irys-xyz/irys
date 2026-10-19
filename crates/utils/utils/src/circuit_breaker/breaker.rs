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
    /// When the latest half-open trial was granted. The open cooldown uses
    /// `last_failure_time_nanos`, and a refusal refreshes `last_access_time_nanos`,
    /// so neither can mark a trial. An exhausted window starts again once
    /// `cooldown_duration` has passed since this stamp.
    last_trial_granted_nanos: AtomicU64,
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
            last_trial_granted_nanos: AtomicU64::new(0),
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
            // Leave the trial count. A recorded failure must not open a new
            // window: `begin_probe` ignores the open cooldown, so a zeroed
            // count would grant another `recovery_attempts` at once.
            // `grant_half_open_trial` refills only after `cooldown_duration`
            // since the last grant. `record_success` still zeroes the count,
            // so Closed → Open by the failure threshold starts from zero.
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
    fn touch_access(&self) {
        self.last_access_time_nanos
            .store(get_current_time_nanos(), Ordering::Relaxed);
    }

    #[inline]
    fn try_enter_half_open(&self) -> bool {
        self.state
            .compare_exchange(
                CircuitState::Open as u8,
                CircuitState::HalfOpen as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[inline]
    fn trial_window_expired(&self) -> bool {
        let last_grant = self.last_trial_granted_nanos.load(Ordering::Acquire);
        has_timeout_elapsed(last_grant, self.config.cooldown_duration)
    }

    #[inline]
    fn stamp_trial_grant(&self) {
        self.last_trial_granted_nanos
            .store(get_current_time_nanos(), Ordering::Release);
    }

    /// Take one half-open trial. When the window is exhausted and the cooldown
    /// has passed since the last grant, start a new window instead of refusing.
    /// One unrecorded trial then blocks the peer for at most one cooldown.
    ///
    /// The reset is a single winner. The stamp is swapped from the expired
    /// value this caller observed to now; only that winner sets the count
    /// back to 1. A count-only swap can still succeed after other tasks have
    /// filled the new window, and would open another full set of trials.
    fn grant_half_open_trial(&self) -> bool {
        let max = self.config.recovery_attempts.get();
        loop {
            let current = self.half_open_trial_count.load(Ordering::Acquire);
            if current < max {
                match self.half_open_trial_count.compare_exchange(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        self.stamp_trial_grant();
                        return true;
                    }
                    Err(_) => continue,
                }
            }

            let observed_stamp = self.last_trial_granted_nanos.load(Ordering::Acquire);
            if !has_timeout_elapsed(observed_stamp, self.config.cooldown_duration) {
                return false;
            }
            let now = get_current_time_nanos();
            match self.last_trial_granted_nanos.compare_exchange(
                observed_stamp,
                now,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.half_open_trial_count.store(1, Ordering::Release);
                    return true;
                }
                Err(_) => continue,
            }
        }
    }

    #[must_use = "ignoring availability check defeats the purpose of circuit breaker"]
    pub(super) fn is_available(&self) -> bool {
        self.touch_access();

        match self.state() {
            CircuitState::Closed => true,
            CircuitState::Open => {
                self.should_attempt_recovery()
                    && self.try_enter_half_open()
                    && self.grant_half_open_trial()
            }
            CircuitState::HalfOpen => self.grant_half_open_trial(),
        }
    }

    /// Non-consuming availability check. Does not change state, grant a trial,
    /// or refresh last access.
    pub(super) fn would_allow(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::Open => self.should_attempt_recovery(),
            CircuitState::HalfOpen => {
                let trials = self.half_open_trial_count.load(Ordering::Acquire);
                trials < self.config.recovery_attempts.get() || self.trial_window_expired()
            }
        }
    }

    /// Grant one trial even when the open cooldown has not elapsed.
    ///
    /// Closed returns true and does not consume a trial. Open moves to
    /// half-open and consumes one trial. Grants stay bounded by
    /// `recovery_attempts` for the current window.
    pub(super) fn begin_probe(&self) -> bool {
        self.touch_access();
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => self.grant_half_open_trial(),
            CircuitState::Open => {
                if !self.try_enter_half_open() && self.state() != CircuitState::HalfOpen {
                    return self.state() == CircuitState::Closed;
                }
                self.grant_half_open_trial()
            }
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

    #[cfg(test)]
    fn trial_count(&self) -> u32 {
        self.half_open_trial_count.load(Ordering::Relaxed)
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

    /// Unrecorded trials must not stick the breaker. Once the cooldown has
    /// passed since the last grant, a new window starts.
    #[test]
    #[serial]
    fn exhausted_half_open_trials_refresh_after_cooldown() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);
        let config = test_config_with_recovery_attempts(3);
        let breaker = CircuitBreaker::new(&config);

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Open);

        advance_test_time(Duration::from_millis(150));

        assert!(breaker.is_available());
        assert!(breaker.is_available());
        assert!(breaker.is_available());
        assert!(!breaker.is_available(), "trials in one window stay bounded");

        advance_test_time(Duration::from_millis(150));

        assert!(
            breaker.is_available(),
            "an exhausted half-open window must grant a trial after the cooldown"
        );
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }

    #[test]
    #[serial]
    fn half_open_failure_reopens_and_success_closes() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);
        let config = TestConfigBuilder::default().build();
        let breaker = CircuitBreaker::new(&config);

        for _ in 0..3 {
            breaker.record_failure();
        }
        advance_test_time(Duration::from_millis(150));
        assert!(breaker.is_available());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(!breaker.is_available());

        advance_test_time(Duration::from_millis(150));
        assert!(breaker.is_available());
        breaker.record_success();
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.trial_count(), 0);
        assert!(breaker.is_available());
    }

    #[test]
    #[serial]
    fn would_allow_does_not_change_state_or_consume_trials() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);
        let config = test_config_with_recovery_attempts(2);
        let breaker = CircuitBreaker::new(&config);

        for _ in 0..3 {
            breaker.record_failure();
        }
        advance_test_time(Duration::from_millis(150));

        assert!(breaker.would_allow());
        assert_eq!(breaker.state(), CircuitState::Open);
        assert_eq!(breaker.trial_count(), 0);
        for _ in 0..20 {
            assert!(breaker.would_allow());
        }
        assert_eq!(breaker.state(), CircuitState::Open);
        assert_eq!(breaker.trial_count(), 0);

        assert!(breaker.is_available());
        assert!(breaker.is_available());
        assert!(!breaker.is_available());
    }

    #[test]
    #[serial]
    fn would_allow_does_not_refresh_last_access() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);
        let config = test_config_with_stale_timeout(Duration::from_millis(100));
        let breaker = CircuitBreaker::new(&config);

        advance_test_time(Duration::from_millis(60));
        assert!(breaker.would_allow());
        advance_test_time(Duration::from_millis(50));

        assert!(breaker.is_stale());
    }

    #[test]
    #[serial]
    fn begin_probe_ignores_open_cooldown_and_caps_trials() {
        use super::super::test_utils::TestTimeGuard;

        let _guard = TestTimeGuard::new(1_000_000_000);
        let config = test_config_with_recovery_attempts(3);
        let breaker = CircuitBreaker::new(&config);

        assert!(breaker.begin_probe());
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.trial_count(), 0);

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(!breaker.would_allow());

        assert!(breaker.begin_probe());
        assert!(breaker.begin_probe());
        assert!(breaker.begin_probe());
        assert!(!breaker.begin_probe());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        assert_eq!(breaker.trial_count(), 3);
    }

    /// A recorded half-open failure must not start a new trial window.
    /// `begin_probe` ignores the open cooldown, so clearing the count would
    /// hand out another `recovery_attempts` at once. The window refills only
    /// after `cooldown_duration` since the last grant.
    #[test]
    #[serial]
    fn recorded_half_open_failure_does_not_refill_the_trial_window() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};

        let _guard = TestTimeGuard::new(1_000_000_000);
        let recovery_attempts = 3_u32;
        let config = test_config_with_recovery_attempts(recovery_attempts);
        let breaker = CircuitBreaker::new(&config);

        // Closed → Open by the failure threshold starts from a fresh window.
        // `record_success` is what zeroes the count.
        assert_eq!(breaker.trial_count(), 0);
        for _ in 0..3 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Open);
        assert_eq!(breaker.trial_count(), 0);

        // The open cooldown still gates `is_available`.
        assert!(!breaker.is_available());
        advance_test_time(Duration::from_millis(150));
        assert!(breaker.is_available());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.record_success();
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.trial_count(), 0);

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Open);
        assert_eq!(breaker.trial_count(), 0);

        let mut grants = 0_u32;
        for _ in 0..(recovery_attempts + 2) {
            if breaker.begin_probe() {
                grants += 1;
                breaker.record_failure();
                assert_eq!(breaker.state(), CircuitState::Open);
            }
        }
        assert_eq!(
            grants, recovery_attempts,
            "recorded failures must not grant past recovery_attempts before the cooldown"
        );
        assert!(!breaker.begin_probe());

        advance_test_time(Duration::from_millis(150));
        assert!(
            breaker.begin_probe(),
            "after the cooldown one more grant is allowed"
        );

        breaker.record_success();
        assert_eq!(breaker.trial_count(), 0);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    /// One expired window, many threads. At most `recovery_attempts` grants.
    /// The reset must be a single winner: a caller that observed the old
    /// stamp must not open another window after the new one has filled.
    #[test]
    #[serial]
    fn half_open_window_reset_grants_at_most_recovery_attempts() {
        use super::super::test_utils::{TestTimeGuard, advance_test_time};
        use std::sync::Arc;
        use std::sync::atomic::AtomicU32;
        use std::thread;

        let _guard = TestTimeGuard::new(1_000_000_000);
        let recovery_attempts = 3_u32;
        let config = test_config_with_recovery_attempts(recovery_attempts);
        let breaker = Arc::new(CircuitBreaker::new(&config));

        for _ in 0..3 {
            breaker.record_failure();
        }
        advance_test_time(Duration::from_millis(150));
        assert!(breaker.begin_probe());
        assert!(breaker.begin_probe());
        assert!(breaker.begin_probe());
        assert!(
            !breaker.begin_probe(),
            "the first window is full and the cooldown has not elapsed"
        );

        advance_test_time(Duration::from_millis(150));

        let grants = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let breaker = Arc::clone(&breaker);
            let grants = Arc::clone(&grants);
            handles.push(thread::spawn(move || {
                for _ in 0..8 {
                    if breaker.begin_probe() {
                        grants.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().expect("grant thread");
        }

        let total = grants.load(Ordering::SeqCst);
        assert!(
            total >= 1,
            "an expired window must grant at least one trial"
        );
        assert!(
            total <= recovery_attempts,
            "one window grants at most recovery_attempts, got {total}"
        );
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
