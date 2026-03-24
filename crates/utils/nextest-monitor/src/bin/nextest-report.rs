//! nextest-report: Analyze and report on CPU/memory usage statistics from test runs
//!
//! This tool reads the JSON output from nextest-wrapper and generates
//! reports to help categorize tests by CPU consumption and timeout requirements.
//! It reads classification rules directly from your .config/nextest.toml.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use regex::Regex;
use serde::Deserialize;

use nextest_monitor::types::{AggregatedStats, CpuSample, MemorySample, TestStats};

/// Default exceedance threshold: a test is considered to "need" N threads if it
/// exceeds N threads for more than this fraction of its runtime.
/// 0.20 = 20% — brief spikes up to 20% of runtime are tolerated.
const DEFAULT_EXCEEDANCE_PCT: f64 = 0.20;

/// Default timeout headroom: a test is bumped to the next timeout tier when its
/// duration exceeds this fraction of the current tier's timeout.
/// 0.50 = 50% — a test using more than half its timeout gets bumped up, leaving
/// a comfortable margin for run-to-run variance and parallel load.
const DEFAULT_TIMEOUT_HEADROOM: f64 = 0.50;

// ============================================================================
// Nextest config parsing
// ============================================================================

#[derive(Debug, Clone, Deserialize, Default)]
struct NextestConfig {
    profile: Option<HashMap<String, ProfileConfig>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct ProfileConfig {
    slow_timeout: Option<SlowTimeout>,
    threads_required: Option<u32>,
    overrides: Option<Vec<Override>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SlowTimeout {
    period: String,
    terminate_after: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Override {
    filter: Option<String>,
    slow_timeout: Option<SlowTimeout>,
    threads_required: Option<u32>,
    #[serde(default)]
    priority: i32,
}

/// Parsed classification rule from nextest config
#[derive(Debug, Clone)]
pub struct ClassificationRule {
    pub name: String,
    pub pattern: Regex,
    pub threads_required: Option<u32>,
    pub timeout_ms: Option<u64>,
    pub priority: i32,
}

/// Complete classification config derived from nextest.toml
#[derive(Debug, Clone)]
pub struct ClassificationConfig {
    pub default_threads: u32,
    pub default_timeout_ms: u64,
    pub rules: Vec<ClassificationRule>,
}

impl ClassificationConfig {
    /// Load configuration from nextest.toml
    pub fn from_nextest_toml(path: &PathBuf) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

        let config: NextestConfig = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

        let default_profile = config
            .profile
            .as_ref()
            .and_then(|p| p.get("default"))
            .cloned()
            .unwrap_or_default();

        // Parse default values
        let default_threads = default_profile.threads_required.unwrap_or(1);
        let default_timeout_ms = parse_timeout(&default_profile.slow_timeout, 60_000);

        // Parse overrides into rules
        let mut rules = Vec::new();
        if let Some(overrides) = default_profile.overrides {
            for ov in overrides.iter() {
                if let Some(ref filter) = ov.filter {
                    // Extract regex pattern from filter like 'test(/.*slow_.*/)'
                    if let Some(pattern) = extract_test_pattern(filter) {
                        let regex = Regex::new(&pattern)
                            .map_err(|e| format!("Invalid regex in filter '{}': {}", filter, e))?;

                        rules.push(ClassificationRule {
                            name: extract_rule_name(filter),
                            pattern: regex,
                            threads_required: ov.threads_required,
                            timeout_ms: ov
                                .slow_timeout
                                .as_ref()
                                .map(|st| parse_timeout(&Some(st.clone()), default_timeout_ms)),
                            priority: ov.priority,
                        });
                    }
                }
            }
        }

        // Sort by priority (higher = checked first)
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));

        Ok(Self {
            default_threads,
            default_timeout_ms,
            rules,
        })
    }

    /// Create a default configuration if no nextest.toml is found
    pub fn default_config() -> Self {
        Self {
            default_threads: 1,
            default_timeout_ms: 60_000,
            rules: vec![
                ClassificationRule {
                    name: "slow".to_string(),
                    pattern: Regex::new(r".*slow_.*").unwrap(),
                    threads_required: None,
                    timeout_ms: Some(180_000),
                    priority: 100,
                },
                ClassificationRule {
                    name: "heavy".to_string(),
                    pattern: Regex::new(r".*heavy_.*").unwrap(),
                    threads_required: Some(2),
                    timeout_ms: None,
                    priority: 90,
                },
                ClassificationRule {
                    name: "heavy3".to_string(),
                    pattern: Regex::new(r".*heavy3_.*").unwrap(),
                    threads_required: Some(3),
                    timeout_ms: None,
                    priority: 80,
                },
                ClassificationRule {
                    name: "heavy4".to_string(),
                    pattern: Regex::new(r".*heavy4_.*").unwrap(),
                    threads_required: Some(4),
                    timeout_ms: None,
                    priority: 70,
                },
            ],
        }
    }

    /// Classify a test name, returning all matching rules and effective values
    pub fn classify(&self, test_name: &str) -> TestClassification {
        let mut matching_rules = Vec::new();
        let mut effective_threads = self.default_threads;
        let mut effective_timeout = self.default_timeout_ms;

        // Find all matching rules (already sorted by priority)
        for rule in &self.rules {
            if rule.pattern.is_match(test_name) {
                matching_rules.push(rule.clone());
            }
        }

        // Apply rules - each rule can override threads and/or timeout independently
        for rule in &matching_rules {
            if let Some(t) = rule.threads_required {
                effective_threads = effective_threads.max(t);
            }
            if let Some(t) = rule.timeout_ms {
                effective_timeout = effective_timeout.max(t);
            }
        }

        TestClassification {
            test_name: test_name.to_string(),
            matching_rules,
            effective_threads,
            effective_timeout_ms: effective_timeout,
            default_threads: self.default_threads,
            default_timeout_ms: self.default_timeout_ms,
        }
    }

