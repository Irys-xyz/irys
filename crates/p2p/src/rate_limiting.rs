use dashmap::DashMap;
use irys_types::IrysPeerId;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

// Constants for rate limiting configuration
const WINDOW_DURATION: Duration = Duration::from_secs(60); // 1 minute window
const MAX_SCORE_PER_MINUTE: u32 = 5; // Max score points per minute
const MAX_REQUESTS_PER_MINUTE: u32 = 100; // Max requests per minute
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60); // Cleanup every minute
const ENTRY_EXPIRY: Duration = Duration::from_secs(120); // 2 minutes

/// Result of checking a data request for rate limiting and scoring
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCheckResult {
    /// Grant score and serve data (normal request)
    GrantScoreAndServe,
    /// Serve data but don't grant score (duplicate or score cap reached)
    ServeOnly,
    /// Block request entirely (rate limit exceeded)
    BlockRequest,
}

impl RequestCheckResult {
    /// Check if the request should be served
    pub fn should_serve(&self) -> bool {
        matches!(self, Self::GrantScoreAndServe | Self::ServeOnly)
    }

    /// Check if score should be updated
    pub fn should_update_score(&self) -> bool {
        matches!(self, Self::GrantScoreAndServe)
    }
}

/// Record of data requests from a peer
#[derive(Debug, Clone)]
pub struct DataRequestRecord {
    /// When the tracking window started (monotonic clock).
    pub window_start: Instant,
    /// Total number of requests in current window
    pub request_count: u32,
    /// Score points given in current window
    pub score_given: u32,
    /// Last request timestamp for deduplication
    pub last_request: Instant,
    /// Consider a duplicate if within this duration
    pub duplicate_request_window: Duration,
}

impl DataRequestRecord {
    pub fn new(duplicate_request_window: Duration) -> Self {
        let now = Instant::now();

        Self {
            window_start: now,
            request_count: 1,
            score_given: 0,
            last_request: now,
            duplicate_request_window,
        }
    }

    /// Check if the tracking window has expired
    pub fn is_window_expired(&self) -> bool {
        self.window_start.elapsed() > WINDOW_DURATION
    }

    /// Check if enough time has passed since last request (deduplication window)
    pub fn is_duplicate_request(&self) -> bool {
        self.last_request.elapsed() < self.duplicate_request_window
    }

    /// Reset for new tracking window
    pub fn reset_window(&mut self) {
        let now = Instant::now();

        self.window_start = now;
        self.request_count = 1;
        self.score_given = 0;
        self.last_request = now;
    }

    /// Update for new request
    pub fn update_request(&mut self) {
        self.request_count += 1;
        self.last_request = Instant::now();
    }
}

/// Tracks data requests per peer to enforce score caps and prevent farming
#[derive(Clone, Debug)]
pub struct DataRequestTracker {
    /// Per-peer request history
    request_history: Arc<DashMap<IrysPeerId, DataRequestRecord>>,
    /// Maximum score points a peer can gain per minute from data requests
    max_score_per_minute: u32,
    /// Maximum requests per minute before blocking
    max_requests_per_minute: u32,
    /// Interval for cleaning up old entries
    cleanup_interval: Duration,
    /// Last cleanup time (monotonic)
    last_cleanup: Arc<Mutex<Instant>>,
}

