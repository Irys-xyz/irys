use core::time::Duration;
use irys_types::Address;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::Instant;
use tokio::sync::RwLock;

// Threshold of 5 balances responsiveness with stability. Lower values (e.g., 3) would
// trip often on transient network issues. Higher values delay problem detection
const FAILURES_TO_TRIP: u32 = 5;

/// Long enough for TCP reconnects and DNS updates, short enough to maintain service responsiveness
const COOLDOWN_DURATION: Duration = Duration::from_secs(30);

// 10 minutes allows for temporary network partitions while preventing
// unbounded cache growth from departed peers
const STALE_BREAKER_TIMEOUT: Duration = Duration::from_secs(600);

// Multiple trials in half-open distinguish between lingering issues and true recovery.
// Single trial could fail due to timing; 3 trials provide good confidence
const RECOVERY_ATTEMPTS: u32 = 3;

// Atomic increment with ceiling, returns true if incremented, false if already at max
fn atomic_increment_bounded(counter: &AtomicU32, max: u32) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
            if x < max {
                Some(x + 1)
            } else {
                None
            }
        })
        .is_ok()
}

const MAX_CIRCUIT_BREAKERS: usize = 1_000;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CircuitState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl CircuitState {
    fn try_from_u8(value: u8) -> Result<Self, u8> {
        match value {
            0 => Ok(Self::Closed),
            1 => Ok(Self::Open),
            2 => Ok(Self::HalfOpen),
            invalid => Err(invalid),
        }
    }
}

