//! nextest-cpu-wrapper: A wrapper binary that monitors CPU usage of test processes
//!
//! This tool is designed to be used as a custom test runner for cargo-nextest.
//! It spawns the actual test binary, monitors its CPU usage, and records statistics.

use std::env;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// CPU usage sample at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSample {
    /// Milliseconds since test start
    pub elapsed_ms: u64,
    /// CPU usage as equivalent full threads (e.g., 2.5 means 250% CPU)
    pub cpu_threads: f64,
}

/// Statistics for a single test run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCpuStats {
    /// Test binary path
    pub binary: String,
    /// Test name (from arguments if available)
    pub test_name: Option<String>,
    /// Timestamp when test started
    pub started_at: DateTime<Utc>,
    /// Total duration in milliseconds
    pub duration_ms: u64,
    /// Exit code of the test
    pub exit_code: Option<i32>,
    /// Maximum CPU threads used at any sample point
    pub peak_cpu: f64,
    /// Average CPU threads used across all samples
    pub avg_cpu: f64,
    /// P50 (median) CPU usage
    pub p50_cpu: f64,
    /// P90 CPU usage
    pub p90_cpu: f64,
    /// Time in ms where CPU was >= P90 value
    pub time_at_p90_ms: u64,
    /// Time in ms where CPU was >= 80% of peak
    pub time_near_peak_ms: u64,
    /// Time in ms where CPU exceeded 1.0 threads (normal allocation)
    #[serde(default)]
    pub time_above_1t_ms: u64,
    /// Time in ms where CPU exceeded 2.0 threads (heavy allocation)
    #[serde(default)]
    pub time_above_2t_ms: u64,
    /// Time in ms where CPU exceeded 3.0 threads (heavy3 allocation)
    #[serde(default)]
    pub time_above_3t_ms: u64,
    /// Time in ms where CPU exceeded 4.0 threads (heavy4 allocation)
    #[serde(default)]
    pub time_above_4t_ms: u64,
    /// All CPU samples (if detailed mode enabled)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<CpuSample>>,
}

/// Aggregated statistics across all tests
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AggregatedStats {
    pub tests: Vec<TestCpuStats>,
}

impl AggregatedStats {
    pub fn load_or_default(path: &PathBuf) -> Self {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(stats) = serde_json::from_str(&content) {
                    return stats;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        fs::write(path, content)
    }

    pub fn append(&mut self, stats: TestCpuStats) {
        self.tests.push(stats);
    }
}

/// Platform-specific CPU monitoring implementation
#[cfg(target_os = "linux")]
mod cpu_monitor {
    use std::fs;
    use std::time::Instant;

    /// Get CPU time (user + system) in ticks for a process (includes all threads)
    fn get_process_cpu_ticks(pid: u32) -> Option<u64> {
        // Use /proc/[pid]/stat which aggregates all thread CPU time
        let stat_path = format!("/proc/{}/stat", pid);
        let content = fs::read_to_string(&stat_path).ok()?;

        // Find the end of comm field (it's in parentheses and may contain spaces)
        let comm_end = content.find(')')?;
        let after_comm = &content[comm_end + 2..];
        let fields: Vec<&str> = after_comm.split_whitespace().collect();

        // utime is field index 11, stime is field index 12
        if fields.len() > 12 {
            let utime: u64 = fields[11].parse().ok()?;
            let stime: u64 = fields[12].parse().ok()?;
            Some(utime + stime)
        } else {
            None
        }
    }

    /// Get clock ticks per second
    fn get_clk_tck() -> u64 {
        unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 }
    }

    pub struct CpuMonitor {
        pid: u32,
        last_cpu_ticks: u64,
        last_check: Instant,
        clk_tck: u64,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            let clk_tck = get_clk_tck();
            let initial_ticks = get_process_cpu_ticks(pid).unwrap_or(0);
            Self {
                pid,
                last_cpu_ticks: initial_ticks,
                last_check: Instant::now(),
                clk_tck,
            }
        }

        /// Sample current CPU usage, returns cpu_threads (e.g., 2.5 = 250% CPU)
        pub fn sample(&mut self) -> f64 {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_check);

            let current_ticks = get_process_cpu_ticks(self.pid).unwrap_or(self.last_cpu_ticks);
            let tick_delta = current_ticks.saturating_sub(self.last_cpu_ticks);

            // Convert ticks to CPU threads
            let elapsed_secs = elapsed.as_secs_f64();
            let cpu_threads = if elapsed_secs > 0.0 {
                (tick_delta as f64 / self.clk_tck as f64) / elapsed_secs
            } else {
                0.0
            };

            self.last_cpu_ticks = current_ticks;
            self.last_check = now;