    /// Determine what classification a test SHOULD have based on actual usage.
    #[allow(clippy::too_many_arguments)]
    pub fn suggest_classification(
        &self,
        _test_name: &str,
        avg_cpu: Option<f64>,
        duration_ms: u64,
        time_above_1t_ms: Option<u64>,
        time_above_2t_ms: Option<u64>,
        time_above_3t_ms: Option<u64>,
        time_above_4t_ms: Option<u64>,
        exceedance_pct: f64,
        timeout_headroom: f64,
    ) -> SuggestedClassification {
        let mut thread_options: Vec<u32> = vec![self.default_threads];
        thread_options.extend(self.rules.iter().filter_map(|r| r.threads_required));
        thread_options.sort();
        thread_options.dedup();

        let duration = duration_ms.max(1) as f64;

        let suggested_threads = thread_options
            .iter()
            .find(|&&t| {
                let time_above = match t {
                    1 => time_above_1t_ms,
                    2 => time_above_2t_ms,
                    3 => time_above_3t_ms,
                    4 => time_above_4t_ms,
                    _ => None,
                };
                // If telemetry is missing, treat as unknown (don't assume it fits)
                let Some(time_above) = time_above else {
                    return false;
                };
                let pct = time_above as f64 / duration;
                pct <= exceedance_pct
            })
            .copied()
            .unwrap_or(self.default_threads);

        // Sanity floor: if avg_cpu exceeds a bucket, don't use that bucket
        let suggested_threads = if let Some(avg_cpu) = avg_cpu {
            thread_options
                .iter()
                .find(|&&t| t >= suggested_threads && (t as f64) >= avg_cpu)
                .copied()
                .or_else(|| thread_options.last().copied())
                .unwrap_or(suggested_threads)
        } else {
            suggested_threads
        };

        let mut timeout_options: Vec<u64> = vec![self.default_timeout_ms];
        timeout_options.extend(self.rules.iter().filter_map(|r| r.timeout_ms));
        timeout_options.sort();
        timeout_options.dedup();

        // Apply headroom: bump to the next tier when duration exceeds
        // `timeout * headroom`. E.g., with 0.50 headroom a test taking 31s
        // exceeds 50% of the 60s default and gets bumped to the slow tier.
        let suggested_timeout = timeout_options
            .iter()
            .find(|&&t| duration_ms as f64 <= t as f64 * timeout_headroom)
            .copied()
            .unwrap_or_else(|| *timeout_options.last().unwrap_or(&self.default_timeout_ms));

        let mut suggested_rules = Vec::new();
        for rule in &self.rules {
            let matches_threads = rule
                .threads_required
                .map(|t| t == suggested_threads)
                .unwrap_or(false);
            let matches_timeout = rule
                .timeout_ms
                .map(|t| t == suggested_timeout)
                .unwrap_or(false);
            if (matches_threads || matches_timeout) && !suggested_rules.contains(&rule.name) {
                suggested_rules.push(rule.name.clone());
            }
        }

        SuggestedClassification {
            threads_required: suggested_threads,
            timeout_ms: suggested_timeout,
            rule_names: suggested_rules,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestClassification {
    pub test_name: String,
    pub matching_rules: Vec<ClassificationRule>,
    pub effective_threads: u32,
    pub effective_timeout_ms: u64,
    pub default_threads: u32,
    pub default_timeout_ms: u64,
}

impl TestClassification {
    pub fn rule_names(&self) -> Vec<String> {
        self.matching_rules.iter().map(|r| r.name.clone()).collect()
    }

    pub fn is_default(&self) -> bool {
        self.matching_rules.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SuggestedClassification {
    pub threads_required: u32,
    pub timeout_ms: u64,
    pub rule_names: Vec<String>,
}

/// Parse timeout string like "30s" or "90s" into milliseconds
fn parse_timeout(slow_timeout: &Option<SlowTimeout>, default: u64) -> u64 {
    match slow_timeout {
        Some(st) => {
            let period = &st.period;
            let multiplier = st.terminate_after.unwrap_or(2) as u64;

            let seconds = if period.ends_with('s') {
                period.trim_end_matches('s').parse::<u64>().unwrap_or(30)
            } else if period.ends_with('m') {
                period.trim_end_matches('m').parse::<u64>().unwrap_or(1) * 60
            } else {
                period.parse::<u64>().unwrap_or(30)
            };

            seconds * 1000 * multiplier
        }
        None => default,
    }
}

/// Extract regex pattern from nextest filter like 'test(/.*slow_.*/)'
fn extract_test_pattern(filter: &str) -> Option<String> {
    if let Some(start) = filter.find("test(") {
        let rest = &filter[start + 5..];
        if let Some(end) = rest.rfind(')') {
            let inner = &rest[..end];
            let pattern = inner.trim_matches('/');
            if let Some(stripped) = pattern.strip_prefix('~') {
                return Some(format!(".*{}.*", stripped));
            }
            return Some(pattern.to_string());
        }
    }

    if filter.starts_with('/') && filter.ends_with('/') {
        return Some(filter[1..filter.len() - 1].to_string());
    }

    None
}

/// Extract a human-readable name from a filter pattern
fn extract_rule_name(filter: &str) -> String {
    let pattern = extract_test_pattern(filter).unwrap_or_else(|| filter.to_string());

    for prefix in &["slow_", "heavy4_", "heavy3_", "heavy_", "serial_"] {
        if pattern.contains(prefix) {
            return prefix.trim_end_matches('_').to_string();
        }
    }

    pattern
        .replace(".*", "")
        .replace('_', "")
        .chars()
        .take(20)
        .collect()
}

// ============================================================================
// Aggregation
// ============================================================================

#[derive(Debug, Clone)]
pub struct TestAggregation {
    pub test_name: String,
    pub run_count: usize,
    pub avg_peak_cpu: Option<f64>,
    pub avg_avg_cpu: Option<f64>,
    pub avg_p50_cpu: Option<f64>,
    pub avg_p90_cpu: Option<f64>,
    pub avg_duration_ms: u64,
    pub avg_time_at_p90_ms: Option<u64>,
    pub avg_time_near_peak_ms: Option<u64>,
    pub avg_time_above_1t_ms: Option<u64>,
    pub avg_time_above_2t_ms: Option<u64>,
    pub avg_time_above_3t_ms: Option<u64>,
    pub avg_time_above_4t_ms: Option<u64>,
    pub max_peak_cpu: f64,
    pub max_duration_ms: u64,
    // Memory aggregation fields
    pub avg_peak_rss_bytes: Option<u64>,
    pub avg_avg_rss_bytes: Option<u64>,
    pub avg_p90_rss_bytes: Option<u64>,
    pub max_peak_rss_bytes: Option<u64>,
}

/// Helper to compute the average of optional f64 fields across runs.
/// Returns `None` if none of the runs have the field set.
fn avg_opt_f64<'a>(
    runs: impl Iterator<Item = &'a TestStats>,
    f: fn(&TestStats) -> Option<f64>,
) -> Option<f64> {
    let values: Vec<f64> = runs.filter_map(f).collect();
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// Helper to compute the average of optional u64 fields across runs.
/// Returns `None` if none of the runs have the field set.
fn avg_opt_u64<'a>(
    runs: impl Iterator<Item = &'a TestStats>,
    f: fn(&TestStats) -> Option<u64>,
) -> Option<u64> {
    let values: Vec<u64> = runs.filter_map(f).collect();
    if values.is_empty() {
        None
    } else {
        let sum: u128 = values.iter().map(|&v| v as u128).sum();
        Some((sum / values.len() as u128) as u64)
    }
}

/// Helper to compute the max of optional u64 fields across runs.
fn max_opt_u64<'a>(
    runs: impl Iterator<Item = &'a TestStats>,
    f: fn(&TestStats) -> Option<u64>,
) -> Option<u64> {
    runs.filter_map(f).max()
}

fn aggregate_by_test(stats: &AggregatedStats) -> Vec<TestAggregation> {
    let mut by_name: HashMap<String, Vec<&TestStats>> = HashMap::new();

    for test in &stats.tests {
        let name = test
            .test_name
            .clone()
            .unwrap_or_else(|| test.binary.clone());
        by_name.entry(name).or_default().push(test);
    }

    by_name
        .into_iter()
        .map(|(name, runs)| {
            let run_count = runs.len();

            // CPU fields: average only over runs that have telemetry data
            let avg_peak_cpu = avg_opt_f64(runs.iter().copied(), |r| r.peak_cpu);
            let avg_avg_cpu = avg_opt_f64(runs.iter().copied(), |r| r.avg_cpu);
            let avg_p50_cpu = avg_opt_f64(runs.iter().copied(), |r| r.p50_cpu);
            let avg_p90_cpu = avg_opt_f64(runs.iter().copied(), |r| r.p90_cpu);
            let avg_duration_ms =
                runs.iter().map(|r| r.duration_ms).sum::<u64>() / run_count as u64;
            let avg_time_at_p90_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_at_p90_ms);
            let avg_time_near_peak_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_near_peak_ms);
            let avg_time_above_1t_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_above_1t_ms);
            let avg_time_above_2t_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_above_2t_ms);
            let avg_time_above_3t_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_above_3t_ms);
            let avg_time_above_4t_ms = avg_opt_u64(runs.iter().copied(), |r| r.time_above_4t_ms);
            let max_peak_cpu = runs
                .iter()
                .map(|r| r.peak_cpu.unwrap_or(0.0))
                .fold(0.0f64, f64::max);
            let max_duration_ms = runs.iter().map(|r| r.duration_ms).max().unwrap_or(0);

            // Memory fields
            let avg_peak_rss_bytes = avg_opt_u64(runs.iter().copied(), |r| r.peak_rss_bytes);
            let avg_avg_rss_bytes = avg_opt_u64(runs.iter().copied(), |r| r.avg_rss_bytes);
            let avg_p90_rss_bytes = avg_opt_u64(runs.iter().copied(), |r| r.p90_rss_bytes);
            let max_peak_rss_bytes = max_opt_u64(runs.iter().copied(), |r| r.peak_rss_bytes);

            TestAggregation {
                test_name: name,
                run_count,
                avg_peak_cpu,
                avg_avg_cpu,
                avg_p50_cpu,
                avg_p90_cpu,
                avg_duration_ms,
                avg_time_at_p90_ms,
                avg_time_near_peak_ms,
                avg_time_above_1t_ms,
                avg_time_above_2t_ms,
                avg_time_above_3t_ms,
                avg_time_above_4t_ms,
                max_peak_cpu,
                max_duration_ms,
                avg_peak_rss_bytes,
                avg_avg_rss_bytes,
                avg_p90_rss_bytes,
                max_peak_rss_bytes,
            }
        })
        .collect()
}

// ============================================================================
// Reclassification analysis
// ============================================================================

#[derive(Debug, Clone)]
pub struct Reclassification {
    pub test_name: String,
    pub stats: TestAggregation,
    pub current: TestClassification,
    pub suggested: SuggestedClassification,
    pub issues: Vec<String>,
}

impl Reclassification {
    pub fn needs_change(&self) -> bool {
        !self.issues.is_empty()
    }
}

