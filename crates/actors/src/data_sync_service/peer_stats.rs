use irys_domain::{ChunkTimeRecord, CircularBuffer};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Halve `capacity` once per consecutive failure. A zero capacity stays zero.
/// Any positive capacity stays at least 1, so one timed-out peer is retried.
fn penalized_allowance(capacity: u32, consecutive_failures: u32) -> u32 {
    if capacity == 0 || consecutive_failures == 0 {
        return capacity;
    }
    (capacity >> consecutive_failures.min(31)).max(1)
}

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
    /// Ceiling for [`Self::max_concurrency`]. This is the value computed in
    /// [`Self::new`]: the bandwidth-scaled count, or the 10 MiB sweep span
    /// when that is larger. `baseline_concurrency` can be 1 while the sweep
    /// span is 64, so a small multiple of the baseline would cut that span.
    pub concurrency_ceiling: u32,
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

        // One 10 MiB sweep is the disk-lane packed-run cap, so it is the
        // concurrency ceiling. A fresh peer starts at half the bandwidth-scaled
        // count and ramps toward that ceiling only as bytes arrive. Tiny test
        // chunks keep the bandwidth-scaled count.
        const SWEEP_SPAN_BYTES: u64 = 10 * 1024 * 1024;
        let span_floor = if chunk_size >= 1024 {
            u32::try_from((SWEEP_SPAN_BYTES / chunk_size.max(1)).clamp(1, 64)).unwrap_or(64)
        } else {
            0
        };

        let bandwidth_max =
            ((base_concurrency as f64 * chunk_multiplier).round() as u32).clamp(1, 32);
        let max_concurrency = bandwidth_max.max(span_floor);
        let baseline_concurrency = (bandwidth_max / 2).max(1);

        Self {
            chunk_size,
            baseline_concurrency,
            current_max_concurrency: baseline_concurrency,
            max_concurrency,
            concurrency_ceiling: max_concurrency,
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

    /// Slots this peer can still accept.
    ///
    /// With no delivered bytes the allowance is [`Self::baseline_concurrency`].
    /// Delivered bytes scale it toward [`Self::max_concurrency`]. Each
    /// consecutive failure recorded by [`Self::record_request_failed`] halves
    /// that allowance, down to 1, including before any bytes are recorded. A
    /// completed request clears the streak. A 404 does not: it is not a
    /// transport failure.
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
        let allowance = penalized_allowance(total_capacity, self.consecutive_failures);

        // Available = allowance - currently active requests
        allowance.saturating_sub(self.active_requests as u32)
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

    /// A 404 is offset-specific negative information, not evidence that the
    /// peer transport is unhealthy. Release concurrency without penalizing the
    /// peer's global health score.
    pub fn record_request_not_found(&mut self) {
        self.active_requests = self.active_requests.saturating_sub(1);
    }

    /// The orchestrator that started this request is gone, so no completion
    /// will arrive. Release the slot. Do not change health: the peer did not
    /// fail, and a completion that already ran released its own slot.
    pub fn release_abandoned_request(&mut self) {
        self.active_requests = self.active_requests.saturating_sub(1);
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
        // `checked_sub` is `None` when the window reaches past the monotonic
        // origin (the machine has been up for less than the window). Every
        // bucket is then inside the window. Elapsed time starts at the oldest.
        let cutoff_time = now.checked_sub(window.min(self.window_duration));
        let in_window = |timestamp: Instant| cutoff_time.is_none_or(|cutoff| timestamp >= cutoff);

        // Sum bytes from buckets within the specified window
        let total_bytes: u64 = self
            .buckets
            .iter()
            .filter(|(timestamp, _)| in_window(*timestamp))
            .map(|(_, bytes)| bytes)
            .sum();

        // Find the oldest bucket within our window for elapsed time calculation
        if let Some((oldest_timestamp, _)) = self
            .buckets
            .iter()
            .find(|(timestamp, _)| in_window(*timestamp))
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

#[cfg(test)]
mod tests {
    use super::BandwidthWindow;
    use std::time::{Duration, Instant};

    /// A window longer than monotonic uptime must still count every bucket.
    /// `Instant::checked_sub` is `None` until the machine has been up for the
    /// whole window; unwrapping that is a panic on the data-sync task.
    #[test]
    fn bandwidth_window_counts_buckets_when_the_window_predates_boot() {
        let now = Instant::now();
        let mut span = Duration::from_secs(1);
        while now.checked_sub(span).is_some() {
            let Some(next) = span.checked_mul(2) else {
                span = Duration::MAX;
                break;
            };
            span = next;
        }
        assert!(
            now.checked_sub(span).is_none(),
            "the window must not fit before the monotonic origin"
        );

        let mut window = BandwidthWindow::new(Duration::from_secs(10));
        window.window_duration = span;
        window.add_bytes(4_096);
        let oldest = now
            .checked_sub(Duration::from_millis(100))
            .expect("host uptime covers 100ms");
        window.buckets.front_mut().expect("bucket").0 = oldest;

        let bps = window.bytes_per_second_in_window(span);
        // 4096 bytes over ~100ms is 40960 B/s. A few tens of milliseconds of
        // scheduling slack stays inside this range; dropping the bucket or
        // dividing by the whole pre-boot span does not.
        assert!(
            (20_000..100_000).contains(&bps),
            "expected the single bucket over ~100ms, got {bps}"
        );
    }
}
