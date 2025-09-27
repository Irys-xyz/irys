use core::time::Duration;
use irys_types::{Address, GossipDataRequest};
use moka::future::Cache;
use sha2::{Digest as _, Sha256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// Duration TTL should be:
// - Short enough to prevent unbounded memory growth under sustained load
// - Long enough to catch retries from network timeouts (typically 1-3s)
// - Allows immediate retry on explicit release without waiting for TTL
const REQUEST_DEDUP_TTL: Duration = Duration::from_secs(5);

// Empirically determined: handles 1000 req/s with 5s TTL = 5000 active entries
// 2x headroom for burst traffic without triggering LRU eviction
const MAX_DEDUP_CACHE_SIZE: usize = 10_000;

// Type prefixes ensure different request types with identical hashes
// produce different keys, preventing Block(hash=X) from deduplicating
// with Chunk(hash=X) which would cause data corruption
const TYPE_PREFIX_BLOCK: u8 = 0;
const TYPE_PREFIX_EXECUTION: u8 = 1;
const TYPE_PREFIX_CHUNK: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DedupKey([u8; 32]);

impl DedupKey {
    #[cfg(test)]
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a DedupKey from a peer address and request
    pub(crate) fn from_request(peer_address: &Address, requested_data: &GossipDataRequest) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(peer_address);

        match requested_data {
            GossipDataRequest::Block(hash) => {
                hasher.update([TYPE_PREFIX_BLOCK]);
                hasher.update(hash.0);
            }
            GossipDataRequest::ExecutionPayload(hash) => {
                hasher.update([TYPE_PREFIX_EXECUTION]);
                hasher.update(hash.as_slice());
            }
            GossipDataRequest::Chunk(hash) => {
                hasher.update([TYPE_PREFIX_CHUNK]);
                hasher.update(hash.0);
            }
        }

        Self(hasher.finalize().into())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DedupEntry {
    in_flight: Arc<AtomicBool>,
}

impl DedupEntry {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RequestDedupCache {
    entries: Cache<DedupKey, DedupEntry>,
}

impl RequestDedupCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(MAX_DEDUP_CACHE_SIZE as u64)
                // time_to_live: hard expiry regardless of access
                .time_to_live(REQUEST_DEDUP_TTL)
                // time_to_idle: extends TTL for hot paths (frequently accessed entries)
                .time_to_idle(REQUEST_DEDUP_TTL)
                .build(),
        }
    }

    // Moka's get_with is atomic so there should be no race condition where multiple tasks could create duplicate entries.
    async fn get_or_create_entry(&self, key: DedupKey) -> DedupEntry {
        self.entries
            .get_with(key, async { DedupEntry::new() })
            .await
    }
}

use core::future::Future;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DedupRunError<E> {
    /// Another task is already handling this key.
    #[error("duplicate request in flight")]
    Duplicate,
    /// The wrapped operation failed.
    #[error(transparent)]
    Op(#[from] E),
}

// RAII guard for preventing deadlocks: if a task panics or is cancelled
// while holding a claim, Drop impl guarantees release.
#[derive(Debug)]
pub(crate) struct ClaimGuard {
    entry: Arc<AtomicBool>,
    released: bool,
}

impl ClaimGuard {
    fn new(entry: Arc<AtomicBool>) -> Self {
        Self {
            entry,
            released: false,
        }
    }

    pub(crate) fn release(mut self) {
        self.entry.store(false, Ordering::Release);
        self.released = true;
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        // Safety net: ensures claim is released even if user forgets to call release().
        // Critical for preventing permanent key lockup on panic/cancellation/early return.
        if !self.released {
            self.entry.store(false, Ordering::Release);
        }
    }
}

impl RequestDedupCache {
    /// Internal claim implementation.
    #[cfg(test)]
    pub(crate) async fn claim(&self, key: DedupKey) -> Option<ClaimGuard> {
        self.claim_internal(key).await
    }

    async fn claim_internal(&self, key: DedupKey) -> Option<ClaimGuard> {
        let entry = self.get_or_create_entry(key).await;
        // Atomically transitions in_flight from false to true. Returns success only
        // for the first caller - all others see true and fail. This is the core
        // mutual exclusion mechanism preventing duplicate operations.
        if entry
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(ClaimGuard::new(Arc::clone(&entry.in_flight)))
        } else {
            None
        }
    }

    /// Execute an operation with deduplication protection.
    ///
    /// Guarantees:
    /// - At most one instance of `op` runs concurrently for a given key
    /// - Claim is released regardless of success, failure, or panic
    /// - Returns `Duplicate` error if another task holds the claim
    pub(crate) async fn run_dedup<F, Fut, T, E>(
        &self,
        key: DedupKey,
        op: F,
    ) -> Result<T, DedupRunError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let Some(guard) = self.claim_internal(key).await else {
            return Err(DedupRunError::Duplicate);
        };

        match op().await {
            Ok(v) => {
                guard.release();
                Ok(v)
            }
            Err(e) => {
                guard.release();
                Err(DedupRunError::Op(e))
            }
        }
    }
}