fn analyze_reclassifications(
    stats: &AggregatedStats,
    config: &ClassificationConfig,
) -> Vec<Reclassification> {
    let aggregated = aggregate_by_test(stats);
    let mut results = Vec::new();

    for test in aggregated {
        let current = config.classify(&test.test_name);
        let suggested = config.suggest_classification(
            &test.test_name,
            test.avg_avg_cpu,
            test.avg_duration_ms,
            test.avg_time_above_1t_ms,
            test.avg_time_above_2t_ms,
            test.avg_time_above_3t_ms,
            test.avg_time_above_4t_ms,
            DEFAULT_EXCEEDANCE_PCT,
            DEFAULT_TIMEOUT_HEADROOM,
        );

        let mut issues = Vec::new();

        // Determine time above current allocation
        let (time_above_allocation_ms, allocation_threshold) = match current.effective_threads {
            1 => (test.avg_time_above_1t_ms, 1.0),
            2 => (test.avg_time_above_2t_ms, 2.0),
            3 => (test.avg_time_above_3t_ms, 3.0),
            4 => (test.avg_time_above_4t_ms, 4.0),
            t => {
                if t < 2 {
                    (test.avg_time_above_1t_ms, 1.0)
                } else if t < 3 {
                    (test.avg_time_above_2t_ms, 2.0)
                } else if t < 4 {
                    (test.avg_time_above_3t_ms, 3.0)
                } else {
                    (test.avg_time_above_4t_ms, 4.0)
                }
            }
        };

        // Only check CPU classification if telemetry is available
        if let Some(time_above_ms) = time_above_allocation_ms {
            let pct_above_allocation = if test.avg_duration_ms > 0 {
                (time_above_ms as f64 / test.avg_duration_ms as f64) * 100.0
            } else {
                0.0
            };

            // Check CPU classification using sustained exceedance
            let exceedance_threshold = DEFAULT_EXCEEDANCE_PCT * 100.0;
            if pct_above_allocation > exceedance_threshold {
                issues.push(format!(
                    "CPU regularly exceeds {}T for {:.0}% of runtime (>{:.0}% threshold): avg={:.2}x, peak={:.2}x, above {}T for {:.1}s",
                    current.effective_threads,
                    pct_above_allocation,
                    exceedance_threshold,
                    test.avg_avg_cpu.unwrap_or(0.0),
                    test.avg_peak_cpu.unwrap_or(0.0),
                    allocation_threshold as u32,
                    time_above_ms as f64 / 1000.0,
                ));
            } else if suggested.threads_required < current.effective_threads
                && current.effective_threads > config.default_threads
            {
                // Check async-wait ratio: if the test spends >70% of its time
                // below 1T CPU, it's I/O/coordination-bound (async waits for
                // block migration, chunk sync, etc.) rather than compute-bound.
                // These tests are sensitive to system contention — the slot
                // reservation protects them from noisy neighbors, not because
                // they need the CPU themselves.
                let pct_above_1t = test
                    .avg_time_above_1t_ms
                    .map(|t| t as f64 / test.avg_duration_ms.max(1) as f64)
                    .unwrap_or(0.0);
                let high_idle = pct_above_1t < 0.30;

                if high_idle {
                    issues.push(format!(
                        "CPU over-allocated: avg={:.2}x, above {}T for only {:.0}% of runtime, but allocated {}T \
                         — WARNING: test is {:.0}% idle (I/O/coordination-bound), slot reservation may protect against contention. Verify manually",
                        test.avg_avg_cpu.unwrap_or(0.0),
                        allocation_threshold as u32,
                        pct_above_allocation,
                        current.effective_threads,
                        (1.0 - pct_above_1t) * 100.0,
                    ));
                } else {
                    issues.push(format!(
                        "CPU over-allocated: avg={:.2}x, above {}T for only {:.0}% of runtime, but allocated {}T - could downgrade",
                        test.avg_avg_cpu.unwrap_or(0.0),
                        allocation_threshold as u32,
                        pct_above_allocation,
                        current.effective_threads,
                    ));
                }

                // Warn when the suggestion drops more than one tier (e.g.
                // heavy4→default). The raw suggestion is still surfaced so the
                // operator can see the target, but large jumps have historically
                // introduced flakiness and should be verified incrementally.
                let tier_drop = current
                    .effective_threads
                    .saturating_sub(suggested.threads_required);
                if tier_drop > 1 {
                    issues.push(format!(
                        "NOTE: suggestion drops {}T→{}T ({} tiers). Consider stepping down one tier at a time and verifying stability at each level",
                        current.effective_threads,
                        suggested.threads_required,
                        tier_drop,
                    ));
                }
            }
        }

        // Check timeout classification
        if test.avg_duration_ms > current.effective_timeout_ms {
            issues.push(format!(
                "Duration exceeds timeout: {:.1}s but timeout is {:.1}s",
                test.avg_duration_ms as f64 / 1000.0,
                current.effective_timeout_ms as f64 / 1000.0
            ));
        } else if test.max_duration_ms > current.effective_timeout_ms {
            issues.push(format!(
                "Max duration exceeds timeout: {:.1}s but timeout is {:.1}s (flaky timing?)",
                test.max_duration_ms as f64 / 1000.0,
                current.effective_timeout_ms as f64 / 1000.0
            ));
        } else if suggested.timeout_ms < current.effective_timeout_ms
            && current.effective_timeout_ms > config.default_timeout_ms
        {
            issues.push(format!(
                "Timeout over-allocated: {:.1}s duration but {:.1}s timeout - could remove slow_",
                test.avg_duration_ms as f64 / 1000.0,
                current.effective_timeout_ms as f64 / 1000.0
            ));
        }

        results.push(Reclassification {
            test_name: test.test_name.clone(),
            stats: test,
            current,
            suggested,
            issues,
        });
    }

    // Sort by number of issues (most issues first), then by severity
    results.sort_by(|a, b| {
        b.issues.len().cmp(&a.issues.len()).then_with(|| {
            let a_cpu_issue = a.issues.iter().any(|i| i.contains("CPU regularly exceeds"));
            let b_cpu_issue = b.issues.iter().any(|i| i.contains("CPU regularly exceeds"));
            b_cpu_issue.cmp(&a_cpu_issue)
        })
    });

    results
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser)]
#[command(name = "nextest-report")]
#[command(about = "Analyze CPU and memory usage statistics from nextest test runs")]
struct Cli {
    /// Path to the stats JSON file
    #[arg(short, long, default_value = "target/nextest-monitor/stats")]
    input: PathBuf,

    /// Path to nextest.toml config (default: .config/nextest.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show a summary of all tests sorted by CPU usage
    Summary {
        /// Sort by: p90, peak, avg, duration, nearpeak, atp90, peak_rss, avg_rss
        #[arg(short, long, default_value = "p90")]
        sort: String,

        /// Show top N tests (0 for all)
        #[arg(short, long, default_value = "0")]
        top: usize,
    },

    /// Analyze tests and suggest prefix reclassifications
    Analyze {
        /// Output format: text, json
        #[arg(short, long, default_value = "text")]
        format: String,

        /// Show all tests, not just those needing reclassification
        #[arg(long)]
        all: bool,
    },

    /// Show the parsed configuration from nextest.toml
    Config,

    /// Show detailed stats for a specific test
    Detail {
        /// Test name pattern to match
        pattern: String,
    },

    /// Export stats in various formats
    Export {
        /// Output format: csv, json
        #[arg(short, long, default_value = "csv")]
        format: String,

        /// Output file (stdout if not specified)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Apply suggested reclassifications by renaming test functions in source files
    Apply {
        /// Dry run: show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,

        /// Root directory to search for source files (default: current directory)
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Clear the stats file
    Clear,

    #[cfg(feature = "heap-profile")]
    /// Show heap profiling results for a test
    Heap {
        /// Test name pattern to show heap profile for
        pattern: String,
        /// Number of top allocation sites to show
        #[arg(short, long, default_value = "20")]
        top: usize,
    },

    #[cfg(feature = "heap-profile")]
    /// List available heap profiles
    HeapList,
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    // Load nextest config
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(".config/nextest.toml"));
    let config = if config_path.exists() {
        match ClassificationConfig::from_nextest_toml(&config_path) {
            Ok(c) => {
                eprintln!("Loaded config from: {}", config_path.display());
                c
            }
            Err(e) => {
                eprintln!("Warning: {}", e);
                eprintln!("Using default configuration");
                ClassificationConfig::default_config()
            }
        }
    } else {
        eprintln!(
            "No nextest.toml found at {}, using defaults",
            config_path.display()
        );
        ClassificationConfig::default_config()
    };

    match cli.command {
        Commands::Clear => {
            if cli.input.exists() {
                fs::remove_file(&cli.input)?;
                println!("Cleared stats file: {}", cli.input.display());
            } else {
                println!("Stats file does not exist: {}", cli.input.display());
            }
        }
        Commands::Config => {
            cmd_config(&config);
        }
        #[cfg(feature = "heap-profile")]
        Commands::Heap { pattern, top } => {
            let stats = load_stats(&cli.input)?;
            cmd_heap(&stats, &pattern, top)?;
        }
        #[cfg(feature = "heap-profile")]
        Commands::HeapList => {
            let stats = load_stats(&cli.input)?;
            cmd_heap_list(&stats);
        }
        _ => {
            let stats = load_stats(&cli.input)?;

            match cli.command {
                Commands::Summary { sort, top } => cmd_summary(&stats, &config, &sort, top),
                Commands::Analyze { format, all } => cmd_analyze(&stats, &config, &format, all),
                Commands::Detail { pattern } => cmd_detail(&stats, &config, &pattern),
                Commands::Export { format, output } => {
                    cmd_export(&stats, &format, output.as_ref())?
                }
                Commands::Apply { dry_run, root } => {
                    let root = root.unwrap_or_else(|| PathBuf::from("."));
                    cmd_apply(&stats, &config, &root, dry_run)?;
                }
                Commands::Clear | Commands::Config => unreachable!(),
                #[cfg(feature = "heap-profile")]
                Commands::Heap { .. } | Commands::HeapList => unreachable!(),
            }
        }
    }

    Ok(())
}

fn load_stats(path: &std::path::Path) -> std::io::Result<AggregatedStats> {
    AggregatedStats::load(path)
}

fn cmd_config(config: &ClassificationConfig) {
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         NEXTEST CONFIGURATION                               ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("Defaults:");
    println!("  Threads required: {}", config.default_threads);
    println!(
        "  Timeout:          {:.0}s",
        config.default_timeout_ms as f64 / 1000.0
    );
    println!();
    println!("Classification Rules (by priority):");
    println!(
        "{:<15} {:>10} {:>12} {:>8}  Pattern",
        "Name", "Threads", "Timeout", "Priority"
    );
    println!("{}", "-".repeat(80));

    for rule in &config.rules {
        let threads_str = rule
            .threads_required
            .map(|t| format!("{}", t))
            .unwrap_or_else(|| "-".to_string());
        let timeout_str = rule
            .timeout_ms
            .map(|t| format!("{}s", t / 1000))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<15} {:>10} {:>12} {:>8}  {}",
            rule.name,
            threads_str,
            timeout_str,
            rule.priority,
            rule.pattern.as_str()
        );
    }
}

