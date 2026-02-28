use std::fs;
use std::io::Write as _;
use std::path::Path;

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

/// Memory usage sample at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySample {
    /// Milliseconds since test start
    pub elapsed_ms: u64,
    /// Resident set size in bytes
    pub rss_bytes: u64,
}

/// Unified statistics for a single test run.
///
/// Always-present fields capture pass/fail and timing. Optional CPU and memory
/// fields are populated only when the respective monitor is enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStats {
    /// Test binary path
    pub binary: String,
    /// Test name (from arguments if available)
    pub test_name: Option<String>,
    /// Whether the test passed
    pub passed: bool,
    /// Timestamp when test started
    pub started_at: DateTime<Utc>,
    /// Total duration in milliseconds
    pub duration_ms: u64,
    /// Exit code of the test
    pub exit_code: Option<i32>,

    // -- CPU fields (optional) --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_at_p90_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_near_peak_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_1t_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_2t_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_3t_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_4t_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_samples: Option<Vec<CpuSample>>,

    // -- Memory fields (optional) --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_100mb_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_500mb_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_above_1gb_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_samples: Option<Vec<MemorySample>>,
}

/// Aggregated statistics across all tests
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AggregatedStats {
    pub tests: Vec<TestStats>,
}

impl AggregatedStats {
    /// Load stats from a JSONL file (one TestStats per line).
    pub fn load_or_default(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        let Ok(content) = fs::read_to_string(path) else {
            return Self::default();
        };

        let mut tests = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TestStats>(line) {
                Ok(stats) => tests.push(stats),
                Err(e) => {
                    eprintln!("Warning: skipping malformed stats line: {e}");
                }
            }
        }
        AggregatedStats { tests }
    }

    pub fn append(&mut self, stats: TestStats) {
        self.tests.push(stats);
    }
}

/// Append a single test's stats as one JSON line (JSONL format).
///
/// Uses O_APPEND so concurrent writers don't need external locking —
/// each line is written atomically on Linux/macOS for typical payload sizes.
pub fn append_stats(path: &Path, stats: TestStats) -> std::io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut line = serde_json::to_string(&stats)?;
    line.push('\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_roundtrip() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("stats.json");

        let stats = TestStats {
            binary: "test-bin".to_string(),
            test_name: Some("test_one".to_string()),
            passed: true,
            started_at: Utc::now(),
            duration_ms: 2000,
            exit_code: Some(0),
            peak_cpu: Some(3.0),
            avg_cpu: Some(2.0),
            p50_cpu: Some(1.8),
            p90_cpu: Some(2.5),
            time_at_p90_ms: Some(200),
            time_near_peak_ms: Some(100),
            time_above_1t_ms: Some(1500),
            time_above_2t_ms: Some(800),
            time_above_3t_ms: Some(0),
            time_above_4t_ms: Some(0),
            cpu_samples: None,
            peak_rss_bytes: Some(100_000_000),
            avg_rss_bytes: Some(50_000_000),
            p50_rss_bytes: Some(45_000_000),
            p90_rss_bytes: Some(90_000_000),
            time_above_100mb_ms: Some(0),
            time_above_500mb_ms: Some(0),
            time_above_1gb_ms: Some(0),
            memory_samples: None,
        };

        append_stats(&path, stats.clone()).unwrap();

        let loaded = AggregatedStats::load_or_default(&path);
        assert_eq!(loaded.tests.len(), 1);
        assert_eq!(loaded.tests[0].test_name, Some("test_one".to_string()));
        assert!(loaded.tests[0].passed);
        assert_eq!(loaded.tests[0].peak_cpu, Some(3.0));
        assert_eq!(loaded.tests[0].peak_rss_bytes, Some(100_000_000));
    }
}
