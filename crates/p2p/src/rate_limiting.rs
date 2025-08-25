use irys_types::Address;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, trace};

/// Record of data requests from a peer
#[derive(Debug, Clone)]
pub struct DataRequestRecord {
    /// Timestamp when the tracking window started (Unix timestamp in seconds)
    pub window_start: u64,
    /// Total number of requests in current window
    pub request_count: u32,
    /// Score points given in current window
    pub score_given: u32,
    /// Last request timestamp for deduplication
    pub last_request: u64,
    /// Consider a duplicate if within (milliseconds)
    pub duplicate_request_milliseconds: u128,
}

impl DataRequestRecord {
    pub(crate) fn now_as_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
    pub fn new(duplicate_request_milliseconds: u128) -> Self {
        let now = Self::now_as_secs();

        Self {
            window_start: now,
            request_count: 1,
            score_given: 0,
            last_request: now,
            duplicate_request_milliseconds,
        }
    }

    /// Check if the tracking window has expired (1 hour)
    pub fn is_window_expired(&self) -> bool {
        let now = Self::now_as_secs();
        now - self.window_start > 3_600_000 // 1 hour in milliseconds
    }

    /// Check if enough time has passed since last request (deduplication window)
    pub fn is_duplicate_request(&self) -> bool {
        let now = Self::now_as_secs();
        // Consider duplicate if within the configured milliseconds
        ((now - self.last_request) as u128) < self.duplicate_request_milliseconds
    }

    /// Reset for new tracking window
    pub fn reset_window(&mut self) {
        let now = Self::now_as_secs();

        self.window_start = now;
        self.request_count = 1;
        self.score_given = 0;
        self.last_request = now;
    }

    /// Update for new request
    pub fn update_request(&mut self) {
        let now = Self::now_as_secs();

        self.request_count += 1;
        self.last_request = now;
    }
}

/// Tracks data requests per peer to enforce score caps and prevent farming
#[derive(Clone, Debug)]
pub struct DataRequestTracker {
    /// Per-peer request history
    request_history: Arc<RwLock<HashMap<Address, DataRequestRecord>>>,
    /// Maximum score points a peer can gain per hour from data requests
    max_score_per_hour: u32,
    /// Maximum requests per hour before blocking
    max_requests_per_hour: u32,
    /// Interval for cleaning up old entries
    cleanup_interval: Duration,
    /// Last cleanup timestamp
    last_cleanup: Arc<RwLock<Instant>>,
}

impl DataRequestTracker {
    pub fn new() -> Self {
        Self {
            request_history: Arc::new(RwLock::new(HashMap::new())),
            max_score_per_hour: 5,      // Max 5 score points per hour
            max_requests_per_hour: 100, // Max 100 requests per hour
            cleanup_interval: Duration::from_secs(300), // Cleanup every 5 minutes
            last_cleanup: Arc::new(RwLock::new(Instant::now())),
        }
    }

    /// Check if score should be increased for this peer's data request
    /// Returns (should_update_score, should_serve_data)
    pub async fn check_request(
        &self,
        peer_address: &Address,
        duplicate_request_milliseconds: u128,
    ) -> (bool, bool) {
        // Perform cleanup if needed
        self.cleanup_if_needed().await;

        let mut history = self.request_history.write().await;

        // Get or create record for this peer
        let entry = history
            .entry(*peer_address)
            .or_insert_with(|| DataRequestRecord::new(duplicate_request_milliseconds));

        // If this is the first request ever for this peer, allow score update immediately
        let is_first_request = entry.request_count == 1 && entry.score_given == 0;

        // Check if tracking window expired (1 hour)
        if entry.is_window_expired() {
            debug!(
                "Request tracking window expired for peer {:?}, resetting",
                peer_address
            );
            entry.reset_window();
            entry.score_given = 1;
            return (true, true);
        }

        // Handle first request case
        if is_first_request {
            entry.score_given = 1;
            debug!(
                "First request from peer {:?}, allowing score update",
                peer_address
            );
            return (true, true);
        }

        // Check for duplicate request (deduplication)
        if entry.is_duplicate_request() {
            trace!(
                "Duplicate request from peer {:?} within deduplication window",
                peer_address
            );
            return (false, true); // Still serve data but don't update score
        }

        // Update request tracking
        entry.update_request();

        // Check if peer exceeded request limit
        if entry.request_count > self.max_requests_per_hour {
            debug!(
                "Peer {:?} exceeded request limit ({}/{})",
                peer_address, entry.request_count, self.max_requests_per_hour
            );
            return (false, false); // Don't serve data or update score
        }

        // Check if we can give more score
        let should_update_score = if entry.score_given < self.max_score_per_hour {
            entry.score_given += 1;
            debug!(
                "Peer {:?} score update allowed ({}/{})",
                peer_address, entry.score_given, self.max_score_per_hour
            );
            true
        } else {
            debug!(
                "Peer {:?} reached score cap for this hour ({}/{})",
                peer_address, entry.score_given, self.max_score_per_hour
            );
            false
        };

        (should_update_score, true)
    }

