//! Nextest-based flaky-test detection.
//!
//! Three phases, each narrowing the signal:
//!
//! 1. **Full suite** — run the entire test suite `iterations` times under normal
//!    parallelism. Any test that fails at least once becomes a "suspect". This
//!    reflects real-world contention (many tests sharing the machine).
//! 2. **Stress** — run *only* the suspect set together, at high concurrency,
//!    `stress_iterations` times. This reproduces peer-contention flakiness far
//!    faster than re-running the whole suite, and sharpens the diagnosis.
//! 3. **Isolation** — run each suspect *alone* (single-threaded, one test per
//!    process) `isolation_iterations` times, capturing full logs. A test that
//!    fails here is genuinely flaky/broken; one that only failed earlier is
//!    contention-sensitive.
//!
//! Pass/fail for phases 1 and 2 is captured via the `nextest-wrapper` run-wrapper
//! (structured per-test JSON), pointed at a per-invocation output path. Phase 3
//! derives pass/fail directly from the process exit code, since exactly one test
//! runs per invocation.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Local;
use nextest_monitor::types::AggregatedStats;
use serde::Serialize;
use xshell::{Shell, cmd};

use crate::failures::{
    FailuresFile, build_failure_filter, generate_nextest_config, get_monitor_dir,
};
use crate::util::{RING_ENV_VARS, build_wrapper, remove_ring_env_vars, shell_quote};

/// Options for the flaky-detection run, parsed from the CLI.
#[derive(Debug, Clone)]
pub struct FlakyOptions {
    /// Phase 1: number of full-suite iterations.
    pub iterations: usize,
    /// Phase 2: number of stress iterations over the suspect set.
    pub stress_iterations: usize,
    /// Phase 3: per-test isolated iterations.
    pub isolation_iterations: usize,
    /// `--test-threads` for phases 1 and 2. `None` lets nextest auto-detect.
    pub threads: Option<usize>,
    /// Clean the workspace and prebuild before running.
    pub clean: bool,
    /// Skip the stress phase.
    pub no_stress: bool,
    /// Skip the isolation phase.
    pub no_isolation: bool,
    /// Number of genuinely-flaky tests to tolerate before exiting non-zero.
    pub tolerable_failures: usize,
    /// `RUST_LOG` value to set during the isolation phase (for richer logs).
    pub isolation_log: Option<String>,
    /// Emit the machine-readable report to stdout (sentinel-wrapped) on completion.
    pub json: bool,
    /// Known flaky tests to target directly, skipping full-suite discovery.
    /// Phase 1 is scoped to just these (fast) instead of running the whole suite.
    pub tests: Vec<String>,
    /// Read the target test list from a prior `report.json` or `failures.json`
    /// instead of (or in addition to) `--tests`.
    pub tests_from: Option<PathBuf>,
    /// Verify mode: run *only* the isolation phase on the given tests (the
    /// post-fix "is it fixed?" check). Skips discovery and stress.
    pub verify: Vec<String>,
    /// Passthrough args forwarded to the phase-1 nextest invocation.
    pub args: Vec<String>,
}

/// Outcome of a single test within a single run.
#[derive(Debug, Clone, Copy)]
struct Outcome {
    passed: bool,
    timed_out: bool,
    duration_ms: u64,
}

/// A distinct failure fingerprint extracted from an isolated run's log, so the
/// report tells you *why* a test failed without opening the log files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FailureSignature {
    /// The panic/assertion message (or a timeout note).
    message: String,
    /// Source location (`file:line:col`) when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
}

/// Strip ANSI escape sequences (tracing colors) so extracted messages are clean.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip until the terminating letter of the CSI sequence (e.g. 'm').
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Collapse internal whitespace so signatures dedupe cleanly.
fn normalize_msg(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract a failure signature from an isolated run's captured output.
///
/// Recognizes the standard Rust panic form
/// `thread '..' panicked at <file:line:col>: <message>` (message may spill to the
/// next line), falling back to an `assertion .. failed` line. Returns `None`
/// when nothing recognizable is found (e.g. a bare timeout with no panic).
fn extract_failure_signature(text: &str) -> Option<FailureSignature> {
    let lines: Vec<String> = text.lines().map(strip_ansi).collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(pos) = line.find("panicked at ") {
            let rest = line[pos + "panicked at ".len()..].trim();
            let (location, inline_msg) = match rest.split_once(": ") {
                Some((loc, msg)) => (loc.trim().to_string(), Some(msg.trim().to_string())),
                None => (rest.trim_end_matches(':').trim().to_string(), None),
            };
            let message = inline_msg
                .filter(|m| !m.is_empty())
                .or_else(|| lines.get(i + 1).map(|l| l.trim().to_string()))
                .unwrap_or_default();
            return Some(FailureSignature {
                message: normalize_msg(&message),
                location: (!location.is_empty()).then_some(location),
            });
        }
    }
    for line in &lines {
        let t = line.trim();
        if (t.starts_with("assertion") && t.contains("failed")) || t.starts_with("Error:") {
            return Some(FailureSignature {
                message: normalize_msg(t),
                location: None,
            });
        }
    }
    None
}

