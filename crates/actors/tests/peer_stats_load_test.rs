use irys_actors::peer_stats::PeerStats;
use irys_domain::ChunkTimeRecord;
use irys_types::PartitionChunkOffset;
use std::thread;
use std::time::{Duration, Instant};

/// Simulate a realistic chunk processing pattern for a fixed chunk size
struct ChunkSimulator {
    chunk_size: u64,
    failure_rate: f64,
    next_chunk_offset: u64,
    // Pre-calculated timing based on target bandwidth
    base_transfer_time: Duration,
    min_time: Duration,
    max_time: Duration,
}

impl ChunkSimulator {
    fn new(chunk_size: u64, target_bandwidth_bps: f64, failure_rate: f64) -> Self {
        // Calculate realistic transfer time for this chunk size at target bandwidth
        let base_transfer_time = Duration::from_secs_f64(chunk_size as f64 / target_bandwidth_bps);

        // Realistic network overhead based on chunk size
        let overhead_ms = if chunk_size < 4096 {
            // Small chunks: minimal HTTP overhead (1-3ms)
            2
        } else if chunk_size < 65536 {
            // Medium chunks: typical HTTP overhead (3-10ms)
            5
        } else {
            // Large chunks: higher overhead for connection setup (10-50ms)
            20
        };

        // Add realistic bounds with network overhead and variance
        let variance_factor = 0.3;
        let base_ms = base_transfer_time.as_millis() as u64;

        let min_time = Duration::from_millis(
            overhead_ms + ((base_ms as f64) * (1.0 - variance_factor)) as u64,
        );
        let max_time = Duration::from_millis(
            overhead_ms + ((base_ms as f64) * (1.0 + variance_factor)) as u64,
        );

        Self {
            chunk_size,
            failure_rate,
            next_chunk_offset: 0,
            base_transfer_time,
            min_time,
            max_time,
        }
    }

    fn generate_completion_time(&self) -> Duration {
        // Use base transfer time as the center point, with realistic variance
        let base_ms = self.base_transfer_time.as_millis() as u64;
        let min_ms = self.min_time.as_millis() as u64;
        let max_ms = self.max_time.as_millis() as u64;

        // 70% of the time, stay close to the base transfer time (±20%)
        if fastrand::f64() < 0.7 {
            let variance = (base_ms as f64 * 0.2) as u64; // ±20% of base time
            let adjusted_min = base_ms.saturating_sub(variance).max(min_ms);
            let adjusted_max = (base_ms + variance).min(max_ms);

            // Ensure adjusted_min <= adjusted_max to prevent overflow
            if adjusted_min <= adjusted_max {
                let random_ms = adjusted_min + fastrand::u64(0..=(adjusted_max - adjusted_min));
                Duration::from_millis(random_ms)
            } else {
                // Fallback to base time if range is invalid
                Duration::from_millis(base_ms.clamp(min_ms, max_ms))
            }
        } else {
            // 30% of the time, allow full range for network variability
            let random_ms = min_ms + fastrand::u64(0..=(max_ms - min_ms));
            Duration::from_millis(random_ms)
        }
    }

    fn should_fail(&self) -> bool {
        fastrand::f64() < self.failure_rate
    }

    fn create_chunk_record(&mut self, start_time: Instant, duration: Duration) -> ChunkTimeRecord {
        let completion_time = start_time + duration;
        let chunk_offset = PartitionChunkOffset::from(self.next_chunk_offset);
        self.next_chunk_offset += 1;

        ChunkTimeRecord {
            chunk_offset,
            start_time,
            completion_time,
            duration,
        }
    }
}

// Test constants
const CHUNK_256KIB: u64 = 256 * 1024;
const TARGET_50MBPS_256KIB: usize = 50;
const TARGET_100MBPS_256KIB: usize = 100;
// const CHUNK_32B: u64 = 32;
// const TARGET_50MBPS_32B: usize = 10;  // 10KB/s equivalent
// const TARGET_100MBPS_32B: usize = 20; // 20KB/s equivalent