    /// Get request statistics for a peer
    pub async fn get_peer_stats(&self, peer_address: &Address) -> Option<DataRequestRecord> {
        let history = self.request_history.read().await;
        history.get(peer_address).cloned()
    }

    /// Cleanup expired entries to prevent memory growth
    async fn cleanup_expired_entries(&self) {
        let mut history = self.request_history.write().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Remove entries older than 2 hours
        let before = history.len();
        history.retain(|addr, record| {
            let age = now - record.window_start;
            if age > 7_200_000 {
                // 2 hours in milliseconds
                trace!("Removing expired tracking record for peer {:?}", addr);
                false
            } else {
                true
            }
        });

        if before != history.len() {
            debug!(
                "Cleaned up {} expired peer tracking records",
                before - history.len()
            );
        }
    }

    /// Check if cleanup is needed and perform it
    async fn cleanup_if_needed(&self) {
        let should_cleanup = {
            let last_cleanup = self.last_cleanup.read().await;
            last_cleanup.elapsed() > self.cleanup_interval
        };

        if should_cleanup {
            self.cleanup_expired_entries().await;
            *self.last_cleanup.write().await = Instant::now();
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
    use irys_types::Address;

    #[tokio::test]
    async fn test_data_request_tracker_score_limiting() {
        let tracker = DataRequestTracker::new();
        let peer_addr = Address::from([1_u8; 20]);

        // First 5 requests should allow score updates
        for i in 1..=5 {
            // Wait a bit to avoid deduplication window
            tokio::time::sleep(Duration::from_millis(11_000)).await;
            let (should_update, should_serve) =
                tracker.check_request(&peer_addr, 10_000_u128).await;
            assert!(should_serve, "Should serve data for request {}", i);
            if i == 1 {
                // First request always updates score
                assert!(should_update, "Should update score for first request");
            } else if i <= 5 {
                assert!(should_update, "Should update score for request {}", i);
            }
        }

        // 6th request should not update score but still serve
        tokio::time::sleep(Duration::from_millis(11_000)).await;
        let (should_update, should_serve) = tracker.check_request(&peer_addr, 10_000_u128).await;
        assert!(!should_update, "Should not update score after cap");
        assert!(should_serve, "Should still serve data after score cap");

        // Check stats
        let stats = tracker.get_peer_stats(&peer_addr).await.unwrap();
        assert_eq!(stats.score_given, 5);
        assert!(stats.request_count >= 6);
    }

    #[tokio::test]
    async fn test_data_request_deduplication() {
        let tracker = DataRequestTracker::new();
        let peer_addr = Address::from([2_u8; 20]);

        // First request
        let (should_update1, should_serve1) = tracker.check_request(&peer_addr, 10_000_u128).await;
        assert!(should_update1);
        assert!(should_serve1);

        // Immediate second request should be deduplicated
        let (should_update2, should_serve2) = tracker.check_request(&peer_addr, 10_000_u128).await;
        assert!(!should_update2, "Should not update score for duplicate");
        assert!(should_serve2, "Should still serve data for duplicate");

        // After deduplication window, should allow score update
        tokio::time::sleep(Duration::from_millis(11_000)).await;
        let (should_update3, should_serve3) = tracker.check_request(&peer_addr, 10_000_u128).await;
        assert!(should_update3, "Should update score after dedup window");
        assert!(should_serve3);
    }

    #[test]
    fn test_data_request_record_expiry() {
        let mut record = DataRequestRecord::new(10_00_u128);

        // Fresh record should not be expired
        assert!(!record.is_window_expired());

        // Simulate old record
        record.window_start = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(3_700_000); // Over 1 hour ago

        assert!(record.is_window_expired());

        // Reset should make it fresh again
        record.reset_window();
        assert!(!record.is_window_expired());
    }
}