/// Aggregated pass/fail counters for one test across one phase.
#[derive(Debug, Clone, Copy, Default, Serialize)]
struct PhaseCounts {
    runs: usize,
    fails: usize,
    timeouts: usize,
    min_ms: u64,
    max_ms: u64,
    total_ms: u64,
}

impl PhaseCounts {
    fn record(&mut self, o: Outcome) {
        self.runs += 1;
        if !o.passed {
            self.fails += 1;
        }
        if o.timed_out {
            self.timeouts += 1;
        }
        if self.runs == 1 || o.duration_ms < self.min_ms {
            self.min_ms = o.duration_ms;
        }
        if o.duration_ms > self.max_ms {
            self.max_ms = o.duration_ms;
        }
        self.total_ms += o.duration_ms;
    }

    fn avg_ms(&self) -> u64 {
        if self.runs == 0 {
            0
        } else {
            self.total_ms / self.runs as u64
        }
    }
}

/// How a suspect test is ultimately classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Classification {
    /// Failed in isolation every single time — not flaky, just broken.
    Broken,
    /// Failed in isolation sometimes — a genuine flake.
    GenuineFlaky,
    /// Passed in isolation but failed when the suspect set ran together.
    PeerContention,
    /// Passed in isolation and under stress, but failed under full-suite load.
    SuiteContention,
    /// Failed only via timeouts (no assertion failures observed).
    TimeoutBound,
    /// Passed every run we performed with no earlier failure evidence — e.g. a
    /// verify run confirming a fix.
    Clean,
    /// Isolation was skipped, so we can't distinguish genuine from contention.
    Unverified,
}

impl Classification {
    fn label(self) -> &'static str {
        match self {
            Self::Broken => "BROKEN (fails every isolated run)",
            Self::GenuineFlaky => "GENUINE FLAKY (fails in isolation)",
            Self::PeerContention => "CONTENTION (fails only alongside peers)",
            Self::SuiteContention => "CONTENTION (fails only under full-suite load)",
            Self::TimeoutBound => "TIMEOUT-BOUND (failures are timeouts)",
            Self::Clean => "CLEAN (passed all runs)",
            Self::Unverified => "UNVERIFIED (isolation skipped)",
        }
    }

    /// Whether this classification represents a genuine (non-contention) flake
    /// that should count toward the CI failure gate.
    fn is_genuine(self) -> bool {
        matches!(self, Self::Broken | Self::GenuineFlaky | Self::TimeoutBound)
    }
}

/// Per-test aggregated report across all phases.
#[derive(Debug, Clone, Serialize)]
struct TestReport {
    name: String,
    phase1: PhaseCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    stress: Option<PhaseCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    isolation: Option<PhaseCounts>,
    classification: Classification,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_dir: Option<String>,
    /// Distinct failure fingerprints observed across isolated runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failure_signatures: Vec<FailureSignature>,
}

impl TestReport {
    fn classify(&self) -> Classification {
        // Isolation is the strongest signal when available.
        if let Some(iso) = self.isolation {
            if iso.fails > 0 {
                // All failures were timeouts → distinct, actionable category.
                if iso.timeouts == iso.fails {
                    return Classification::TimeoutBound;
                }
                if iso.fails == iso.runs && iso.runs > 0 {
                    return Classification::Broken;
                }
                return Classification::GenuineFlaky;
            }
            // Passed every isolated run — attribute to contention only when there
            // is actual earlier failure evidence; otherwise it's clean (e.g. a
            // verify run confirming a fix).
            if let Some(stress) = self.stress
                && stress.fails > 0
            {
                return Classification::PeerContention;
            }
            if self.phase1.fails > 0 {
                return Classification::SuiteContention;
            }
            return Classification::Clean;
        }

        // No isolation data: PeerContention asserts the test passes *alone*, which
        // only an isolation pass can establish — so a stress failure here can't be
        // called contention. Leave it Unverified (unless it's purely timeouts).
        if self.phase1.timeouts == self.phase1.fails && self.phase1.fails > 0 {
            return Classification::TimeoutBound;
        }
        Classification::Unverified
    }
}

/// Full flaky run report, serialized to `report.json`.
#[derive(Debug, Serialize)]
struct Report {
    started_at: String,
    iterations: usize,
    stress_iterations: usize,
    isolation_iterations: usize,
    total_suspects: usize,
    genuine_flakes: usize,
    tests: Vec<TestReport>,
}

/// Load per-test outcomes from a wrapper stats base path (reads `<base>.d/`).
fn load_outcomes(base: &Path) -> HashMap<String, Outcome> {
    let stats = AggregatedStats::load_or_default(base);
    let mut map = HashMap::new();
    for t in stats.tests {
        if let Some(name) = t.test_name {
            // With retries=0 there should be one entry per test; last wins.
            map.insert(
                name,
                Outcome {
                    passed: t.passed,
                    timed_out: t.timed_out.unwrap_or(false),
                    duration_ms: t.duration_ms,
                },
            );
        }
    }
    map
}

/// Sanitize a test name into a filesystem-safe directory component.
fn sanitize(name: &str) -> String {
    name.replace(['/', '\\', ':', ' '], "_")
}

