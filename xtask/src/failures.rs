//! Shared types and logic for tracking test failures across runs.

use nextest_monitor::types::AggregatedStats;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Subdirectory within target for all nextest-monitor files
const MONITOR_SUBDIR: &str = "nextest-monitor";

/// Filename for the list of failed tests from previous runs
const FAILURES_FILENAME: &str = "failures.json";

/// Filename for the unified stats JSONL (written by nextest-wrapper)
const STATS_FILENAME: &str = "stats.jsonl";

/// Walk up the directory tree to find the workspace root (contains Cargo.lock)
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();

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

/// Get the target directory, using various environment variables and heuristics
fn get_target_dir() -> PathBuf {
    // Try CARGO_TARGET_DIR if set (usually points to workspace target)
    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target_dir);
    }

    // Try to find workspace root by looking for Cargo.lock
    // Start from CARGO_MANIFEST_DIR if available, otherwise current dir
    let start_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if let Some(workspace_root) = find_workspace_root(&start_dir) {
        return workspace_root.join("target");
    }

    // Last resort: use current directory's target
    PathBuf::from("target")
}

/// Get the nextest-monitor directory path (contains both failures and stats)
pub fn get_monitor_dir() -> PathBuf {
    get_target_dir().join(MONITOR_SUBDIR)
}

/// Get the failures file path
pub fn get_failures_file_path() -> PathBuf {
    get_monitor_dir().join(FAILURES_FILENAME)
}

/// Get the unified stats file path (used by nextest-wrapper during test runs)
pub fn get_stats_file_path() -> PathBuf {
    get_monitor_dir().join(STATS_FILENAME)
}

/// Represents the stored test failures (persisted between runs)
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FailuresFile {
    /// Full test names that failed (e.g., "crate::module::test_name")
    pub failed_tests: Vec<String>,
}

impl FailuresFile {
    pub fn load() -> Self {
        Self::load_from(&get_failures_file_path())
    }

