//! nextest-cpu-report: Analyze and report on CPU usage statistics from test runs
//!
//! This tool reads the JSON output from nextest-cpu-wrapper and generates
//! reports to help categorize tests by CPU consumption and timeout requirements.
//! It reads classification rules directly from your .config/nextest.toml.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Default exceedance threshold: a test is considered to "need" N threads if it
/// exceeds N threads for more than this fraction of its runtime.
/// 0.20 = 20% — brief spikes up to 20% of runtime are tolerated.
const DEFAULT_EXCEEDANCE_PCT: f64 = 0.20;

// ============================================================================
// Data structures for CPU stats (from wrapper)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSample {
    pub elapsed_ms: u64,
    pub cpu_threads: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCpuStats {
    pub binary: String,
    pub test_name: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub peak_cpu: f64,
    pub avg_cpu: f64,
    /// P50 (median) CPU usage
    #[serde(default)]
    pub p50_cpu: f64,
    /// P90 CPU usage
    #[serde(default)]
    pub p90_cpu: f64,
    /// Time in ms where CPU was >= P90 value
    #[serde(default)]
    pub time_at_p90_ms: u64,
    /// Time in ms where CPU was >= 80% of peak
    #[serde(default)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<CpuSample>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AggregatedStats {
    pub tests: Vec<TestCpuStats>,
}

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
    ///
    /// Uses a **sustained exceedance** heuristic: for each thread bucket, check
    /// what fraction of the test's runtime exceeds that allocation. If more than
    /// `exceedance_pct` of the runtime is above the bucket, the bucket is too
    /// small and we move to the next one. This avoids over-provisioning for
    /// brief CPU spikes while still catching sustained high usage.
    #[allow(clippy::too_many_arguments)]
    pub fn suggest_classification(
        &self,
        avg_cpu: f64,
        duration_ms: u64,
        time_above_1t_ms: u64,
        time_above_2t_ms: u64,
        time_above_3t_ms: u64,
        time_above_4t_ms: u64,
        exceedance_pct: f64,
    ) -> SuggestedClassification {
        // Collect all thread thresholds
        let mut thread_options: Vec<u32> = vec![self.default_threads];
        thread_options.extend(self.rules.iter().filter_map(|r| r.threads_required));
        thread_options.sort();
        thread_options.dedup();

        let duration = duration_ms.max(1) as f64;

        // Find the smallest allocation where the test doesn't exceed it for
        // more than exceedance_pct of its runtime. This is the key insight:
        // brief spikes (e.g. 2% of runtime at 4x) don't warrant a higher
        // allocation, but sustained exceedance (e.g. 30% of runtime at 3x)
        // does.
        let suggested_threads = thread_options
            .iter()
            .find(|&&t| {
                let time_above = match t {
                    1 => time_above_1t_ms,
                    2 => time_above_2t_ms,
                    3 => time_above_3t_ms,
                    4 => time_above_4t_ms,
                    _ => 0,
                };
                let pct = time_above as f64 / duration;
                pct <= exceedance_pct
            })
            .copied()
            .unwrap_or_else(|| *thread_options.last().unwrap_or(&self.default_threads));

        // Sanity floor: if avg_cpu exceeds a bucket, don't use that bucket
        // regardless of the time-above metric (avg > bucket means the test
        // is continuously above, just with measurement noise).
        let suggested_threads = thread_options
            .iter()
            .find(|&&t| t >= suggested_threads && (t as f64) >= avg_cpu)
            .copied()
            .unwrap_or(suggested_threads);

        // Collect all timeout thresholds
        let mut timeout_options: Vec<u64> = vec![self.default_timeout_ms];
        timeout_options.extend(self.rules.iter().filter_map(|r| r.timeout_ms));
        timeout_options.sort();
        timeout_options.dedup();

        // Find minimum timeout needed
        let suggested_timeout = timeout_options
            .iter()
            .find(|&&t| t >= duration_ms)
            .copied()
            .unwrap_or_else(|| *timeout_options.last().unwrap_or(&self.default_timeout_ms));

        // Find rules that would give us the needed classification
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

    for prefix in &["slow_", "heavy_", "2xheavy_", "serial_"] {
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
    pub avg_peak_cpu: f64,
    pub avg_avg_cpu: f64,
    pub avg_p50_cpu: f64,
    pub avg_p90_cpu: f64,
    pub avg_duration_ms: u64,
    pub avg_time_at_p90_ms: u64,
    pub avg_time_near_peak_ms: u64,
    pub avg_time_above_1t_ms: u64,
    pub avg_time_above_2t_ms: u64,
    pub avg_time_above_3t_ms: u64,
    pub avg_time_above_4t_ms: u64,
    pub max_peak_cpu: f64,
    pub max_duration_ms: u64,
}

fn aggregate_by_test(stats: &AggregatedStats) -> Vec<TestAggregation> {
    let mut by_name: HashMap<String, Vec<&TestCpuStats>> = HashMap::new();

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
            let avg_peak_cpu = runs.iter().map(|r| r.peak_cpu).sum::<f64>() / run_count as f64;
            let avg_avg_cpu = runs.iter().map(|r| r.avg_cpu).sum::<f64>() / run_count as f64;
            let avg_p50_cpu = runs.iter().map(|r| r.p50_cpu).sum::<f64>() / run_count as f64;
            let avg_p90_cpu = runs.iter().map(|r| r.p90_cpu).sum::<f64>() / run_count as f64;
            let avg_duration_ms =
                runs.iter().map(|r| r.duration_ms).sum::<u64>() / run_count as u64;
            let avg_time_at_p90_ms =
                runs.iter().map(|r| r.time_at_p90_ms).sum::<u64>() / run_count as u64;
            let avg_time_near_peak_ms =
                runs.iter().map(|r| r.time_near_peak_ms).sum::<u64>() / run_count as u64;
            let avg_time_above_1t_ms =
                runs.iter().map(|r| r.time_above_1t_ms).sum::<u64>() / run_count as u64;
            let avg_time_above_2t_ms =
                runs.iter().map(|r| r.time_above_2t_ms).sum::<u64>() / run_count as u64;
            let avg_time_above_3t_ms =
                runs.iter().map(|r| r.time_above_3t_ms).sum::<u64>() / run_count as u64;
            let avg_time_above_4t_ms =
                runs.iter().map(|r| r.time_above_4t_ms).sum::<u64>() / run_count as u64;
            let max_peak_cpu = runs.iter().map(|r| r.peak_cpu).fold(0.0f64, f64::max);
            let max_duration_ms = runs.iter().map(|r| r.duration_ms).max().unwrap_or(0);

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
            test.avg_avg_cpu,
            test.avg_duration_ms,
            test.avg_time_above_1t_ms,
            test.avg_time_above_2t_ms,
            test.avg_time_above_3t_ms,
            test.avg_time_above_4t_ms,
            DEFAULT_EXCEEDANCE_PCT,
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

        let pct_above_allocation = if test.avg_duration_ms > 0 {
            (time_above_allocation_ms as f64 / test.avg_duration_ms as f64) * 100.0
        } else {
            0.0
        };

        // Check CPU classification using sustained exceedance
        // A test needs more threads if it spends >20% of runtime above its allocation
        let exceedance_threshold = DEFAULT_EXCEEDANCE_PCT * 100.0; // as percentage
        if pct_above_allocation > exceedance_threshold {
            issues.push(format!(
                "CPU regularly exceeds {}T for {:.0}% of runtime (>{:.0}% threshold): avg={:.2}x, peak={:.2}x, above {}T for {:.1}s",
                current.effective_threads,
                pct_above_allocation,
                exceedance_threshold,
                test.avg_avg_cpu,
                test.avg_peak_cpu,
                allocation_threshold as u32,
                time_above_allocation_ms as f64 / 1000.0,
            ));
        } else if suggested.threads_required < current.effective_threads
            && current.effective_threads > config.default_threads
        {
            issues.push(format!(
                "CPU over-allocated: avg={:.2}x, above {}T for only {:.0}% of runtime, but allocated {}T - could downgrade",
                test.avg_avg_cpu,
                allocation_threshold as u32,
                pct_above_allocation,
                current.effective_threads,
            ));
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
#[command(name = "nextest-cpu-report")]
#[command(about = "Analyze CPU usage statistics from nextest test runs")]
struct Cli {
    /// Path to the stats JSON file
    #[arg(short, long, default_value = "target/nextest-cpu-stats.json")]
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
        /// Sort by: p90, peak, avg, duration, nearpeak, atp90
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

    /// Clear the stats file
    Clear,
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
        _ => {
            let stats = load_stats(&cli.input)?;

            match cli.command {
                Commands::Summary { sort, top } => cmd_summary(&stats, &config, &sort, top),
                Commands::Analyze { format, all } => cmd_analyze(&stats, &config, &format, all),
                Commands::Detail { pattern } => cmd_detail(&stats, &config, &pattern),
                Commands::Export { format, output } => {
                    cmd_export(&stats, &format, output.as_ref())?
                }
                Commands::Clear | Commands::Config => unreachable!(),
            }
        }
    }

    Ok(())
}

fn load_stats(path: &PathBuf) -> std::io::Result<AggregatedStats> {
    if !path.exists() {
        eprintln!("Stats file not found: {}", path.display());
        eprintln!("Run your tests with nextest-cpu-wrapper first.");
        std::process::exit(1);
    }

    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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

fn cmd_summary(stats: &AggregatedStats, config: &ClassificationConfig, sort: &str, top: usize) {
    let mut aggregated = aggregate_by_test(stats);

    match sort {
        "p90" => aggregated.sort_by(|a, b| b.avg_p90_cpu.partial_cmp(&a.avg_p90_cpu).unwrap()),
        "peak" => aggregated.sort_by(|a, b| b.avg_peak_cpu.partial_cmp(&a.avg_peak_cpu).unwrap()),
        "avg" => aggregated.sort_by(|a, b| b.avg_avg_cpu.partial_cmp(&a.avg_avg_cpu).unwrap()),
        "duration" => aggregated.sort_by(|a, b| b.avg_duration_ms.cmp(&a.avg_duration_ms)),
        "nearpeak" => {
            aggregated.sort_by(|a, b| b.avg_time_near_peak_ms.cmp(&a.avg_time_near_peak_ms))
        }
        "atp90" => aggregated.sort_by(|a, b| b.avg_time_at_p90_ms.cmp(&a.avg_time_at_p90_ms)),
        _ => {
            eprintln!("Unknown sort option: {}. Using 'p90'.", sort);
            aggregated.sort_by(|a, b| b.avg_p90_cpu.partial_cmp(&a.avg_p90_cpu).unwrap());
        }
    }

    let to_show = if top > 0 {
        top.min(aggregated.len())
    } else {
        aggregated.len()
    };

    println!(
        "{:<45} {:>5} {:>6} {:>6} {:>6} {:>8} {:>6} {:>6} {:>4}",
        "Test Name", "Alloc", "P90", "Peak", "Avg", "Duration", "≥P90", "NrPk", "Runs"
    );
    println!("{}", "-".repeat(110));

    for test in aggregated.iter().take(to_show) {
        let name = if test.test_name.len() > 43 {
            format!("...{}", &test.test_name[test.test_name.len() - 40..])
        } else {
            test.test_name.clone()
        };

        // Get current allocation
        let classification = config.classify(&test.test_name);
        let alloc_str = format!("{}T", classification.effective_threads);

        let pct_at_p90 = if test.avg_duration_ms > 0 {
            (test.avg_time_at_p90_ms as f64 / test.avg_duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_near_peak = if test.avg_duration_ms > 0 {
            (test.avg_time_near_peak_ms as f64 / test.avg_duration_ms as f64) * 100.0
        } else {
            0.0
        };

        let duration_str = format_duration(test.avg_duration_ms);

        println!(
            "{:<45} {:>5} {:>5.2}x {:>5.2}x {:>5.2}x {:>8} {:>5.0}% {:>5.0}% {:>4}",
            name,
            alloc_str,
            test.avg_p90_cpu,
            test.avg_peak_cpu,
            test.avg_avg_cpu,
            duration_str,
            pct_at_p90,
            pct_near_peak,
            test.run_count
        );
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
                        "stats": {
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
                        }
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        _ => {
            let needs_change: Vec<_> = to_show.iter().filter(|r| r.needs_change()).collect();
            let ok: Vec<_> = to_show.iter().filter(|r| !r.needs_change()).collect();

            if !needs_change.is_empty() {
                println!("╔══════════════════════════════════════════════════════════════════════════════╗");
                println!("║                     TESTS NEEDING RECLASSIFICATION                          ║");
                println!("╚══════════════════════════════════════════════════════════════════════════════╝");
                println!();

                for r in &needs_change {
                    let current_rules = if r.current.is_default() {
                        "(default)".to_string()
                    } else {
                        r.current.rule_names().join(", ")
                    };

                    // Calculate time above allocation percentage
                    let (time_above_ms, threshold) = match r.current.effective_threads {
                        1 => (r.stats.avg_time_above_1t_ms, 1),
                        2 => (r.stats.avg_time_above_2t_ms, 2),
                        3 => (r.stats.avg_time_above_3t_ms, 3),
                        4 => (r.stats.avg_time_above_4t_ms, 4),
                        _ => (r.stats.avg_time_above_1t_ms, 1),
                    };
                    let pct_above = if r.stats.avg_duration_ms > 0 {
                        (time_above_ms as f64 / r.stats.avg_duration_ms as f64) * 100.0
                    } else {
                        0.0
                    };
                    let pct_at_p90 = if r.stats.avg_duration_ms > 0 {
                        (r.stats.avg_time_at_p90_ms as f64 / r.stats.avg_duration_ms as f64) * 100.0
                    } else {
                        0.0
                    };
                    let pct_near_peak = if r.stats.avg_duration_ms > 0 {
                        (r.stats.avg_time_near_peak_ms as f64 / r.stats.avg_duration_ms as f64)
                            * 100.0
                    } else {
                        0.0
                    };

                    println!("┌─ {}", r.test_name);
                    println!(
                        "│  Current: {} ({}T, {:.0}s timeout)",
                        current_rules,
                        r.current.effective_threads,
                        r.current.effective_timeout_ms as f64 / 1000.0
                    );
                    println!(
                        "│  CPU:     peak: {:.2}x | P90: {:.2}x | P50: {:.2}x | avg: {:.2}x",
                        r.stats.avg_peak_cpu,
                        r.stats.avg_p90_cpu,
                        r.stats.avg_p50_cpu,
                        r.stats.avg_avg_cpu
                    );
                    println!(
                        "│  Time:    {} runs | {} | at P90: {:.0}% | near peak: {:.0}%",
                        r.stats.run_count,
                        format_duration(r.stats.avg_duration_ms),
                        pct_at_p90,
                        pct_near_peak
                    );
                    if time_above_ms > 0 {
                        println!(
                            "│  Above {}T: {:.1}s ({:.0}% of runtime)",
                            threshold,
                            time_above_ms as f64 / 1000.0,
                            pct_above
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
                    println!("└────────────────────────────────────────────────────────────────────────────────");
                    println!();
                }

                println!(
                    "Summary: {} tests need reclassification",
                    needs_change.len()
                );
            } else {
                println!("✅ All tests are correctly classified!");
            }

            if show_all && !ok.is_empty() {
                println!();
                println!("╔══════════════════════════════════════════════════════════════════════════════╗");
                println!("║                        CORRECTLY CLASSIFIED TESTS                           ║");
                println!("╚══════════════════════════════════════════════════════════════════════════════╝");
                println!();

                for r in &ok {
                    let rules = if r.current.is_default() {
                        "(default)".to_string()
                    } else {
                        r.current.rule_names().join(", ")
                    };

                    println!(
                        "  ✓ [{}] {} (peak: {:.2}x, P90: {:.2}x, avg: {:.2}x, {:.1}s)",
                        rules,
                        r.test_name,
                        r.stats.avg_peak_cpu,
                        r.stats.avg_p90_cpu,
                        r.stats.avg_avg_cpu,
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
        println!();
        println!("  CPU Usage:");
        println!("    Peak:     {:.2}x", test.peak_cpu);
        println!("    P90:      {:.2}x", test.p90_cpu);
        println!("    P50:      {:.2}x (median)", test.p50_cpu);
        println!("    Average:  {:.2}x", test.avg_cpu);
        println!();
        println!("  Time Above Allocation Thresholds:");
        let pct_1t = if test.duration_ms > 0 {
            (test.time_above_1t_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_2t = if test.duration_ms > 0 {
            (test.time_above_2t_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_3t = if test.duration_ms > 0 {
            (test.time_above_3t_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_4t = if test.duration_ms > 0 {
            (test.time_above_4t_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_at_p90 = if test.duration_ms > 0 {
            (test.time_at_p90_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        let pct_near_peak = if test.duration_ms > 0 {
            (test.time_near_peak_ms as f64 / test.duration_ms as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "    Above 1T: {:.2}s ({:.1}%)",
            test.time_above_1t_ms as f64 / 1000.0,
            pct_1t
        );
        println!(
            "    Above 2T: {:.2}s ({:.1}%)",
            test.time_above_2t_ms as f64 / 1000.0,
            pct_2t
        );
        println!(
            "    Above 3T: {:.2}s ({:.1}%)",
            test.time_above_3t_ms as f64 / 1000.0,
            pct_3t
        );
        println!(
            "    Above 4T: {:.2}s ({:.1}%)",
            test.time_above_4t_ms as f64 / 1000.0,
            pct_4t
        );
        println!();
        println!("  Time at P90 Level (≥{:.2}x):", test.p90_cpu);
        println!(
            "    Duration: {:.2}s ({:.1}% of runtime)",
            test.time_at_p90_ms as f64 / 1000.0,
            pct_at_p90
        );
        println!();
        println!(
            "  Time Near Peak (≥80% of {:.2}x = ≥{:.2}x):",
            test.peak_cpu,
            test.peak_cpu * 0.8
        );
        println!(
            "    Duration: {:.2}s ({:.1}% of runtime)",
            test.time_near_peak_ms as f64 / 1000.0,
            pct_near_peak
        );
        println!();
        println!("  Timing:");
        println!("    Duration: {}", format_duration(test.duration_ms));
        println!();

        if let Some(ref samples) = test.samples {
            println!("  CPU Usage Over Time:");
            println!();
            print_cpu_chart(samples, classification.effective_threads as f64);
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

    // Scale should include the allocation threshold for reference
    let scale = max_cpu.max(allocation_threshold * 1.2);
    let threshold_row = ((allocation_threshold / scale) * chart_height as f64) as usize;

    for row in (0..chart_height).rev() {
        let level = ((row as f64 + 0.5) / chart_height as f64) * scale;
        print!("    {:>5.1}x │", level);

        for &val in &downsampled {
            let bar_height = (val / scale * chart_height as f64) as usize;
            if bar_height > row {
                if val > allocation_threshold {
                    print!("█"); // Above allocation
                } else {
                    print!("▒"); // Below allocation
                }
            } else if row == threshold_row {
                print!("─"); // Threshold line
            } else {
                print!(" ");
            }
        }

        // Mark the threshold row
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
                    serde_json::json!({
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
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        _ => {
            let mut lines = vec![
                "test_name,run_count,avg_peak_cpu,avg_p90_cpu,avg_p50_cpu,avg_avg_cpu,avg_duration_ms,time_at_p90_ms,time_near_peak_ms,time_above_1t_ms,time_above_2t_ms,time_above_3t_ms,time_above_4t_ms,max_peak_cpu,max_duration_ms".to_string()
            ];
            for t in &aggregated {
                lines.push(format!(
                    "\"{}\",{},{:.4},{:.4},{:.4},{:.4},{},{},{},{},{},{},{},{:.4},{}",
                    t.test_name.replace('"', "\"\""),
                    t.run_count,
                    t.avg_peak_cpu,
                    t.avg_p90_cpu,
                    t.avg_p50_cpu,
                    t.avg_avg_cpu,
                    t.avg_duration_ms,
                    t.avg_time_at_p90_ms,
                    t.avg_time_near_peak_ms,
                    t.avg_time_above_1t_ms,
                    t.avg_time_above_2t_ms,
                    t.avg_time_above_3t_ms,
                    t.avg_time_above_4t_ms,
                    t.max_peak_cpu,
                    t.max_duration_ms,
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