/// Run a nextest invocation that streams to the terminal *and* tees to a log
/// file, with the given wrapper output base and env. Test failures are expected
/// and do not abort — pass/fail is read from the wrapper stats afterwards.
fn run_teed(
    sh: &Shell,
    cargo_args: &[String],
    stats_base: &Path,
    log_path: &Path,
) -> eyre::Result<()> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let quoted: Vec<String> = cargo_args.iter().map(|a| shell_quote(a)).collect();
    let inner = format!("cargo {}", quoted.join(" "));
    let quoted_log = shell_quote(&log_path.to_string_lossy());
    // tee exits 0, so a failing test suite won't surface as an error here — we
    // intentionally read results from the wrapper stats instead.
    let full = format!("{inner} 2>&1 | tee {quoted_log}");

    remove_ring_env_vars(cmd!(sh, "bash -c {full}"))
        .env("RUST_BACKTRACE", "1")
        .env("NEXTEST_MONITOR_OUTPUT", stats_base)
        .env("NEXTEST_MONITOR_CPU", "0")
        .env("NEXTEST_MONITOR_MEMORY", "0")
        .run()?;
    Ok(())
}

/// Result of a single isolated test invocation.
enum IsoResult {
    Passed,
    Failed {
        timed_out: bool,
        duration_ms: u64,
        /// Failure fingerprint parsed from the captured output, when available.
        signature: Option<FailureSignature>,
    },
    /// The filter matched no tests — surfaced as a hard error, not a flake.
    NoMatch,
}

/// Run a single test in isolation (single-threaded, one process), capturing all
/// output to `log_path`. Pass/fail comes from the process exit code.
fn run_isolated(
    test_name: &str,
    log_path: &Path,
    isolation_log: Option<&str>,
) -> eyre::Result<IsoResult> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let filter = format!("test(={test_name})");
    let start = std::time::Instant::now();

    let mut cmd = std::process::Command::new("cargo");
    cmd.args([
        "nextest",
        "run",
        "-E",
        &filter,
        "--test-threads",
        "1",
        "--no-fail-fast",
        // Stream the test's own stdout/stderr (panics, tracing) into our capture
        // instead of letting nextest buffer it — this is what makes the isolated
        // failure logs actually useful.
        "--no-capture",
    ]);
    // Full backtraces in isolation to maximize debugging signal.
    cmd.env("RUST_BACKTRACE", "full");
    if let Some(log) = isolation_log {
        cmd.env("RUST_LOG", log);
    }
    for k in RING_ENV_VARS {
        cmd.env_remove(k);
    }

    let output = cmd.output()?;
    let duration_ms = start.elapsed().as_millis() as u64;

    let mut combined = Vec::new();
    combined.extend_from_slice(&output.stdout);
    combined.extend_from_slice(&output.stderr);
    fs::write(log_path, &combined)?;

    let text = String::from_utf8_lossy(&combined);
    // nextest exits with code 4 when no tests match the filter; treat that as a
    // configuration problem rather than a passing/failing test. The string check
    // is a fallback in case the code changes across nextest versions.
    if output.status.code() == Some(4)
        || text.contains("no tests to run")
        || text.contains("did not match any")
    {
        return Ok(IsoResult::NoMatch);
    }

    if output.status.success() {
        Ok(IsoResult::Passed)
    } else {
        // A SIGTERM-driven timeout shows up in nextest output; best-effort tag.
        let timed_out = text.contains("SIGTERM") || text.contains("timed out");
        Ok(IsoResult::Failed {
            timed_out,
            duration_ms,
            signature: extract_failure_signature(&text),
        })
    }
}