            cpu_threads
        }

        /// Check if the process is still running
        pub fn is_running(&self) -> bool {
            std::path::Path::new(&format!("/proc/{}", self.pid)).exists()
        }
    }
}

#[cfg(target_os = "macos")]
mod cpu_monitor {
    use std::process::Command;
    use std::time::Instant;

    pub struct CpuMonitor {
        pid: u32,
        last_check: Instant,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            Self {
                pid,
                last_check: Instant::now(),
            }
        }

        /// Sample current CPU usage using ps command
        pub fn sample(&mut self) -> f64 {
            self.last_check = Instant::now();

            // ps on macOS: %cpu is the percentage (100 = 1 core fully used)
            if let Ok(output) = Command::new("ps")
                .args(["-p", &self.pid.to_string(), "-o", "%cpu="])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(cpu) = stdout.trim().parse::<f64>() {
                    return cpu / 100.0;
                }
            }
            0.0
        }

        /// Check if the process is still running
        pub fn is_running(&self) -> bool {
            Command::new("kill")
                .args(["-0", &self.pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod cpu_monitor {
    pub struct CpuMonitor {
        pid: u32,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            Self { pid }
        }

        pub fn sample(&mut self) -> f64 {
            0.0
        }

        pub fn is_running(&self) -> bool {
            true
        }
    }
}

use cpu_monitor::CpuMonitor;

fn extract_test_name(args: &[String]) -> Option<String> {
    // nextest typically passes test filter as an argument
    // Look for the test name pattern
    for arg in args {
        // Skip flags
        if arg.starts_with('-') {
            continue;
        }
        // Skip if it looks like a path
        if arg.contains('/') || arg.contains('\\') {
            continue;
        }
        // This is likely the test name or filter
        return Some(arg.clone());
    }
    None
}

fn get_output_path() -> PathBuf {
    // First check explicit override
    if let Ok(path) = env::var("NEXTEST_CPU_OUTPUT") {
        return PathBuf::from(path);
    }

    // Try CARGO_TARGET_DIR if set (usually points to workspace target)
    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target_dir).join("nextest-cpu-stats.json");
    }

    // Try to find workspace root by looking for Cargo.lock
    // Start from CARGO_MANIFEST_DIR if available, otherwise current dir
    let start_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if let Some(workspace_root) = find_workspace_root(&start_dir) {
        return workspace_root.join("target").join("nextest-cpu-stats.json");
    }

    // Last resort: use current directory's target
    PathBuf::from("target").join("nextest-cpu-stats.json")
}

/// Walk up the directory tree to find the workspace root (contains Cargo.lock)
fn find_workspace_root(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();

    loop {
        // Check for Cargo.lock (indicates workspace root)
        if current.join("Cargo.lock").exists() {
            return Some(current);
        }

        // Check for Cargo.toml with [workspace] section
        let cargo_toml = current.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(content) = fs::read_to_string(&cargo_toml) {
                if content.contains("[workspace]") {
                    return Some(current);
                }
            }
        }

        // Move up one directory
        if !current.pop() {
            break;
        }
    }

    None
}

fn get_sample_interval() -> Duration {
    env::var("NEXTEST_CPU_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(50))
}

fn should_record_samples() -> bool {
    env::var("NEXTEST_CPU_DETAILED")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: nextest-cpu-wrapper <test-binary> [args...]");
        eprintln!();
        eprintln!("Environment variables:");
        eprintln!("  NEXTEST_CPU_OUTPUT       - Path to output JSON file");
        eprintln!(
            "                             (default: <workspace>/target/nextest-cpu-stats.json)"
        );
        eprintln!("  NEXTEST_CPU_INTERVAL_MS  - Sampling interval in ms (default: 50)");
        eprintln!("  NEXTEST_CPU_DETAILED     - Set to '1' to record all samples");
        eprintln!("                             (default: false)");
        std::process::exit(1);
    }

    let binary = &args[1];
    let test_args: Vec<String> = args[2..].to_vec();
    let test_name = extract_test_name(&test_args);

    let output_path = get_output_path();
    let sample_interval = get_sample_interval();
    let record_samples = should_record_samples();

    let exit_code = run_with_monitoring(
        binary,
        &test_args,
        &output_path,
        sample_interval,
        record_samples,
        test_name,
    )?;

    std::process::exit(exit_code);
}