fn format_rss(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn cmd_summary(stats: &AggregatedStats, config: &ClassificationConfig, sort: &str, top: usize) {
    let mut aggregated = aggregate_by_test(stats);

    // Check if any test has memory data
    let has_memory = aggregated.iter().any(|t| {
        t.avg_peak_rss_bytes.is_some()
            || t.avg_avg_rss_bytes.is_some()
            || t.avg_p90_rss_bytes.is_some()
            || t.max_peak_rss_bytes.is_some()
    });

    match sort {
        "p90" => aggregated.sort_by(|a, b| {
            b.avg_p90_cpu
                .unwrap_or(0.0)
                .total_cmp(&a.avg_p90_cpu.unwrap_or(0.0))
        }),
        "peak" => aggregated.sort_by(|a, b| {
            b.avg_peak_cpu
                .unwrap_or(0.0)
                .total_cmp(&a.avg_peak_cpu.unwrap_or(0.0))
        }),
        "avg" => aggregated.sort_by(|a, b| {
            b.avg_avg_cpu
                .unwrap_or(0.0)
                .total_cmp(&a.avg_avg_cpu.unwrap_or(0.0))
        }),
        "duration" => aggregated.sort_by(|a, b| b.avg_duration_ms.cmp(&a.avg_duration_ms)),
        "nearpeak" => aggregated.sort_by(|a, b| {
            b.avg_time_near_peak_ms
                .unwrap_or(0)
                .cmp(&a.avg_time_near_peak_ms.unwrap_or(0))
        }),
        "atp90" => aggregated.sort_by(|a, b| {
            b.avg_time_at_p90_ms
                .unwrap_or(0)
                .cmp(&a.avg_time_at_p90_ms.unwrap_or(0))
        }),
        "peak_rss" => aggregated.sort_by(|a, b| {
            b.avg_peak_rss_bytes
                .unwrap_or(0)
                .cmp(&a.avg_peak_rss_bytes.unwrap_or(0))
        }),
        "avg_rss" => aggregated.sort_by(|a, b| {
            b.avg_avg_rss_bytes
                .unwrap_or(0)
                .cmp(&a.avg_avg_rss_bytes.unwrap_or(0))
        }),
        _ => {
            eprintln!("Unknown sort option: {}. Using 'p90'.", sort);
            aggregated.sort_by(|a, b| {
                b.avg_p90_cpu
                    .unwrap_or(0.0)
                    .total_cmp(&a.avg_p90_cpu.unwrap_or(0.0))
            });
        }
    }

    let to_show = if top > 0 {
        top.min(aggregated.len())
    } else {
        aggregated.len()
    };

    if has_memory {
        println!(
            "{:<45} {:>5} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6} {:>7} {:>7} {:>4}",
            "Test Name",
            "Alloc",
            "P90",
            "Peak",
            "Avg",
            "Duration",
            "≥P90",
            "NrPk",
            "PeakRSS",
            "AvgRSS",
            "Runs"
        );
        println!("{}", "-".repeat(125));
    } else {
        println!(
            "{:<45} {:>5} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6} {:>4}",
            "Test Name", "Alloc", "P90", "Peak", "Avg", "Duration", "≥P90", "NrPk", "Runs"
        );
        println!("{}", "-".repeat(110));
    }

    for test in aggregated.iter().take(to_show) {
        let name = if test.test_name.len() > 43 {
            format!("...{}", &test.test_name[test.test_name.len() - 40..])
        } else {
            test.test_name.clone()
        };

        let classification = config.classify(&test.test_name);
        let alloc_str = format!("{}T", classification.effective_threads);

        let pct_at_p90 = if test.avg_duration_ms > 0 {
            (test.avg_time_at_p90_ms.unwrap_or(0) as f64 / test.avg_duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_near_peak = if test.avg_duration_ms > 0 {
            (test.avg_time_near_peak_ms.unwrap_or(0) as f64 / test.avg_duration_ms as f64) * 100.0
        } else {
            0.0
        };

        let duration_str = format_duration(test.avg_duration_ms);

        if has_memory {
            let peak_rss_str = test
                .avg_peak_rss_bytes
                .map(format_rss)
                .unwrap_or_else(|| "-".to_string());
            let avg_rss_str = test
                .avg_avg_rss_bytes
                .map(format_rss)
                .unwrap_or_else(|| "-".to_string());

            println!(
                "{:<45} {:>5} {:>5.2}x {:>5.2}x {:>5.2}x {:>8} {:>5.0}% {:>5.0}% {:>7} {:>7} {:>4}",
                name,
                alloc_str,
                test.avg_p90_cpu.unwrap_or(0.0),
                test.avg_peak_cpu.unwrap_or(0.0),
                test.avg_avg_cpu.unwrap_or(0.0),
                duration_str,
                pct_at_p90,
                pct_near_peak,
                peak_rss_str,
                avg_rss_str,
                test.run_count
            );
        } else {
            println!(
                "{:<45} {:>5} {:>5.2}x {:>5.2}x {:>5.2}x {:>8} {:>5.0}% {:>5.0}% {:>4}",
                name,
                alloc_str,
                test.avg_p90_cpu.unwrap_or(0.0),
                test.avg_peak_cpu.unwrap_or(0.0),
                test.avg_avg_cpu.unwrap_or(0.0),
                duration_str,
                pct_at_p90,
                pct_near_peak,
                test.run_count
            );
        }
    }

    println!();
    println!(
        "Total tests: {}, Total runs: {}",
        aggregated.len(),
        stats.tests.len()
    );
    println!();
    println!("Columns: Alloc=current allocation, P90=90th percentile, Peak=max, Avg=mean");
    println!("         ≥P90=% time at/above P90, NrPk=% time near peak (≥80% of peak)");
    if has_memory {
        println!("         PeakRSS=peak resident memory, AvgRSS=average resident memory");
    }
}

/// Format duration in a human-readable way
fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format an optional CPU multiplier, showing "N/A" when absent.
fn fmt_opt_cpu(v: Option<f64>) -> String {
    v.map(|x| format!("{:.2}x", x))
        .unwrap_or_else(|| "N/A".to_string())
}

/// Format an optional percentage, showing "N/A" when absent.
fn fmt_opt_pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}%", x))
        .unwrap_or_else(|| "N/A".to_string())
}

fn cmd_analyze(
    stats: &AggregatedStats,
    config: &ClassificationConfig,
    format: &str,
    show_all: bool,
) {
    let reclassifications = analyze_reclassifications(stats, config);

    let to_show: Vec<_> = if show_all {
        reclassifications.iter().collect()
    } else {
        reclassifications
            .iter()
            .filter(|r| r.needs_change())
            .collect()
    };

    match format {
        "json" => {
            let output: Vec<serde_json::Value> = to_show
                .iter()
                .map(|r| {
                    let mut stats_json = serde_json::json!({
                        "avg_peak_cpu": r.stats.avg_peak_cpu,
                        "avg_p90_cpu": r.stats.avg_p90_cpu,
                        "avg_p50_cpu": r.stats.avg_p50_cpu,
                        "avg_avg_cpu": r.stats.avg_avg_cpu,
                        "avg_duration_ms": r.stats.avg_duration_ms,
                        "time_at_p90_ms": r.stats.avg_time_at_p90_ms,
                        "time_near_peak_ms": r.stats.avg_time_near_peak_ms,
                        "time_above_1t_ms": r.stats.avg_time_above_1t_ms,
                        "time_above_2t_ms": r.stats.avg_time_above_2t_ms,
                        "time_above_3t_ms": r.stats.avg_time_above_3t_ms,
                        "time_above_4t_ms": r.stats.avg_time_above_4t_ms,
                        "run_count": r.stats.run_count,
                    });
                    stats_json["avg_peak_rss_bytes"] = r
                        .stats
                        .avg_peak_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    stats_json["avg_avg_rss_bytes"] = r
                        .stats
                        .avg_avg_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    stats_json["avg_p90_rss_bytes"] = r
                        .stats
                        .avg_p90_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    stats_json["max_peak_rss_bytes"] = r
                        .stats
                        .max_peak_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));

                    serde_json::json!({
                        "test_name": r.test_name,
                        "current_classification": {
                            "rules": r.current.rule_names(),
                            "threads": r.current.effective_threads,
                            "timeout_ms": r.current.effective_timeout_ms,
                        },
                        "suggested_classification": {
                            "rules": r.suggested.rule_names,
                            "threads": r.suggested.threads_required,
                            "timeout_ms": r.suggested.timeout_ms,
                        },
                        "issues": r.issues,
                        "stats": stats_json,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        _ => {
            let needs_change: Vec<_> = to_show.iter().filter(|r| r.needs_change()).collect();
            let ok: Vec<_> = to_show.iter().filter(|r| !r.needs_change()).collect();

            if !needs_change.is_empty() {
                println!(
                    "╔══════════════════════════════════════════════════════════════════════════════╗"
                );
                println!(
                    "║                     TESTS NEEDING RECLASSIFICATION                          ║"
                );
                println!(
                    "╚══════════════════════════════════════════════════════════════════════════════╝"
                );
                println!();

                for r in &needs_change {
                    let current_rules = if r.current.is_default() {
                        "(default)".to_string()
                    } else {
                        r.current.rule_names().join(", ")
                    };

                    let (time_above_ms, threshold) = match r.current.effective_threads {
                        1 => (r.stats.avg_time_above_1t_ms, 1),
                        2 => (r.stats.avg_time_above_2t_ms, 2),
                        3 => (r.stats.avg_time_above_3t_ms, 3),
                        4 => (r.stats.avg_time_above_4t_ms, 4),
                        _ => (r.stats.avg_time_above_1t_ms, 1),
                    };
                    let dur = r.stats.avg_duration_ms;
                    let pct_above = time_above_ms
                        .and_then(|t| (dur > 0).then(|| (t as f64 / dur as f64) * 100.0));
                    let pct_at_p90 = r
                        .stats
                        .avg_time_at_p90_ms
                        .and_then(|t| (dur > 0).then(|| (t as f64 / dur as f64) * 100.0));
                    let pct_near_peak = r
                        .stats
                        .avg_time_near_peak_ms
                        .and_then(|t| (dur > 0).then(|| (t as f64 / dur as f64) * 100.0));

                    println!("┌─ {}", r.test_name);
                    println!(
                        "│  Current: {} ({}T, {:.0}s timeout)",
                        current_rules,
                        r.current.effective_threads,
                        r.current.effective_timeout_ms as f64 / 1000.0
                    );
                    println!(
                        "│  CPU:     peak: {} | P90: {} | P50: {} | avg: {}",
                        fmt_opt_cpu(r.stats.avg_peak_cpu),
                        fmt_opt_cpu(r.stats.avg_p90_cpu),
                        fmt_opt_cpu(r.stats.avg_p50_cpu),
                        fmt_opt_cpu(r.stats.avg_avg_cpu)
                    );
                    println!(
                        "│  Time:    {} runs | {} | at P90: {} | near peak: {}",
                        r.stats.run_count,
                        format_duration(r.stats.avg_duration_ms),
                        fmt_opt_pct(pct_at_p90),
                        fmt_opt_pct(pct_near_peak)
                    );
                    if let Some(time_above) = time_above_ms {
                        if time_above > 0 {
                            let pct_str = fmt_opt_pct(pct_above);
                            println!(
                                "│  Above {}T: {:.1}s ({} of runtime)",
                                threshold,
                                time_above as f64 / 1000.0,
                                pct_str
                            );
                        }
                    }
                    if let Some(peak_rss) = r.stats.avg_peak_rss_bytes {
                        let avg_rss_str = r
                            .stats
                            .avg_avg_rss_bytes
                            .map(format_rss)
                            .unwrap_or_else(|| "-".to_string());
                        println!(
                            "│  Memory:  peak: {} | avg: {}",
                            format_rss(peak_rss),
                            avg_rss_str
                        );
                    }

                    for issue in &r.issues {
                        println!("│  ⚠️  {}", issue);
                    }

                    let suggested_rules = if r.suggested.rule_names.is_empty() {
                        "(default)".to_string()
                    } else {
                        r.suggested.rule_names.join(" + ")
                    };
                    println!(
                        "│  → Suggested: {} ({}T, {:.0}s timeout)",
                        suggested_rules,
                        r.suggested.threads_required,
                        r.suggested.timeout_ms as f64 / 1000.0
                    );
                    println!(
                        "└────────────────────────────────────────────────────────────────────────────────"
                    );
                    println!();
                }

                println!(
                    "Summary: {} tests need reclassification",
                    needs_change.len()
                );
            } else {
                println!("All tests are correctly classified!");
            }

            if show_all && !ok.is_empty() {
                println!();
                println!(
                    "╔══════════════════════════════════════════════════════════════════════════════╗"
                );
                println!(
                    "║                        CORRECTLY CLASSIFIED TESTS                           ║"
                );
                println!(
                    "╚══════════════════════════════════════════════════════════════════════════════╝"
                );
                println!();

                for r in &ok {
                    let rules = if r.current.is_default() {
                        "(default)".to_string()
                    } else {
                        r.current.rule_names().join(", ")
                    };

                    println!(
                        "  [{}] {} (peak: {}, P90: {}, avg: {}, {:.1}s)",
                        rules,
                        r.test_name,
                        fmt_opt_cpu(r.stats.avg_peak_cpu),
                        fmt_opt_cpu(r.stats.avg_p90_cpu),
                        fmt_opt_cpu(r.stats.avg_avg_cpu),
                        r.stats.avg_duration_ms as f64 / 1000.0
                    );
                }
            }
        }
    }
}

fn cmd_detail(stats: &AggregatedStats, config: &ClassificationConfig, pattern: &str) {
    let matching: Vec<_> = stats
        .tests
        .iter()
        .filter(|t| {
            t.test_name
                .as_ref()
                .map(|n| n.contains(pattern))
                .unwrap_or(false)
                || t.binary.contains(pattern)
        })
        .collect();

    if matching.is_empty() {
        println!("No tests matching pattern: {}", pattern);
        return;
    }

    for test in matching {
        let name = test.test_name.as_ref().unwrap_or(&test.binary);
        let classification = config.classify(name);

        println!("═══════════════════════════════════════════════════════════════════════════════");
        println!("Test: {}", name);
        println!("═══════════════════════════════════════════════════════════════════════════════");
        println!();
        println!("  Binary:    {}", test.binary);
        println!("  Started:   {}", test.started_at);
        println!("  Passed:    {}", test.passed);
        println!("  Exit Code: {:?}", test.exit_code);
        println!();
        println!("  Classification:");
        if classification.is_default() {
            println!("    Rules:    (default)");
        } else {
            println!("    Rules:    {}", classification.rule_names().join(", "));
        }
        println!("    Threads:  {}", classification.effective_threads);
        println!(
            "    Timeout:  {:.0}s",
            classification.effective_timeout_ms as f64 / 1000.0
        );

        // CPU section
        if test.peak_cpu.is_some() {
            println!();
            println!("  CPU Usage:");
            println!("    Peak:     {:.2}x", test.peak_cpu.unwrap_or(0.0));
            println!("    P90:      {:.2}x", test.p90_cpu.unwrap_or(0.0));
            println!("    P50:      {:.2}x (median)", test.p50_cpu.unwrap_or(0.0));
            println!("    Average:  {:.2}x", test.avg_cpu.unwrap_or(0.0));
            println!();
            println!("  Time Above Allocation Thresholds:");
            let duration_ms = test.duration_ms;
            let pct = |v: Option<u64>| -> f64 {
                if duration_ms > 0 {
                    (v.unwrap_or(0) as f64 / duration_ms as f64) * 100.0
                } else {
                    0.0
                }
            };
            println!(
                "    Above 1T: {:.2}s ({:.1}%)",
                test.time_above_1t_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_above_1t_ms)
            );
            println!(
                "    Above 2T: {:.2}s ({:.1}%)",
                test.time_above_2t_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_above_2t_ms)
            );
            println!(
                "    Above 3T: {:.2}s ({:.1}%)",
                test.time_above_3t_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_above_3t_ms)
            );
            println!(
                "    Above 4T: {:.2}s ({:.1}%)",
                test.time_above_4t_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_above_4t_ms)
            );
            println!();
            println!(
                "  Time at P90 Level (≥{:.2}x):",
                test.p90_cpu.unwrap_or(0.0)
            );
            println!(
                "    Duration: {:.2}s ({:.1}% of runtime)",
                test.time_at_p90_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_at_p90_ms)
            );
            println!();
            let peak = test.peak_cpu.unwrap_or(0.0);
            println!(
                "  Time Near Peak (≥80% of {:.2}x = ≥{:.2}x):",
                peak,
                peak * 0.8
            );
            println!(
                "    Duration: {:.2}s ({:.1}% of runtime)",
                test.time_near_peak_ms.unwrap_or(0) as f64 / 1000.0,
                pct(test.time_near_peak_ms)
            );
        }

        // Memory section
        if test.peak_rss_bytes.is_some() {
            println!();
            println!("  Memory Usage:");
            println!(
                "    Peak RSS: {}",
                format_rss(test.peak_rss_bytes.unwrap_or(0))
            );
            println!(
                "    P90 RSS:  {}",
                format_rss(test.p90_rss_bytes.unwrap_or(0))
            );
            println!(
                "    P50 RSS:  {}",
                format_rss(test.p50_rss_bytes.unwrap_or(0))
            );
            println!(
                "    Avg RSS:  {}",
                format_rss(test.avg_rss_bytes.unwrap_or(0))
            );
            println!();
            println!("  Time Above Memory Thresholds:");
            let duration_ms = test.duration_ms;
            let pct_mem = |v: Option<u64>| -> f64 {
                if duration_ms > 0 {
                    (v.unwrap_or(0) as f64 / duration_ms as f64) * 100.0
                } else {
                    0.0
                }
            };
            println!(
                "    Above 100MB: {:.2}s ({:.1}%)",
                test.time_above_100mb_ms.unwrap_or(0) as f64 / 1000.0,
                pct_mem(test.time_above_100mb_ms)
            );
            println!(
                "    Above 500MB: {:.2}s ({:.1}%)",
                test.time_above_500mb_ms.unwrap_or(0) as f64 / 1000.0,
                pct_mem(test.time_above_500mb_ms)
            );
            println!(
                "    Above 1GB:   {:.2}s ({:.1}%)",
                test.time_above_1gb_ms.unwrap_or(0) as f64 / 1000.0,
                pct_mem(test.time_above_1gb_ms)
            );
        }

        println!();
        println!("  Timing:");
        println!("    Duration: {}", format_duration(test.duration_ms));
        println!();

        if let Some(ref samples) = test.cpu_samples {
            println!("  CPU Usage Over Time:");
            println!();
            print_cpu_chart(samples, classification.effective_threads as f64);
        }

        if let Some(ref samples) = test.memory_samples {
            println!("  Memory Usage Over Time:");
            println!();
            print_memory_chart(samples);
        }

        println!();
    }
}

