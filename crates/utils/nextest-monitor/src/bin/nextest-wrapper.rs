//! nextest-wrapper: A unified wrapper binary for cargo-nextest.
//!
//! Tracks pass/fail status for every test, and optionally monitors CPU and
//! memory usage when the corresponding env vars are set.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use nextest_monitor::cpu_monitor::CpuMonitor;
use nextest_monitor::memory_monitor::MemoryMonitor;
use nextest_monitor::types::{append_stats, CpuSample, MemorySample, TestStats};

/// Flags known to take a following value argument.
const VALUE_TAKING_FLAGS: &[&str] = &[
    "--test-threads",
    "--test",
    "--package",
    "--bin",
    "--example",
    "--manifest-path",
    "--color",
    "--format",
    "--logfile",
    "--skip",
    "--report-time",
    "-Z",
    "-j",
];

fn flag_takes_value(flag: &str) -> bool {
    // Exact match (e.g. "--test-threads")
    if VALUE_TAKING_FLAGS.contains(&flag) {
        return true;
    }
    // Prefix form with '=' already consumed the value (e.g. "--test-threads=4"),
    // so the *next* token is NOT a value — return false for those.
    false
}

fn extract_test_name(args: &[String]) -> Option<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg.starts_with('-') {
            continue;
        }
        if arg.contains('/') || arg.contains('\\') {
            continue;
        }
        // If the previous argument is a flag that takes a value, treat this
        // token as that flag's value (e.g. the "1" after "--test-threads").
        if i > 0 && flag_takes_value(&args[i - 1]) {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn get_output_path() -> PathBuf {
    if let Ok(path) = env::var("NEXTEST_MONITOR_OUTPUT") {
        return PathBuf::from(path);
    }

    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target_dir).join("nextest-monitor/stats");
    }

    let start_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if let Some(workspace_root) = find_workspace_root(&start_dir) {
        return workspace_root.join("target").join("nextest-monitor/stats");
    }

    PathBuf::from("target").join("nextest-monitor/stats")
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();

    loop {
        if current.join("Cargo.lock").exists() {
            return Some(current);
        }

        let cargo_toml = current.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(content) = fs::read_to_string(&cargo_toml) {
                if content.contains("[workspace]") {
                    return Some(current);
                }
            }
        }

        if !current.pop() {
            break;
        }
    }

    None
}

fn env_is_enabled(name: &str) -> bool {
    env::var(name)
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

fn get_sample_interval() -> Duration {
    env::var("NEXTEST_MONITOR_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| Duration::from_millis(ms.max(1)))
        .unwrap_or(Duration::from_millis(50))
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: nextest-wrapper <test-binary> [args...]");
        eprintln!();
        eprintln!("Environment variables:");
        eprintln!("  NEXTEST_MONITOR_OUTPUT       - Path to output JSON file");
        eprintln!("  NEXTEST_MONITOR_CPU           - Set to '1' to enable CPU monitoring");
        eprintln!("  NEXTEST_MONITOR_MEMORY        - Set to '1' to enable memory monitoring");
        eprintln!("  NEXTEST_MONITOR_INTERVAL_MS   - Sampling interval in ms (default: 50)");
        eprintln!("  NEXTEST_MONITOR_DETAILED      - Set to '1' to record all samples");
        std::process::exit(1);
    }

    let binary = &args[1];
    let test_args: Vec<String> = args[2..].to_vec();
    let test_name = extract_test_name(&test_args);

    let output_path = get_output_path();
    let sample_interval = get_sample_interval();
    let monitor_cpu = env_is_enabled("NEXTEST_MONITOR_CPU");
    let monitor_memory = env_is_enabled("NEXTEST_MONITOR_MEMORY");
    let record_samples = env_is_enabled("NEXTEST_MONITOR_DETAILED");

    #[cfg(feature = "heap-profile")]
    let heap_profile = env_is_enabled("NEXTEST_MONITOR_HEAP_PROFILE");
    #[cfg(not(feature = "heap-profile"))]
    let heap_profile = false;

    let exit_code = run_with_monitoring(MonitorConfig {
        binary,
        test_args: &test_args,
        output_path: &output_path,
        sample_interval,
        monitor_cpu,
        monitor_memory,
        record_samples,
        test_name,
        heap_profile,
    })?;

    std::process::exit(exit_code);
}

struct MonitorConfig<'a> {
    binary: &'a str,
    test_args: &'a [String],
    output_path: &'a Path,
    sample_interval: Duration,
    monitor_cpu: bool,
    monitor_memory: bool,
    record_samples: bool,
    test_name: Option<String>,
    heap_profile: bool,
}