fn run_with_monitoring(
    binary: &str,
    test_args: &[String],
    output_path: &PathBuf,
    sample_interval: Duration,
    record_samples: bool,
    test_name: Option<String>,
) -> std::io::Result<i32> {
    // Spawn the test process
    let mut child = Command::new(binary)
        .args(test_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let pid = child.id();
    let started_at = Utc::now();
    let start_instant = Instant::now();

    let mut samples = Vec::new();
    let mut monitor = CpuMonitor::new(pid);

    // Initial delay to let the process start
    thread::sleep(Duration::from_millis(10));

    // Monitor loop with non-blocking wait
    loop {
        match child.try_wait()? {
            Some(status) => {
                // Process finished
                let duration_ms = start_instant.elapsed().as_millis() as u64;

                // Calculate statistics
                let calc = calculate_stats(&samples, sample_interval);

                let stats = TestCpuStats {
                    binary: binary.to_string(),
                    test_name,
                    started_at,
                    duration_ms,
                    exit_code: status.code(),
                    peak_cpu: calc.peak_cpu,
                    avg_cpu: calc.avg_cpu,
                    p50_cpu: calc.p50_cpu,
                    p90_cpu: calc.p90_cpu,
                    time_at_p90_ms: calc.time_at_p90_ms,
                    time_near_peak_ms: calc.time_near_peak_ms,
                    time_above_1t_ms: calc.time_above_1t_ms,
                    time_above_2t_ms: calc.time_above_2t_ms,
                    time_above_3t_ms: calc.time_above_3t_ms,
                    time_above_4t_ms: calc.time_above_4t_ms,
                    samples: if record_samples { Some(samples) } else { None },
                };

                // Append to output file (with file locking for concurrent tests)
                append_stats(output_path, stats)?;

                return Ok(status.code().unwrap_or(1));
            }
            None => {
                // Process still running, take a sample
                let cpu_threads = monitor.sample();
                let elapsed_ms = start_instant.elapsed().as_millis() as u64;

                samples.push(CpuSample {
                    elapsed_ms,
                    cpu_threads,
                });

                thread::sleep(sample_interval);
            }
        }
    }
}

/// Statistics calculated from CPU samples
struct CalculatedStats {
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

fn calculate_stats(samples: &[CpuSample], sample_interval: Duration) -> CalculatedStats {
    if samples.is_empty() {
        return CalculatedStats {
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

    // Collect CPU values and sort for percentile calculation
    let mut cpu_values: Vec<f64> = samples.iter().map(|s| s.cpu_threads).collect();
    cpu_values.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let n = cpu_values.len();
    let peak_cpu = cpu_values[n - 1];
    let avg_cpu = cpu_values.iter().sum::<f64>() / n as f64;

    // Percentiles (using nearest-rank method)
    let p50_cpu = cpu_values[n * 50 / 100];
    let p90_cpu = cpu_values[(n * 90 / 100).min(n - 1)];

    let sample_ms = sample_interval.as_millis() as u64;

    // Time at or above P90
    let time_at_p90_ms =
        samples.iter().filter(|s| s.cpu_threads >= p90_cpu).count() as u64 * sample_ms;

    // Time near peak (>= 80% of peak)
    let near_peak_threshold = peak_cpu * 0.8;
    let time_near_peak_ms = samples
        .iter()
        .filter(|s| s.cpu_threads >= near_peak_threshold)
        .count() as u64
        * sample_ms;

    // Time above allocation thresholds
    let time_above_1t_ms =
        samples.iter().filter(|s| s.cpu_threads > 1.0).count() as u64 * sample_ms;

    let time_above_2t_ms =
        samples.iter().filter(|s| s.cpu_threads > 2.0).count() as u64 * sample_ms;

    let time_above_3t_ms =
        samples.iter().filter(|s| s.cpu_threads > 3.0).count() as u64 * sample_ms;

    let time_above_4t_ms =
        samples.iter().filter(|s| s.cpu_threads > 4.0).count() as u64 * sample_ms;

    CalculatedStats {
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

fn append_stats(path: &PathBuf, stats: TestCpuStats) -> std::io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Use a lock file for concurrent access
    let lock_path = path.with_extension("json.lock");

    // Simple spin-lock with file
    let mut attempts = 0;
    while attempts < 100 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_lock_file) => {
                // We have the lock
                let mut aggregated = AggregatedStats::load_or_default(path);
                aggregated.append(stats);
                let result = aggregated.save(path);

                // Release lock
                let _ = fs::remove_file(&lock_path);
                return result;
            }
            Err(_) => {
                // Lock held by another process
                thread::sleep(Duration::from_millis(10));
                attempts += 1;
            }
        }
    }

    // Fallback: just try to write anyway
    eprintln!("Warning: Could not acquire lock for CPU stats file, writing anyway");
    let mut aggregated = AggregatedStats::load_or_default(path);
    aggregated.append(stats);
    aggregated.save(path)
}