impl From<u8> for CircuitState {
    fn from(value: u8) -> Self {
        match Self::try_from_u8(value) {
            Ok(state) => state,
            Err(invalid) => {
                debug_assert!(false, "Invalid CircuitState byte value: {}. This indicates memory corruption or a bug.", invalid);
                tracing::error!(
                    "Invalid CircuitState byte value: {}. Defaulting to Open for safety.",
                    invalid
                );
                // Fail-safe: Open state denies requests rather than allowing corrupt state to pass traffic
                Self::Open
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    last_failure_time_nanos: AtomicU64,
    last_access_time_nanos: AtomicU64,
    half_open_trial_count: AtomicU32,
}

// Monotonic time reference prevents issues with system clock changes
static BASE_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

impl CircuitBreaker {
    pub(crate) fn new() -> Self {
        let now_nanos = Self::instant_to_nanos(Instant::now());
        Self {
            state: AtomicU8::new(CircuitState::Closed as u8),
            failure_count: AtomicU32::new(0),
            last_failure_time_nanos: AtomicU64::new(0),
            last_access_time_nanos: AtomicU64::new(now_nanos),
            half_open_trial_count: AtomicU32::new(0),
        }
    }

    fn instant_to_nanos(instant: Instant) -> u64 {
        let base = BASE_TIME.get_or_init(Instant::now);
        Self::handle_instant_before_base(instant, *base)
    }

    // Race condition safety: Instant might predate BASE_TIME if captured
    // before OnceLock initialization on another thread. Returns 0 (epoch)
    // rather than underflowing/panicking.
    fn handle_instant_before_base(instant: Instant, base: Instant) -> u64 {
        match instant.checked_duration_since(base) {
            Some(duration) => duration.as_nanos() as u64,
            None => 0,
        }
    }

    fn nanos_to_instant(nanos: u64) -> Instant {
        let base = BASE_TIME.get_or_init(Instant::now);
        *base + Duration::from_nanos(nanos)
    }

    fn has_timeout_elapsed(stored_nanos: u64, timeout: Duration) -> bool {
        if stored_nanos == 0 {
            return false;
        }
        let stored_instant = Self::nanos_to_instant(stored_nanos);
        match Instant::now().checked_duration_since(stored_instant) {
            Some(elapsed) => elapsed >= timeout,
            None => false,
        }
    }

    pub(crate) fn record_success(&self) {
        let now_nanos = Self::instant_to_nanos(Instant::now());
        self.failure_count.store(0, Ordering::Relaxed);
        self.half_open_trial_count.store(0, Ordering::Relaxed);
        self.last_failure_time_nanos.store(0, Ordering::Relaxed);
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);
        self.state
            .store(CircuitState::Closed as u8, Ordering::Release);
    }

    pub(crate) fn record_failure(&self) {
        let now_nanos = Self::instant_to_nanos(Instant::now());
        let current_state = CircuitState::from(self.state.load(Ordering::Acquire));

        // Be conservative in HalfOpen state: single failure proves service still unstable.
        // Immediate reopen prevents cascading failures to recovering service
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

        let old_count = self.failure_count.fetch_add(1, Ordering::Relaxed);
        let new_count = old_count.saturating_add(1);
        self.last_failure_time_nanos
            .store(now_nanos, Ordering::Relaxed);
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);

        if new_count >= FAILURES_TO_TRIP {
            self.state
                .store(CircuitState::Open as u8, Ordering::Release);
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        let now_nanos = Self::instant_to_nanos(Instant::now());
        self.last_access_time_nanos
            .store(now_nanos, Ordering::Relaxed);

        let current_state = CircuitState::from(self.state.load(Ordering::Acquire));

        match current_state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                let last_failure_nanos = self.last_failure_time_nanos.load(Ordering::Acquire);
                if Self::has_timeout_elapsed(last_failure_nanos, COOLDOWN_DURATION) {
                    // Check transition without race
                    let transition_succeeded = self
                        .state
                        .compare_exchange_weak(
                            CircuitState::Open as u8,
                            CircuitState::HalfOpen as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok();

                    if transition_succeeded {
                        // Automatic trial prevents deadlock where no one tests recovery
                        let _ = atomic_increment_bounded(
                            &self.half_open_trial_count,
                            RECOVERY_ATTEMPTS,
                        );
                    }

                    transition_succeeded
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                // Limited trials prevent infinite test attempts against unstable service
                atomic_increment_bounded(&self.half_open_trial_count, RECOVERY_ATTEMPTS)
            }
        }
    }

    pub(crate) fn is_stale(&self) -> bool {
        // Stale breakers represent departed/replaced peers - safe to evict
        let last_access_nanos = self.last_access_time_nanos.load(Ordering::Relaxed);
        Self::has_timeout_elapsed(last_access_nanos, STALE_BREAKER_TIMEOUT)
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub(crate) struct CircuitBreakerManager {
    breakers: std::sync::Arc<RwLock<LruCache<Address, CircuitBreaker>>>,
}

impl CircuitBreakerManager {
    pub(crate) fn new() -> Self {
        Self {
            // LRU eviction strategy: Retains active peer states while bounding memory
            breakers: std::sync::Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(MAX_CIRCUIT_BREAKERS).unwrap(),
            ))),
        }
    }

    pub(crate) async fn is_available(&self, peer_address: &Address) -> bool {
        // Read lock optimization: Most calls hit existing breakers (hot path).
        // Avoids write lock contention by checking read-only first.
        if let Some(br) = self.breakers.read().await.peek(peer_address) {
            return br.is_available();
        }
        let mut w = self.breakers.write().await;
        let br = w.get_or_insert_mut(*peer_address, CircuitBreaker::new);
        br.is_available()
    }

    pub(crate) async fn record_success(&self, peer_address: &Address) {
        // Recording success should always use write path
        let mut breakers = self.breakers.write().await;
        if let Some(breaker) = breakers.get(peer_address) {
            breaker.record_success();
        }
    }

    pub(crate) async fn record_failure(&self, peer_address: &Address) {
        let mut breakers = self.breakers.write().await;

        let breaker = breakers.get_or_insert_mut(*peer_address, CircuitBreaker::new);

        breaker.record_failure();
    }

    pub(crate) async fn cleanup_stale(&self) {
        let mut breakers = self.breakers.write().await;
        let stale_addresses = Self::collect_stale_addresses(&breakers);
        Self::remove_stale_entries(&mut breakers, stale_addresses);
    }

    fn collect_stale_addresses(breakers: &lru::LruCache<Address, CircuitBreaker>) -> Vec<Address> {
        breakers
            .iter()
            .filter_map(|(addr, breaker)| breaker.is_stale().then_some(*addr))
            .collect()
    }

    fn remove_stale_entries(
        breakers: &mut lru::LruCache<Address, CircuitBreaker>,
        stale_addresses: Vec<Address>,
    ) {
        if !stale_addresses.is_empty() {
            for addr in stale_addresses {
                breakers.pop(&addr);
            }
        }
    }
}

impl Default for CircuitBreakerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;
    use std::sync::Arc;