/// Derive the heap profile output path from the test name.
/// heaptrack will append `.heaptrack.zst` to this base path.
#[cfg(feature = "heap-profile")]
fn heap_profile_output_path(test_name: &Option<String>) -> Option<PathBuf> {
    let name = test_name.as_deref().unwrap_or("unknown");
    let sanitized = name.replace("::", "__");

    let base_dir = if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        PathBuf::from(target_dir).join("nextest-monitor/heap-profiles")
    } else {
        let start_dir = env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        if let Some(workspace_root) = find_workspace_root(&start_dir) {
            workspace_root
                .join("target")
                .join("nextest-monitor/heap-profiles")
        } else {
            PathBuf::from("target/nextest-monitor/heap-profiles")
        }
    };

    fs::create_dir_all(&base_dir).ok()?;
    Some(base_dir.join(sanitized))
}

fn run_with_monitoring(config: MonitorConfig<'_>) -> std::io::Result<i32> {
    let MonitorConfig {
        binary,
        test_args,
        output_path,
        sample_interval,
        monitor_cpu,
        monitor_memory,
        record_samples,
        test_name,
        heap_profile,
    } = config;

    #[cfg(feature = "heap-profile")]
    let heap_profile_path = if heap_profile {
        heap_profile_output_path(&test_name)
    } else {
        None
    };
    #[cfg(not(feature = "heap-profile"))]
    let heap_profile_path: Option<PathBuf> = None;

    #[cfg(feature = "heap-profile")]
    let mut child = if let Some(ref hp_path) = heap_profile_path {
        eprintln!("[nextest-wrapper] heap profiling: {}", hp_path.display());
        Command::new("heaptrack")
            .arg("-o")
            .arg(hp_path)
            .arg("--")
            .arg(binary)
            .args(test_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?
    } else {
        Command::new(binary)
            .args(test_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?
    };

    #[cfg(not(feature = "heap-profile"))]
    let mut child = Command::new(binary)
        .args(test_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let pid = child.id();
    let started_at = Utc::now();
    let start_instant = Instant::now();

    let mut cpu_samples = Vec::new();
    let mut memory_samples = Vec::new();

    // Skip RSS monitoring when heap profiling to avoid measuring heaptrack's overhead
    let effective_monitor_memory = monitor_memory && !heap_profile;

    let monitoring_enabled = monitor_cpu || effective_monitor_memory;

    let mut cpu_monitor = if monitor_cpu {
        Some(CpuMonitor::new(pid))
    } else {
        None
    };
    let memory_monitor = if effective_monitor_memory {
        Some(MemoryMonitor::new(pid))
    } else {
        None
    };

    let status = if monitoring_enabled {
        // Initial delay to let the process start before first CPU sample
        thread::sleep(Duration::from_millis(10));

        loop {
            match child.try_wait()? {
                Some(status) => break status,
                None => {
                    let elapsed_ms = start_instant.elapsed().as_millis() as u64;

                    if let Some(ref mut cpu_mon) = cpu_monitor {
                        let cpu_threads = cpu_mon.sample();
                        cpu_samples.push(CpuSample {
                            elapsed_ms,
                            cpu_threads,
                        });
                    }

                    if let Some(ref mem_mon) = memory_monitor {
                        let rss_bytes = mem_mon.sample();
                        memory_samples.push(MemorySample {
                            elapsed_ms,
                            rss_bytes,
                        });
                    }

                    thread::sleep(sample_interval);
                }
            }
        }
    } else {
        child.wait()?
    };

    let duration_ms = start_instant.elapsed().as_millis() as u64;
    let exit_code = status.code();
    let passed = exit_code == Some(0);

    let cpu_stats = if monitor_cpu {
        Some(calculate_cpu_stats(&cpu_samples, duration_ms))
    } else {
        None
    };

    let mem_stats = if effective_monitor_memory {
        Some(calculate_memory_stats(&memory_samples, duration_ms))
    } else {
        None
    };

    let stats = TestStats {
        binary: binary.to_string(),
        test_name,
        passed,
        started_at,
        duration_ms,
        exit_code,
        peak_cpu: cpu_stats.as_ref().map(|s| s.peak_cpu),
        avg_cpu: cpu_stats.as_ref().map(|s| s.avg_cpu),
        p50_cpu: cpu_stats.as_ref().map(|s| s.p50_cpu),
        p90_cpu: cpu_stats.as_ref().map(|s| s.p90_cpu),
        time_at_p90_ms: cpu_stats.as_ref().map(|s| s.time_at_p90_ms),
        time_near_peak_ms: cpu_stats.as_ref().map(|s| s.time_near_peak_ms),
        time_above_1t_ms: cpu_stats.as_ref().map(|s| s.time_above_1t_ms),
        time_above_2t_ms: cpu_stats.as_ref().map(|s| s.time_above_2t_ms),
        time_above_3t_ms: cpu_stats.as_ref().map(|s| s.time_above_3t_ms),
        time_above_4t_ms: cpu_stats.as_ref().map(|s| s.time_above_4t_ms),
        cpu_samples: if record_samples && monitor_cpu {
            Some(cpu_samples)
        } else {
            None
        },
        peak_rss_bytes: mem_stats.as_ref().map(|s| s.peak_rss_bytes),
        avg_rss_bytes: mem_stats.as_ref().map(|s| s.avg_rss_bytes),
        p50_rss_bytes: mem_stats.as_ref().map(|s| s.p50_rss_bytes),
        p90_rss_bytes: mem_stats.as_ref().map(|s| s.p90_rss_bytes),
        time_above_100mb_ms: mem_stats.as_ref().map(|s| s.time_above_100mb_ms),
        time_above_500mb_ms: mem_stats.as_ref().map(|s| s.time_above_500mb_ms),
        time_above_1gb_ms: mem_stats.as_ref().map(|s| s.time_above_1gb_ms),
        memory_samples: if record_samples && effective_monitor_memory {
            Some(memory_samples)
        } else {
            None
        },
        heap_profile_path: heap_profile_path.and_then(|hp_path| {
            find_heaptrack_output(&hp_path).map(|p| p.to_string_lossy().to_string())
        }),
    };

    if let Err(e) = append_stats(output_path, stats) {
        eprintln!("Warning: failed to append telemetry stats: {e}");
    }

    Ok(exit_code.unwrap_or(1))
}

/// Find the actual heaptrack output file. heaptrack appends a suffix to the
/// output path — the exact format varies by version:
/// - v1.2.x: `<base>.zst`
/// - v1.3+: `<base>.<pid>.heaptrack.zst` or `<base>.heaptrack.zst`
fn find_heaptrack_output(base_path: &Path) -> Option<PathBuf> {
    let parent = base_path.parent()?;
    let base_name = base_path.file_name()?.to_str()?;

    let entries = fs::read_dir(parent).ok()?;
    let mut best: Option<PathBuf> = None;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let is_match = name_str.as_ref() == format!("{base_name}.zst")
            || name_str.starts_with(&format!("{base_name}."));
        if is_match && name_str.ends_with(".zst") {
            match (&best, entry.metadata().ok()) {
                (None, _) => best = Some(entry.path()),
                (Some(prev), Some(meta)) => {
                    if let (Ok(prev_time), Ok(cur_time)) = (
                        fs::metadata(prev).and_then(|m| m.modified()),
                        meta.modified(),
                    ) {
                        if cur_time > prev_time {
                            best = Some(entry.path());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    best
}

struct CalculatedCpuStats {
    peak_cpu: f64,
    avg_cpu: f64,
    p50_cpu: f64,
    p90_cpu: f64,
    time_at_p90_ms: u64,
    time_near_peak_ms: u64,
    time_above_1t_ms: u64,
    time_above_2t_ms: u64,
    time_above_3t_ms: u64,
    time_above_4t_ms: u64,
}

/// Compute the actual elapsed duration each sample represents by taking deltas
/// between consecutive `elapsed_ms` timestamps. This is more accurate than
/// assuming each sample spans exactly `sample_interval` because scheduler jitter
/// can cause variable spacing between samples.
fn sample_deltas(elapsed_ms_values: &[u64]) -> Vec<u64> {
    elapsed_ms_values
        .iter()
        .enumerate()
        .map(|(i, &ms)| {
            if i == 0 {
                ms
            } else {
                ms.saturating_sub(elapsed_ms_values[i - 1])
            }
        })
        .collect()
}

fn percentile_index(len: usize, pct: f64) -> usize {
    let k = (len as f64 * pct / 100.0).ceil() as usize;
    k.saturating_sub(1).min(len - 1)
}

fn calculate_cpu_stats(samples: &[CpuSample], duration_ms: u64) -> CalculatedCpuStats {
    if samples.is_empty() {
        return CalculatedCpuStats {
            peak_cpu: 0.0,
            avg_cpu: 0.0,
            p50_cpu: 0.0,
            p90_cpu: 0.0,
            time_at_p90_ms: 0,
            time_near_peak_ms: 0,
            time_above_1t_ms: 0,
            time_above_2t_ms: 0,
            time_above_3t_ms: 0,
            time_above_4t_ms: 0,
        };
    }

    let mut cpu_values: Vec<f64> = samples.iter().map(|s| s.cpu_threads).collect();
    cpu_values.sort_by(|a, b| a.total_cmp(b));

    let n = cpu_values.len();
    let peak_cpu = cpu_values[n - 1];
    let avg_cpu = cpu_values.iter().sum::<f64>() / n as f64;

    let p50_cpu = cpu_values[percentile_index(n, 50.0)];
    let p90_cpu = cpu_values[percentile_index(n, 90.0)];

    let elapsed_values: Vec<u64> = samples.iter().map(|s| s.elapsed_ms).collect();
    let deltas = sample_deltas(&elapsed_values);

    let near_peak_threshold = peak_cpu * 0.8;

    let (
        mut time_at_p90_ms,
        mut time_near_peak_ms,
        mut time_above_1t_ms,
        mut time_above_2t_ms,
        mut time_above_3t_ms,
        mut time_above_4t_ms,
    ) = samples.iter().zip(deltas.iter()).fold(
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64),
        |(p90, near_peak, a1, a2, a3, a4), (s, &d)| {
            (
                p90 + if s.cpu_threads >= p90_cpu { d } else { 0 },
                near_peak
                    + if s.cpu_threads >= near_peak_threshold {
                        d
                    } else {
                        0
                    },
                a1 + if s.cpu_threads > 1.0 { d } else { 0 },
                a2 + if s.cpu_threads > 2.0 { d } else { 0 },
                a3 + if s.cpu_threads > 3.0 { d } else { 0 },
                a4 + if s.cpu_threads > 4.0 { d } else { 0 },
            )
        },
    );

    // Account for the interval from the last sample to process exit,
    // using the last sample's CPU value as the best estimate.
    let sum_of_deltas: u64 = deltas.iter().sum();
    let final_delta = duration_ms.saturating_sub(sum_of_deltas);
    if final_delta > 0 {
        let last = &samples[samples.len() - 1];
        if last.cpu_threads >= p90_cpu {
            time_at_p90_ms += final_delta;
        }
        if last.cpu_threads >= near_peak_threshold {
            time_near_peak_ms += final_delta;
        }
        if last.cpu_threads > 1.0 {
            time_above_1t_ms += final_delta;
        }
        if last.cpu_threads > 2.0 {
            time_above_2t_ms += final_delta;
        }
        if last.cpu_threads > 3.0 {
            time_above_3t_ms += final_delta;
        }
        if last.cpu_threads > 4.0 {
            time_above_4t_ms += final_delta;
        }
    }

    CalculatedCpuStats {
        peak_cpu,
        avg_cpu,
        p50_cpu,
        p90_cpu,
        time_at_p90_ms,
        time_near_peak_ms,
        time_above_1t_ms,
        time_above_2t_ms,
        time_above_3t_ms,
        time_above_4t_ms,
    }
}

const MB_100: u64 = 100 * 1024 * 1024;
const MB_500: u64 = 500 * 1024 * 1024;
const GB_1: u64 = 1024 * 1024 * 1024;

struct CalculatedMemoryStats {
    peak_rss_bytes: u64,
    avg_rss_bytes: u64,
    p50_rss_bytes: u64,
    p90_rss_bytes: u64,
    time_above_100mb_ms: u64,
    time_above_500mb_ms: u64,
    time_above_1gb_ms: u64,
}

fn calculate_memory_stats(samples: &[MemorySample], duration_ms: u64) -> CalculatedMemoryStats {
    if samples.is_empty() {
        return CalculatedMemoryStats {
            peak_rss_bytes: 0,
            avg_rss_bytes: 0,
            p50_rss_bytes: 0,
            p90_rss_bytes: 0,
            time_above_100mb_ms: 0,
            time_above_500mb_ms: 0,
            time_above_1gb_ms: 0,
        };
    }

    let mut rss_values: Vec<u64> = samples.iter().map(|s| s.rss_bytes).collect();
    rss_values.sort();

    let n = rss_values.len();
    let peak_rss_bytes = rss_values[n - 1];
    let avg_rss_bytes = (rss_values.iter().map(|&v| v as u128).sum::<u128>() / n as u128) as u64;

    let p50_rss_bytes = rss_values[percentile_index(n, 50.0)];
    let p90_rss_bytes = rss_values[percentile_index(n, 90.0)];

    let elapsed_values: Vec<u64> = samples.iter().map(|s| s.elapsed_ms).collect();
    let deltas = sample_deltas(&elapsed_values);

    let (mut time_above_100mb_ms, mut time_above_500mb_ms, mut time_above_1gb_ms) = samples
        .iter()
        .zip(deltas.iter())
        .fold((0u64, 0u64, 0u64), |(a100, a500, a1g), (s, &d)| {
            (
                a100 + if s.rss_bytes > MB_100 { d } else { 0 },
                a500 + if s.rss_bytes > MB_500 { d } else { 0 },
                a1g + if s.rss_bytes > GB_1 { d } else { 0 },
            )
        });

    // Account for the interval from the last sample to process exit,
    // using the last sample's RSS value as the best estimate.
    let sum_of_deltas: u64 = deltas.iter().sum();
    let final_delta = duration_ms.saturating_sub(sum_of_deltas);
    if final_delta > 0 {
        let last = &samples[samples.len() - 1];
        if last.rss_bytes > MB_100 {
            time_above_100mb_ms += final_delta;
        }
        if last.rss_bytes > MB_500 {
            time_above_500mb_ms += final_delta;
        }
        if last.rss_bytes > GB_1 {
            time_above_1gb_ms += final_delta;
        }
    }

    CalculatedMemoryStats {
        peak_rss_bytes,
        avg_rss_bytes,
        p50_rss_bytes,
        p90_rss_bytes,
        time_above_100mb_ms,
        time_above_500mb_ms,
        time_above_1gb_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::basic(
        &["my_module::test_foo", "--exact", "--nocapture"],
        Some("my_module::test_foo")
    )]
    #[case::leading_flags(
        &["--some-flag", "test_name"],
        Some("test_name")
    )]
    #[case::skips_option_values(
        &["--test-threads", "1", "my_test", "--exact"],
        Some("my_test")
    )]
    #[case::skips_forward_slash_path(
        &["src/tests/foo.rs", "my_test"],
        Some("my_test")
    )]
    #[case::skips_backslash_path(
        &["src\\tests\\foo.rs", "my_test"],
        Some("my_test")
    )]
    #[case::only_path_arg(&["src/tests/foo.rs"], None)]
    #[case::empty(&[], None)]
    #[case::only_flags(&["--exact", "--nocapture"], None)]
    fn extract_test_name_cases(#[case] args: &[&str], #[case] expected: Option<&str>) {
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(extract_test_name(&args), expected.map(String::from));
    }

    #[test]
    fn test_calculate_cpu_stats_empty() {
        let stats = calculate_cpu_stats(&[], 0);
        assert_eq!(stats.peak_cpu, 0.0);
        assert_eq!(stats.avg_cpu, 0.0);
    }

    #[test]
    fn test_calculate_cpu_stats_basic() {
        let samples = vec![
            CpuSample {
                elapsed_ms: 50,
                cpu_threads: 1.0,
            },
            CpuSample {
                elapsed_ms: 100,
                cpu_threads: 2.0,
            },
            CpuSample {
                elapsed_ms: 150,
                cpu_threads: 3.0,
            },
            CpuSample {
                elapsed_ms: 200,
                cpu_threads: 2.0,
            },
            CpuSample {
                elapsed_ms: 250,
                cpu_threads: 1.0,
            },
        ];
        let stats = calculate_cpu_stats(&samples, 250);
        assert_eq!(stats.peak_cpu, 3.0);
        assert!((stats.avg_cpu - 1.8).abs() < 0.01);
        assert!(stats.time_above_1t_ms > 0);
        assert!(stats.time_above_2t_ms > 0);
    }

    #[test]
    fn test_calculate_cpu_stats_variable_spacing() {
        // Simulate scheduler jitter: non-uniform sample intervals
        let samples = vec![
            CpuSample {
                elapsed_ms: 50,
                cpu_threads: 2.5,
            },
            CpuSample {
                elapsed_ms: 120, // 70ms gap (jitter)
                cpu_threads: 3.0,
            },
            CpuSample {
                elapsed_ms: 160, // 40ms gap
                cpu_threads: 0.5,
            },
        ];
        let stats = calculate_cpu_stats(&samples, 160);
        // time_above_2t: first two samples qualify (2.5 and 3.0)
        // deltas: 50 + 70 = 120ms
        assert_eq!(stats.time_above_2t_ms, 120);
        // time_above_1t: same first two qualify
        assert_eq!(stats.time_above_1t_ms, 120);
    }

    #[test]
    fn test_calculate_memory_stats_empty() {
        let stats = calculate_memory_stats(&[], 0);
        assert_eq!(stats.peak_rss_bytes, 0);
        assert_eq!(stats.avg_rss_bytes, 0);
    }

    #[test]
    fn test_calculate_memory_stats_basic() {
        let samples = vec![
            MemorySample {
                elapsed_ms: 50,
                rss_bytes: 50 * 1024 * 1024,
            },
            MemorySample {
                elapsed_ms: 100,
                rss_bytes: 150 * 1024 * 1024,
            },
            MemorySample {
                elapsed_ms: 150,
                rss_bytes: 200 * 1024 * 1024,
            },
            MemorySample {
                elapsed_ms: 200,
                rss_bytes: 600 * 1024 * 1024,
            },
            MemorySample {
                elapsed_ms: 250,
                rss_bytes: 100 * 1024 * 1024,
            },
        ];
        let stats = calculate_memory_stats(&samples, 250);
        assert_eq!(stats.peak_rss_bytes, 600 * 1024 * 1024);
        // 150MB, 200MB, 600MB are above 100MB; deltas: 50+50+50 = 150ms
        assert_eq!(stats.time_above_100mb_ms, 150);
        // 600MB is above 500MB; delta: 50ms
        assert_eq!(stats.time_above_500mb_ms, 50);
        assert_eq!(stats.time_above_1gb_ms, 0);
    }

    #[test]
    fn test_calculate_memory_stats_thresholds() {
        let samples = vec![
            MemorySample {
                elapsed_ms: 50,
                rss_bytes: 2 * 1024 * 1024 * 1024,
            }, // 2GB
        ];
        let stats = calculate_memory_stats(&samples, 50);
        assert_eq!(stats.time_above_100mb_ms, 50);
        assert_eq!(stats.time_above_500mb_ms, 50);
        assert_eq!(stats.time_above_1gb_ms, 50);
    }

    #[test]
    fn test_calculate_memory_stats_variable_spacing() {
        // Non-uniform intervals to verify delta-based computation
        let samples = vec![
            MemorySample {
                elapsed_ms: 60,
                rss_bytes: 200 * 1024 * 1024, // 200MB
            },
            MemorySample {
                elapsed_ms: 180,              // 120ms gap
                rss_bytes: 600 * 1024 * 1024, // 600MB
            },
            MemorySample {
                elapsed_ms: 210,             // 30ms gap
                rss_bytes: 50 * 1024 * 1024, // 50MB
            },
        ];
        let stats = calculate_memory_stats(&samples, 210);
        // above 100MB: first two samples, deltas: 60 + 120 = 180ms
        assert_eq!(stats.time_above_100mb_ms, 180);
        // above 500MB: second sample only, delta: 120ms
        assert_eq!(stats.time_above_500mb_ms, 120);
    }

    #[test]
    fn test_sample_deltas() {
        assert_eq!(sample_deltas(&[]), Vec::<u64>::new());
        assert_eq!(sample_deltas(&[50]), vec![50]);
        assert_eq!(sample_deltas(&[50, 100, 150]), vec![50, 50, 50]);
        assert_eq!(sample_deltas(&[50, 120, 160]), vec![50, 70, 40]);
    }

    mod proptest_percentile {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn percentile_index_always_in_bounds(
                len in 1_usize..=10_000,
                pct in 0.0_f64..=100.0,
            ) {
                let idx = percentile_index(len, pct);
                prop_assert!(idx < len, "index {} out of bounds for len {}", idx, len);
            }

            #[test]
            fn percentile_index_monotonic(
                len in 2_usize..=1_000,
                pct_a in 0.0_f64..=100.0,
                pct_b in 0.0_f64..=100.0,
            ) {
                let (lo, hi) = if pct_a <= pct_b { (pct_a, pct_b) } else { (pct_b, pct_a) };
                let idx_lo = percentile_index(len, lo);
                let idx_hi = percentile_index(len, hi);
                prop_assert!(
                    idx_lo <= idx_hi,
                    "percentile_index not monotonic: p{}={} > p{}={}",
                    lo, idx_lo, hi, idx_hi
                );
            }

            #[test]
            fn percentile_100_returns_last(len in 1_usize..=10_000) {
                let idx = percentile_index(len, 100.0);
                prop_assert_eq!(idx, len - 1);
            }
        }
    }
}
