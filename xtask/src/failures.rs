//! Shared types and logic for tracking test failures across runs.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Subdirectory within target for failure tracking files
const FAILURES_SUBDIR: &str = "nextest-failure-tracking";

/// Filename for the list of failed tests from previous runs
const FAILURES_FILENAME: &str = "failures.json";

/// Filename where the wrapper writes test results during a run
const RESULTS_FILENAME: &str = "results.jsonl";

/// Find the workspace root by looking for Cargo.lock
fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join("Cargo.lock").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
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

/// Get the failures tracking directory path
pub fn get_failures_dir() -> PathBuf {
    get_target_dir().join(FAILURES_SUBDIR)
}

/// Get the failures file path
pub fn get_failures_file_path() -> PathBuf {
    get_failures_dir().join(FAILURES_FILENAME)
}

/// Get the results file path (used during test runs)
pub fn get_results_file_path() -> PathBuf {
    get_failures_dir().join(RESULTS_FILENAME)
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

/// Result of a single test execution (written by wrapper)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Full test name
    pub name: String,
    /// Whether the test passed
    pub passed: bool,
}

/// Collection of test results from a single run
#[derive(Debug, Default)]
pub struct RunResults {
    pub results: Vec<TestResult>,
}

impl RunResults {
    /// Load results from the JSONL file
    pub fn load() -> Self {
        Self::load_from(&get_results_file_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let mut results = Vec::new();

        if path.exists() {
            if let Ok(contents) = fs::read_to_string(path) {
                for line in contents.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<TestResult>(line) {
                        Ok(result) => results.push(result),
                        Err(e) => {
                            eprintln!("Warning: Failed to parse result line: {}", e);
                        }
                    }
                }
            }
        }

        Self { results }
    }

    /// Clear the results file (call before a new run)
    pub fn clear() -> std::io::Result<()> {
        let path = get_results_file_path();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        // Also remove any stale lock file
        let lock_path = path.with_extension("jsonl.lock");
        if lock_path.exists() {
            let _ = fs::remove_file(lock_path);
        }
        Ok(())
    }

    /// Get sets of passed and failed tests
    pub fn into_sets(self) -> (HashSet<String>, HashSet<String>) {
        let mut passed = HashSet::new();
        let mut failed = HashSet::new();

        for result in self.results {
            if result.passed {
                passed.insert(result.name);
            } else {
                failed.insert(result.name);
            }
        }

        (passed, failed)
    }
}

/// Append a test result to the results file with locking
pub fn append_result(result: &TestResult) -> std::io::Result<()> {
    append_result_to(&get_results_file_path(), result)
}

pub fn append_result_to(path: &Path, result: &TestResult) -> std::io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Use a lock file for concurrent access
    let lock_path = path.with_extension("jsonl.lock");

    // Simple spin-lock with file
    let mut attempts = 0;
    while attempts < 100 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_lock_file) => {
                // We have the lock - append the result
                let line = serde_json::to_string(result).map_err(std::io::Error::other)?;

                let mut file = OpenOptions::new().create(true).append(true).open(path)?;

                writeln!(file, "{}", line)?;

                // Release lock
                let _ = fs::remove_file(&lock_path);
                return Ok(());
            }
            Err(_) => {
                // Lock held by another process
                thread::sleep(Duration::from_millis(10));
                attempts += 1;
            }
        }
    }

    // Fallback: just try to write anyway
    eprintln!("Warning: Could not acquire lock for results file, writing anyway");
    let line = serde_json::to_string(result).map_err(std::io::Error::other)?;

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    use std::io::Write as _;
    writeln!(file, "{}", line)
}

/// Ensure the failures directory exists
pub fn ensure_dir() -> std::io::Result<()> {
    fs::create_dir_all(get_failures_dir())
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

    // Add wrapper script definition
    config_content.push_str("[scripts.wrapper.failure-tracker]\n");
    config_content.push_str(&format!("command = '{}'\n\n", wrapper_path));

    // Add filter if we're rerunning specific tests
    if let Some(tests) = failed_tests {
        let filter_expr = build_failure_filter(tests);

        config_content.push_str("[profile.xtask-rerun-failures]\n");
        config_content.push_str(&format!("default-filter = '{}'\n\n", filter_expr));

        config_content.push_str("[[profile.xtask-rerun-failures.scripts]]\n");
        config_content.push_str("filter = 'all()'\n");
        config_content.push_str("run-wrapper = 'failure-tracker'\n");
    } else {
        // Add script rule for the default profile
        config_content.push_str("[[profile.default.scripts]]\n");
        config_content.push_str("filter = 'all()'\n");
        config_content.push_str("run-wrapper = 'failure-tracker'\n");
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
    use tempfile::TempDir;

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
    fn test_run_results_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("results.jsonl");

        let results = vec![
            TestResult {
                name: "test::one".to_string(),
                passed: true,
            },
            TestResult {
                name: "test::two".to_string(),
                passed: false,
            },
            TestResult {
                name: "test::three".to_string(),
                passed: true,
            },
        ];

        for result in &results {
            append_result_to(&path, result).unwrap();
        }

        let loaded = RunResults::load_from(&path);
        assert_eq!(loaded.results.len(), 3);

        let (passed, failed) = loaded.into_sets();
        assert!(passed.contains("test::one"));
        assert!(passed.contains("test::three"));
        assert!(failed.contains("test::two"));
    }

    #[test]
    fn test_failures_file_empty() {
        let failures = FailuresFile::default();
        assert!(failures.is_empty());
    }
}