    pub fn load_from(path: &Path) -> Self {
        if path.exists() {
            match fs::read_to_string(path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(f) => return f,
                    Err(e) => {
                        eprintln!("Warning: Failed to parse {}: {}", path.display(), e);
                    }
                },
                Err(e) => {
                    eprintln!("Warning: Failed to read {}: {}", path.display(), e);
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&get_failures_file_path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(path, contents)
    }

    pub fn clear() -> std::io::Result<()> {
        let path = get_failures_file_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.failed_tests.is_empty()
    }

    pub fn as_set(&self) -> HashSet<String> {
        self.failed_tests.iter().cloned().collect()
    }
}

/// Collection of test results from a single run, loaded from the stats JSONL
#[derive(Debug, Default)]
pub struct RunResults {
    pub passed: HashSet<String>,
    pub failed: HashSet<String>,
}

impl RunResults {
    /// Load results from the stats JSONL file
    pub fn load() -> Self {
        Self::load_from(&get_stats_file_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let stats = AggregatedStats::load_or_default(path);
        let mut passed = HashSet::new();
        let mut failed = HashSet::new();

        for test in &stats.tests {
            if let Some(ref name) = test.test_name {
                if test.passed {
                    failed.remove(name);
                    passed.insert(name.clone());
                } else {
                    passed.remove(name);
                    failed.insert(name.clone());
                }
            }
        }

        Self { passed, failed }
    }

    /// Get sets of passed and failed tests
    pub fn into_sets(self) -> (HashSet<String>, HashSet<String>) {
        (self.passed, self.failed)
    }
}

/// Ensure the nextest-monitor directory exists
pub fn ensure_dir() -> std::io::Result<()> {
    fs::create_dir_all(get_monitor_dir())
}

/// Path to the default nextest config file
const NEXTEST_CONFIG_PATH: &str = ".config/nextest.toml";

/// Generate a temporary config file that copies the existing config and adds wrapper script settings
pub fn generate_nextest_config(
    wrapper_path: &str,
    failed_tests: Option<&[String]>,
) -> eyre::Result<tempfile::NamedTempFile> {
    let mut temp_file = tempfile::Builder::new()
        .prefix("nextest-config-")
        .suffix(".toml")
        .tempfile()?;

    let mut config_content = String::new();

    // Start with existing config if present
    let existing_config_path = std::path::Path::new(NEXTEST_CONFIG_PATH);
    if existing_config_path.exists() {
        let existing = fs::read_to_string(existing_config_path)?;
        config_content.push_str(&existing);
        config_content.push_str("\n\n# === Added by xtask for failure tracking ===\n\n");
    }

    // Add experimental feature flag for wrapper scripts (if not already present)
    if !config_content.contains("experimental") {
        config_content.push_str("experimental = [\"wrapper-scripts\"]\n\n");
    } else if !config_content.contains("wrapper-scripts") {
        // Need to modify existing experimental line - this is tricky
        // For now, just add it and hope TOML merges correctly or warn the user
        eprintln!("Warning: existing config has 'experimental' key - you may need to add 'wrapper-scripts' manually");
    }

    // Add wrapper script definition — use a distinct name to avoid collisions
    // with any manually-configured wrapper in the base nextest.toml
    config_content.push_str("[scripts.wrapper.xtask-monitor]\n");
    config_content.push_str(&format!("command = '{}'\n\n", wrapper_path));

    // Add filter if we're rerunning specific tests
    if let Some(tests) = failed_tests {
        let filter_expr = build_failure_filter(tests);

        config_content.push_str("[profile.xtask-rerun-failures]\n");
        config_content.push_str(&format!("default-filter = '{}'\n\n", filter_expr));

        config_content.push_str("[[profile.xtask-rerun-failures.scripts]]\n");
        config_content.push_str("filter = 'all()'\n");
        config_content.push_str("run-wrapper = 'xtask-monitor'\n");
    } else {
        // Add script rule for the default profile
        config_content.push_str("[[profile.default.scripts]]\n");
        config_content.push_str("filter = 'all()'\n");
        config_content.push_str("run-wrapper = 'xtask-monitor'\n");
    }

    temp_file.write_all(config_content.as_bytes())?;
    temp_file.flush()?;

    Ok(temp_file)
}

/// Build the filter expression for failed tests
pub fn build_failure_filter(failed_tests: &[String]) -> String {
    let filter_parts: Vec<String> = failed_tests
        .iter()
        .map(|name| format!("test(={})", name))
        .collect();
    filter_parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nextest_monitor::types::{append_stats, TestStats};
    use tempfile::TempDir;

    fn make_stats(name: &str, passed: bool) -> TestStats {
        TestStats {
            binary: "bin".to_string(),
            test_name: Some(name.to_string()),
            passed,
            started_at: chrono::Utc::now(),
            duration_ms: 100,
            exit_code: if passed { Some(0) } else { Some(1) },
            peak_cpu: None,
            avg_cpu: None,
            p50_cpu: None,
            p90_cpu: None,
            time_at_p90_ms: None,
            time_near_peak_ms: None,
            time_above_1t_ms: None,
            time_above_2t_ms: None,
            time_above_3t_ms: None,
            time_above_4t_ms: None,
            cpu_samples: None,
            peak_rss_bytes: None,
            avg_rss_bytes: None,
            p50_rss_bytes: None,
            p90_rss_bytes: None,
            time_above_100mb_ms: None,
            time_above_500mb_ms: None,
            time_above_1gb_ms: None,
            memory_samples: None,
        }
    }

    #[test]
    fn test_failures_file_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("failures.json");

        let failures = FailuresFile {
            failed_tests: vec!["test::one".to_string(), "test::two".to_string()],
        };

        failures.save_to(&path).unwrap();
        let loaded = FailuresFile::load_from(&path);

        assert_eq!(loaded.failed_tests, failures.failed_tests);
    }

    #[test]
    fn test_run_results_from_stats() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("stats.jsonl");

        append_stats(&path, make_stats("test::one", true)).unwrap();
        append_stats(&path, make_stats("test::two", false)).unwrap();
        append_stats(&path, make_stats("test::three", true)).unwrap();

        let loaded = RunResults::load_from(&path);
        let (passed, failed) = loaded.into_sets();
        assert_eq!(passed.len(), 2);
        assert_eq!(failed.len(), 1);
        assert!(passed.contains("test::one"));
        assert!(passed.contains("test::three"));
        assert!(failed.contains("test::two"));
    }

    #[test]
    fn test_run_results_later_entries_override() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("stats.jsonl");

        // test::one fails first, then passes — should end up only in passed
        append_stats(&path, make_stats("test::one", false)).unwrap();
        append_stats(&path, make_stats("test::one", true)).unwrap();
        // test::two passes first, then fails — should end up only in failed
        append_stats(&path, make_stats("test::two", true)).unwrap();
        append_stats(&path, make_stats("test::two", false)).unwrap();

        let loaded = RunResults::load_from(&path);
        let (passed, failed) = loaded.into_sets();
        assert!(passed.contains("test::one"), "test::one should be passed");
        assert!(!failed.contains("test::one"), "test::one should NOT be failed");
        assert!(failed.contains("test::two"), "test::two should be failed");
        assert!(!passed.contains("test::two"), "test::two should NOT be passed");
    }

    #[test]
    fn test_failures_file_empty() {
        let failures = FailuresFile::default();
        assert!(failures.is_empty());
    }
}
