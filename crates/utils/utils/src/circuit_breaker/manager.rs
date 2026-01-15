use dashmap::DashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::breaker::CircuitBreaker;
use super::config::CircuitBreakerConfig;
use super::metrics::CircuitBreakerMetrics;
use super::state::CircuitState;

#[derive(Debug)]
pub struct CircuitBreakerManager<K>
where
    K: Eq + Hash + Clone + Debug,
{
    breakers: Arc<DashMap<K, CircuitBreaker>>,
    active_count: Arc<AtomicUsize>,
    capacity: usize,
    config: CircuitBreakerConfig,
}

impl<K> CircuitBreakerManager<K>
where
    K: Eq + Hash + Clone + Debug,
{
    pub fn new(capacity: NonZeroUsize, config: CircuitBreakerConfig) -> Self {
        Self {
            breakers: Arc::new(DashMap::new()),
            active_count: Arc::new(AtomicUsize::new(0)),
            capacity: capacity.get(),
            config,
        }
    }

    #[must_use = "ignoring availability check defeats the purpose of circuit breaker"]
    pub fn is_available(&self, key: &K) -> bool {
        if let Some(breaker) = self.breakers.get(key) {
            let available = breaker.is_available();
            tracing::trace!(?key, available, "circuit breaker availability check");
            return available;
        }

        match self.get_or_create_breaker(key) {
            Some(breaker) => breaker.is_available(),
            None => false, // Capacity reached, fail-closed
        }
    }

    pub fn record_success(&self, key: &K) {
        if let Some(breaker) = self.breakers.get(key) {
            breaker.record_success();
            tracing::trace!(?key, "recorded success");
        }
    }

    pub fn record_failure(&self, key: &K) {
        if let Some(breaker) = self.breakers.get(key) {
            breaker.record_failure();
            let state = breaker.state();
            tracing::debug!(?key, ?state, "recorded failure");
            return;
        }

        if let Some(breaker) = self.get_or_create_breaker(key) {
            breaker.record_failure();
            let state = breaker.state();
            tracing::debug!(?key, ?state, "recorded failure");
        }
    }

    /// Atomically reserve a slot using CAS.
    fn try_reserve_slot(&self, key: &K) -> bool {
        // First attempt to reserve atomically
        let result =
            self.active_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < self.capacity).then_some(count + 1)
                });

        if result.is_ok() {
            return true;
        }

        // At capacity - try cleanup and retry once
        self.cleanup_stale();

        let result =
            self.active_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < self.capacity).then_some(count + 1)
                });

        if result.is_err() {
            tracing::warn!(
                ?key,
                capacity = self.capacity,
                current = self.active_count.load(Ordering::Relaxed),
                "circuit breaker capacity reached, cannot create new breaker"
            );
        }

        result.is_ok()
    }

    fn get_or_create_breaker(
        &self,
        key: &K,
    ) -> Option<dashmap::mapref::one::Ref<K, CircuitBreaker>> {
        // Fast path: breaker already exists
        if let Some(breaker) = self.breakers.get(key) {
            return Some(breaker);
        }

        // Try to atomically reserve a slot
        if !self.try_reserve_slot(key) {
            return None; // Capacity reached
        }

        // Try to insert the breaker
        use dashmap::mapref::entry::Entry;
        match self.breakers.entry(key.clone()) {
            Entry::Occupied(entry) => {
                // Another thread inserted it first - undo reservation
                self.active_count.fetch_sub(1, Ordering::Relaxed);
                Some(entry.into_ref().downgrade())
            }
            Entry::Vacant(entry) => {
                tracing::trace!(?key, "created new circuit breaker");
                Some(entry.insert(CircuitBreaker::new(&self.config)).downgrade())
            }
        }
    }

    pub fn cleanup_stale(&self) {
        let stale_keys: Vec<K> = self
            .breakers
            .iter()
            .filter(|entry| entry.value().is_stale())
            .map(|entry| entry.key().clone())
            .collect();

        for key in &stale_keys {
            if self.breakers.remove(key).is_some() {
                self.active_count.fetch_sub(1, Ordering::Relaxed);
            }
        }

        let removed = stale_keys.len();
        if removed > 0 {
            tracing::debug!(removed, "cleaned up stale circuit breakers");
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active_count.load(Ordering::Relaxed) == 0
    }

    pub fn metrics(&self) -> CircuitBreakerMetrics<K> {
        let mut metrics = CircuitBreakerMetrics::default();

        for entry in self.breakers.iter() {
            metrics.total_count += 1;
            match entry.value().state() {
                CircuitState::Closed => metrics.closed_count += 1,
                CircuitState::Open => {
                    metrics.open_count += 1;
                    metrics.open_keys.push(entry.key().clone());
                }
                CircuitState::HalfOpen => metrics.half_open_count += 1,
            }
        }

        metrics
    }
}