#[test]
fn run_256kib_tests() {
    println!("=== 256KiB Chunk Test Suite ===");

    run_steady_test(CHUNK_256KIB, TARGET_50MBPS_256KIB);
    run_steady_test(CHUNK_256KIB, TARGET_100MBPS_256KIB);
    run_burst_test(CHUNK_256KIB, TARGET_50MBPS_256KIB);
    run_burst_test(CHUNK_256KIB, TARGET_100MBPS_256KIB);
    run_degrading_test(CHUNK_256KIB, TARGET_50MBPS_256KIB);
    run_degrading_test(CHUNK_256KIB, TARGET_100MBPS_256KIB);

    println!("=== 256KiB tests completed ===\n");
}

// #[test]
// fn run_32b_tests() {
//     println!("=== 32B Chunk Test Suite ===");

//     run_steady_test(CHUNK_32B, TARGET_50MBPS_32B);
//     run_steady_test(CHUNK_32B, TARGET_100MBPS_32B);
//     run_burst_test(CHUNK_32B, TARGET_50MBPS_32B);
//     run_burst_test(CHUNK_32B, TARGET_100MBPS_32B);
//     run_degrading_test(CHUNK_32B, TARGET_50MBPS_32B);
//     run_degrading_test(CHUNK_32B, TARGET_100MBPS_32B);

//     println!("=== 32B tests completed ===\n");
// }

fn run_steady_test(chunk_size: u64, target_bandwidth_mbps: usize) {
    let bandwidth_display = if chunk_size == 32 {
        format!("{}KB/s", target_bandwidth_mbps * 1024)
    } else {
        format!("{}MB/s", target_bandwidth_mbps)
    };

    let test_name = format!(
        "{} Steady {}",
        if chunk_size == 32 { "32B" } else { "256KiB" },
        bandwidth_display
    );

    println!("=== {} ===", test_name);

    let mut stats = PeerStats::new(target_bandwidth_mbps, chunk_size, Duration::from_secs(30));
    let mut simulator = ChunkSimulator::new(
        chunk_size,
        (target_bandwidth_mbps * 1024 * 1024) as f64,
        0.01,
    );

    run_scenario(
        &mut stats,
        &mut simulator,
        Duration::from_secs(10),
        &test_name,
    );
}

fn run_burst_test(chunk_size: u64, target_bandwidth_mbps: usize) {
    let bandwidth_display = if chunk_size == 32 {
        format!("{}KB/s", target_bandwidth_mbps * 1024)
    } else {
        format!("{}MB/s", target_bandwidth_mbps)
    };

    let test_name = format!(
        "{} Burst {}",
        if chunk_size == 32 { "32B" } else { "256KiB" },
        bandwidth_display
    );

    println!("=== {} ===", test_name);

    let mut stats = PeerStats::new(target_bandwidth_mbps, chunk_size, Duration::from_secs(30));
    let mut simulator = ChunkSimulator::new(
        chunk_size,
        (target_bandwidth_mbps * 1024 * 1024) as f64,
        0.01,
    );

    let start_time = Instant::now();
    let mut total_chunks = 0;
    let mut failed_chunks = 0;
    let mut total_bytes = 0_u64;
    let duration = Duration::from_secs(8);

    while start_time.elapsed() < duration {
        // Random burst sizes
        let burst_size = if fastrand::f64() < 0.3 {
            fastrand::usize(8..=20) // Large burst
        } else {
            fastrand::usize(1..=4) // Normal activity
        };

        for _ in 0..burst_size {
            let request_start = Instant::now();
            stats.record_request_started();
            total_chunks += 1;

            let completion_duration = simulator.generate_completion_time();

            if simulator.should_fail() {
                stats.record_request_failed();
                failed_chunks += 1;
            } else {
                let chunk_record =
                    simulator.create_chunk_record(request_start, completion_duration);
                stats.record_request_completed(chunk_record);
                total_bytes += chunk_size;
            }
        }

        // Variable delay between bursts
        let delay = if burst_size > 6 {
            Duration::from_millis(fastrand::u64(100..=300))
        } else {
            Duration::from_millis(fastrand::u64(20..=80))
        };
        thread::sleep(delay);
    }

    print_results(
        &test_name,
        &stats,
        total_chunks,
        failed_chunks,
        total_bytes,
        start_time.elapsed(),
    );
}