fn print_cpu_chart(samples: &[CpuSample], allocation_threshold: f64) {
    if samples.is_empty() {
        return;
    }

    let max_cpu = samples.iter().map(|s| s.cpu_threads).fold(0.0f64, f64::max);
    let chart_height = 10;
    let chart_width = 60.min(samples.len());

    let step = samples.len().div_ceil(chart_width);
    let downsampled: Vec<f64> = samples
        .chunks(step)
        .map(|chunk| chunk.iter().map(|s| s.cpu_threads).fold(0.0f64, f64::max))
        .collect();

    let scale = max_cpu.max(allocation_threshold * 1.2);
    let threshold_row = ((allocation_threshold / scale) * chart_height as f64) as usize;

    for row in (0..chart_height).rev() {
        let level = ((row as f64 + 0.5) / chart_height as f64) * scale;
        print!("    {:>5.1}x │", level);

        for &val in &downsampled {
            let bar_height = (val / scale * chart_height as f64) as usize;
            if bar_height > row {
                if val > allocation_threshold {
                    print!("█");
                } else {
                    print!("▒");
                }
            } else if row == threshold_row {
                print!("─");
            } else {
                print!(" ");
            }
        }

        if row == threshold_row {
            print!(" ← {}T", allocation_threshold as u32);
        }
        println!();
    }

    print!("          └");
    for _ in 0..downsampled.len() {
        print!("─");
    }
    println!();

    let duration_s = samples.last().map(|s| s.elapsed_ms / 1000).unwrap_or(0);
    println!(
        "           0s{:>width$}{}s",
        "",
        duration_s,
        width = downsampled.len().saturating_sub(4)
    );
    println!();
    println!(
        "    Legend: █ = above {}T allocation, ▒ = below allocation",
        allocation_threshold as u32
    );
}