impl<K> Clone for CircuitBreakerManager<K>
where
    K: Eq + Hash + Clone + Debug,
{
    fn clone(&self) -> Self {
        Self {
            breakers: Arc::clone(&self.breakers),
            active_count: Arc::clone(&self.active_count),
            capacity: self.capacity,
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_utils::TestConfigBuilder;
    use super::*;
    use std::time::Duration;

    #[derive(Debug, Clone, Hash, Eq, PartialEq)]
    struct TestKey(String);

    fn test_config() -> CircuitBreakerConfig {
        TestConfigBuilder::default()
            .cooldown_duration(Duration::from_secs(10))
            .build()
    }

    fn test_config_with_stale_timeout(timeout: Duration) -> CircuitBreakerConfig {
        TestConfigBuilder::default().stale_timeout(timeout).build()
    }

    #[tokio::test]
    async fn test_is_available_creates_breaker() {
        let config = CircuitBreakerConfig::p2p_defaults();
        let manager =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key = TestKey("test1".to_string());
        assert!(manager.is_available(&key));
        assert_eq!(manager.len(), 1);
    }

    #[tokio::test]
    async fn test_record_failure_trips_circuit() {
        let config = test_config();
        let manager =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key = TestKey("test1".to_string());

        for _ in 0..3 {
            manager.record_failure(&key);
        }

        assert!(!manager.is_available(&key));
    }

    #[tokio::test]
    async fn test_record_success_closes_circuit() {
        let config = test_config();
        let manager =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key = TestKey("test1".to_string());

        for _ in 0..3 {
            manager.record_failure(&key);
        }
        assert!(!manager.is_available(&key));

        manager.record_success(&key);
        assert!(manager.is_available(&key));
    }

    #[tokio::test]
    async fn test_cleanup_removes_stale_breakers() {
        let config = test_config_with_stale_timeout(Duration::from_millis(100));
        let manager =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key1 = TestKey("test1".to_string());
        let key2 = TestKey("test2".to_string());

        let _ = manager.is_available(&key1);
        let _ = manager.is_available(&key2);
        assert_eq!(manager.len(), 2);

        tokio::time::sleep(Duration::from_millis(150)).await;

        manager.cleanup_stale();
        assert_eq!(manager.len(), 0);
    }

    #[tokio::test]
    async fn test_cleanup_keeps_active_breakers() {
        let config = test_config_with_stale_timeout(Duration::from_millis(200));
        let manager =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key1 = TestKey("test1".to_string());
        let key2 = TestKey("test2".to_string());

        let _ = manager.is_available(&key1);
        let _ = manager.is_available(&key2);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = manager.is_available(&key1);

        // Wait for key2 to become stale
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Cleanup should only remove key2
        manager.cleanup_stale();
        assert_eq!(manager.len(), 1);

        // key1 should still work
        assert!(manager.is_available(&key1));
    }

    #[tokio::test]
    async fn test_capacity_enforcement_fail_closed() {
        let config = CircuitBreakerConfig::p2p_defaults();
        let manager = CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(3).unwrap(), config);

        // Fill to capacity
        let key1 = TestKey("test1".to_string());
        let key2 = TestKey("test2".to_string());
        let key3 = TestKey("test3".to_string());

        let _ = manager.is_available(&key1);
        let _ = manager.is_available(&key2);
        let _ = manager.is_available(&key3);
        assert_eq!(manager.len(), 3);

        // Attempt to create 4th breaker should fail-closed
        let key4 = TestKey("test4".to_string());
        assert!(!manager.is_available(&key4));
        assert_eq!(manager.len(), 3);

        // Record failure on new key should be ignored
        manager.record_failure(&key4);
        assert_eq!(manager.len(), 3);
    }

    #[tokio::test]
    async fn test_capacity_enforcement_allows_after_cleanup() {
        let config = test_config_with_stale_timeout(Duration::from_millis(100));
        let manager = CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(2).unwrap(), config);

        // Fill to capacity
        let key1 = TestKey("test1".to_string());
        let key2 = TestKey("test2".to_string());

        let _ = manager.is_available(&key1);
        let _ = manager.is_available(&key2);
        assert_eq!(manager.len(), 2);

        // Wait for them to become stale
        tokio::time::sleep(Duration::from_millis(150)).await;

        // New key should trigger cleanup and succeed
        let key3 = TestKey("test3".to_string());
        assert!(manager.is_available(&key3));
        assert_eq!(manager.len(), 1);
    }

    #[tokio::test]
    async fn test_manager_clone_shares_state() {
        let config = CircuitBreakerConfig::p2p_defaults();
        let manager1 =
            CircuitBreakerManager::<TestKey>::new(NonZeroUsize::new(100).unwrap(), config);

        let key = TestKey("test1".to_string());
        let _ = manager1.is_available(&key);

        // Clone should share the same state across async tasks
        let manager2 = manager1.clone();
        let key_clone = key.clone();
        let handle = tokio::spawn(async move {
            assert_eq!(manager2.len(), 1);
            manager2.record_failure(&key_clone);
        });

        handle.await.unwrap();
        // Both should see the updated state
        assert_eq!(manager1.len(), 1);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {

            #[test]
            fn prop_success_after_failure_always_available(
                num_failures in 0_u32..20,
            ) {
                let config = test_config();
                let manager = CircuitBreakerManager::<String>::new(
                    NonZeroUsize::new(100).unwrap(),
                    config,
                );

                let key = "test-key".to_string();

                for _ in 0..num_failures {
                    manager.record_failure(&key);
                }

                manager.record_success(&key);

                // Invariant: After success, circuit is available
                prop_assert!(manager.is_available(&key));
            }

            #[test]
            fn prop_metrics_counts_match_reality(
                operations in prop::collection::vec(
                    (prop::bool::ANY, 0_usize..10),
                    0..50
                )
            ) {
                let config = CircuitBreakerConfig::p2p_defaults();
                let manager = CircuitBreakerManager::<String>::new(
                    NonZeroUsize::new(100).unwrap(),
                    config,
                );

                for (is_success, key_idx) in operations {
                    let key = format!("key-{}", key_idx);
                    if is_success {
                        manager.record_success(&key);
                    } else {
                        manager.record_failure(&key);
                    }
                }

                let metrics = manager.metrics();

                // Invariant: Metrics counts sum to total
                prop_assert_eq!(
                    metrics.total_count,
                    metrics.closed_count + metrics.open_count + metrics.half_open_count
                );

                // Invariant: open_keys length matches open_count
                prop_assert_eq!(metrics.open_keys.len(), metrics.open_count);

                // Invariant: Health ratio between 0.0 and 1.0
                let health = metrics.health_ratio();
                prop_assert!((0.0..=1.0).contains(&health));
            }

            #[test]
            fn prop_state_consistency_under_operations(
                operations in prop::collection::vec(
                    prop::bool::ANY,
                    1..20
                )
            ) {
                let config = CircuitBreakerConfig::p2p_defaults();
                let manager = CircuitBreakerManager::<String>::new(
                    NonZeroUsize::new(100).unwrap(),
                    config,
                );

                let key = "shared-key".to_string();

                for is_success in operations.iter() {
                    if *is_success {
                        manager.record_success(&key);
                    } else {
                        manager.record_failure(&key);
                    }
                }

                // Invariant: Manager state remains consistent
                let metrics = manager.metrics();
                prop_assert_eq!(
                    metrics.total_count,
                    metrics.closed_count + metrics.open_count + metrics.half_open_count
                );

                // Invariant: Availability check is consistent
                let _ = manager.is_available(&key);
            }
        }
    }
}
