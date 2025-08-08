use irys_domain::{ChunkTimeRecord, CircularBuffer};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
pub enum BandwidthRating {
    Low,    // 0-40 MB/s of throughput
    Medium, // 40-80 MB/s of throughput
    High,   // >80 MB/s of throughput
}

#[derive(Debug)]
pub struct PeerStats {
    pub chunk_size: u64,
    pub baseline_concurrency: u32,
    pub current_max_concurrency: u32,
    pub max_concurrency: u32,
    pub bandwidth_rating: BandwidthRating,
    pub timeout: Duration,

    // Simplified tracking
    pub consecutive_failures: u32,
    pub total_failures: u64,
    pub active_requests: usize,

    // Bandwidth tracking for the last 5 min
    pub bandwidth_window: BandwidthWindow,

    // Track average chunk completion time for better concurrency calculations
    pub completion_time_samples: CircularBuffer<Duration>,
}

impl PeerStats {
    pub fn new(target_bandwidth_mbps: usize, chunk_size: u64, timeout: Duration) -> Self {
        // Assume a gigabit connection (1000 Mbps = 125 MB/s max)
        let connection_limit_mbps = 125;
        let effective_target = target_bandwidth_mbps.min(connection_limit_mbps);

        // Calculate what fraction of the connection bandwidth we want to use
        let bandwidth_fraction = effective_target as f64 / connection_limit_mbps as f64;

        // Scale concurrency based on bandwidth fraction
        let base_concurrency = (bandwidth_fraction * 24.0).ceil() as u32; // Scale from 1-24 connections
        let base_concurrency = base_concurrency.clamp(2, 24); // Minimum 2, maximum 24

        // Small chunks might need more concurrency due to per-request overhead
        // (chunks smaller than 245KiB only used in testing)
        let chunk_multiplier = if chunk_size < 1024 { 1.5 } else { 1.0 };

        let max_concurrency =
            ((base_concurrency as f64 * chunk_multiplier).round() as u32).clamp(1, 32);

        let baseline_concurrency = (max_concurrency / 2).max(1);

        Self {
            chunk_size,
            baseline_concurrency,
            current_max_concurrency: baseline_concurrency,
            max_concurrency,
            bandwidth_rating: BandwidthRating::Medium,
            timeout,
            consecutive_failures: 0,
            total_failures: 0,
            active_requests: 0,
            // Use 300-second window (5 minutes) to capture bandwidth throughput
            bandwidth_window: BandwidthWindow::new(Duration::from_secs(300)),
            completion_time_samples: CircularBuffer::new(50),
        }
    }

    /// Calculate the amount of concurrency that can be safely added beyond the
    /// current_max_concurrency based on recent bandwidth measurements
    pub fn available_concurrency(&self) -> u32 {
        let expected_bytes_per_sec = self.expected_bandwidth_bps();
        let observed_bytes_per_sec = self.short_term_bandwidth_bps() as f64;

        let total_capacity = if observed_bytes_per_sec > 0.0 {
            // If peer is performing better than expected, allow more concurrency
            let performance_ratio = observed_bytes_per_sec / expected_bytes_per_sec;
            let base_capacity = self.baseline_concurrency as f64;
            (base_capacity * performance_ratio).min(self.max_concurrency as f64) as u32
        } else {
            // No observed data yet, use baseline capacity
            self.baseline_concurrency
        };

        // Available = Total capacity - Currently active requests
        total_capacity.saturating_sub(self.active_requests as u32)
    }

    pub fn expected_bandwidth_bps(&self) -> f64 {
        let time_seconds = self.average_completion_time().as_secs_f64();
        if time_seconds > 0.0 {
            self.chunk_size as f64 / time_seconds
        } else {
            0.0
        }
    }

    pub fn record_request_started(&mut self) {
        self.active_requests += 1;
    }

    pub fn record_request_completed(&mut self, chunk_time_record: ChunkTimeRecord) {
        self.completion_time_samples
            .push(chunk_time_record.duration);

        self.active_requests -= 1;
        self.consecutive_failures = 0;

        // Add bytes to the bandwidth window
        self.bandwidth_window.add_bytes(self.chunk_size);
    }

    pub fn record_request_failed(&mut self) {
        self.active_requests = self.active_requests.saturating_sub(1);
        self.consecutive_failures += 1;
        self.total_failures += 1;
    }

    pub fn average_completion_time(&self) -> Duration {
        if self.completion_time_samples.is_empty() {
            return Duration::from_millis(100); // Default to 100ms as a baseline
        }

        let total_millis: u64 = self
            .completion_time_samples
            .iter()
            .map(|d| d.as_millis() as u64)
            .sum();

        let average_millis = total_millis / self.completion_time_samples.len() as u64;
        Duration::from_millis(average_millis)
    }

