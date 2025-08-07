#[cfg(test)]
use actix_rt::test;
use irys_actors::peer_stats::{BandwidthRating, BandwidthWindow, PeerStats};
use irys_domain::ChunkTimeRecord;
use irys_types::PartitionChunkOffset;
use std::time::{Duration, Instant};

// Configurable chunk size for tests
const CHUNK_SIZE: u64 = 32; // 32 bytes for sanity checks (normally 256KiB)
const TARGET_BANDWIDTH_MBPS: usize = 50; // 50 MB/s target
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

fn create_test_peer_stats() -> PeerStats {
    PeerStats::new(TARGET_BANDWIDTH_MBPS, CHUNK_SIZE, DEFAULT_TIMEOUT)
}

fn create_chunk_time_record(duration_ms: u64) -> ChunkTimeRecord {
    ChunkTimeRecord {
        duration: Duration::from_millis(duration_ms),
        chunk_offset: PartitionChunkOffset::from(0),
        start_time: Instant::now(),
        completion_time: Instant::now(),
    }
}

#[test]
async fn test_new_peer_stats_initialization() {
    let stats = create_test_peer_stats();

    assert_eq!(stats.chunk_size, CHUNK_SIZE);
    assert_eq!(stats.bandwidth_rating, BandwidthRating::Medium);
    assert_eq!(stats.timeout, DEFAULT_TIMEOUT);
    assert_eq!(stats.consecutive_failures, 0);
    assert_eq!(stats.active_requests, 0);
    assert!(stats.baseline_concurrency > 0);
    assert!(stats.current_max_concurrency <= stats.max_concurrency);
    assert_eq!(stats.average_completion_time(), Duration::from_millis(100));
}

#[test]
async fn test_request_lifecycle_tracking() {
    let mut stats = create_test_peer_stats();

    // Initially no active requests
    assert_eq!(stats.active_requests, 0);

    // Start a request
    stats.record_request_started();
    assert_eq!(stats.active_requests, 1);

    // Start another request
    stats.record_request_started();
    assert_eq!(stats.active_requests, 2);

    // Complete one request
    let chunk_record = create_chunk_time_record(150);
    stats.record_request_completed(chunk_record);
    assert_eq!(stats.active_requests, 1);
    assert_eq!(stats.consecutive_failures, 0);

    // Fail the other request
    stats.record_request_failed();
    assert_eq!(stats.active_requests, 0);
    assert_eq!(stats.consecutive_failures, 1);
}

#[test]
async fn test_consecutive_failures_tracking() {
    let mut stats = create_test_peer_stats();

    // Multiple failures should increment counter
    stats.record_request_failed();
    assert_eq!(stats.consecutive_failures, 1);

    stats.record_request_failed();
    assert_eq!(stats.consecutive_failures, 2);

    stats.record_request_failed();
    assert_eq!(stats.consecutive_failures, 3);

    // Successful completion should reset failures
    let chunk_record = create_chunk_time_record(100);
    stats.record_request_started();
    stats.record_request_completed(chunk_record);
    assert_eq!(stats.consecutive_failures, 0);
}

#[test]
async fn test_completion_time_tracking() {
    let mut stats = create_test_peer_stats();

    // Record several completion times
    let times = vec![100, 150, 200, 120, 180];
    for time in &times {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(*time);
        stats.record_request_completed(chunk_record);
    }

    // Average should be calculated correctly
    let expected_avg = times.iter().sum::<u64>() / times.len() as u64;
    assert_eq!(
        stats.average_completion_time().as_millis() as u64,
        expected_avg
    );

    // Completion time samples should contain the recorded times
    assert_eq!(stats.completion_time_samples.len(), times.len());
}

#[test]
async fn test_bandwidth_window_functionality() {
    let mut window = BandwidthWindow::new(Duration::from_secs(2));

    // Initially no bytes transferred
    assert_eq!(window.bytes_per_second(), 0);

    // Add some bytes
    window.add_bytes(CHUNK_SIZE);

    // Give a small delay to ensure elapsed time calculation works
    actix_rt::time::sleep(Duration::from_millis(100)).await;

    // Should have some throughput now
    let throughput = window.bytes_per_second();
    assert!(throughput > 0);

    // Add more bytes within the window
    window.add_bytes(CHUNK_SIZE * 2);

    // Throughput should reflect total bytes added
    let new_throughput = window.bytes_per_second();
    assert!(new_throughput >= throughput);
}

#[test]
async fn test_bandwidth_window_expiration() {
    let mut window = BandwidthWindow::new(Duration::from_millis(500));

    // Add bytes
    window.add_bytes(CHUNK_SIZE);

    // Give a small delay to establish elapsed time
    actix_rt::time::sleep(Duration::from_millis(100)).await;

    let initial_throughput = window.bytes_per_second();
    assert!(initial_throughput > 0);

    // Wait for window to expire
    actix_rt::time::sleep(Duration::from_millis(600)).await;

    // Old data should have expired, throughput should be 0 or very low
    let expired_throughput = window.bytes_per_second();
    assert!(expired_throughput < initial_throughput);

    // Adding new bytes should create fresh throughput measurement
    window.add_bytes(CHUNK_SIZE * 2);

    // Give small delay for calculation
    actix_rt::time::sleep(Duration::from_millis(50)).await;

    let fresh_throughput = window.bytes_per_second();
    assert!(fresh_throughput > 0);
}