fn print_memory_chart(samples: &[MemorySample]) {
    if samples.is_empty() {
        return;
    }

    let max_rss = samples.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
    if max_rss == 0 {
        return;
    }

    let chart_height = 10;
    let chart_width = 60.min(samples.len());

    let step = samples.len().div_ceil(chart_width);
    let downsampled: Vec<u64> = samples
        .chunks(step)
        .map(|chunk| chunk.iter().map(|s| s.rss_bytes).max().unwrap_or(0))
        .collect();

    let scale = max_rss as f64;

    for row in (0..chart_height).rev() {
        let level = ((row as f64 + 0.5) / chart_height as f64) * scale;
        print!("    {:>6} │", format_rss(level as u64));

        for &val in &downsampled {
            let bar_height = (val as f64 / scale * chart_height as f64) as usize;
            if bar_height > row {
                print!("▓");
            } else {
                print!(" ");
            }
        }
        println!();
    }

    print!("           └");
    for _ in 0..downsampled.len() {
        print!("─");
    }
    println!();

    let duration_s = samples.last().map(|s| s.elapsed_ms / 1000).unwrap_or(0);
    println!(
        "            0s{:>width$}{}s",
        "",
        duration_s,
        width = downsampled.len().saturating_sub(4)
    );
}

fn cmd_export(
    stats: &AggregatedStats,
    format: &str,
    output: Option<&PathBuf>,
) -> std::io::Result<()> {
    let aggregated = aggregate_by_test(stats);

    let content = match format {
        "json" => serde_json::to_string_pretty(
            &aggregated
                .iter()
                .map(|t| {
                    let mut json = serde_json::json!({
                        "test_name": t.test_name,
                        "run_count": t.run_count,
                        "avg_peak_cpu": t.avg_peak_cpu,
                        "avg_p90_cpu": t.avg_p90_cpu,
                        "avg_p50_cpu": t.avg_p50_cpu,
                        "avg_avg_cpu": t.avg_avg_cpu,
                        "avg_duration_ms": t.avg_duration_ms,
                        "avg_time_at_p90_ms": t.avg_time_at_p90_ms,
                        "avg_time_near_peak_ms": t.avg_time_near_peak_ms,
                        "avg_time_above_1t_ms": t.avg_time_above_1t_ms,
                        "avg_time_above_2t_ms": t.avg_time_above_2t_ms,
                        "avg_time_above_3t_ms": t.avg_time_above_3t_ms,
                        "avg_time_above_4t_ms": t.avg_time_above_4t_ms,
                        "max_peak_cpu": t.max_peak_cpu,
                        "max_duration_ms": t.max_duration_ms,
                    });

                    json["avg_peak_rss_bytes"] = t
                        .avg_peak_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    json["avg_avg_rss_bytes"] = t
                        .avg_avg_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    json["avg_p90_rss_bytes"] = t
                        .avg_p90_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));
                    json["max_peak_rss_bytes"] = t
                        .max_peak_rss_bytes
                        .map_or(serde_json::Value::Null, |v| serde_json::json!(v));

                    json
                })
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        _ => {
            let mut lines = vec![
                "test_name,run_count,avg_peak_cpu,avg_p90_cpu,avg_p50_cpu,avg_avg_cpu,avg_duration_ms,time_at_p90_ms,time_near_peak_ms,time_above_1t_ms,time_above_2t_ms,time_above_3t_ms,time_above_4t_ms,max_peak_cpu,max_duration_ms,avg_peak_rss_bytes,avg_avg_rss_bytes,avg_p90_rss_bytes,max_peak_rss_bytes".to_string()
            ];
            for t in &aggregated {
                lines.push(format!(
                    "\"{}\",{},{},{},{},{},{},{},{},{},{},{},{},{:.4},{},{},{},{},{}",
                    t.test_name.replace('"', "\"\""),
                    t.run_count,
                    t.avg_peak_cpu
                        .map(|v| format!("{:.4}", v))
                        .unwrap_or_default(),
                    t.avg_p90_cpu
                        .map(|v| format!("{:.4}", v))
                        .unwrap_or_default(),
                    t.avg_p50_cpu
                        .map(|v| format!("{:.4}", v))
                        .unwrap_or_default(),
                    t.avg_avg_cpu
                        .map(|v| format!("{:.4}", v))
                        .unwrap_or_default(),
                    t.avg_duration_ms,
                    t.avg_time_at_p90_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_time_near_peak_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_time_above_1t_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_time_above_2t_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_time_above_3t_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_time_above_4t_ms
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.max_peak_cpu,
                    t.max_duration_ms,
                    t.avg_peak_rss_bytes
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_avg_rss_bytes
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.avg_p90_rss_bytes
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    t.max_peak_rss_bytes
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                ));
            }
            lines.join("\n")
        }
    };

    match output {
        Some(path) => {
            let mut file = File::create(path)?;
            file.write_all(content.as_bytes())?;
            println!("Exported to: {}", path.display());
        }
        None => {
            println!("{}", content);
        }
    }

    Ok(())
}

// ============================================================================
// Apply reclassifications
// ============================================================================

/// Metadata for one capacity class derived from the nextest config.
#[derive(Debug, Clone)]
struct CapacityClass {
    name: String,
    threads: Option<u32>,
    timeout_ms: Option<u64>,
}

/// Collect the set of known capacity-class names from the loaded config rules.
fn capacity_classes_from_config(config: &ClassificationConfig) -> Vec<CapacityClass> {
    config
        .rules
        .iter()
        .map(|r| CapacityClass {
            name: r.name.clone(),
            threads: r.threads_required,
            timeout_ms: r.timeout_ms,
        })
        .collect()
}

/// Parse the leading capacity-class prefix from a function name.
///
/// Greedily consumes segments (`"<class>_"`) from the start of the name as
/// long as each segment is a recognised capacity class. Returns the list of
/// matched class names and the byte length of the prefix consumed.
///
/// A segment only "counts" if every segment before it is also a capacity
/// class — i.e. they must form a contiguous leading prefix.
///
/// Examples (given classes = ["slow", "heavy", "heavy3", "heavy4", "serial"]):
///   `"slow_heavy_foo"`  → (["slow","heavy"], 11)
///   `"heavy_slow_foo"`  → (["heavy","slow"], 11) — any order
///   `"test_slow_foo"`   → ([], 0)                — `test` is not a class
///   `"heavy3_bar"`      → (["heavy3"], 7)
fn parse_capacity_prefix(func_name: &str, classes: &[CapacityClass]) -> (Vec<String>, usize) {
    let mut matched = Vec::new();
    let mut pos = 0;
    // Sort class names longest-first so "heavy3" is tried before "heavy".
    let mut sorted_names: Vec<&str> = classes.iter().map(|c| c.name.as_str()).collect();
    sorted_names.sort_by(|a, b| b.len().cmp(&a.len()));
    loop {
        let remaining = &func_name[pos..];
        let mut found = false;
        for &name in &sorted_names {
            let token = format!("{name}_");
            if remaining.starts_with(&token) {
                matched.push(name.to_string());
                pos += token.len();
                found = true;
                break;
            }
        }
        if !found {
            break;
        }
    }
    (matched, pos)
}