fn run_degrading_test(chunk_size: u64, target_bandwidth_mbps: usize) {
    let bandwidth_display = if chunk_size == 32 {
        format!("{}KB/s", target_bandwidth_mbps * 1024)
    } else {
        format!("{}MB/s", target_bandwidth_mbps)
    };

    let test_name = format!(
        "{} Degrading {}",
        if chunk_size == 32 { "32B" } else { "256KiB" },
        bandwidth_display
    );

    println!("=== {} ===", test_name);

    let mut stats = PeerStats::new(target_bandwidth_mbps, chunk_size, Duration::from_secs(30));
    let mut simulator = ChunkSimulator::new(
        chunk_size,
        (target_bandwidth_mbps * 1024 * 1024) as f64,
        0.02,
    );

    let start_time = Instant::now();
    let mut total_chunks = 0;
    let mut failed_chunks = 0;
    let mut total_bytes = 0_u64;
    let duration = Duration::from_secs(10);

    while start_time.elapsed() < duration {
        let progress = start_time.elapsed().as_secs_f64() / duration.as_secs_f64();

        // Gradually degrade performance
        let degradation = 1.0 + (progress * 1.5); // Up to 2.5x slower
        let base_duration = simulator.generate_completion_time();
        let degraded_duration =
            Duration::from_millis((base_duration.as_millis() as f64 * degradation) as u64);

        let request_start = Instant::now();
        stats.record_request_started();
        total_chunks += 1;

        if fastrand::f64() < (simulator.failure_rate * (1.0 + progress)) {
            stats.record_request_failed();
            failed_chunks += 1;
        } else {
            let chunk_record = simulator.create_chunk_record(request_start, degraded_duration);
            stats.record_request_completed(chunk_record);
            total_bytes += chunk_size;
        }

        // Increasing delays
        thread::sleep(Duration::from_millis((25.0 * (1.0 + progress)) as u64));
    }

    print_results(
        &test_name,
        &stats,
        total_chunks,
        failed_chunks,
        total_bytes,
        start_time.elapsed(),
    );
}