#[test]
async fn test_bandwidth_window_buckets() {
    let mut window = BandwidthWindow::new(Duration::from_secs(3));

    // Add bytes in different time periods
    window.add_bytes(CHUNK_SIZE);

    // Wait for new bucket
    actix_rt::time::sleep(Duration::from_millis(1100)).await;
    window.add_bytes(CHUNK_SIZE * 2);

    // Wait for another new bucket
    actix_rt::time::sleep(Duration::from_millis(1100)).await;
    window.add_bytes(CHUNK_SIZE);

    // Should have throughput that accounts for all buckets within the window
    let total_throughput = window.bytes_per_second();
    assert!(total_throughput > 0);

    // The throughput should be roughly (CHUNK_SIZE + CHUNK_SIZE*2 + CHUNK_SIZE) / time_elapsed
    // but we'll just verify it's reasonable
    assert!(total_throughput >= CHUNK_SIZE); // At minimum should be chunk_size per second
}

#[test]
async fn test_health_score_calculation() {
    let mut stats = create_test_peer_stats();

    // Fresh stats should have decent health score
    let initial_score = stats.health_score();
    assert!(initial_score > 0.5);

    // Successful completions should improve health
    for _ in 0..3 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(100);
        stats.record_request_completed(chunk_record);
    }
    let good_score = stats.health_score();
    assert!(good_score >= initial_score);

    // Failures should decrease health score
    stats.record_request_failed();
    stats.record_request_failed();
    stats.record_request_failed();
    let bad_score = stats.health_score();
    assert!(bad_score < good_score);

    // Health score should be clamped between 0.0 and 1.0
    assert!((0.0..=1.0).contains(&bad_score));
}

#[test]
async fn test_throughput_stability_detection() {
    let mut stats = create_test_peer_stats();

    // Initially should not be stable (no data)
    assert!(!stats.is_throughput_stable());

    // Add consistent throughput data with some spacing to establish windows
    for i in 0..10 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(100); // Consistent timing
        stats.record_request_completed(chunk_record);

        // Add delay every few iterations to establish different time windows
        if i % 3 == 0 {
            actix_rt::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // After establishing some data across time windows, check stability
    let is_stable = stats.is_throughput_stable();
    assert!(is_stable);
}

#[test]
async fn test_throughput_improvement_detection() {
    let mut stats = create_test_peer_stats();

    // Initially should not be improving (no baseline)
    assert!(!stats.is_throughput_improving());

    // Establish a baseline in the medium/long-term windows with slower completions
    // We need to space these out over a longer period to ensure they establish
    // a different baseline than the short-term window
    for _ in 0..5 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(200); // Slower
        stats.record_request_completed(chunk_record);

        // Space out requests significantly to establish different time windows
        // We need gaps large enough that the later "fast" requests won't be
        // averaged with these early "slow" requests in the short-term window
        actix_rt::time::sleep(Duration::from_millis(2500)).await; // 2.5 seconds between requests
    }

    // At this point we've taken ~12.5 seconds, so the medium-term window
    // should have a good baseline of slow requests

    // Now add much faster completions to the short-term window
    // Do these quickly so they establish a different short-term average
    for _ in 0..5 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(50); // Much faster
        stats.record_request_completed(chunk_record);

        // Small delay between fast requests
        actix_rt::time::sleep(Duration::from_millis(100)).await;
    }

    // Give a moment for the windows to update
    actix_rt::time::sleep(Duration::from_millis(100)).await;

    // Debug the actual throughput values
    let short = stats.short_term_bandwidth_bps();
    let medium = stats.medium_term_bandwidth_bps();
    let long = stats.long_term_bandwidth_bps();

    println!("Throughput values:");
    println!("  Short term (10s): {} bytes/s", short);
    println!("  Medium term (30s): {} bytes/s", medium);
    println!("  Long term (300s): {} bytes/s", long);

    // Check improvement detection
    let is_improving = stats.is_throughput_improving();
    println!("  Is improving: {}", is_improving);

    // Now we should see improvement since recent performance (fast requests)
    // should be better than the earlier baseline (slow requests)
    assert!(is_improving);
}

#[test]
async fn test_available_concurrency_calculation() {
    let mut stats = create_test_peer_stats();

    // Initially should have some available concurrency
    let initial_available = stats.available_concurrency();
    assert!(initial_available > 0);

    // Record some activity to establish bandwidth usage
    for _ in 0..3 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(100);
        stats.record_request_completed(chunk_record);
    }

    let available_after_activity = stats.available_concurrency();

    // Available concurrency should be a reasonable number
    assert!(available_after_activity <= stats.max_concurrency);
}