/// Build the canonical prefix string for a set of capacity classes.
///
/// The canonical order is: timeout classes first (e.g. `slow_`), then thread
/// classes in ascending order (`heavy_`, `heavy3_`, `heavy4_`), then others
/// (e.g. `serial_`).
fn classes_to_prefix(matched: &[String], all_classes: &[CapacityClass]) -> String {
    // Partition into timeout-only, thread, and other classes
    let mut timeout_parts = Vec::new();
    let mut thread_parts: Vec<(u32, &str)> = Vec::new();
    let mut other_parts = Vec::new();
    for name in matched {
        if let Some(cls) = all_classes.iter().find(|c| c.name == *name) {
            if cls.threads.is_some() {
                thread_parts.push((cls.threads.unwrap(), name.as_str()));
            } else if cls.timeout_ms.is_some() {
                timeout_parts.push(name.as_str());
            } else {
                other_parts.push(name.as_str());
            }
        } else {
            other_parts.push(name.as_str());
        }
    }
    // Only keep the highest thread class (don't emit both heavy_ and heavy3_)
    thread_parts.sort_by(|a, b| b.0.cmp(&a.0));
    thread_parts.truncate(1);

    let mut parts: Vec<&str> = Vec::new();
    parts.extend(timeout_parts);
    parts.extend(thread_parts.iter().map(|(_, n)| *n));
    parts.extend(other_parts);

    if parts.is_empty() {
        String::new()
    } else {
        format!("{}_", parts.join("_"))
    }
}

/// Convert a suggested classification (threads, timeout) into the canonical
/// function-name prefix, using the class names from the loaded config.
fn classification_to_prefix(
    threads: u32,
    timeout_ms: u64,
    config: &ClassificationConfig,
    all_classes: &[CapacityClass],
) -> String {
    let mut matched = Vec::new();

    // Find the timeout class (if any)
    if timeout_ms > config.default_timeout_ms {
        if let Some(cls) = all_classes
            .iter()
            .find(|c| c.timeout_ms == Some(timeout_ms))
        {
            matched.push(cls.name.clone());
        }
    }

    // Find the thread class (if any)
    if threads > config.default_threads {
        if let Some(cls) = all_classes.iter().find(|c| c.threads == Some(threads)) {
            matched.push(cls.name.clone());
        }
    }

    classes_to_prefix(&matched, all_classes)
}

/// Map a thread class name to its thread count for step-down comparison.
fn thread_class_tier(name: &str, all_classes: &[CapacityClass]) -> u32 {
    all_classes
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.threads)
        .unwrap_or(1)
}

/// Clamp a suggested thread count so it drops at most one tier from the
/// current thread class. Up-allocations are never clamped.
fn clamp_thread_downsize(
    current_thread_class: Option<&str>,
    suggested_threads: u32,
    all_classes: &[CapacityClass],
) -> u32 {
    let current_tier = current_thread_class
        .map(|n| thread_class_tier(n, all_classes))
        .unwrap_or(1);
    if suggested_threads >= current_tier {
        return suggested_threads;
    }
    // Build sorted list of available thread tiers
    let mut tiers: Vec<u32> = vec![1]; // default
    tiers.extend(all_classes.iter().filter_map(|c| c.threads));
    tiers.sort();
    tiers.dedup();
    // Find the tier one step below current
    let current_idx = tiers.iter().position(|&t| t == current_tier).unwrap_or(0);
    let min_tier = if current_idx > 0 {
        tiers[current_idx - 1]
    } else {
        1
    };
    suggested_threads.max(min_tier)
}

/// Format a prefix string for display (showing "(default)" for empty prefix).
fn display_prefix(prefix: &str) -> &str {
    if prefix.is_empty() {
        "(default)"
    } else {
        prefix.trim_end_matches('_')
    }
}

/// Extract the function name from a test path (strips module path and case suffixes).
fn extract_func_from_path(test_path: &str) -> &str {
    // Strip ::case_N suffixes
    let base = if let Some(idx) = test_path.find("::case_") {
        &test_path[..idx]
    } else {
        test_path
    };
    // Function name is the last :: segment
    base.rsplit("::").next().unwrap_or(base)
}

/// A rename action: old function name → new function name, with the file it was found in.
#[derive(Debug, Clone)]
struct RenameAction {
    old_name: String,
    new_name: String,
    file_path: PathBuf,
    line_number: usize,
}

/// Recursively find all .rs files under `root`, skipping target/ and hidden directories.
fn find_rs_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Skip hidden dirs, target/, .worktrees/
                if name.starts_with('.') || name == "target" {
                    continue;
                }
            }
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    walk(root, &mut files);
    files
}

/// Find the file and line where `fn <func_name>` is defined.
fn find_function_def(func_name: &str, rs_files: &[PathBuf]) -> Option<(PathBuf, usize)> {
    // Match patterns like: `fn func_name(`, `fn func_name (`, `async fn func_name(`
    let pattern = format!("fn {func_name}");
    for file_path in rs_files {
        let Ok(content) = fs::read_to_string(file_path) else {
            continue;
        };
        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.contains(&pattern) {
                // Verify it's actually a function definition, not just a comment or string
                let before_fn = trimmed.split("fn ").next().unwrap_or("");
                // Should be empty, start with pub/async/unsafe, or be a #[test] etc.
                if before_fn.is_empty()
                    || before_fn.ends_with("pub ")
                    || before_fn.ends_with("async ")
                    || before_fn.ends_with("pub async ")
                    || before_fn.ends_with("pub(crate) ")
                    || before_fn.ends_with("pub(crate) async ")
                    || before_fn.ends_with("unsafe ")
                    || before_fn.ends_with("pub unsafe ")
                    || before_fn.ends_with("const ")
                    || before_fn.ends_with("pub const ")
                {
                    // Confirm the character after the name is `(` or `<` or whitespace
                    let after_name_start = trimmed.find(&pattern).unwrap() + pattern.len();
                    if let Some(ch) = trimmed[after_name_start..].chars().next() {
                        if ch == '(' || ch == '<' || ch == ' ' {
                            return Some((file_path.clone(), line_idx + 1));
                        }
                    }
                }
            }
        }
    }
    None
}