    #[rstest]
    #[case(0, CircuitState::Closed, true)]
    #[case(1, CircuitState::Closed, true)]
    #[case(4, CircuitState::Closed, true)]
    #[case(5, CircuitState::Open, false)]
    #[case(10, CircuitState::Open, false)]
    #[test]
    fn test_failure_threshold_and_availability(
        #[case] failure_count: usize,
        #[case] expected_state: CircuitState,
        #[case] expected_available: bool,
    ) {
        let breaker = CircuitBreaker::new();

        for _ in 0..failure_count {
            breaker.record_failure();
        }

        let actual_state = CircuitState::from(breaker.state.load(Ordering::Acquire));
        assert_eq!(actual_state, expected_state);

        // For open state, set last_failure_time to recent (no timeout elapsed)
        if expected_state == CircuitState::Open {
            breaker.last_failure_time_nanos.store(
                CircuitBreaker::instant_to_nanos(Instant::now()),
                Ordering::Relaxed,
            );
        }

        assert_eq!(breaker.is_available(), expected_available);
    }

    #[rstest]
    #[case(CircuitState::Closed, vec![0, 1, 2, 3, 4, 4, 4], CircuitState::Closed)]
    #[case(CircuitState::Closed, vec![0, 1, 2, 3, 4, 5], CircuitState::Open)]
    #[case(CircuitState::Open, vec![5, 5, 5], CircuitState::Open)]
    #[case(CircuitState::HalfOpen, vec![5], CircuitState::Open)]
    #[test]
    fn test_state_transitions_with_failures(
        #[case] initial_state: CircuitState,
        #[case] expected_counts: Vec<u32>,
        #[case] expected_final_state: CircuitState,
    ) {
        let breaker = CircuitBreaker::new();
        breaker.state.store(initial_state as u8, Ordering::Release);

        if initial_state == CircuitState::Open {
            breaker
                .failure_count
                .store(FAILURES_TO_TRIP, Ordering::Release);
        }

        for expected_count in expected_counts {
            breaker.record_failure();
            let count = breaker.failure_count.load(Ordering::Relaxed);
            assert!(
                count == expected_count || count == FAILURES_TO_TRIP,
                "Count {} doesn't match expected {}",
                count,
                expected_count
            );
        }

        let final_state = CircuitState::from(breaker.state.load(Ordering::Acquire));
        assert_eq!(final_state, expected_final_state);
    }

    #[rstest]
    #[case(CircuitState::Closed)]
    #[case(CircuitState::Open)]
    #[case(CircuitState::HalfOpen)]
    #[test]
    fn test_state_transitions_with_success(#[case] initial_state: CircuitState) {
        let breaker = CircuitBreaker::new();
        breaker.state.store(initial_state as u8, Ordering::Release);

        if initial_state != CircuitState::Closed {
            breaker
                .failure_count
                .store(FAILURES_TO_TRIP, Ordering::Release);
        }

        breaker.record_success();

        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::Closed,
            "Success should always close circuit from {:?}",
            initial_state
        );
        assert_eq!(breaker.failure_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_half_open_transition_complete_flow() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use std::time::Instant;

        let breaker = CircuitBreaker::new();

        // --- Trip to Open ---
        for _ in 0..FAILURES_TO_TRIP {
            breaker.record_failure();
        }
        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::Open
        );

        // --- Simulate cooldown elapsed so next availability check moves to HalfOpen ---
        let past_time = CircuitBreaker::instant_to_nanos(
            Instant::now()
                .checked_sub(COOLDOWN_DURATION + Duration::from_secs(1))
                .expect("instant subtraction should not underflow"),
        );
        breaker
            .last_failure_time_nanos
            .store(past_time, Ordering::Release);