    pub fn health_score(&self) -> f64 {
        let mut score = 0.5;

        // Failure impact (-0.5 to +0.2) - most critical factor
        if self.consecutive_failures == 0 {
            score += 0.2; // Strong bonus for reliability
        } else {
            score -= (self.consecutive_failures as f64 * 0.15).min(0.5);
        }

        // Latency-based scoring
        let avg_latency = self.average_completion_time();

        // Better latency = higher score
        if avg_latency < Duration::from_millis(250) {
            score += 0.3;
        } else if avg_latency < Duration::from_millis(500) {
            score += 0.15;
        } else if avg_latency > Duration::from_secs(1) {
            score -= 0.2;
        }

        score.clamp(0.0, 1.0)
    }

    /// Get short-term bandwidth throughput (10s window)
    pub fn short_term_bandwidth_bps(&self) -> u64 {
        self.bandwidth_window
            .bytes_per_second_in_window(Duration::from_secs(10))
    }

    /// Get medium-term bandwidth throughput (30s window)
    pub fn medium_term_bandwidth_bps(&self) -> u64 {
        self.bandwidth_window
            .bytes_per_second_in_window(Duration::from_secs(30))
    }

    /// Get long-term bandwidth throughput (300s window)
    pub fn long_term_bandwidth_bps(&self) -> u64 {
        self.bandwidth_window
            .bytes_per_second_in_window(Duration::from_secs(300))
    }

    /// Check if bandwidth throughput is currently trending upward
    pub fn is_throughput_improving(&self) -> bool {
        let short = self.short_term_bandwidth_bps();
        let medium = self.medium_term_bandwidth_bps();
        let long = self.long_term_bandwidth_bps();

        short > medium && medium >= long && short > 0
    }

    /// Check if bandwidth throughput is stable (not fluctuating much)
    pub fn is_throughput_stable(&self) -> bool {
        let short = self.short_term_bandwidth_bps();
        let medium = self.medium_term_bandwidth_bps();

        if medium == 0 {
            return false;
        }

        let variance_ratio = (short as f64 - medium as f64).abs() / medium as f64;
        variance_ratio < 0.15 // Within 15% is considered stable
    }
}

#[derive(Debug)]
pub struct BandwidthWindow {
    buckets: VecDeque<(Instant, u64)>, // (timestamp, bytes)
    bucket_duration: Duration,         // Duration per bucket (e.g., 1 second)
    window_duration: Duration,         // Total window duration (e.g., 10 seconds)
}

impl BandwidthWindow {
    pub fn new(window_duration: Duration) -> Self {
        // Use 1-second buckets for good granularity and intuitive behavior
        let bucket_duration = Duration::from_secs(1);
        let bucket_count = window_duration.as_secs().max(1) as usize;

        Self {
            buckets: VecDeque::with_capacity(bucket_count + 2), // +2 for safety margin
            bucket_duration,
            window_duration,
        }
    }

    pub fn add_bytes(&mut self, bytes: u64) {
        let now = Instant::now();

        // Remove expired buckets from the front
        while let Some((timestamp, _)) = self.buckets.front() {
            if now.duration_since(*timestamp) > self.window_duration {
                self.buckets.pop_front();
            } else {
                break;
            }
        }

        // Add to current bucket or create new one
        if let Some((last_timestamp, last_bytes)) = self.buckets.back_mut() {
            if now.duration_since(*last_timestamp) < self.bucket_duration {
                // Add to current bucket (within the same 1-second period)
                *last_bytes += bytes;
            } else {
                // Create new bucket (new 1-second period)
                self.buckets.push_back((now, bytes));
            }
        } else {
            // First bucket
            self.buckets.push_back((now, bytes));
        }
    }

    /// Get bytes per second for the full window duration
    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second_in_window(self.window_duration)
    }

    /// Get bytes per second for the specified time window (must be <= window_duration)
    pub fn bytes_per_second_in_window(&self, window: Duration) -> u64 {
        if self.buckets.is_empty() {
            return 0;
        }

        let now = Instant::now();
        let cutoff_time = now.checked_sub(window.min(self.window_duration)).unwrap();

        // Sum bytes from buckets within the specified window
        let total_bytes: u64 = self
            .buckets
            .iter()
            .filter(|(timestamp, _)| *timestamp >= cutoff_time)
            .map(|(_, bytes)| bytes)
            .sum();

        // Find the oldest bucket within our window for elapsed time calculation
        if let Some((oldest_timestamp, _)) = self
            .buckets
            .iter()
            .find(|(timestamp, _)| *timestamp >= cutoff_time)
        {
            let elapsed = now.duration_since(*oldest_timestamp);
            let elapsed_secs = elapsed.as_secs_f64();

            if elapsed_secs > 0.001 {
                return (total_bytes as f64 / elapsed_secs) as u64;
            }
        }

        0
    }
}