fn cmd_apply(
    stats: &AggregatedStats,
    config: &ClassificationConfig,
    root: &Path,
    dry_run: bool,
) -> std::io::Result<()> {
    let all_classes = capacity_classes_from_config(config);
    let reclassifications = analyze_reclassifications(stats, config);
    let needs_change: Vec<_> = reclassifications
        .iter()
        .filter(|r| r.needs_change())
        .collect();

    if needs_change.is_empty() {
        println!("All tests are correctly classified. Nothing to apply.");
        return Ok(());
    }

    // Group by function name (stripping case suffixes) and take max tier across all cases
    let mut func_suggestions: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    for r in &needs_change {
        let func = extract_func_from_path(&r.test_name).to_string();
        let entry = func_suggestions
            .entry(func)
            .or_insert((r.suggested.threads_required, r.suggested.timeout_ms));
        entry.0 = entry.0.max(r.suggested.threads_required);
        entry.1 = entry.1.max(r.suggested.timeout_ms);
    }

    // Compute rename pairs: old_func_name → new_func_name
    let mut rename_pairs: Vec<(String, String)> = Vec::new();
    let mut clamped_count = 0;
    for (func_name, (suggested_threads, timeout_ms)) in &func_suggestions {
        let (old_classes, prefix_len) = parse_capacity_prefix(func_name, &all_classes);
        let base_name = &func_name[prefix_len..];

        // Find the current thread class (the one with threads_required set)
        let current_thread_class = old_classes.iter().find_map(|c| {
            let cls = all_classes.iter().find(|ac| ac.name == *c)?;
            cls.threads.map(|_| c.as_str())
        });

        // Clamp downsize: drop at most one thread tier per apply run
        let clamped_threads =
            clamp_thread_downsize(current_thread_class, *suggested_threads, &all_classes);
        if clamped_threads != *suggested_threads {
            clamped_count += 1;
        }

        let new_prefix =
            classification_to_prefix(clamped_threads, *timeout_ms, config, &all_classes);
        // Compare canonical forms so ordering differences don't cause spurious renames
        let old_canonical = classes_to_prefix(&old_classes, &all_classes);
        if old_canonical == new_prefix {
            continue;
        }
        let new_name = format!("{new_prefix}{base_name}");
        rename_pairs.push((func_name.clone(), new_name));
    }

    if rename_pairs.is_empty() {
        println!("Analysis found issues but no renames are needed.");
        return Ok(());
    }

    if clamped_count > 0 {
        println!(
            "Note: {} tests had their downsize clamped to one tier step.",
            clamped_count
        );
        println!("Run `apply` again after verifying stability to continue stepping down.\n");
    }

    println!("Finding {} test functions to rename...", rename_pairs.len());

    // Collect all .rs files once
    let rs_files = find_rs_files(root);
    println!("Scanning {} source files...", rs_files.len());

    // Find each function definition
    let mut actions: Vec<RenameAction> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();

    for (old_name, new_name) in &rename_pairs {
        match find_function_def(old_name, &rs_files) {
            Some((file_path, line_number)) => {
                actions.push(RenameAction {
                    old_name: old_name.clone(),
                    new_name: new_name.clone(),
                    file_path,
                    line_number,
                });
            }
            None => {
                not_found.push(old_name.clone());
            }
        }
    }

    // Group actions by file for efficient batch editing
    let mut by_file: BTreeMap<PathBuf, Vec<&RenameAction>> = BTreeMap::new();
    for action in &actions {
        by_file
            .entry(action.file_path.clone())
            .or_default()
            .push(action);
    }

    // Print summary
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         APPLY RECLASSIFICATIONS                             ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Group by change direction for display
    let mut by_change: BTreeMap<String, Vec<&RenameAction>> = BTreeMap::new();
    for action in &actions {
        let (old_classes, _) = parse_capacity_prefix(&action.old_name, &all_classes);
        let old_pfx = classes_to_prefix(&old_classes, &all_classes);
        let (new_classes, _) = parse_capacity_prefix(&action.new_name, &all_classes);
        let new_pfx = classes_to_prefix(&new_classes, &all_classes);
        let key = format!(
            "'{}' → '{}'",
            display_prefix(&old_pfx),
            display_prefix(&new_pfx),
        );
        by_change.entry(key).or_default().push(action);
    }

    for (change, items) in &by_change {
        println!("{} ({} functions):", change, items.len());
        for action in items {
            println!("  {} → {}", action.old_name, action.new_name);
            println!(
                "    in {}:{}",
                action.file_path.display(),
                action.line_number
            );
        }
        println!();
    }

    if !not_found.is_empty() {
        println!(
            "⚠️  {} functions not found in source (skipped):",
            not_found.len()
        );
        for name in &not_found {
            println!("  {}", name);
        }
        println!();
    }

    if dry_run {
        println!(
            "DRY RUN: {} renames across {} files would be applied.",
            actions.len(),
            by_file.len()
        );
        if clamped_count > 0 {
            println!(
                "({} downsizes were clamped to one tier step)",
                clamped_count
            );
        }
        return Ok(());
    }

    // Apply renames
    let mut files_modified = 0;
    let mut renames_applied = 0;

    for (file_path, file_actions) in &by_file {
        let content = fs::read_to_string(file_path)?;
        let mut new_content = content.clone();

        for action in file_actions {
            // Replace `fn old_name` with `fn new_name` — the fn keyword anchors the match
            // to avoid renaming unrelated occurrences of the same identifier
            let old_pattern = format!("fn {}", action.old_name);
            let new_pattern = format!("fn {}", action.new_name);
            new_content = new_content.replace(&old_pattern, &new_pattern);
        }

        if new_content != content {
            fs::write(file_path, &new_content)?;
            files_modified += 1;
            renames_applied += file_actions.len();
        }
    }

    println!(
        "Applied {} renames across {} files.",
        renames_applied, files_modified
    );
    println!();
    println!("Next steps:");
    println!("  1. Run `cargo fmt --all` to format changed files");
    println!("  2. Run `cargo xtask check` to verify compilation");
    println!("  3. Run `cargo xtask test --monitor` to verify with updated classifications");
    if clamped_count > 0 {
        println!("  4. Run `apply` again to continue stepping down clamped tests");
    }

    Ok(())
}

// ============================================================================
// Heap profiling commands (feature-gated)
// ============================================================================

#[cfg(feature = "heap-profile")]
fn cmd_heap_list(stats: &AggregatedStats) {
    let profiles: Vec<&TestStats> = stats
        .tests
        .iter()
        .filter(|t| t.heap_profile_path.is_some())
        .collect();

    if profiles.is_empty() {
        println!("No heap profiles found.");
        println!("Run tests with --heap-profile to generate profiles.");
        return;
    }

    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         AVAILABLE HEAP PROFILES                             ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("{:<8} {:>10}  Test Name", "Status", "File Size");
    println!("{}", "-".repeat(80));

    for test in &profiles {
        let name = test.test_name.as_deref().unwrap_or("<unknown>");
        let status = if test.passed { "PASS" } else { "FAIL" };
        let size = test
            .heap_profile_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| format_bytes_heap(m.len()))
            .unwrap_or_else(|| "missing".to_string());

        println!("{:<8} {:>10}  {}", status, size, name);
    }
    println!();
    println!("Use 'nextest-report heap <pattern>' to view allocation details.");
    println!("Use 'heaptrack_gui <path>' for interactive analysis.");
}

#[cfg(feature = "heap-profile")]
fn cmd_heap(stats: &AggregatedStats, pattern: &str, top: usize) -> std::io::Result<()> {
    let matches: Vec<&TestStats> = stats
        .tests
        .iter()
        .filter(|t| {
            t.heap_profile_path.is_some()
                && t.test_name.as_deref().is_some_and(|n| n.contains(pattern))
        })
        .collect();

    if matches.is_empty() {
        eprintln!("No heap profiles found matching '{}'.", pattern);
        eprintln!("Run 'nextest-report heap-list' to see available profiles.");
        std::process::exit(1);
    }

    for test in &matches {
        let name = test.test_name.as_deref().unwrap_or("<unknown>");
        let path = test.heap_profile_path.as_deref().unwrap();

        println!(
            "╔══════════════════════════════════════════════════════════════════════════════╗"
        );
        println!("║  Heap Profile: {:<61}║", truncate_str(name, 61));
        println!(
            "╚══════════════════════════════════════════════════════════════════════════════╝"
        );
        println!();
        println!("Profile file: {}", path);
        println!();

        let output = std::process::Command::new("heaptrack_print")
            .arg(path)
            .output();

        match output {
            Ok(result) => {
                let stdout = String::from_utf8_lossy(&result.stdout);
                let stderr = String::from_utf8_lossy(&result.stderr);

                if !result.status.success() {
                    eprintln!("heaptrack_print failed for {}:", path);
                    eprintln!("{}", stderr);
                    continue;
                }

                display_heaptrack_summary(&stdout, top);

                if !stderr.is_empty() {
                    display_heaptrack_stats(&stderr);
                }
            }
            Err(e) => {
                eprintln!("Failed to run heaptrack_print: {}", e);
                eprintln!("Install heaptrack to view profiles:");
                eprintln!("  Ubuntu/Debian: sudo apt-get install heaptrack");
                eprintln!();
                eprintln!("You can also use heaptrack_gui for interactive analysis:");
                eprintln!("  heaptrack_gui {}", path);
            }
        }
        println!();
    }

    Ok(())
}

#[cfg(feature = "heap-profile")]
fn display_heaptrack_summary(output: &str, top: usize) {
    let lines: Vec<&str> = output.lines().collect();

    let mut in_section = false;
    let mut section_name = String::new();
    let mut section_lines: Vec<String> = Vec::new();
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();

    for line in &lines {
        if line.starts_with("MOST CALLS")
            || line.starts_with("PEAK MEMORY")
            || line.starts_with("TOTAL MEMORY")
            || line.starts_with("MEMORY LEAKS")
        {
            if in_section && !section_lines.is_empty() {
                sections.push((section_name.clone(), section_lines.clone()));
            }
            section_name = line.to_string();
            section_lines.clear();
            in_section = true;
            continue;
        }

        if in_section {
            if line.is_empty() && !section_lines.is_empty() {
                sections.push((section_name.clone(), section_lines.clone()));
                in_section = false;
                section_lines.clear();
            } else if !line.is_empty() {
                section_lines.push(line.to_string());
            }
        }
    }
    if in_section && !section_lines.is_empty() {
        sections.push((section_name, section_lines));
    }

    if sections.is_empty() {
        println!("--- heaptrack_print output ---");
        for (i, line) in lines.iter().enumerate() {
            if top > 0 && i >= top * 3 {
                println!("  ... ({} more lines)", lines.len() - i);
                break;
            }
            println!("  {}", line);
        }
        return;
    }

    for (name, lines) in &sections {
        println!("--- {} ---", name);
        let limit = if top > 0 {
            top.min(lines.len())
        } else {
            lines.len()
        };
        for line in &lines[..limit] {
            println!("  {}", line);
        }
        if top > 0 && lines.len() > top {
            println!("  ... ({} more entries)", lines.len() - top);
        }
        println!();
    }
}

#[cfg(feature = "heap-profile")]
fn display_heaptrack_stats(stderr: &str) {
    let interesting_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| {
            l.contains("total memory leaked")
                || l.contains("peak heap memory")
                || l.contains("total allocations")
                || l.contains("peak RSS")
                || l.contains("calls to allocation")
        })
        .collect();

    if !interesting_lines.is_empty() {
        println!("--- Summary ---");
        for line in &interesting_lines {
            println!("  {}", line.trim());
        }
        println!();
    }
}

#[cfg(feature = "heap-profile")]
fn format_bytes_heap(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(feature = "heap-profile")]
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
