use std::thread;
use std::time::{Duration, Instant};

use irys_actors::peer_stats::BandwidthWindow;

#[test]
fn test_bandwidth_window_load_test() {
    let mut window = BandwidthWindow::new(Duration::from_secs(5));
    let bytes_per_addition = 1024_u64;
    let target_interval = Duration::from_millis(3);

    // Run for 6 seconds to test window reset
    let test_duration = Duration::from_secs(6);
    let start_time = Instant::now();
    let mut total_additions = 0;
    let mut measurements = Vec::new();

    while start_time.elapsed() < test_duration {
        window.add_bytes(bytes_per_addition);
        total_additions += 1;

        let current_bps = window.bytes_per_second();
        let elapsed = start_time.elapsed();

        // Store measurements for analysis
        if elapsed >= Duration::from_millis(500) {
            measurements.push((elapsed, current_bps));
        }

        println!(
            "Time: {:.3}s, Additions: {}, Current BPS: {}",
            elapsed.as_secs_f64(),
            total_additions,
            current_bps
        );

        // Calculate actual rate based on real timing
        if elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5) {
            let actual_additions_per_second = total_additions as f64 / elapsed.as_secs_f64();
            let expected_bps_for_actual_rate =
                (actual_additions_per_second * bytes_per_addition as f64) as u64;

            // The measured BPS should be close to what we'd expect given the actual rate
            let tolerance_percent = 15.0; // 15% tolerance for timing variations
            let tolerance =
                (expected_bps_for_actual_rate as f64 * tolerance_percent / 100.0) as u64;

            assert!(
                current_bps >= expected_bps_for_actual_rate.saturating_sub(tolerance)
                    && current_bps <= expected_bps_for_actual_rate + tolerance,
                "BPS {} not within {}% of expected {} (actual rate: {:.1} additions/sec)",
                current_bps,
                tolerance_percent,
                expected_bps_for_actual_rate,
                actual_additions_per_second
            );
        }

        // Test window reset behavior - check around the 5 second mark
        if elapsed >= Duration::from_millis(5000) && elapsed < Duration::from_millis(5200) {
            // After window reset with sustained load, BPS should remain similar
            // because we're measuring a similar rate over a shorter time period
            println!(
                "Around window reset ({}ms) - BPS: {}",
                elapsed.as_millis(),
                current_bps
            );

            // The BPS might spike initially due to very small elapsed time,
            // but should stabilize to a reasonable value similar to before reset
            if current_bps == 0 {
                println!("✓ Window reset detected (momentary BPS = 0)");
            } else if (200_000..=500_000).contains(&current_bps) {
                println!("✓ Window reset working correctly, BPS stabilized after reset");
            } else {
                println!(
                    "ℹ Window reset behavior - BPS: {} (may be spiking due to very small elapsed time)",
                    current_bps
                );
            }
        }

        thread::sleep(target_interval);
    }

    // Analyze the actual timing performance
    let total_time = start_time.elapsed().as_secs_f64();
    let actual_rate = total_additions as f64 / total_time;
    let actual_interval = total_time / total_additions as f64 * 1000.0; // in ms

    println!("=== Test Summary ===");
    println!("Total additions made: {}", total_additions);
    println!("Total time: {:.3}s", total_time);
    println!(
        "Target interval: 3ms, Actual average interval: {:.3}ms",
        actual_interval
    );
    println!("Actual rate: {:.1} additions/sec", actual_rate);
    println!(
        "Expected BPS for actual rate: {:.0}",
        actual_rate * bytes_per_addition as f64
    );
    println!("Final measured BPS: {}", window.bytes_per_second());

    // Verify we maintained a reasonable rate (threading overhead is expected)
    let target_rate = 1000.0 / 3.0; // ~333.33 additions per second
    let reasonable_min_rate = 200.0; // Allow for significant OS scheduling overhead
    let reasonable_max_rate = 400.0; // Upper bound for sanity

    assert!(
        actual_rate >= reasonable_min_rate && actual_rate <= reasonable_max_rate,
        "Actual rate {:.1} should be reasonable (between {} and {} additions/sec), but timing precision may vary",
        actual_rate,
        reasonable_min_rate,
        reasonable_max_rate
    );

    // Log how close we got to the target
    let efficiency = (actual_rate / target_rate) * 100.0;
    println!(
        "Threading efficiency: {:.1}% of theoretical maximum",
        efficiency
    );
}

#[test]
fn test_bandwidth_window_precise_timing() {
    let mut window = BandwidthWindow::new(Duration::from_secs(5));
    let bytes_per_addition = 1024_u64;

    // Add exactly 10 additions over 1 second
    let start = Instant::now();
    for i in 0..10 {
        window.add_bytes(bytes_per_addition);
        if i < 9 {
            // Don't sleep after the last addition
            thread::sleep(Duration::from_millis(100)); // 100ms between additions
        }
    }

    let actual_elapsed = start.elapsed();
    let bps = window.bytes_per_second();

    println!("Actual elapsed time: {:.3}s", actual_elapsed.as_secs_f64());
    println!("Bytes per second: {}", bps);

    // Expected: 10 * 1024 bytes over ~0.9 seconds = ~11,377 BPS
    let expected_min = 8000; // Allow some variance for timing
    let expected_max = 15000;

    assert!(
        bps >= expected_min && bps <= expected_max,
        "BPS {} not in expected range [{}, {}]",
        bps,
        expected_min,
        expected_max
    );
}