impl DataRequestTracker {
    pub fn new() -> Self {
        Self {
            request_history: Arc::new(DashMap::new()),
            max_score_per_minute: MAX_SCORE_PER_MINUTE,
            max_requests_per_minute: MAX_REQUESTS_PER_MINUTE,
            cleanup_interval: CLEANUP_INTERVAL,
            last_cleanup: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Check if score should be increased for this peer's data request
    pub fn check_request(
        &self,
        peer_id: &IrysPeerId,
        duplicate_request_milliseconds: u128,
    ) -> RequestCheckResult {
        // Perform cleanup if needed
        self.cleanup_if_needed();

        let duplicate_request_window = Duration::from_millis(
            u64::try_from(duplicate_request_milliseconds)
                .expect("duplicate_request_milliseconds out of range for u64"),
        );

        // Get or create record for this peer
        let mut entry = self
            .request_history
            .entry(*peer_id)
            .or_insert_with(|| DataRequestRecord::new(duplicate_request_window));

        // If this is the first request ever for this peer, allow score update immediately
        let is_first_request = entry.request_count == 1 && entry.score_given == 0;

        // Check if tracking window expired
        if entry.is_window_expired() {
            debug!(
                "Request tracking window expired for peer {:?}, resetting",
                peer_id
            );
            entry.reset_window();
            entry.score_given = 1;
            return RequestCheckResult::GrantScoreAndServe;
        }

        // Handle first request case
        if is_first_request {
            entry.score_given = 1;
            debug!(
                "First request from peer {:?}, allowing score update",
                peer_id
            );
            return RequestCheckResult::GrantScoreAndServe;
        }

        // Check for duplicate request (deduplication)
        if entry.is_duplicate_request() {
            trace!(
                "Duplicate request from peer {:?} within deduplication window",
                peer_id
            );
            return RequestCheckResult::ServeOnly; // Still serve data but don't update score
        }

        // Update request tracking
        entry.update_request();

        // Check if peer exceeded request limit
        if entry.request_count > self.max_requests_per_minute {
            debug!(
                "Peer {:?} exceeded request limit ({}/{})",
                peer_id, entry.request_count, self.max_requests_per_minute
            );
            return RequestCheckResult::BlockRequest; // Don't serve data or update score
        }

        // Check if we can give more score
        let should_update_score = if entry.score_given < self.max_score_per_minute {
            entry.score_given += 1;
            debug!(
                "Peer {:?} score update allowed ({}/{})",
                peer_id, entry.score_given, self.max_score_per_minute
            );
            true
        } else {
            debug!(
                "Peer {:?} reached score cap for this minute ({}/{})",
                peer_id, entry.score_given, self.max_score_per_minute
            );
            false
        };

        if should_update_score {
            RequestCheckResult::GrantScoreAndServe
        } else {
            RequestCheckResult::ServeOnly
        }
    }

    /// Get request statistics for a peer
    pub fn get_peer_stats(&self, peer_id: &IrysPeerId) -> Option<DataRequestRecord> {
        self.request_history.get(peer_id).map(|entry| entry.clone())
    }

    /// Cleanup expired entries to prevent memory growth
    fn cleanup_expired_entries(&self) {
        let _before = self.request_history.len();
        let mut removed_count = 0;

        // Collect keys to remove (can't remove while iterating)
        let mut keys_to_remove = Vec::new();
        for entry in self.request_history.iter() {
            if entry.window_start.elapsed() > ENTRY_EXPIRY {
                keys_to_remove.push(*entry.key());
            }
        }

        // Remove expired entries
        for key in keys_to_remove {
            if self.request_history.remove(&key).is_some() {
                trace!("Removing expired tracking record for peer {:?}", key);
                removed_count += 1;
            }
        }

        if removed_count > 0 {
            debug!("Cleaned up {} expired peer tracking records", removed_count);
        }
    }

    /// Check if cleanup is needed and perform it
    fn cleanup_if_needed(&self) {
        let now = Instant::now();
        let Ok(mut last) = self.last_cleanup.lock() else {
            return;
        };
        if now.duration_since(*last) > self.cleanup_interval {
            *last = now;
            drop(last);
            self.cleanup_expired_entries();
        }
    }
}

impl Default for DataRequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{IrysAddress, IrysPeerId};
    use rstest::rstest;

    const TEST_DEDUP_WINDOW_MS: u128 = 10_000;
    const TEST_SLEEP_MS: u64 = 11_000;
    #[tokio::test]
    async fn slow_test_data_request_tracker_score_limiting() {
        let tracker = DataRequestTracker::new();
        let peer_id = IrysPeerId::from(IrysAddress::from([1_u8; 20]));

        // First 5 requests should allow score updates
        for i in 1..=5 {
            // Wait a bit to avoid deduplication window
            tokio::time::sleep(Duration::from_millis(TEST_SLEEP_MS)).await;
            let result = tracker.check_request(&peer_id, TEST_DEDUP_WINDOW_MS);
            assert!(result.should_serve(), "Should serve data for request {}", i);
            if i == 1 {
                // First request always updates score
                assert!(
                    result.should_update_score(),
                    "Should update score for first request"
                );
            } else if i <= 5 {
                assert!(
                    result.should_update_score(),
                    "Should update score for request {}",
                    i
                );
            }
        }

        // Nth request should not update score but still serve
        tokio::time::sleep(Duration::from_millis(TEST_SLEEP_MS)).await;
        let result = tracker.check_request(&peer_id, TEST_DEDUP_WINDOW_MS);
        assert!(
            !result.should_update_score(),
            "Should not update score after cap"
        );
        assert!(
            result.should_serve(),
            "Should still serve data after score cap"
        );

        // Check stats
        let stats = tracker.get_peer_stats(&peer_id).unwrap();
        assert_eq!(stats.score_given, 5);
        assert!(stats.request_count >= 6);
    }

    #[tokio::test]
    async fn test_data_request_deduplication() {
        let tracker = DataRequestTracker::new();
        let peer_id = IrysPeerId::from(IrysAddress::from([2_u8; 20]));

        // First request
        let result1 = tracker.check_request(&peer_id, TEST_DEDUP_WINDOW_MS);
        assert!(result1.should_update_score());
        assert!(result1.should_serve());

        // Immediate second request should be deduplicated
        let result2 = tracker.check_request(&peer_id, TEST_DEDUP_WINDOW_MS);
        assert!(
            !result2.should_update_score(),
            "Should not update score for duplicate"
        );
        assert!(
            result2.should_serve(),
            "Should still serve data for duplicate"
        );

        // After deduplication window, should allow score update
        tokio::time::sleep(Duration::from_millis(TEST_SLEEP_MS)).await;
        let result3 = tracker.check_request(&peer_id, TEST_DEDUP_WINDOW_MS);
        assert!(
            result3.should_update_score(),
            "Should update score after dedup window"
        );
        assert!(result3.should_serve());
    }

    #[test]
    fn test_data_request_record_expiry() {
        let mut record = DataRequestRecord::new(Duration::from_millis(TEST_DEDUP_WINDOW_MS as u64));

        // Fresh record should not be expired
        assert!(!record.is_window_expired());

        // Simulate old record (back-date past the window)
        record.window_start = Instant::now()
            .checked_sub(WINDOW_DURATION + Duration::from_secs(10))
            .expect("monotonic clock must be at least WINDOW_DURATION+10s past zero in tests");

        assert!(record.is_window_expired());

        // Reset should make it fresh again
        record.reset_window();
        assert!(!record.is_window_expired());
    }

    #[rstest]
    #[case::at_limit(100, true)]
    #[case::one_over_limit(101, false)]
    #[case::well_over_limit(110, false)]
    fn check_request_blocks_after_rate_limit_exceeded(
        #[case] total_requests: u32,
        #[case] last_request_should_serve: bool,
    ) {
        let tracker = DataRequestTracker::new();
        let peer_id = IrysPeerId::from(IrysAddress::from([3_u8; 20]));

        let mut last_result = tracker.check_request(&peer_id, 0);
        for _ in 1..total_requests {
            last_result = tracker.check_request(&peer_id, 0);
        }

        assert_eq!(
            last_result.should_serve(),
            last_request_should_serve,
            "After {} requests, should_serve should be {}",
            total_requests,
            last_request_should_serve
        );

        if !last_request_should_serve {
            assert_eq!(last_result, RequestCheckResult::BlockRequest,);
        }
    }
}