fn run_scenario(
    stats: &mut PeerStats,
    simulator: &mut ChunkSimulator,
    duration: Duration,
    test_name: &str,
) {
    let start_time = Instant::now();
    let mut total_chunks = 0;
    let mut failed_chunks = 0;
    let mut total_bytes = 0_u64;

    // Track pending requests for realistic concurrency simulation
    let mut pending_requests: Vec<(Instant, Duration, u64)> = Vec::new();
    let mut next_chunk_id = 0_u64;

    // Calculate realistic request rate
    let target_bandwidth_bps = (stats.max_concurrency as f64 * simulator.chunk_size as f64) / 0.1; // Rough estimate
    let chunks_per_second = target_bandwidth_bps / simulator.chunk_size as f64;
    let target_inter_request_delay =
        Duration::from_millis((1000.0 / chunks_per_second.max(1.0)) as u64);

    let mut last_request_time = start_time;
    let mut last_optimization_time = start_time;
    let mut max_concurrent = 0;

    while start_time.elapsed() < duration {
        let now = Instant::now();

        // Simulate controller optimization every 1 second
        if now.duration_since(last_optimization_time) >= Duration::from_secs(1) {
            simulate_concurrency_optimization(stats);
            last_optimization_time = now;
        }

        // Start new requests at the target rate, but respect current_max_concurrency
        if now.duration_since(last_request_time) >= target_inter_request_delay
            && pending_requests.len() < stats.current_max_concurrency as usize
        {
            let request_start = now;
            let completion_duration = simulator.generate_completion_time();

            stats.record_request_started();
            pending_requests.push((request_start, completion_duration, next_chunk_id));
            next_chunk_id += 1;
            total_chunks += 1;
            last_request_time = now;
        }

        // Track max concurrency
        max_concurrent = max_concurrent.max(pending_requests.len());

        // Complete finished requests
        let mut completed_indices = Vec::new();
        for (i, (start_time, duration, _chunk_id)) in pending_requests.iter().enumerate() {
            if now.duration_since(*start_time) >= *duration {
                completed_indices.push(i);
            }
        }

        // Process completions (reverse order to maintain indices)
        for &i in completed_indices.iter().rev() {
            let (start_time, duration, _chunk_id) = pending_requests.remove(i);

            if simulator.should_fail() {
                stats.record_request_failed();
                failed_chunks += 1;
            } else {
                let chunk_record = simulator.create_chunk_record(start_time, duration);
                stats.record_request_completed(chunk_record);
                total_bytes += simulator.chunk_size;
            }
        }

        // Small sleep to prevent busy waiting
        thread::sleep(Duration::from_micros(100));
    }

    // Complete any remaining pending requests
    for (start_time, duration, _chunk_id) in pending_requests {
        if simulator.should_fail() {
            stats.record_request_failed();
            failed_chunks += 1;
        } else {
            let chunk_record = simulator.create_chunk_record(start_time, duration);
            stats.record_request_completed(chunk_record);
            total_bytes += simulator.chunk_size;
        }
    }

    print_results(
        test_name,
        stats,
        total_chunks,
        failed_chunks,
        total_bytes,
        start_time.elapsed(),
    );
}

/// Simulate the controller's concurrency optimization logic
fn simulate_concurrency_optimization(stats: &mut PeerStats) {
    let health_score = stats.health_score();
    let available_concurrency = stats.available_concurrency();

    // Mimic the controller logic: increase concurrency if health is good and capacity available
    if health_score > 0.7 && available_concurrency > 0 {
        let current_max = stats.current_max_concurrency;
        let increase = std::cmp::min(available_concurrency, 5); // Max increase of 5 per optimization cycle
        stats.current_max_concurrency =
            std::cmp::min(current_max + increase, stats.max_concurrency);
    }
    // Could add logic to decrease concurrency if health is poor
    else if health_score < 0.3 && stats.current_max_concurrency > stats.baseline_concurrency {
        stats.current_max_concurrency = std::cmp::max(
            stats.current_max_concurrency.saturating_sub(2),
            stats.baseline_concurrency,
        );
    }
}

fn format_bandwidth(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_second / (1024.0 * 1024.0))
    } else if bytes_per_second >= 1024.0 {
        format!("{:.1} KB/s", bytes_per_second / 1024.0)
    } else {
        format!("{:.1} B/s", bytes_per_second)
    }
}

fn print_results(
    test_name: &str,
    stats: &PeerStats,
    total: u32,
    failed: u32,
    total_bytes: u64,
    elapsed: Duration,
) {
    let completed = total - failed;
    let actual_bandwidth_bps = total_bytes as f64 / elapsed.as_secs_f64();

    println!("{} Results:", test_name);
    println!(
        "  Chunks: {} completed, {} failed ({:.1}% success)",
        completed,
        failed,
        (completed as f64 / total as f64) * 100.0
    );
    println!(
        "  Bandwidth: {} ({} bytes in {:.1}s)",
        format_bandwidth(actual_bandwidth_bps),
        total_bytes,
        elapsed.as_secs_f64()
    );
    println!("  Health: {:.3}", stats.health_score());
    println!("  Concurrency:");
    println!(
        "    Limits: baseline={}, current_max={}, absolute_max={}",
        stats.baseline_concurrency, stats.current_max_concurrency, stats.max_concurrency
    );
    println!(
        "    Expected BW per stream: {}",
        format_bandwidth(stats.expected_bandwidth_bps())
    );
    println!();
}