#[test]
async fn test_bandwidth_rating_initialization() {
    // Test different bandwidth targets affect initialization
    let low_bandwidth_stats = PeerStats::new(20, CHUNK_SIZE, DEFAULT_TIMEOUT);
    let high_bandwidth_stats = PeerStats::new(100, CHUNK_SIZE, DEFAULT_TIMEOUT);

    // Both should start with Medium rating by default
    assert_eq!(
        low_bandwidth_stats.bandwidth_rating,
        BandwidthRating::Medium
    );
    assert_eq!(
        high_bandwidth_stats.bandwidth_rating,
        BandwidthRating::Medium
    );

    // With very small test chunks, concurrency might be capped by overhead calculations
    // Let's verify they're at least not smaller than expected
    assert!(high_bandwidth_stats.max_concurrency >= low_bandwidth_stats.max_concurrency);

    // Test with realistic chunk sizes to verify the logic works properly
    const REALISTIC_CHUNK_SIZE: u64 = 256 * 1024; // 256 KiB
    let low_realistic = PeerStats::new(20, REALISTIC_CHUNK_SIZE, DEFAULT_TIMEOUT);
    let high_realistic = PeerStats::new(100, REALISTIC_CHUNK_SIZE, DEFAULT_TIMEOUT);

    // With realistic chunk sizes, high bandwidth should definitely have higher concurrency
    assert!(high_realistic.max_concurrency > low_realistic.max_concurrency);
}

#[test]
async fn test_completion_time_samples_circular_buffer() {
    let mut stats = create_test_peer_stats();

    // Fill beyond the circular buffer capacity (50 samples)
    for i in 0..60 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(100 + i);
        stats.record_request_completed(chunk_record);
    }

    // Should not exceed the buffer size
    assert_eq!(stats.completion_time_samples.len(), 50);
}

#[test]
async fn test_expected_bandwidth_usage() {
    let mut stats = create_test_peer_stats();

    // Record some completion times to establish a realistic average
    let completion_times = vec![80, 120, 100, 90, 110]; // milliseconds
    for time_ms in completion_times {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(time_ms);
        stats.record_request_completed(chunk_record);
    }

    let expected = stats.expected_bandwidth_bps();
    assert!(expected > 0.0);

    // Should be based on chunk size and average completion time
    let manual_calc = stats.chunk_size as f64 / stats.average_completion_time().as_secs_f64();
    assert_eq!(expected, manual_calc);

    // Verify the calculation makes sense - with small test chunks and ~100ms completion time
    // we should get a reasonable bandwidth usage value
    assert!(expected >= 0.0); // At minimum should be non-negative
}

#[test]
async fn test_large_chunk_size_scenario() {
    const LARGE_CHUNK_SIZE: u64 = 256 * 1024; // 256 KiB - realistic size
    let mut stats = PeerStats::new(TARGET_BANDWIDTH_MBPS, LARGE_CHUNK_SIZE, DEFAULT_TIMEOUT);

    // Should handle large chunk sizes appropriately
    assert_eq!(stats.chunk_size, LARGE_CHUNK_SIZE);
    assert!(stats.baseline_concurrency > 0);
    assert!(stats.max_concurrency > 0);

    // Record some realistic completion times for large chunks
    let realistic_completion_times = vec![2000, 2500, 1800, 2200, 2100]; // 2-2.5 seconds for 256KB chunks
    for time_ms in realistic_completion_times {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(time_ms);
        stats.record_request_completed(chunk_record);
    }

    // Now bandwidth calculations should work with realistic completion times
    let expected_usage = stats.expected_bandwidth_bps();
    assert!(expected_usage > 0.0);

    // Verify the average completion time was updated
    assert!(stats.average_completion_time().as_millis() > 100); // Should be much higher than default 100ms
    assert!(stats.average_completion_time().as_millis() < 3000); // But reasonable for large chunks

    // Available concurrency should be calculated based on realistic bandwidth usage
    let available_concurrency = stats.available_concurrency();
    assert!(available_concurrency <= stats.max_concurrency);
}

#[test]
async fn test_multi_window_bandwidth_tracking() {
    let mut stats = create_test_peer_stats();

    // Add data to establish different bandwidth measurements across windows
    for _ in 0..10 {
        stats.record_request_started();
        let chunk_record = create_chunk_time_record(100);
        stats.record_request_completed(chunk_record);

        // Add small delays to distribute across time
        actix_rt::time::sleep(Duration::from_millis(50)).await;
    }

    // All bandwidth windows should have some throughput
    let short_term = stats.short_term_bandwidth_bps();
    let medium_term = stats.medium_term_bandwidth_bps();
    let long_term = stats.long_term_bandwidth_bps();

    // All should be non-zero if we have recent activity
    assert!(short_term > 0);
    assert!(medium_term > 0);
    assert!(long_term > 0);
}