/// Parse a `--tests-from` file (either a flaky `report.json` or a
/// `failures.json`) into a list of test names.
fn parse_tests_from(path: &Path) -> eyre::Result<Vec<String>> {
    let content = fs::read_to_string(path)
        .map_err(|e| eyre::eyre!("failed to read {}: {e}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| eyre::eyre!("failed to parse {} as JSON: {e}", path.display()))?;

    // report.json: { "tests": [ { "name": "..." }, ... ] }
    if let Some(tests) = json.get("tests").and_then(|t| t.as_array()) {
        return Ok(tests
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .collect());
    }
    // failures.json: { "failed_tests": [ "...", ... ] }
    if let Some(failed) = json.get("failed_tests").and_then(|f| f.as_array()) {
        return Ok(failed
            .iter()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect());
    }
    eyre::bail!(
        "{} has neither a `tests` nor a `failed_tests` array",
        path.display()
    )
}

/// Resolve the explicit target-test list from `--tests`, `--tests-from`, and
/// `--verify`, de-duplicated and sorted. Empty means "discover via full suite".
fn resolve_target_tests(opts: &FlakyOptions) -> eyre::Result<Vec<String>> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for t in opts.tests.iter().chain(opts.verify.iter()) {
        let t = t.trim();
        if !t.is_empty() {
            set.insert(t.to_owned());
        }
    }
    if let Some(ref path) = opts.tests_from {
        for t in parse_tests_from(path)? {
            set.insert(t);
        }
    }
    Ok(set.into_iter().collect())
}

/// Entry point for `cargo xtask flaky`.
pub fn run_flaky(sh: &Shell, opts: FlakyOptions) -> eyre::Result<()> {
    // Resolve the run mode up front (errors early on a bad --tests-from file):
    //   - verify   : isolation-only over the given tests (post-fix check)
    //   - targeted : known tests supplied → scope discovery to them (fast)
    //   - discovery: no tests supplied → run the full suite to find suspects
    let verify_mode = !opts.verify.is_empty();
    let target_tests = resolve_target_tests(&opts)?;
    let targeted = !target_tests.is_empty();

    // Ensure nextest is available (matches the version pinned by `xtask test`).
    let _ = cmd!(sh, "cargo install --locked --version 0.9.124 cargo-nextest").run();

    if opts.clean {
        println!("Cleaning workspace...");
        cmd!(sh, "cargo clean").run()?;
    }

    // Build tests once up front so compile time doesn't pollute iteration 1 and
    // every iteration measures pure test time. Fail fast: a compile error here
    // must abort, otherwise every phase fails confusingly downstream.
    println!("Prebuilding tests (cargo nextest run --no-run)...");
    remove_ring_env_vars(cmd!(sh, "cargo nextest run --workspace --no-run"))
        .env("RUST_BACKTRACE", "1")
        .run()?;

    // Build the wrapper and generate the phase-1 config (default profile with
    // the monitoring run-wrapper attached — retries stay at the profile default
    // of 0 so a nextest-level retry never masks a flake).
    let wrapper_path = build_wrapper(sh, None)?;
    let wrapper_str = wrapper_path.to_string_lossy().to_string();
    let phase1_config = generate_nextest_config(&wrapper_str, None, false)?;
    let phase1_config_path = phase1_config.path().to_string_lossy().to_string();

    // Per-run output directory.
    let started = Local::now();
    let run_id = started.format("%Y-%m-%d_%H-%M-%S").to_string();
    let run_dir = get_monitor_dir().join("flaky").join(&run_id);
    fs::create_dir_all(&run_dir)?;
    println!("Flaky run artifacts: {}", run_dir.display());

    let threads_arg: Option<Vec<String>> = opts
        .threads
        .map(|t| vec!["--test-threads".to_string(), t.to_string()]);

    // test_name -> counts across phase 1
    let mut phase1: BTreeMap<String, PhaseCounts> = BTreeMap::new();
    let suspects: Vec<String>;

    if verify_mode {
        // ---- Verify mode: isolation only, over the given tests ----
        println!(
            "\n=== Verify mode: isolation only ({} test(s)) ===",
            target_tests.len()
        );
        suspects = target_tests;
    } else {
        // ---- Phase 1: run the suite N times ----
        // Targeted runs scope phase 1 to just the supplied tests (fast); discovery
        // runs the whole suite to find suspects.
        let scope_filter = targeted.then(|| build_failure_filter(&target_tests));
        println!(
            "\n=== Phase 1: {} × {} ({} threads) ===",
            if targeted {
                format!("targeted ({} test(s))", target_tests.len())
            } else {
                "full suite".to_string()
            },
            opts.iterations,
            opts.threads
                .map_or_else(|| "auto".to_string(), |t| t.to_string())
        );

        for i in 1..=opts.iterations {
            println!("\n--- Phase 1 iteration {i}/{} ---", opts.iterations);
            let stats_base = run_dir.join(format!("phase1/iter-{i}/stats"));
            let log_path = run_dir.join(format!("phase1/iter-{i}/output.log"));

            let mut args = vec![
                "nextest".to_string(),
                "run".to_string(),
                "--workspace".to_string(),
                "--tests".to_string(),
                "--all-targets".to_string(),
                "--no-fail-fast".to_string(),
                "--config-file".to_string(),
                phase1_config_path.clone(),
            ];
            if let Some(ref filter) = scope_filter {
                args.push("-E".to_string());
                args.push(filter.clone());
            }
            if let Some(ref t) = threads_arg {
                args.extend(t.clone());
            }
            args.extend(opts.args.iter().cloned());

            run_teed(sh, &args, &stats_base, &log_path)?;

            let outcomes = load_outcomes(&stats_base);
            for (name, outcome) in outcomes {
                phase1.entry(name).or_default().record(outcome);
            }
        }

        if targeted {
            // Always investigate every supplied test, even if it happened to pass
            // in phase 1 — that's the point of naming them.
            suspects = target_tests;
        } else {
            // Suspects: any test that failed at least once in phase 1.
            let mut s: Vec<String> = phase1
                .iter()
                .filter(|(_, c)| c.fails > 0)
                .map(|(name, _)| name.clone())
                .collect();
            s.sort();
            println!(
                "\nPhase 1 complete: {} test(s) failed at least once out of {} observed.",
                s.len(),
                phase1.len()
            );
            if s.is_empty() {
                println!("No flaky tests detected. 🎉");
                // No genuine flakes → clear stale failures so `--rerun-failures`
                // doesn't re-run a previous run's entries.
                if let Err(e) = FailuresFile::clear() {
                    eprintln!("warning: failed to clear failures.json: {e}");
                }
                // Only phase 1 ran on this path.
                write_reports(&run_dir, &started, &opts, opts.iterations, 0, 0, Vec::new())?;
                return Ok(());
            }
            for name in &s {
                let c = &phase1[name];
                println!("  {} — {}/{} failed", name, c.fails, c.runs);
            }
            suspects = s;
        }
    }

    // ---- Phase 2: stress the suspect set together ----
    // Skipped in verify mode (isolation-only).
    let mut stress: BTreeMap<String, PhaseCounts> = BTreeMap::new();
    if !verify_mode && !opts.no_stress && opts.stress_iterations > 0 {
        println!(
            "\n=== Phase 2: stress suspect set × {} ===",
            opts.stress_iterations
        );
        // Config restricting the run to just the suspect set, wrapper attached.
        let stress_config = generate_nextest_config(&wrapper_str, Some(&suspects), false)?;
        let stress_config_path = stress_config.path().to_string_lossy().to_string();
        // High concurrency to maximize peer contention.
        let stress_threads = opts
            .threads
            .or_else(|| {
                std::thread::available_parallelism()
                    .ok()
                    .map(std::num::NonZero::get)
            })
            .unwrap_or(8);

        for i in 1..=opts.stress_iterations {
            println!("\n--- Stress iteration {i}/{} ---", opts.stress_iterations);
            let stats_base = run_dir.join(format!("phase2-stress/iter-{i}/stats"));
            let log_path = run_dir.join(format!("phase2-stress/iter-{i}/output.log"));
            let args = vec![
                "nextest".to_string(),
                "run".to_string(),
                "--workspace".to_string(),
                "--tests".to_string(),
                "--all-targets".to_string(),
                "--no-fail-fast".to_string(),
                "--config-file".to_string(),
                stress_config_path.clone(),
                "--profile".to_string(),
                "xtask-rerun-failures".to_string(),
                "--test-threads".to_string(),
                stress_threads.to_string(),
            ];
            run_teed(sh, &args, &stats_base, &log_path)?;
            let outcomes = load_outcomes(&stats_base);
            for (name, outcome) in outcomes {
                stress.entry(name).or_default().record(outcome);
            }
        }
        drop(stress_config);
    } else {
        println!("\n=== Phase 2: stress — skipped ===");
    }

    // ---- Phase 3: isolation ----
    let mut isolation: BTreeMap<String, PhaseCounts> = BTreeMap::new();
    let mut log_dirs: HashMap<String, PathBuf> = HashMap::new();
    // Distinct failure fingerprints per test, so the report explains the "why".
    let mut sig_map: HashMap<String, Vec<FailureSignature>> = HashMap::new();
    if !opts.no_isolation && opts.isolation_iterations > 0 {
        println!(
            "\n=== Phase 3: isolation × {} per test ({} suspects) ===",
            opts.isolation_iterations,
            suspects.len()
        );
        for (idx, test) in suspects.iter().enumerate() {
            let test_dir = run_dir.join("phase3-isolation").join(sanitize(test));
            fs::create_dir_all(&test_dir)?;
            log_dirs.insert(test.clone(), test_dir.clone());
            print!("[{}/{}] {} ", idx + 1, suspects.len(), test);
            std::io::stdout().flush().ok();

            let counts = isolation.entry(test.clone()).or_default();
            // Per-test summary of each iteration's outcome, written alongside the
            // logs so a failing test's history is readable at a glance.
            let mut summary = String::new();
            // Distinct failure fingerprints across this test's iterations.
            let mut sigs: Vec<FailureSignature> = Vec::new();
            for i in 1..=opts.isolation_iterations {
                // Write to a temp name, then rename to encode the outcome so the
                // failing runs' logs are trivially findable (*.FAIL.log).
                let tmp_log = test_dir.join(format!("run-{i}.log"));
                let result = run_isolated(test, &tmp_log, opts.isolation_log.as_deref())?;
                match result {
                    IsoResult::Passed => {
                        counts.record(Outcome {
                            passed: true,
                            timed_out: false,
                            duration_ms: 0,
                        });
                        let _ = fs::rename(&tmp_log, test_dir.join(format!("run-{i}.pass.log")));
                        summary.push_str(&format!("run {i}: PASS\n"));
                        print!(".");
                    }
                    IsoResult::Failed {
                        timed_out,
                        duration_ms,
                        signature,
                    } => {
                        counts.record(Outcome {
                            passed: false,
                            timed_out,
                            duration_ms,
                        });
                        let tag = if timed_out { "TIMEOUT" } else { "FAIL" };
                        let _ = fs::rename(&tmp_log, test_dir.join(format!("run-{i}.{tag}.log")));
                        summary.push_str(&format!("run {i}: {tag} ({duration_ms}ms)"));
                        if let Some(sig) = signature {
                            summary.push_str(&format!(
                                " — {}{}",
                                sig.location
                                    .as_deref()
                                    .map(|l| format!("{l}: "))
                                    .unwrap_or_default(),
                                sig.message
                            ));
                            if !sigs.contains(&sig) {
                                sigs.push(sig);
                            }
                        }
                        summary.push('\n');
                        print!("{}", if timed_out { "T" } else { "F" });
                    }
                    IsoResult::NoMatch => {
                        // A suspect whose filter matches nothing in isolation is a
                        // configuration problem (renamed/removed test), not a flake —
                        // fail hard rather than let it fall through and be silently
                        // classified as contention/clean on zero recorded runs.
                        let _ = fs::remove_file(&tmp_log);
                        println!();
                        eyre::bail!(
                            "isolation filter matched no tests for '{test}' — the test name may have changed or been removed"
                        );
                    }
                }
                std::io::stdout().flush().ok();
            }
            let c = &isolation[test];
            let header = format!("{test}\n{}/{} isolated runs failed\n\n", c.fails, c.runs);
            if let Err(e) = fs::write(test_dir.join("summary.txt"), header + &summary) {
                eprintln!("warning: failed to write isolation summary for {test}: {e}");
            }
            if !sigs.is_empty() {
                sig_map.insert(test.clone(), sigs);
            }
            println!(" → {}/{} failed", c.fails, c.runs);
        }
    } else {
        println!("\n=== Phase 3: isolation — skipped ===");
    }

    // ---- Assemble report ----
    let mut reports: Vec<TestReport> = Vec::new();
    for name in &suspects {
        let mut tr = TestReport {
            name: name.clone(),
            phase1: phase1.get(name).copied().unwrap_or_default(),
            stress: stress.get(name).copied(),
            isolation: isolation.get(name).copied(),
            classification: Classification::Unverified,
            log_dir: log_dirs.get(name).map(|p| p.to_string_lossy().to_string()),
            failure_signatures: sig_map.get(name).cloned().unwrap_or_default(),
        };
        tr.classification = tr.classify();
        reports.push(tr);
    }

    // Order: genuine flakes first (most actionable), then by phase-1 fail rate.
    reports.sort_by(|a, b| {
        b.classification
            .is_genuine()
            .cmp(&a.classification.is_genuine())
            .then(b.phase1.fails.cmp(&a.phase1.fails))
            .then(a.name.cmp(&b.name))
    });

    let genuine: Vec<&TestReport> = reports
        .iter()
        .filter(|r| r.classification.is_genuine())
        .collect();
    let genuine_count = genuine.len();

    // Feed genuine flakes into failures.json so `xtask test --rerun-failures`
    // can pick them up. Always rewrite it for the current run — when there are no
    // genuine flakes, clear it so `--rerun-failures` doesn't re-run stale entries
    // from a previous run.
    if genuine_count > 0 {
        let mut failures = FailuresFile {
            failed_tests: genuine.iter().map(|r| r.name.clone()).collect(),
        };
        failures.failed_tests.sort();
        if let Err(e) = failures.save() {
            eprintln!("warning: failed to update failures.json: {e}");
        }
    } else if let Err(e) = FailuresFile::clear() {
        eprintln!("warning: failed to clear failures.json: {e}");
    }

    print_summary(&reports);
    drop(phase1_config);

    // Report the counts that actually ran (verify mode skips phases 1 and 2).
    let eff_iterations = if verify_mode { 0 } else { opts.iterations };
    let eff_stress = if verify_mode || opts.no_stress {
        0
    } else {
        opts.stress_iterations
    };
    let eff_isolation = if opts.no_isolation {
        0
    } else {
        opts.isolation_iterations
    };
    let report_paths = write_reports(
        &run_dir,
        &started,
        &opts,
        eff_iterations,
        eff_stress,
        eff_isolation,
        reports,
    )?;
    println!("\nReports written:");
    println!("  {}", report_paths.0.display());
    println!("  {}", report_paths.1.display());

    // ---- CI gate ----
    if genuine_count > opts.tolerable_failures {
        return Err(eyre::eyre!(
            "{genuine_count} genuinely-flaky test(s) detected (tolerable: {})",
            opts.tolerable_failures
        ));
    }
    if genuine_count > 0 {
        println!(
            "\n{genuine_count} genuine flake(s) within tolerance ({}).",
            opts.tolerable_failures
        );
    }

    Ok(())
}

fn print_summary(reports: &[TestReport]) {
    println!("\n=== Flaky Test Summary ===");
    for r in reports {
        let p1 = &r.phase1;
        print!(
            "\n{}\n  classification: {}\n  phase1: {}/{} failed",
            r.name,
            r.classification.label(),
            p1.fails,
            p1.runs
        );
        if p1.timeouts > 0 {
            print!(" ({} timeouts)", p1.timeouts);
        }
        if let Some(s) = r.stress {
            print!("\n  stress: {}/{} failed", s.fails, s.runs);
            if s.timeouts > 0 {
                print!(" ({} timeouts)", s.timeouts);
            }
        }
        if let Some(iso) = r.isolation {
            print!(
                "\n  isolation: {}/{} failed (avg {}ms)",
                iso.fails,
                iso.runs,
                iso.avg_ms()
            );
            if iso.timeouts > 0 {
                print!(" ({} timeouts)", iso.timeouts);
            }
        }
        for sig in &r.failure_signatures {
            print!(
                "\n  ↳ {}{}",
                sig.location
                    .as_deref()
                    .map(|l| format!("{l}: "))
                    .unwrap_or_default(),
                sig.message
            );
        }
        if let Some(ref d) = r.log_dir {
            print!("\n  logs: {d}");
        }
        println!();
    }
}

/// Write `report.json` and `report.md`, returning their paths. The iteration
/// counts reflect what actually ran (e.g. 0 for phases skipped in verify mode).
fn write_reports(
    run_dir: &Path,
    started: &chrono::DateTime<Local>,
    opts: &FlakyOptions,
    iterations: usize,
    stress_iterations: usize,
    isolation_iterations: usize,
    reports: Vec<TestReport>,
) -> eyre::Result<(PathBuf, PathBuf)> {
    let genuine_flakes = reports
        .iter()
        .filter(|r| r.classification.is_genuine())
        .count();
    let report = Report {
        started_at: started.to_rfc3339(),
        iterations,
        stress_iterations,
        isolation_iterations,
        total_suspects: reports.len(),
        genuine_flakes,
        tests: reports,
    };

    let json_path = run_dir.join("report.json");
    fs::write(&json_path, serde_json::to_string_pretty(&report)?)?;

    let md_path = run_dir.join("report.md");
    fs::write(&md_path, render_markdown(&report))?;

    // When requested, emit the report to stdout for downstream tooling (CI).
    // The phases stream lots of nextest output to stdout, so the payload is
    // wrapped in unambiguous sentinel lines and printed as a single compact
    // line. Extract with e.g.:
    //   sed -n "/$JSON_BEGIN/{n;p;}" <log>
    if opts.json {
        println!("{JSON_BEGIN}");
        println!("{}", serde_json::to_string(&report)?);
        println!("{JSON_END}");
    }

    Ok((json_path, md_path))
}

/// Sentinel lines bracketing the machine-readable JSON payload on stdout.
/// Kept distinctive so tooling can extract the payload amid nextest output.
pub const JSON_BEGIN: &str = "<<<FLAKY_REPORT_JSON_BEGIN>>>";
pub const JSON_END: &str = "<<<FLAKY_REPORT_JSON_END>>>";

fn render_markdown(report: &Report) -> String {
    let mut s = String::new();
    s.push_str("# Flaky Test Detection Report\n\n");
    s.push_str(&format!("- **Started:** {}\n", report.started_at));
    s.push_str(&format!(
        "- **Phases:** full-suite ×{}, stress ×{}, isolation ×{}\n",
        report.iterations, report.stress_iterations, report.isolation_iterations
    ));
    s.push_str(&format!("- **Suspects:** {}\n", report.total_suspects));
    s.push_str(&format!(
        "- **Genuine flakes:** {}\n\n",
        report.genuine_flakes
    ));

    if report.tests.is_empty() {
        s.push_str("No flaky tests detected. 🎉\n");
        return s;
    }

    s.push_str("| Test | Classification | Phase 1 | Stress | Isolation |\n");
    s.push_str("|------|----------------|---------|--------|-----------|\n");
    for t in &report.tests {
        let cell = |c: &Option<PhaseCounts>| match c {
            Some(p) => {
                let to = if p.timeouts > 0 {
                    format!(" ({}T)", p.timeouts)
                } else {
                    String::new()
                };
                format!("{}/{}{}", p.fails, p.runs, to)
            }
            None => "—".to_string(),
        };
        let p1to = if t.phase1.timeouts > 0 {
            format!(" ({}T)", t.phase1.timeouts)
        } else {
            String::new()
        };
        s.push_str(&format!(
            "| `{}` | {} | {}/{}{} | {} | {} |\n",
            t.name,
            t.classification.label(),
            t.phase1.fails,
            t.phase1.runs,
            p1to,
            cell(&t.stress),
            cell(&t.isolation),
        ));
    }

    // Failure signatures — the "why", so a reader (or agent) can go straight to
    // the root cause without opening the logs.
    if report
        .tests
        .iter()
        .any(|t| !t.failure_signatures.is_empty())
    {
        s.push_str("\n## Failure signatures\n\n");
        for t in &report.tests {
            if t.failure_signatures.is_empty() {
                continue;
            }
            s.push_str(&format!("- `{}`\n", t.name));
            for sig in &t.failure_signatures {
                let loc = sig
                    .location
                    .as_deref()
                    .map(|l| format!("`{l}`: "))
                    .unwrap_or_default();
                s.push_str(&format!("  - {loc}{}\n", sig.message));
            }
        }
    }

    s.push_str("\n## Logs\n\n");
    for t in &report.tests {
        if let Some(ref d) = t.log_dir {
            s.push_str(&format!("- `{}` → `{}`\n", t.name, d));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(runs: usize, fails: usize, timeouts: usize) -> PhaseCounts {
        PhaseCounts {
            runs,
            fails,
            timeouts,
            min_ms: 0,
            max_ms: 0,
            total_ms: 0,
        }
    }

    fn report(
        phase1: PhaseCounts,
        stress: Option<PhaseCounts>,
        isolation: Option<PhaseCounts>,
    ) -> TestReport {
        TestReport {
            name: "t".to_string(),
            phase1,
            stress,
            isolation,
            classification: Classification::Unverified,
            log_dir: None,
            failure_signatures: Vec::new(),
        }
    }

    #[test]
    fn classify_broken_when_isolation_always_fails() {
        let r = report(counts(5, 5, 0), None, Some(counts(10, 10, 0)));
        assert_eq!(r.classify(), Classification::Broken);
    }

    #[test]
    fn classify_genuine_when_isolation_sometimes_fails() {
        let r = report(counts(5, 3, 0), None, Some(counts(10, 4, 0)));
        assert_eq!(r.classify(), Classification::GenuineFlaky);
    }

    #[test]
    fn classify_timeout_bound_when_all_isolation_failures_are_timeouts() {
        let r = report(counts(5, 3, 3), None, Some(counts(10, 3, 3)));
        assert_eq!(r.classify(), Classification::TimeoutBound);
    }

    #[test]
    fn classify_peer_contention_when_isolation_clean_but_stress_fails() {
        let r = report(
            counts(5, 2, 0),
            Some(counts(5, 2, 0)),
            Some(counts(10, 0, 0)),
        );
        assert_eq!(r.classify(), Classification::PeerContention);
    }

    #[test]
    fn classify_suite_contention_when_only_full_suite_fails() {
        let r = report(
            counts(5, 2, 0),
            Some(counts(5, 0, 0)),
            Some(counts(10, 0, 0)),
        );
        assert_eq!(r.classify(), Classification::SuiteContention);
    }

    #[test]
    fn classify_clean_when_isolation_passes_with_no_earlier_failures() {
        // verify mode: phase1/stress empty, isolation all-pass → CLEAN, not contention.
        let r = report(counts(0, 0, 0), None, Some(counts(10, 0, 0)));
        assert_eq!(r.classify(), Classification::Clean);
        assert!(!Classification::Clean.is_genuine());
    }

    #[test]
    fn classify_unverified_when_isolation_skipped_and_stress_clean() {
        let r = report(counts(5, 2, 0), Some(counts(5, 0, 0)), None);
        assert_eq!(r.classify(), Classification::Unverified);
    }

    #[test]
    fn classify_unverified_when_isolation_skipped_even_if_stress_fails() {
        // Without an isolation pass we can't prove the test passes alone, so a
        // stress failure must not be labeled PeerContention — stays Unverified.
        let r = report(counts(5, 2, 0), Some(counts(5, 2, 0)), None);
        assert_eq!(r.classify(), Classification::Unverified);
    }

    #[test]
    fn genuine_classifications_gate_ci() {
        assert!(Classification::Broken.is_genuine());
        assert!(Classification::GenuineFlaky.is_genuine());
        assert!(Classification::TimeoutBound.is_genuine());
        assert!(!Classification::PeerContention.is_genuine());
        assert!(!Classification::SuiteContention.is_genuine());
        assert!(!Classification::Unverified.is_genuine());
    }

    #[test]
    fn phase_counts_track_min_max_avg() {
        let mut c = PhaseCounts::default();
        c.record(Outcome {
            passed: true,
            timed_out: false,
            duration_ms: 100,
        });
        c.record(Outcome {
            passed: false,
            timed_out: false,
            duration_ms: 300,
        });
        assert_eq!(c.runs, 2);
        assert_eq!(c.fails, 1);
        assert_eq!(c.min_ms, 100);
        assert_eq!(c.max_ms, 300);
        assert_eq!(c.avg_ms(), 200);
    }

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize("a::b::c"), "a__b__c");
        assert_eq!(sanitize("crate/mod::test name"), "crate_mod__test_name");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\u{1b}[32m INFO\u{1b}[0m hi"), " INFO hi");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn extract_signature_multiline_panic() {
        let log = "test foo ... FAILED\n\
            thread 'foo' (123) panicked at crates/chain-tests/src/x.rs:304:5:\n\
            tx2 should be promoted (chunks uploaded)\n\
            stack backtrace:\n";
        let sig = extract_failure_signature(log).unwrap();
        assert_eq!(
            sig.location.as_deref(),
            Some("crates/chain-tests/src/x.rs:304:5")
        );
        assert_eq!(sig.message, "tx2 should be promoted (chunks uploaded)");
    }

    #[test]
    fn extract_signature_inline_panic() {
        let log = "thread 'main' panicked at src/lib.rs:10:5: boom happened\n";
        let sig = extract_failure_signature(log).unwrap();
        assert_eq!(sig.location.as_deref(), Some("src/lib.rs:10:5"));
        assert_eq!(sig.message, "boom happened");
    }

    #[test]
    fn extract_signature_strips_ansi_from_message() {
        let log = "thread 'x' panicked at src/a.rs:1:1:\n\u{1b}[31massertion failed: left == right\u{1b}[0m\n";
        let sig = extract_failure_signature(log).unwrap();
        assert_eq!(sig.message, "assertion failed: left == right");
    }

    #[test]
    fn extract_signature_none_when_no_panic() {
        assert!(extract_failure_signature("everything is fine\nPASS\n").is_none());
    }

    #[test]
    fn parse_tests_from_report_and_failures() {
        let dir = irys_testing_utils::TempDirBuilder::new().build();

        let report = dir.path().join("report.json");
        std::fs::write(&report, r#"{"tests":[{"name":"a::x"},{"name":"b::y"}]}"#).unwrap();
        let mut got = parse_tests_from(&report).unwrap();
        got.sort();
        assert_eq!(got, vec!["a::x".to_string(), "b::y".to_string()]);

        let failures = dir.path().join("failures.json");
        std::fs::write(&failures, r#"{"failed_tests":["c::z"]}"#).unwrap();
        assert_eq!(
            parse_tests_from(&failures).unwrap(),
            vec!["c::z".to_string()]
        );
    }
}
