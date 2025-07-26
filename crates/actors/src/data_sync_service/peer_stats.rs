use irys_domain::{ChunkTimeRecord, CircularBuffer};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub enum BandwidthRating {
    Low,    // 0-40 MB/s of throughput
    Medium, // 40-80 MB/s of throughput
    High,   // >80 MB/s of throughput
}

#[derive(Debug)]
pub struct PeerStats {
    pub chunk_size: u64,
    pub baseline_concurrency: u32, // Starting point for adjustments
    pub current_max_concurrency: u32,
    pub max_concurrency: u32,
    pub bandwidth_rating: BandwidthRating,
    pub timeout: Duration,

    // Simplified tracking
    pub consecutive_failures: u32,
    pub active_requests: usize,

    // Multi-window throughput tracking for better trend analysis
    pub short_term_window: BandwidthWindow, // resets every 10 seconds - immediate performance
    pub medium_term_window: BandwidthWindow, // resets every 30 seconds - recent trend
    pub long_term_window: BandwidthWindow,  // resets every 300 seconds - stable baseline

    // Track average chunk completion time for better concurrency calculations
    pub completion_time_samples: CircularBuffer<Duration>,
}

impl PeerStats {
    pub fn new(target_bandwidth_mbps: usize, chunk_size: u64, timeout: Duration) -> Self {
        // Convert target bandwidth from MB/s to bytes/s, then calculate how many
        // chunks per second we need to process to achieve that bandwidth
        let target_bandwidth_bps = target_bandwidth_mbps * 1024 * 1024;
        let chunks_per_second = target_bandwidth_bps as f64 / chunk_size as f64;
        let chunk_download_time = Duration::from_secs_f64(1.0 / chunks_per_second);

        // Assume 100ms of connection overhead for each chunk HTTP request
        // TODO: This estimate will have to change with socket based communication
        let estimated_chunk_download_time = chunk_download_time + Duration::from_millis(100);

        // Calculate max concurrent requests needed to sustain the target rate
        let max_concurrency =
            (estimated_chunk_download_time.as_secs_f64() * chunks_per_second).ceil() as u32;

        // Clamp to reasonable bounds
        let baseline_concurrency = (max_concurrency / 2).max(1); // 50% of max

        Self {
            chunk_size,
            baseline_concurrency,
            current_max_concurrency: baseline_concurrency,
            max_concurrency,
            bandwidth_rating: BandwidthRating::Medium,
            timeout,
            consecutive_failures: 0,
            active_requests: 0,
            short_term_window: BandwidthWindow::new(Duration::from_secs(10)),
            medium_term_window: BandwidthWindow::new(Duration::from_secs(30)),
            long_term_window: BandwidthWindow::new(Duration::from_secs(300)),
            completion_time_samples: CircularBuffer::new(50), // Track last 50 completion times
        }
    }

    /// Calculate the amount of concurrency that can be safely added beyond the
    /// current_max_concurrency based on recent bandwidth measurements
    pub fn available_concurrency(&self) -> u32 {
        let expected_bytes_per_sec = self.expected_bandwidth_bps();
        let observed_bytes_per_sec = self.short_term_window.bytes_per_second() as f64;

        // Only add concurrency if we have unused bandwidth
        let unused_bandwidth = expected_bytes_per_sec - observed_bytes_per_sec;
        if unused_bandwidth <= 0.0 {
            return self.baseline_concurrency;
        }

        // Calculate how many additional concurrent streams the unused bandwidth can support
        let bytes_per_stream = expected_bytes_per_sec;
        let additional_streams = unused_bandwidth / bytes_per_stream;

        additional_streams as u32 // Truncates to whole streams
    }

    pub fn expected_bandwidth_bps(&self) -> f64 {
        // Convert to bandwidth: chunk_size (bytes) / time (seconds)
        let time_seconds = self.average_completion_time().as_secs_f64();
        if time_seconds > 0.0 {
            self.chunk_size as f64 / time_seconds
        } else {
            0.0 // or handle zero time case appropriately
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

        // Add bytes to all throughput windows
        self.short_term_window.add_bytes(self.chunk_size);
        self.medium_term_window.add_bytes(self.chunk_size);
        self.long_term_window.add_bytes(self.chunk_size);
    }

    pub fn record_request_failed(&mut self) {
        self.active_requests = self.active_requests.saturating_sub(1);
        self.consecutive_failures += 1;
    }

    /// Update the running average of chunk completion times
    pub fn average_completion_time(&self) -> Duration {
        if self.completion_time_samples.len() == 0 {
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

    /// Calculate a health score for the peer (0.0 = worst, 1.0 = best)
    pub fn health_score(&self) -> f64 {
        let mut score = 0.5; // Start at neutral baseline

        // Failure impact (-0.5 to +0.2) - most critical factor
        if self.consecutive_failures == 0 {
            score += 0.2; // Strong bonus for reliability
        } else {
            score -= (self.consecutive_failures as f64 * 0.15).min(0.5);
        }

        // Throughput trend impact (-0.3 to +0.3) - performance trajectory
        if self.is_throughput_improving() {
            score += 0.3;
        } else if !self.is_throughput_stable() {
            let short = self.short_term_bandwidth_bps();
            let medium = self.medium_term_bandwidth_bps();
            if short < medium && medium > 0 {
                score -= 0.3; // Declining performance is serious
            } else {
                score -= 0.1; // General instability
            }
        }

        score.clamp(0.0, 1.0)
    }

    /// Get short-term bandwidth throughput (10s window)
    pub fn short_term_bandwidth_bps(&self) -> u64 {
        self.short_term_window.bytes_per_second()
    }

    /// Get medium-term bandwidth throughput (30s window) - good for recent trends
    pub fn medium_term_bandwidth_bps(&self) -> u64 {
        self.medium_term_window.bytes_per_second()
    }

    /// Get long-term bandwidth throughput (300s window) - good for stable baseline
    pub fn long_term_bandwidth_bps(&self) -> u64 {
        self.long_term_window.bytes_per_second()
    }

    /// Check if bandwidth throughput is currently trending upward
    pub fn is_throughput_improving(&self) -> bool {
        let short = self.short_term_bandwidth_bps();
        let medium = self.medium_term_bandwidth_bps();
        let long = self.long_term_bandwidth_bps();

        // Consider improving if recent performance is better than medium and long term
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
    pub bytes_transferred: u64,
    pub window_start: Instant,
    pub window_duration: Duration,
}

impl BandwidthWindow {
    pub fn new(duration: Duration) -> Self {
        Self {
            bytes_transferred: 0,
            window_start: Instant::now(),
            window_duration: duration,
        }
    }

    pub fn add_bytes(&mut self, bytes: u64) {
        let now = Instant::now();

        // Reset window if it's expired
        if now.duration_since(self.window_start) > self.window_duration {
            self.bytes_transferred = bytes;
            self.window_start = now;
        } else {
            self.bytes_transferred += bytes;
        }
    }

    pub fn bytes_per_second(&self) -> u64 {
        let elapsed = Instant::now().duration_since(self.window_start);
        if elapsed.as_secs() == 0 {
            return 0;
        }

        self.bytes_transferred / elapsed.as_secs()
    }
}