        // First availability check should transition to HalfOpen
        assert!(breaker.is_available());
        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::HalfOpen
        );

        // --- Success path: HalfOpen -> Closed on success ---
        breaker.record_success();
        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::Closed
        );

        // --- Reset and test failure path: HalfOpen -> Open on failure ---
        for _ in 0..FAILURES_TO_TRIP {
            breaker.record_failure();
        }
        breaker
            .state
            .store(CircuitState::HalfOpen as u8, Ordering::Release);
        breaker.half_open_trial_count.store(0, Ordering::Release);

        breaker.record_failure();
        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::Open
        );
        assert_eq!(
            breaker.failure_count.load(Ordering::Relaxed),
            FAILURES_TO_TRIP
        );
    }

    #[rstest]
    #[case(0, true)] // under limit → should allow
    #[case(1, true)] // under limit → should allow
    #[case(2, true)] // exactly RECOVERY_ATTEMPTS-1 → should allow
    #[case(3, false)] // at limit → should deny
    #[case(5, false)] // well over limit → should deny
    #[test]
    fn test_half_open_trial_boundary_values(
        #[case] initial_trials: u32,
        #[case] expected_available: bool,
    ) {
        let breaker = CircuitBreaker::new();
        // Force breaker into HalfOpen
        breaker
            .state
            .store(CircuitState::HalfOpen as u8, Ordering::Release);
        breaker
            .half_open_trial_count
            .store(initial_trials, Ordering::Relaxed);

        let available = breaker.is_available();

        assert_eq!(
            available, expected_available,
            "With {} initial trials, availability should be {}",
            initial_trials, expected_available
        );

        // Verify counter incremented only if it was available
        let new_count = breaker.half_open_trial_count.load(Ordering::Relaxed);
        if expected_available {
            assert_eq!(
                new_count,
                initial_trials + 1,
                "Counter should increment when available"
            );
        } else {
            assert_eq!(
                new_count, initial_trials,
                "Counter should not change when unavailable"
            );
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_manager() {
        let manager = CircuitBreakerManager::new();
        let peer_addr = Address::from([1_u8; 20]);

        assert!(manager.is_available(&peer_addr).await);

        for _ in 0..FAILURES_TO_TRIP {
            manager.record_failure(&peer_addr).await;
        }

        assert!(!manager.is_available(&peer_addr).await);

        manager.record_success(&peer_addr).await;
        assert!(manager.is_available(&peer_addr).await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_cleanup_stale() {
        let manager = CircuitBreakerManager::new();
        let addr = Address::from([1_u8; 20]);

        manager.record_failure(&addr).await;
        {
            let breakers = manager.breakers.read().await;
            assert_eq!(breakers.len(), 1);
            // Force staleness
            let br = breakers.peek(&addr).unwrap();
            br.last_access_time_nanos.store(
                CircuitBreaker::instant_to_nanos(
                    Instant::now() - STALE_BREAKER_TIMEOUT - Duration::from_secs(1),
                ),
                Ordering::Relaxed,
            );
        }

        manager.cleanup_stale().await;
        let breakers = manager.breakers.read().await;
        assert_eq!(breakers.len(), 0, "stale entry should be evicted");
    }

    #[tokio::test]
    async fn test_lru_behavior_on_record_success() {
        let manager = CircuitBreakerManager::new();
        let addr1 = Address::from([1_u8; 20]);
        let addr2 = Address::from([2_u8; 20]);

        // Record failure for addr1 to ensure it's in cache
        manager.record_failure(&addr1).await;

        // Record success for addr2 (not in cache initially)
        manager.record_success(&addr2).await;

        // Both should be accessible
        assert!(manager.is_available(&addr1).await);
        assert!(manager.is_available(&addr2).await);

        // Verify LRU updated
        {
            let breakers = manager.breakers.read().await;
            assert_eq!(breakers.len(), 2);
        }
    }

    #[test]
    fn test_instant_to_nanos_race_condition() {
        use std::thread;

        let handles: Vec<_> = (0..100)
            .map(|_| {
                thread::spawn(|| {
                    let instant = Instant::now();
                    let nanos = CircuitBreaker::instant_to_nanos(instant);
                    assert!(nanos < u64::MAX / 2, "Should not overflow");
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_has_timeout_elapsed_future_and_past() {
        // Test with future instant (should not have elapsed)
        let future_time = Instant::now() + Duration::from_secs(60);
        let future_nanos = CircuitBreaker::instant_to_nanos(future_time);
        assert!(
            !CircuitBreaker::has_timeout_elapsed(future_nanos, Duration::from_secs(1)),
            "Future time should not be considered elapsed"
        );

        // Test with past instant (should have elapsed)
        let past_time = Instant::now() - Duration::from_secs(60);
        let past_nanos = CircuitBreaker::instant_to_nanos(past_time);
        assert!(
            CircuitBreaker::has_timeout_elapsed(past_nanos, Duration::from_secs(1)),
            "Past time beyond timeout should be considered elapsed"
        );
    }

    #[rstest]
    #[case(100, 5)]
    #[case(500, 5)]
    #[case(1000, 5)]
    #[tokio::test]
    async fn test_concurrent_state_transitions_under_load(
        #[case] thread_count: usize,
        #[case] _threshold: u32,
    ) {
        use std::sync::atomic::AtomicUsize;

        let breaker = Arc::new(CircuitBreaker::new());
        let success_count = Arc::new(AtomicUsize::new(0));
        let failure_count = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();

        for i in 0..thread_count {
            let breaker = breaker.clone();
            let success_count = success_count.clone();
            let failure_count = failure_count.clone();

            handles.push(tokio::spawn(async move {
                match i % 3 {
                    0 => {
                        breaker.record_failure();
                        failure_count.fetch_add(1, Ordering::Relaxed);
                    }
                    1 => {
                        breaker.record_success();
                        success_count.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        breaker.is_available();
                    }
                }
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let final_state = CircuitState::from(breaker.state.load(Ordering::Acquire));
        assert!(
            matches!(
                final_state,
                CircuitState::Closed | CircuitState::Open | CircuitState::HalfOpen
            ),
            "State should be valid after concurrent operations"
        );
    }

    #[rstest]
    #[case(CircuitState::Open, CircuitState::HalfOpen, 50)]
    #[case(CircuitState::HalfOpen, CircuitState::Open, 50)]
    #[case(CircuitState::HalfOpen, CircuitState::Closed, 50)]
    #[tokio::test]
    async fn test_concurrent_transition_attempts(
        #[case] from_state: CircuitState,
        #[case] to_state: CircuitState,
        #[case] concurrent_attempts: usize,
    ) {
        let breaker = Arc::new(CircuitBreaker::new());

        breaker.state.store(from_state as u8, Ordering::Release);

        if from_state == CircuitState::Open && to_state == CircuitState::HalfOpen {
            let past_time = CircuitBreaker::instant_to_nanos(
                Instant::now()
                    .checked_sub(COOLDOWN_DURATION)
                    .unwrap()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap(),
            );
            breaker
                .last_failure_time_nanos
                .store(past_time, Ordering::Release);
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(concurrent_attempts));
        let mut handles = Vec::new();
        let successful_transitions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..concurrent_attempts {
            let breaker = breaker.clone();
            let barrier = barrier.clone();
            let successful_transitions = successful_transitions.clone();

            handles.push(tokio::spawn(async move {
                barrier.wait().await;

                match (from_state, to_state) {
                    (CircuitState::Open, CircuitState::HalfOpen) => {
                        if breaker.is_available() {
                            successful_transitions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    (CircuitState::HalfOpen, CircuitState::Open) => {
                        breaker.record_failure();
                        if CircuitState::from(breaker.state.load(Ordering::Acquire))
                            == CircuitState::Open
                        {
                            successful_transitions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    (CircuitState::HalfOpen, CircuitState::Closed) => {
                        breaker.record_success();
                        if CircuitState::from(breaker.state.load(Ordering::Acquire))
                            == CircuitState::Closed
                        {
                            successful_transitions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => {}
                }
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        if from_state == CircuitState::Open && to_state == CircuitState::HalfOpen {
            assert!(
                successful_transitions.load(Ordering::Relaxed) <= RECOVERY_ATTEMPTS as usize,
                "At most {} threads should succeed in half-open state",
                RECOVERY_ATTEMPTS
            );
        }
    }

    #[rstest]
    #[case(1)]
    #[case(3)]
    #[case(5)]
    #[tokio::test]
    async fn test_recover_from_half_open_retries(#[case] retry_attempts: usize) {
        let breaker = CircuitBreaker::new();

        // Trip to Open
        for _ in 0..FAILURES_TO_TRIP {
            breaker.record_failure();
        }
        assert_eq!(
            CircuitState::from(breaker.state.load(Ordering::Acquire)),
            CircuitState::Open,
            "Breaker should be Open after {} failures",
            FAILURES_TO_TRIP
        );

        for attempt in 1..=retry_attempts {
            // Force HalfOpen for the attempt (simulate post-cooldown probe window)
            breaker
                .state
                .store(CircuitState::HalfOpen as u8, Ordering::Release);
            breaker.half_open_trial_count.store(0, Ordering::Relaxed);

            // Fail in HalfOpen - should immediately go back to Open (conservative policy)
            breaker.record_failure();

            let state = CircuitState::from(breaker.state.load(Ordering::Acquire));
            assert_eq!(
                state,
                CircuitState::Open,
                "Attempt {} should have returned to Open",
                attempt
            );

            // Half-open counters should be reset when returning to Open
            assert_eq!(
                breaker.half_open_trial_count.load(Ordering::Relaxed),
                0,
                "Half-open trial count should reset after reopening (attempt {})",
                attempt
            );
        }
    }

    proptest! {
        #[test]
        fn prop_circuit_state_transitions_valid(
            operations in prop::collection::vec(
                (prop::bool::ANY, 0_u64..10),
                1..50
            )
        ) {
            let breaker = CircuitBreaker::new();

            for (is_failure, delay_ms) in operations {
                std::thread::sleep(Duration::from_millis(delay_ms));

                let prev_state = CircuitState::from(breaker.state.load(Ordering::Acquire));
                let prev_count = breaker.failure_count.load(Ordering::Relaxed);

                if is_failure {
                    breaker.record_failure();
                } else {
                    breaker.record_success();
                }

                let new_state = CircuitState::from(breaker.state.load(Ordering::Acquire));
                let new_count = breaker.failure_count.load(Ordering::Relaxed);

                // Verify valid state transitions
                match (prev_state, new_state) {
                    (CircuitState::Closed, CircuitState::Open) => {
                        prop_assert!(new_count >= FAILURES_TO_TRIP);
                    }
                    (CircuitState::Open, CircuitState::Closed) => {
                        prop_assert!(!is_failure, "Success should close from any state");
                    }
                    (CircuitState::HalfOpen, CircuitState::Open) => {
                        prop_assert!(is_failure);
                    }
                    (CircuitState::HalfOpen, CircuitState::Closed) => {
                        prop_assert!(!is_failure);
                    }
                    _ => {}
                }

                // Verify failure count invariants
                if is_failure {
                    prop_assert!(
                        new_count > prev_count || new_count == 0 || new_count == FAILURES_TO_TRIP,
                        "Failure count decreased unexpectedly: {} -> {}", prev_count, new_count
                    );
                } else {
                    prop_assert_eq!(new_count, 0, "Success should reset failure count");
                }

                // Verify state validity
                prop_assert!(matches!(
                    new_state,
                    CircuitState::Closed | CircuitState::Open | CircuitState::HalfOpen
                ));
            }
        }

        #[test]
        fn prop_circuit_breaker_deterministic(
            seed_failures in 0_u32..20,
            seed_successes in 0_u32..20,
        ) {
            let breaker1 = CircuitBreaker::new();
            let breaker2 = CircuitBreaker::new();

            // Apply same operations to both
            for _ in 0..seed_failures {
                breaker1.record_failure();

               breaker2.record_failure();
            }

            for _ in 0..seed_successes {
                breaker1.record_success();
                breaker2.record_success();
            }

            // States should match
            let state1 = breaker1.state.load(Ordering::Acquire);
            let state2 = breaker2.state.load(Ordering::Acquire);
            prop_assert_eq!(state1, state2, "Same operations should produce same state");

            let count1 = breaker1.failure_count.load(Ordering::Relaxed);
            let count2 = breaker2.failure_count.load(Ordering::Relaxed);
            prop_assert_eq!(count1, count2, "Same operations should produce same failure count");
        }
    }
}