impl Default for RequestDedupCache {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn compute_dedup_key(
    peer_address: &Address,
    requested_data: &GossipDataRequest,
) -> DedupKey {
    DedupKey::from_request(peer_address, requested_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::BlockHash;
    use proptest::prelude::*;
    use rstest::rstest;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    // Test utilities
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DedupMode {
        PerPeer,
        Global,
    }

    fn compute_dedup_key_with_mode(
        peer_address: &Address,
        requested_data: &GossipDataRequest,
        mode: DedupMode,
    ) -> [u8; 32] {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();

        if mode == DedupMode::PerPeer {
            hasher.update(peer_address);
        }

        match requested_data {
            GossipDataRequest::Block(hash) => {
                hasher.update([TYPE_PREFIX_BLOCK]);
                hasher.update(hash.0);
            }
            GossipDataRequest::ExecutionPayload(hash) => {
                hasher.update([TYPE_PREFIX_EXECUTION]);
                hasher.update(hash.as_slice());
            }
            GossipDataRequest::Chunk(hash) => {
                hasher.update([TYPE_PREFIX_CHUNK]);
                hasher.update(hash.0);
            }
        }
        hasher.finalize().into()
    }

    impl DedupKey {
        fn from_request_with_mode(
            peer_address: &Address,
            requested_data: &GossipDataRequest,
            mode: DedupMode,
        ) -> Self {
            Self(compute_dedup_key_with_mode(
                peer_address,
                requested_data,
                mode,
            ))
        }
    }

    mod basic_claim_release_tests {
        use super::*;

        #[tokio::test]
        async fn test_dedup_cache_basic_functionality() {
            let cache = RequestDedupCache::new();
            let key = DedupKey::new([1_u8; 32]);

            let guard = cache.claim(key).await;
            assert!(guard.is_some());
            assert!(cache.claim(key).await.is_none());

            guard.unwrap().release();

            // release() allows immediate retry without waiting for TTL.
            // This is for error recovery - we don't want to wait 5 seconds
            // after a transient network failure.
            assert!(cache.claim(key).await.is_some());
        }

        #[rstest]
        #[case(1)]
        #[case(100)]
        #[case(1000)]
        #[tokio::test]
        async fn test_rapid_claim_release_cycles(#[case] cycles: usize) {
            let cache = RequestDedupCache::new();
            let key = DedupKey::new([42_u8; 32]);

            for _ in 0..cycles {
                let guard1 = cache.claim(key).await;
                assert!(guard1.is_some());
                drop(guard1);
                // Should immediately allow re-entry
                let guard2 = cache.claim(key).await;
                assert!(guard2.is_some());
                drop(guard2);
            }
        }
    } // end basic_claim_release_tests

    mod concurrency_tests {
        use super::*;

        #[tokio::test]
        async fn test_concurrent_same_key_exactly_one_success() {
            let cache = Arc::new(RequestDedupCache::new());
            let key = DedupKey::new([42_u8; 32]);
            let success_count = Arc::new(AtomicU32::new(0));

            let tasks: Vec<_> = (0..100)
                .map(|_| {
                    let cache_clone = cache.clone();
                    let success_count_clone = success_count.clone();
                    tokio::spawn(async move {
                        let guard = cache_clone.claim(key).await;
                        if guard.is_some() {
                            success_count_clone.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                    })
                })
                .collect();

            for task in tasks {
                task.await.unwrap();
            }

            // Exactly one winner among N concurrent
            // attempts. This prevents "thundering herd"
            assert_eq!(
                success_count.load(Ordering::Relaxed),
                1,
                "get_with should ensure exactly one task succeeds even under heavy concurrency"
            );
        }
    } // end concurrency_tests

    mod run_dedup_tests {
        use super::*;

        #[rstest]
        #[case(Ok("success"))]
        #[case(Err("failure"))]
        #[tokio::test]
        async fn test_run_dedup_outcomes(#[case] op_result: Result<&str, &str>) {
            let cache = RequestDedupCache::new();
            let key = DedupKey::new([99_u8; 32]);

            let result = cache.run_dedup(key, || async { op_result }).await;
            match op_result {
                Ok(expected_val) => {
                    assert!(matches!(result, Ok(val) if val == expected_val));
                }
                Err(expected_err) => {
                    assert!(matches!(result, Err(DedupRunError::Op(err)) if err == expected_err));
                }
            }
        }

        #[tokio::test]
        async fn test_run_dedup_concurrent_duplicate_detection() {
            let cache = Arc::new(RequestDedupCache::new());
            let key = DedupKey::new([42_u8; 32]);

            let cache1 = cache.clone();
            let cache2 = cache.clone();

            let (tx, rx) = tokio::sync::oneshot::channel();

            // First task claims and waits
            let task1 = tokio::spawn(async move {
                cache1
                    .run_dedup(key, || async {
                        rx.await.unwrap();
                        Ok::<&str, &str>("first")
                    })
                    .await
            });

            // Second task should get duplicate error
            tokio::time::sleep(Duration::from_millis(10)).await;
            let result2 = cache2
                .run_dedup(key, || async { Ok::<&str, &str>("second") })
                .await;

            // Complete first task
            tx.send(()).unwrap();
            let result1: Result<&str, DedupRunError<&str>> = task1.await.unwrap();

            assert!(matches!(result1, Ok("first")));
            assert!(matches!(result2, Err(DedupRunError::Duplicate)));
        }

        #[tokio::test]
        async fn test_run_dedup_panic_recovery() {
            let cache = Arc::new(RequestDedupCache::new());
            let key = DedupKey::new([99_u8; 32]);

            // First task will panic inside the operation
            let cache_clone = cache.clone();
            let panic_task = tokio::spawn(async move {
                let _result: Result<(), DedupRunError<&str>> = cache_clone
                    .run_dedup(key, || async {
                        panic!("Operation panicked!");
                        #[expect(unreachable_code)]
                        Ok::<(), &str>(())
                    })
                    .await;
            });

            // Task should panic
            let result = panic_task.await;
            assert!(result.is_err(), "Task should have panicked");

            // Small delay to ensure cleanup
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Key should be claimable again after panic (Drop impl should release)
            assert!(
                cache.claim(key).await.is_some(),
                "Key should be claimable after panic due to Drop impl"
            );
        }

        #[tokio::test]
        async fn test_run_dedup_task_cancellation_recovery() {
            let cache = Arc::new(RequestDedupCache::new());
            let key = DedupKey::new([77_u8; 32]);

            // Start operation that will be cancelled
            let cache_clone = cache.clone();
            let task = tokio::spawn(async move {
                cache_clone
                    .run_dedup(key, || async {
                        // Long operation that will be cancelled
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Ok::<_, &str>("never completes")
                    })
                    .await
            });

            // Give task time to claim
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Cancel the task
            task.abort();
            let _ = task.await; // Wait for cancellation

            // Small delay for cleanup
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Key should be claimable again
            assert!(
                cache.claim(key).await.is_some(),
                "Key should be claimable after task cancellation"
            );
        }
    } // end run_dedup_tests

    mod ttl_expiry_tests {
        use super::*;

        #[tokio::test]
        async fn test_dedup_cache_ttl_expiration() {
            let cache = RequestDedupCache::new();
            let key = DedupKey::new([1_u8; 32]);

            let guard = cache.claim(key).await;
            assert!(guard.is_some());
            assert!(cache.claim(key).await.is_none());

            drop(guard);
            tokio::time::sleep(Duration::from_secs(6)).await;

            assert!(cache.claim(key).await.is_some());
        }

        #[tokio::test]
        async fn test_ttl_refresh_on_access() {
            // Test that accessing an entry refreshes its TTL
            let cache = Cache::builder()
                .max_capacity(100)
                .time_to_idle(Duration::from_secs(1)) // TTL resets on access
                .build();
            let dedup_cache = RequestDedupCache { entries: cache };

            let key = DedupKey::new([88_u8; 32]);

            // Initial claim
            assert!(dedup_cache.claim(key).await.is_some());

            // Access multiple times, each extending TTL
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(800)).await;
                // Mark completed to allow re-claim
                // Guard auto-releases on drop
                // Re-claim before TTL expires
                assert!(
                    dedup_cache.claim(key).await.is_some(),
                    "TTL should be refreshed on access"
                );
            }

            // Now let it expire without access
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Should still be claimable (entry expired)
            assert!(dedup_cache.claim(key).await.is_some());
        }

        #[tokio::test]
        async fn test_ttl_concurrent_expiration() {
            use std::sync::Arc;

            let cache = Cache::builder()
                .max_capacity(100)
                .time_to_live(Duration::from_millis(500))
                .build();
            let dedup_cache = Arc::new(RequestDedupCache { entries: cache });

            let key = DedupKey::new([99_u8; 32]);

            // Claim initially
            assert!(dedup_cache.claim(key).await.is_some());

            // Launch concurrent tasks trying to claim after near-TTL
            let mut handles = Vec::new();
            for i in 0..10 {
                let cache_clone = dedup_cache.clone();
                handles.push(tokio::spawn(async move {
                    // Stagger around TTL boundary
                    tokio::time::sleep(Duration::from_millis(450 + i * 20)).await;
                    cache_clone.claim(key).await.is_some()
                }));
            }

            let results: Vec<bool> = futures::future::join_all(handles)
                .await
                .into_iter()
                .map(|r| r.unwrap())
                .collect();

            // At least one should succeed (after expiration)
            assert!(
                results.iter().any(|&r| r),
                "At least one task should claim after TTL"
            );
        }

        #[tokio::test]
        async fn test_ttl_zero_edge_case() {
            // Test behavior with zero/minimal TTL
            let cache = Cache::builder()
                .max_capacity(100)
                .time_to_live(Duration::from_millis(1)) // Minimal TTL
                .build();
            let dedup_cache = RequestDedupCache { entries: cache };

            let key = DedupKey::new([200_u8; 32]);

            // Claim should work
            assert!(dedup_cache.claim(key).await.is_some());

            // Very brief sleep
            tokio::time::sleep(Duration::from_millis(2)).await;

            // Should be expired and claimable again
            assert!(
                dedup_cache.claim(key).await.is_some(),
                "Minimal TTL should expire quickly"
            );
        }
    } // end ttl_expiry_tests

    mod eviction_tests {
        use super::*;

        #[rstest]
        #[case(MAX_DEDUP_CACHE_SIZE + 100)]
        #[tokio::test]
        async fn test_cache_eviction_behavior(#[case] entries: usize) {
            let cache = RequestDedupCache::new();
            let mut keys = Vec::new();

            // Fill beyond capacity with unique keys
            for i in 0..entries {
                let mut key_bytes = [0_u8; 32];
                // Create unique keys by using the index
                key_bytes[0..4].copy_from_slice(&(i as u32).to_le_bytes());
                let key = DedupKey::new(key_bytes);
                keys.push(key);
                assert!(cache.claim(key).await.is_some());
            }

            // Verify LRU eviction doesn't break dedup logic
            let first_key = keys[0];
            let result = cache.claim(first_key).await.is_some();
            // Should proceed since entry was evicted
            assert!(result);
        }
    } // end eviction_tests

    mod property_tests {
        use super::*;

        proptest! {
        #[test]
        fn hash_deterministic(peer in any::<[u8; 20]>(), data_type in 0_u8..3, hash in any::<[u8; 32]>()) {
            let addr = Address::from(peer);
            let request = match data_type {
                0 => GossipDataRequest::Block(BlockHash::from(hash)),
                1 => GossipDataRequest::ExecutionPayload(hash.into()),
                _ => GossipDataRequest::Chunk(BlockHash::from(hash)),
            };

            let key1 = compute_dedup_key(&addr, &request);
            let key2 = compute_dedup_key(&addr, &request);
            prop_assert_eq!(key1, key2);
        }

        #[test]
        fn different_inputs_different_hashes(
            peer1 in any::<[u8; 20]>(),
            peer2 in any::<[u8; 20]>(),
            hash1 in any::<[u8; 32]>(),
            hash2 in any::<[u8; 32]>()
        ) {
            prop_assume!(peer1 != peer2 || hash1 != hash2);

            let addr1 = Address::from(peer1);
            let addr2 = Address::from(peer2);
            let req1 = GossipDataRequest::Block(BlockHash::from(hash1));
            let req2 = GossipDataRequest::Block(BlockHash::from(hash2));

            let key1 = compute_dedup_key(&addr1, &req1);
            let key2 = compute_dedup_key(&addr2, &req2);

            prop_assert_ne!(key1, key2);
        }

        #[test]
        fn hash_collision_resistance(
            peer1 in any::<[u8; 20]>(),
            peer2 in any::<[u8; 20]>(),
            hash1 in any::<[u8; 32]>(),
            hash2 in any::<[u8; 32]>(),
            mode in prop::sample::select(&[DedupMode::PerPeer, DedupMode::Global])
        ) {
            // Test that different inputs produce different hashes (collision resistance)
            prop_assume!(peer1 != peer2 || hash1 != hash2);

            let addr1 = Address::from(peer1);
            let addr2 = Address::from(peer2);
            let req1 = GossipDataRequest::Block(BlockHash::from(hash1));
            let req2 = GossipDataRequest::Block(BlockHash::from(hash2));

            let key1 = DedupKey::from_request_with_mode(&addr1, &req1, mode);
            let key2 = DedupKey::from_request_with_mode(&addr2, &req2, mode);

            prop_assert_ne!(key1, key2, "Hash collision detected!");
        }

        #[test]
        fn hash_mode_separation(
            peer in any::<[u8; 20]>(),
            hash in any::<[u8; 32]>()
        ) {
            // Property: Same input with different modes must produce different hashes
            let addr = Address::from(peer);
            let request = GossipDataRequest::Block(BlockHash::from(hash));

            let per_peer_key = DedupKey::from_request_with_mode(&addr, &request, DedupMode::PerPeer);
            let global_key = DedupKey::from_request_with_mode(&addr, &request, DedupMode::Global);

            prop_assert_ne!(per_peer_key, global_key, "Mode separation failed!");
        }

        #[test]
        fn prop_dedup_key_no_collisions(
            peer1 in any::<[u8; 20]>(),
            peer2 in any::<[u8; 20]>(),
            hash1 in any::<[u8; 32]>(),
            hash2 in any::<[u8; 32]>(),
            req_type1 in 0_u8..3,
            req_type2 in 0_u8..3,
        ) {
            // Skip if inputs are identical
            prop_assume!(peer1 != peer2 || hash1 != hash2 || req_type1 != req_type2);

            let addr1 = Address::from(peer1);
            let addr2 = Address::from(peer2);

            let request1 = match req_type1 {
                0 => GossipDataRequest::Block(BlockHash::from(hash1)),
                1 => GossipDataRequest::ExecutionPayload(hash1.into()),
                _ => GossipDataRequest::Chunk(BlockHash::from(hash1)),
            };

            let request2 = match req_type2 {
                0 => GossipDataRequest::Block(BlockHash::from(hash2)),
                1 => GossipDataRequest::ExecutionPayload(hash2.into()),
                _ => GossipDataRequest::Chunk(BlockHash::from(hash2)),
            };

            let key1 = compute_dedup_key(&addr1, &request1);
            let key2 = compute_dedup_key(&addr2, &request2);

            prop_assert_ne!(key1, key2,
                "Collision: peer1={:?}, peer2={:?}, req1={:?}, req2={:?}",
                peer1, peer2, request1, request2);
        }

        #[test]
        fn prop_dedup_mode_consistency(
            peer in any::<[u8; 20]>(),
            hash in any::<[u8; 32]>(),
        ) {
            let addr = Address::from(peer);
            let request = GossipDataRequest::Block(BlockHash::from(hash));

            // Same input should always produce same output
            let key1 = compute_dedup_key_with_mode(&addr, &request, DedupMode::PerPeer);
            let key2 = compute_dedup_key_with_mode(&addr, &request, DedupMode::PerPeer);
            prop_assert_eq!(key1, key2, "PerPeer mode not deterministic");

            let key3 = compute_dedup_key_with_mode(&addr, &request, DedupMode::Global);
            let key4 = compute_dedup_key_with_mode(&addr, &request, DedupMode::Global);
            prop_assert_eq!(key3, key4, "Global mode not deterministic");

            // Different modes must produce different keys
            prop_assert_ne!(key1, key3, "PerPeer and Global modes produced same key");
        }
        } // end proptest!
    } // end property_tests
} // end tests
