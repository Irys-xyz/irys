use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use xshell::{Cmd, Shell, cmd};

use xtask::failures::{
    self, FailuresFile, RunResults, generate_nextest_config, get_failures_file_path,
    get_stats_file_path,
};

const CARGO_FLAKE_VERSION: &str = "0.0.5";
const LLVM_COV_VERSION: &str = "0.6.16";
const NEXTEST_VERSION: &str = "0.9.124";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Runs tests via nextest, with failure tracking (& optional failure-only reruns)
    Test {
        /// Produce coverage files
        #[clap(
            short,
            long,
            default_value_t = false,
            conflicts_with = "rerun_failures"
        )]
        coverage: bool,
        /// Only run tests that failed in the previous run
        #[clap(long, default_value_t = false)]
        rerun_failures: bool,
        /// Clear the failures file and run all tests clean
        #[clap(long, default_value_t = false)]
        clean: bool,
        /// Don't update the failures file after the run
        #[clap(long, default_value_t = false)]
        no_update_failures: bool,
        /// Enable CPU and memory resource monitoring
        #[clap(long, default_value_t = false)]
        monitor: bool,
        /// Enable heap profiling via heaptrack for individual tests (Linux only)
        #[clap(long, default_value_t = false)]
        heap_profile: bool,
        /// Arbitrary passthrough args
        #[clap(last = true)]
        args: Vec<String>,
    },
    Check {
        #[clap(last = true)]
        args: Vec<String>,
    },
    FullCheck {
        #[clap(last = true)]
        args: Vec<String>,
    },
    FullBacon {
        #[clap(last = true)]
        args: Vec<String>,
    },
    Fmt {
        #[clap(short, long, default_value_t = false)]
        check_only: bool,
        #[clap(last = true)]
        args: Vec<String>,
    },
    Clippy {
        #[clap(last = true)]
        args: Vec<String>,
    },
    Doc {
        #[clap(last = true)]
        args: Vec<String>,
    },
    Typos,
    UnusedDeps,
    EmissionSimulation,
    LocalChecks {
        #[clap(short, long, default_value_t = false)]
        with_tests: bool,
        #[clap(short, long, default_value_t = false)]
        fix: bool,
    },
    CleanWorkspace,
    /// Run multiversion integration tests from the multiversion-tests crate.
    ///
    /// Examples:
    ///   cargo xtask multiversion-test                          # all tests
    ///   cargo xtask multiversion-test --test upgrade           # only tests/upgrade.rs
    ///   cargo xtask multiversion-test --filter rolling         # tests matching "rolling"
    ///   cargo xtask multiversion-test --test upgrade --filter rolling   # "rolling" in upgrade.rs
    MultiversionTest {
        /// Path to pre-built binary for the new (HEAD) version
        #[clap(long)]
        binary_new: Option<String>,
        /// Path to pre-built binary for the old (base) version
        #[clap(long)]
        binary_old: Option<String>,
        /// Git ref (branch/tag/hash) for the old version, or "CURRENT" to build
        /// from the working tree (default: CURRENT)
        #[clap(long)]
        old_ref: Option<String>,
        /// Git ref (branch/tag/hash) for the new version, or "CURRENT" to build
        /// from the working tree (default: CURRENT)
        #[clap(long)]
        new_ref: Option<String>,
        /// Which test file to run (e.g. "upgrade", "e2e"). Maps to `cargo test --test <name>`.
        /// Can be specified multiple times.
        #[clap(long = "test", short = 't')]
        test_targets: Vec<String>,
        /// Substring filter for test names (e.g. "rolling_upgrade").
        /// Passed to the test runner after `--`.
        #[clap(long, short)]
        filter: Option<String>,
        /// Cargo profile to build binaries with (e.g. "dev", "release", "debug-release").
        /// When omitted, tries "debug-release" then falls back to "release".
        #[clap(long)]
        profile: Option<String>,
        /// Remove the target/multiversion directory before running
        #[clap(long, default_value_t = false)]
        clean: bool,
        /// Remove only the target/multiversion/test-data directory before running
        #[clap(long, default_value_t = false)]
        clean_data: bool,
        /// Passthrough args forwarded verbatim after the built-in flags.
        /// The first `--` ends xtask flags; a second `--` separates cargo
        /// args from test-runner args (which land in `runner_passthrough`).
        /// e.g. cargo xtask multiversion-test -- -- --test-threads=2
        #[clap(last = true)]
        args: Vec<String>,
    },
    Flaky {
        #[clap(short, long, help = "Number of iterations to run")]
        iterations: Option<usize>,
        #[clap(short, long, help = "Clean workspace before running")]
        clean: bool,
        #[clap(short, long, help = "Number of threads to use")]
        threads: Option<usize>,
        #[clap(short, long, help = "Save output to timestamped file")]
        save: bool,
        #[clap(short = 'f', long, help = "Number of tolerable failures")]
        tolerable_failures: Option<usize>,
        #[clap(last = true, help = "Arguments to pass to cargo-flake")]
        args: Vec<String>,
    },
}

/// Build the nextest-wrapper binary, optionally with additional features
fn build_wrapper(sh: &Shell, features: Option<&str>) -> eyre::Result<PathBuf> {
    println!("Building nextest-wrapper...");
    let mut build_args = vec![
        "build".to_string(),
        "--package".to_string(),
        "nextest-monitor".to_string(),
        "--bin".to_string(),
        "nextest-wrapper".to_string(),
    ];
    if let Some(feat) = features {
        build_args.push("--features".to_string());
        build_args.push(feat.to_string());
    }
    cmd!(sh, "cargo {build_args...}").remove_and_run()?;

    // Get the target directory
    let metadata = MetadataCommand::new().exec()?;
    let target_dir = metadata.target_directory.as_std_path();
    let wrapper_path = target_dir.join("debug").join("nextest-wrapper");

    if !wrapper_path.exists() {
        return Err(eyre::eyre!(
            "Failed to find built wrapper at {}",
            wrapper_path.display()
        ));
    }

    Ok(wrapper_path)
}

fn run_command(command: Commands, sh: &Shell) -> eyre::Result<()> {
    match command {
        Commands::Test {
            args,
            coverage,
            rerun_failures,
            clean,
            no_update_failures,
            monitor,
            heap_profile,
        } => {
            if coverage && heap_profile {
                return Err(eyre::eyre!(
                    "--coverage and --heap-profile cannot be used together"
                ));
            }
            println!("cargo test");
            let _ = cmd!(
                sh,
                "cargo install --locked --version {NEXTEST_VERSION} cargo-nextest"
            )
            .remove_and_run();

            if coverage {
                println!("Installing llvm-tools and cargo-llvm-cov...");
                cmd!(sh, "rustup component add llvm-tools").run()?;

                // Check if the correct version is already installed
                let installed_version = std::process::Command::new("cargo")
                    .args(["llvm-cov", "--version"])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|s| {
                        // output is "cargo-llvm-cov 0.6.16"
                        s.split_whitespace().nth(1).map(str::to_owned)
                    });
                let needs_force = installed_version.as_deref() != Some(LLVM_COV_VERSION);
                if needs_force {
                    cmd!(
                        sh,
                        "cargo install --locked --force --version {LLVM_COV_VERSION} cargo-llvm-cov"
                    )
                    .remove_and_run()?;
                } else {
                    println!(
                        "cargo-llvm-cov {LLVM_COV_VERSION} already installed, skipping install"
                    );
                }
                cmd!(sh, "cargo llvm-cov clean --workspace").remove_and_run()?;
            }

            // this is needed otherwise some tests will fail (that assert panic messages)
            sh.set_var("RUST_BACKTRACE", "1");

            // Handle --clean: clear failures and stats
            if clean {
                println!("Clearing failures and stats...");
                FailuresFile::clear()?;
                let stats_path = get_stats_file_path();
                if stats_path.exists() {
                    fs::remove_file(&stats_path)?;
                }
                // Also remove per-entry stats files in the stats.d/ directory
                let mut stats_dir_name = stats_path.file_name().unwrap_or_default().to_os_string();
                stats_dir_name.push(".d");
                let stats_dir = stats_path.with_file_name(stats_dir_name);
                if stats_dir.exists() {
                    fs::remove_dir_all(&stats_dir)?;
                }
            }

            if !no_update_failures {
                failures::ensure_dir()?;
            }

            // Determine which tests to run
            let failed_tests_filter: Option<Vec<String>> = if rerun_failures && !clean {
                let failures = FailuresFile::load();

                if failures.is_empty() {
                    println!(
                        "Warning: No recorded failures found in {}. Running all tests.",
                        get_failures_file_path().display()
                    );
                    None
                } else {
                    println!(
                        "\x1b[1;33mwarning: Rerunning failed tests\x1b[0m: {:?}",
                        failures.failed_tests
                    );
                    Some(failures.failed_tests)
                }
            } else {
                None
            };

            // Set env vars for the nextest-wrapper
            let stats_path = get_stats_file_path();
            sh.set_var(
                "NEXTEST_MONITOR_OUTPUT",
                stats_path.to_string_lossy().as_ref(),
            );
            if monitor {
                sh.set_var("NEXTEST_MONITOR_CPU", "1");
                sh.set_var("NEXTEST_MONITOR_MEMORY", "1");
                println!("Monitoring CPU and memory usage...");
            } else {
                sh.set_var("NEXTEST_MONITOR_CPU", "0");
                sh.set_var("NEXTEST_MONITOR_MEMORY", "0");
            }

            // Heap profiling setup
            if heap_profile {
                if cmd!(sh, "which heaptrack")
                    .quiet()
                    .remove_and_run()
                    .is_err()
                {
                    return Err(eyre::eyre!(
                        "heaptrack not found. Install it with:\n  \
                         Ubuntu/Debian: sudo apt-get install heaptrack\n  \
                         Fedora: sudo dnf install heaptrack\n  \
                         Arch: sudo pacman -S heaptrack"
                    ));
                }
                sh.set_var("NEXTEST_MONITOR_HEAP_PROFILE", "1");

                let heap_dir = get_stats_file_path()
                    .parent()
                    .unwrap()
                    .join("heap-profiles");
                fs::create_dir_all(&heap_dir)?;

                println!("Heap profiling enabled via heaptrack");
                println!("  Profiles will be written to: {}", heap_dir.display());
                println!(
                    "  Tip: use --test-threads 1 and target specific tests with -E 'test(name)'"
                );
            }

            // Build the wrapper binary and generate config
            let wrapper_features: Option<&str> = if heap_profile {
                Some("heap-profile")
            } else {
                None
            };

            let config_file = {
                let wrapper_path = build_wrapper(sh, wrapper_features)?;
                let wrapper_path_str = wrapper_path.to_string_lossy().to_string();

                generate_nextest_config(
                    &wrapper_path_str,
                    failed_tests_filter.as_deref(),
                    coverage,
                )?
            };

            let user_has_package = args
                .iter()
                .any(|a| a == "-p" || a == "--package" || a.starts_with("--package="));

            let mut nextest_args = if coverage {
                vec![
                    "llvm-cov".to_string(),
                    "nextest".to_string(),
                    "--no-report".to_string(),
                ]
            } else {
                vec!["nextest".to_string(), "run".to_string()]
            };

            if !user_has_package {
                nextest_args.push("--workspace".to_string());
            }
            nextest_args.push("--tests".to_string());
            nextest_args.push("--all-targets".to_string());

            // Validate passthrough args don't conflict with xtask-injected flags.
            let user_has_config_file = args
                .iter()
                .any(|a| a == "--config-file" || a.starts_with("--config-file="));

            let user_has_profile = args
                .iter()
                .any(|a| a == "--profile" || a.starts_with("--profile="));

            let config_path = config_file.path().to_string_lossy().to_string();
            if user_has_config_file {
                return Err(eyre::eyre!(
                    "Do not pass --config-file via xtask passthrough args; xtask manages nextest config generation."
                ));
            }
            nextest_args.push("--config-file".to_string());
            nextest_args.push(config_path);

            // Use the rerun profile if filtering to failed tests
            if failed_tests_filter.is_some() {
                if user_has_profile {
                    return Err(eyre::eyre!(
                        "Do not pass --profile via xtask passthrough args when using --rerun-failures; xtask selects the profile."
                    ));
                }
                nextest_args.push("--profile".to_string());
                nextest_args.push("xtask-rerun-failures".to_string());
            } else if coverage {
                if user_has_profile {
                    return Err(eyre::eyre!(
                        "Do not pass --profile via xtask passthrough args when using --coverage; xtask selects the profile."
                    ));
                }
                nextest_args.push("--profile".to_string());
                nextest_args.push("coverage".to_string());
            } else if heap_profile {
                if user_has_profile {
                    return Err(eyre::eyre!(
                        "Do not pass --profile via xtask passthrough args when using --heap-profile; xtask selects the profile."
                    ));
                }
                nextest_args.push("--profile".to_string());
                nextest_args.push("heap-profile".to_string());
                nextest_args.push("--cargo-profile".to_string());
                nextest_args.push("heap-profile".to_string());
            }

            // Add user-provided args (by reference — args is needed later for coverage scope)
            nextest_args.extend(args.iter().cloned());

            // Run nextest
            let test_result = cmd!(sh, "cargo {nextest_args...}").remove_and_run();

            // Keep config file alive until after the command runs
            drop(config_file);

            // Process results and update failures file
            if !no_update_failures {
                let run_results = RunResults::load();
                let (passed, new_failed) = run_results.into_sets();

                let mut failures = if rerun_failures && !clean {
                    // When rerunning, start with existing failures
                    FailuresFile::load()
                } else {
                    // When running all tests, start clean
                    FailuresFile::default()
                };

                // Remove tests that now pass
                failures.failed_tests.retain(|t| !passed.contains(t));

                // Add new failures
                for failed in &new_failed {
                    if !failures.failed_tests.contains(failed) {
                        failures.failed_tests.push(failed.clone());
                    }
                }

                // Sort for consistent output
                failures.failed_tests.sort();

                // Save the updated failures
                failures.save()?;

                if failures.is_empty() {
                    if !passed.is_empty() {
                        println!("All tests passed! Failures file cleared.");
                    }
                } else {
                    println!(
                        "Recorded {} failed test(s) to {}",
                        failures.failed_tests.len(),
                        get_failures_file_path().display()
                    );
                }
            }

            // Generate coverage reports before propagating test failures,
            // so reports are available even when some tests fail.
            if coverage {
                println!("Generating coverage reports...");
                fs::create_dir_all("target/llvm-cov")?;

                // Forward package scope flags to `cargo llvm-cov report`.
                // The report subcommand only accepts --package/-p, not
                // --workspace or --exclude. When unsupported scope flags
                // are present, warn that report coverage may be broader.
                let mut scope_args: Vec<String> = Vec::new();
                let mut has_unsupported_scope = false;
                let mut iter = args.iter();
                while let Some(arg) = iter.next() {
                    if arg.starts_with("--package=") || arg.starts_with("-p=") {
                        scope_args.push(arg.clone()); // clone: collecting user args for reuse
                    } else if arg == "-p" || arg == "--package" {
                        scope_args.push(arg.clone()); // clone: collecting user args for reuse
                        if let Some(val) = iter.next() {
                            scope_args.push(val.clone()); // clone: collecting user args for reuse
                        }
                    } else if arg == "--exclude"
                        || arg.starts_with("--exclude=")
                        || arg == "--workspace"
                    {
                        has_unsupported_scope = true;
                        if arg == "--exclude" {
                            iter.next(); // skip the value
                        }
                    }
                }
                if has_unsupported_scope {
                    eprintln!(
                        "Warning: cargo llvm-cov report does not support --workspace/--exclude; \
                         report may include crates excluded from the test run"
                    );
                }

                let scope_args_lcov = scope_args.clone(); // clone: needed for lcov cmd! invocation
                let scope_args_mismatch = scope_args.clone(); // clone: needed for mismatch analysis
                let html_result = cmd!(
                    sh,
                    "cargo llvm-cov report --html --output-dir target/llvm-cov/html {scope_args...}"
                )
                .remove_and_run();

                if let Err(e) = &html_result {
                    eprintln!("Warning: HTML coverage report generation failed: {e}");
                }

                let lcov_result = cmd!(
                    sh,
                    "cargo llvm-cov report --lcov --output-path target/llvm-cov/lcov.info {scope_args_lcov...}"
                )
                .remove_and_run();

                if html_result.is_ok() {
                    println!("  HTML report: target/llvm-cov/html/index.html");
                    if std::env::var("CI").is_err() {
                        #[cfg(target_os = "macos")]
                        let _ = cmd!(sh, "open target/llvm-cov/html/index.html").remove_and_run();

                        #[cfg(target_os = "linux")]
                        let _ =
                            cmd!(sh, "xdg-open target/llvm-cov/html/index.html").remove_and_run();
                    }
                }
                if lcov_result.is_ok() {
                    println!("  LCOV report: target/llvm-cov/lcov.info");
                }

                lcov_result.inspect_err(|e| eprintln!("LCOV report generation failed: {e}"))?;

                // Log which functions have mismatched coverage data
                if let Err(e) = log_coverage_mismatches(sh, &scope_args_mismatch) {
                    eprintln!("Warning: coverage mismatch analysis failed: {e}");
                }
            }

            test_result?;
        }
        Commands::Check { args } => {
            println!("cargo check");
            cmd!(sh, "cargo check {args...}").remove_and_run()?;
        }
        Commands::FullCheck { args } => {
            println!("cargo check --all-features --all-targets");
            cmd!(sh, "cargo check --all-features --all-targets {args...}").remove_and_run()?;
        }
        Commands::FullBacon { args } => {
            let _ = cmd!(sh, "cargo install --locked --version 3.16.0 bacon").remove_and_run();
            println!("bacon check-all ");
            cmd!(sh, "bacon check-all {args...}").remove_and_run()?;
        }
        Commands::Clippy { args } => {
            println!("cargo clippy");
            cmd!(sh, "cargo clippy --workspace --tests --locked {args...}").remove_and_run()?;
        }
        Commands::Fmt {
            check_only: only_check,
            args,
        } => {
            if only_check {
                cmd!(sh, "cargo fmt --check {args...}").remove_and_run()?;
            } else {
                println!("cargo fmt & clippy fix");
                cmd!(sh, "cargo fmt --all").remove_and_run()?;
                // clippy --fix applies both rustc and clippy suggestions, so a
                // separate `cargo fix` pass is unnecessary and would force a full
                // recompile (clippy uses a different compiler driver).
                let _rustflags_guard = sh.push_env("RUSTFLAGS", "-D warnings");
                cmd!(
                    sh,
                    "cargo clippy --fix --allow-dirty --allow-staged --workspace --tests {args...}"
                )
                .remove_and_run()?;
            }
        }
        Commands::Doc { args } => {
            println!("cargo doc");
            cmd!(sh, "cargo doc --workspace --no-deps {args...}").remove_and_run()?;

            if std::env::var("CI").is_err() {
                #[cfg(target_os = "macos")]
                cmd!(sh, "open target/doc/irys/index.html").remove_and_run()?;

                #[cfg(target_os = "linux")]
                cmd!(sh, "xdg-open target/doc/irys/index.html").remove_and_run()?;
            }
        }
        Commands::Typos => {
            println!("typos check");
            cmd!(sh, "cargo install --locked --version 1.35.4 typos-cli").remove_and_run()?;
            cmd!(sh, "typos").remove_and_run()?;
        }
        Commands::UnusedDeps => {
            println!("unused deps");
            cmd!(sh, "cargo install --locked --version 0.8.0 cargo-machete").remove_and_run()?;
            cmd!(sh, "cargo-machete").run()?;
        }
        Commands::EmissionSimulation => {
            println!("block reward emission simulation");
            cmd!(
                sh,
                "cargo run --bin irys-reward-curve-simulation --features=emission-sim"
            )
            .remove_and_run()?;
        }
        Commands::LocalChecks { with_tests, fix } => {
            run_command(
                Commands::Fmt {
                    check_only: !fix,
                    args: vec![],
                },
                sh,
            )?;
            if !fix {
                // When --fix is used, `cargo fix` and `cargo clippy --fix` (in the
                // Fmt handler) already compile the full workspace with -D warnings
                // and would fail on any issues, so these verification steps are only
                // needed for the non-fix (check-only) path.
                let _rustflags_guard = sh.push_env("RUSTFLAGS", "-D warnings");
                run_command(
                    Commands::Check {
                        args: vec!["--tests".to_string()],
                    },
                    sh,
                )?;
                run_command(Commands::Clippy { args: vec![] }, sh)?;
            }
            run_command(Commands::UnusedDeps, sh)?;
            run_command(Commands::Typos, sh)?;
            if with_tests {
                run_command(
                    Commands::Test {
                        coverage: false,
                        rerun_failures: false,
                        clean: false,
                        no_update_failures: false,
                        monitor: false,
                        heap_profile: false,
                        args: vec![],
                    },
                    sh,
                )?
            }
        }
        Commands::CleanWorkspace => {
            // get workspace metadata
            let metadata = MetadataCommand::new().exec()?;

            // filter for just workspace member packages
            let workspace_packages: Vec<&Package> = metadata
                .packages
                .iter()
                .filter(|pkg| metadata.workspace_members.contains(&pkg.id))
                .collect();

            // clean
            // note: can't parallelize due to locks on the build dir
            for package in workspace_packages {
                let name = package.name.to_string();
                println!("Cleaning {}", &name);
                cmd!(sh, "cargo clean --package {name}").remove_and_run()?;
            }
        }
        Commands::MultiversionTest {
            binary_new,
            binary_old,
            old_ref,
            new_ref,
            test_targets,
            filter,
            profile,
            clean,
            clean_data,
            args,
        } => {
            println!("multiversion integration tests");
            let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("xtask CARGO_MANIFEST_DIR must have a parent");
            let multiversion_dir = workspace_root.join("target/multiversion");
            if clean {
                if multiversion_dir.exists() {
                    println!("Cleaning {}...", multiversion_dir.display());
                    fs::remove_dir_all(&multiversion_dir)?;
                    println!("Done.");
                } else {
                    println!("Nothing to clean (target/multiversion does not exist).");
                }
            } else if clean_data {
                let test_data_dir = multiversion_dir.join("test-data");
                if test_data_dir.exists() {
                    println!("Cleaning {}...", test_data_dir.display());
                    fs::remove_dir_all(&test_data_dir)?;
                    println!("Done.");
                } else {
                    println!("Nothing to clean (target/multiversion/test-data does not exist).");
                }
            }
            let binary_new = binary_new
                .map(|p| {
                    std::fs::canonicalize(&p)
                        .map_err(|e| eyre::eyre!("binary_new: failed to canonicalize `{p}`: {e}"))
                })
                .transpose()?;
            let binary_old = binary_old
                .map(|p| {
                    std::fs::canonicalize(&p)
                        .map_err(|e| eyre::eyre!("binary_old: failed to canonicalize `{p}`: {e}"))
                })
                .transpose()?;
            // Generate a human-readable, collision-resistant run ID.
            let now = chrono::Local::now();
            let run_id = format!(
                "{}.{:03}-pid{}",
                now.format("%Y-%m-%d_%H-%M-%S"),
                now.timestamp_subsec_millis(),
                std::process::id()
            );
            let _dir = sh.push_dir(workspace_root.join("crates/tooling/multiversion-tests"));
            if let Some(ref path) = binary_new {
                sh.set_var("IRYS_BINARY_NEW", path);
            }
            if let Some(ref path) = binary_old {
                sh.set_var("IRYS_BINARY_OLD", path);
            }
            if let Some(ref git_ref) = old_ref {
                sh.set_var("IRYS_OLD_REF", git_ref);
            }
            if let Some(ref git_ref) = new_ref {
                sh.set_var("IRYS_NEW_REF", git_ref);
            }
            if let Some(ref p) = profile {
                sh.set_var("IRYS_BUILD_PROFILE", p);
            }
            sh.set_var("IRYS_RUN_ID", &run_id);

            // Pre-flight: when running *all* test targets, an explicit old version
            // must be specified to avoid testing CURRENT against itself.
            if test_targets.is_empty()
                && binary_old.is_none()
                && old_ref.as_deref().is_none_or(|r| r == "CURRENT")
            {
                return Err(eyre::eyre!(
                    "multiversion-test: running all tests requires an old version.\n\
                     Provide --binary-old <path> or --old-ref <git-ref> (not CURRENT)."
                ));
            }

            // Build cargo test invocation.
            // cargo test [--test <target>...] [--tests] <passthrough_cargo>
            //   -- --ignored --test-threads=1 --nocapture [<filter>] <passthrough_runner>
            let mut test_args = vec!["test".to_string()];
            if test_targets.is_empty() {
                // No specific test files — run all test targets.
                test_args.push("--tests".to_string());
            } else {
                for target in &test_targets {
                    test_args.push("--test".to_string());
                    test_args.push(target.clone());
                }
            }

            // Split passthrough args on `--` into cargo args and test runner args.
            let (cargo_passthrough, runner_passthrough) = match args.iter().position(|a| a == "--")
            {
                Some(pos) => (args[..pos].to_vec(), args[pos + 1..].to_vec()),
                None => (args, vec![]),
            };
            test_args.extend(cargo_passthrough);
            test_args.extend([
                "--".to_string(),
                "--ignored".to_string(),
                "--test-threads=1".to_string(),
                "--nocapture".to_string(),
            ]);
            if let Some(ref f) = filter {
                test_args.push(f.clone());
            }
            test_args.extend(runner_passthrough);
            let result = cmd!(sh, "cargo {test_args...}").remove_and_run();
            let test_data_dir = multiversion_dir.join("test-data").join(&run_id);

            // Aggregate per-test .status marker files into a summary.
            write_status_summary(&test_data_dir);

            if result.is_err() {
                if test_data_dir.exists() {
                    eprintln!("test artifacts preserved at: {}", test_data_dir.display());
                }
                if let Ok(entries) = std::fs::read_dir(&multiversion_dir) {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().starts_with("worktree-") {
                            eprintln!("worktree preserved at: {}", entry.path().display());
                        }
                    }
                }
            }
            result?;
        }
        Commands::Flaky {
            iterations,
            clean,
            threads,
            save,
            tolerable_failures,
            args,
        } => {
            // Clean workspace if requested
            if clean {
                run_command(Commands::CleanWorkspace, sh)?;

                // Prebuild the project after cleaning
                println!("Prebuilding the project");
                cmd!(sh, "cargo build --workspace --tests").remove_and_run()?;
            }

            // Build command arguments
            let mut command_args = vec!["flake".to_string()];

            // Add iterations (default to 5 if not specified)
            let iters = iterations.unwrap_or(5);
            command_args.push("--iterations".to_string());
            command_args.push(iters.to_string());

            // Add threads if specified
            if let Some(thread_count) = threads {
                command_args.push("--threads".to_string());
                command_args.push(thread_count.to_string());
            }

            // Add tolerable failures if specified
            if let Some(failures) = tolerable_failures {
                command_args.push("--tolerable-failures".to_string());
                command_args.push(failures.to_string());
            }

            // Add any additional arguments after --
            let args_for_header = args.clone();
            if !args.is_empty() {
                command_args.push("--".to_string());
                command_args.extend(args);
            }

            if save {
                // Create target directory if it doesn't exist
                fs::create_dir_all("target")?;

                // Generate timestamp for output file
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs();
                let timestamp = chrono::DateTime::from_timestamp(now as i64, 0)
                    .unwrap()
                    .format("%Y-%m-%d-%H-%M-%S");
                let output_file = format!("target/flaky-test-output-{timestamp}.txt");

                // Create output file and write header
                let mut file = fs::File::create(&output_file)?;
                writeln!(file, "=== Flaky Test Run - {timestamp} ===")?;
                write!(file, "Command: cargo flake --iterations {iters}")?;
                if let Some(thread_count) = threads {
                    write!(file, " --threads {thread_count}")?;
                }
                if let Some(failures) = tolerable_failures {
                    write!(file, " --tolerable-failures {failures}")?;
                }
                if !args_for_header.is_empty() {
                    write!(file, " -- {}", args_for_header.join(" "))?;
                }
                writeln!(file)?;
                writeln!(file)?;
                file.flush()?;
                drop(file);

                // Use script to preserve TTY behavior for progress bars
                println!("Running cargo-flake to detect flaky tests");
                println!("Streaming output to: {output_file}");

                // Install cargo-flake first
                cmd!(
                    sh,
                    "cargo install --locked --version {CARGO_FLAKE_VERSION} cargo-flake"
                )
                .remove_and_run()?;

                // Use script command to preserve TTY and tee to save output
                // Handle different script syntax between platforms
                let script_result = if cfg!(target_os = "macos") {
                    // macOS script syntax
                    let script_command = format!(
                        "script -q /dev/stdout cargo {} | tee -a '{}'",
                        command_args.join(" "),
                        output_file
                    );
                    cmd!(sh, "bash -c {script_command}")
                        .env("RUST_BACKTRACE", "1")
                        .run()
                } else if cfg!(target_os = "linux") {
                    // Linux script syntax
                    let script_command = format!(
                        "script -q -e -c 'cargo {}' /dev/stdout | tee -a '{}'",
                        command_args.join(" "),
                        output_file
                    );
                    cmd!(sh, "bash -c {script_command}")
                        .env("RUST_BACKTRACE", "1")
                        .run()
                } else {
                    // Fallback for other platforms - try basic tee without script
                    eprintln!(
                        "Warning: script command may not be available on this platform, progress bars may not display correctly"
                    );
                    let tee_command = format!(
                        "cargo {} 2>&1 | tee -a '{}'",
                        command_args.join(" "),
                        output_file
                    );
                    cmd!(sh, "bash -c {tee_command}")
                        .env("RUST_BACKTRACE", "1")
                        .run()
                };

                // If script command fails, fallback to basic tee
                if script_result.is_err() {
                    eprintln!(
                        "Warning: script command failed, falling back to basic output capture"
                    );
                    let tee_command = format!(
                        "cargo {} 2>&1 | tee -a '{}'",
                        command_args.join(" "),
                        output_file
                    );
                    cmd!(sh, "bash -c {tee_command}")
                        .env("RUST_BACKTRACE", "1")
                        .run()?;
                } else {
                    script_result?;
                }
            } else {
                // Run command without file output - show output in terminal
                println!("Running cargo-flake to detect flaky tests");

                // Install cargo-flake if not already installed
                cmd!(
                    sh,
                    "cargo install --locked --version {CARGO_FLAKE_VERSION} cargo-flake"
                )
                .remove_and_run()?;

                cmd!(sh, "cargo {command_args...}")
                    .env("RUST_BACKTRACE", "1")
                    .run()?;
            }
        }
    };
    Ok(())
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let sh = Shell::new()?;
    let args = Args::parse();
    run_command(args.command, &sh)
}

/// Scans per-test `.status` marker files and writes an aggregate `status.txt`.
///
/// Each test's `Cluster::start()` writes `.status = "RUNNING"` into its run
/// subdirectory; `Cluster::shutdown()` overwrites it with `"PASSED"`.  If the
/// test panicked, shutdown never ran and the marker stays `RUNNING` → FAILED.
fn write_status_summary(run_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(run_dir) else {
        return;
    };

    let mut tests: Vec<(String, String, Option<String>, Option<String>)> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let raw = match std::fs::read_to_string(e.path().join(".status")) {
                Ok(contents) => contents,
                Err(err) => {
                    eprintln!("warning: failed to read .status for {name}: {err}");
                    "FAILED".to_string()
                }
            };
            let mut lines = raw.lines();
            let status_line = lines.next().unwrap_or("").trim();
            let status = match status_line {
                "PASSED" => "PASSED",
                "RUNNING" => "FAILED",
                other => other,
            };
            let mut old_ref = None;
            let mut new_ref = None;
            for line in lines {
                if let Some(val) = line.strip_prefix("old_ref: ") {
                    old_ref = Some(val.to_owned());
                } else if let Some(val) = line.strip_prefix("new_ref: ") {
                    new_ref = Some(val.to_owned());
                }
            }
            (name, status.to_owned(), old_ref, new_ref)
        })
        .collect();

    if tests.is_empty() {
        return;
    }

    tests.sort();

    let mut summary = String::new();
    for (name, status, old_ref, new_ref) in &tests {
        let mut line = format!("{name}: {status}");
        if let Some(r) = old_ref {
            line.push_str(&format!(" (old: {r}"));
            if let Some(r) = new_ref {
                line.push_str(&format!(", new: {r})"));
            } else {
                line.push(')');
            }
        } else if let Some(r) = new_ref {
            line.push_str(&format!(" (new: {r})"));
        }
        line.push('\n');
        summary.push_str(&line);
    }

    let path = run_dir.join("status.txt");
    if let Err(e) = std::fs::write(&path, &summary) {
        eprintln!("warning: failed to write status summary: {e}");
    } else {
        eprintln!("test status summary: {}", path.display());
    }
}

/// Parse `"warning: N functions have mismatched data"` from llvm-cov stderr.
/// Returns 0 if the line is absent.
fn parse_mismatch_count_from_stderr(stderr: &str) -> usize {
    stderr
        .lines()
        .find_map(|l| {
            let n = l
                .strip_prefix("warning: ")
                .and_then(|r| r.strip_suffix(" functions have mismatched data"))?;
            n.parse().ok()
        })
        .unwrap_or(0)
}

/// Parse `cargo llvm-cov report --json` output and return mangled symbol names
/// whose `count` field is zero, filtered to names that belong to workspace
/// crates (i.e. contain one of `crate_names` as a substring).
fn collect_zero_coverage_from_json(
    json_str: &str,
    crate_names: &[String],
) -> eyre::Result<HashSet<String>> {
    let json: serde_json::Value = serde_json::from_str(json_str)?;
    let set = json
        .pointer("/data/0/functions")
        .and_then(|v| v.as_array())
        .map(|funcs| {
            funcs
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?;
                    let count = f.get("count")?.as_u64()?;
                    if count == 0 && crate_names.iter().any(|c| name.contains(c.as_str())) {
                        Some(name.to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(set)
}

/// Return the path to the most-recently-modified `.profdata` file inside
/// `dir`, or `None` if the directory is missing or contains no `.profdata`.
fn find_latest_profdata(dir: PathBuf) -> Option<PathBuf> {
    let entries = fs::read_dir(&dir).ok()?;
    entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "profdata"))
        .max_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|e| e.path())
}

/// Parse `llvm-profdata show --all-functions` output and return the set of
/// mangled symbol names whose `Function count` is greater than zero, filtered
/// to names belonging to workspace crates.
///
/// llvm-profdata show --all-functions output format (tested LLVM 18/19):
/// ```text
///   <function_name>:        (2-space indent)
///     Hash: 0x...           (4-space indent)
///     Counters: N
///     Function count: M     ← M > 0 means the function was called
/// ```
/// Format is not guaranteed stable; if parsing fails we return an empty set.
fn collect_executed_from_profdata(
    profdata_output: &str,
    crate_names: &[String],
) -> HashSet<String> {
    let mut current_func: Option<String> = None;
    let mut executed: HashSet<String> = HashSet::new();
    for line in profdata_output.lines() {
        if line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(':') {
            let name = line.trim().trim_end_matches(':');
            current_func = if crate_names.iter().any(|c| name.contains(c.as_str())) {
                Some(name.to_owned())
            } else {
                None
            };
        } else if let Some(ref func) = current_func
            && let Some(count_str) = line.trim().strip_prefix("Function count: ")
        {
            if count_str.parse::<u64>().is_ok_and(|c| c > 0) {
                executed.insert(func.clone());
            }
            current_func = None;
        }
    }
    executed
}

/// Detect and log functions with mismatched coverage data.
///
/// When llvm-cov encounters a function whose profdata hash doesn't match the
/// binary's coverage mapping hash, it silently drops that function's counters.
/// This identifies those functions by cross-referencing: functions that were
/// executed (non-zero `Function count` in profdata) but show zero coverage in
/// the JSON export are the hash mismatches. Results are filtered to workspace
/// crates to avoid noise from std/deps.
fn log_coverage_mismatches(sh: &Shell, scope_args: &[String]) -> eyre::Result<()> {
    // Run `cargo llvm-cov report --json` via std::process::Command so we can
    // capture stderr, where llvm-cov emits "warning: N functions have
    // mismatched data". xshell's Cmd::read() only captures stdout.
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["llvm-cov", "report", "--json"]);
    cmd.args(scope_args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    for k in RING_ENV_VARS {
        cmd.env_remove(k);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre::eyre!("cargo llvm-cov report --json failed: {stderr}"));
    }
    let json_str = String::from_utf8(output.stdout)?;
    let stderr_str = String::from_utf8_lossy(&output.stderr);

    if !stderr_str.is_empty() {
        eprintln!(
            "  [debug] cargo llvm-cov report --json stderr ({} bytes):",
            stderr_str.len()
        );
        for line in stderr_str.lines().take(10) {
            eprintln!("  [debug]   {line}");
        }
    } else {
        eprintln!("  [debug] cargo llvm-cov report --json produced no stderr");
    }

    let mismatch_count = parse_mismatch_count_from_stderr(&stderr_str);

    if mismatch_count == 0 {
        println!("  No coverage data mismatches detected.");
        return Ok(());
    }

    println!("  llvm-cov reports {mismatch_count} functions with mismatched coverage data.");
    println!("  Identifying affected workspace functions...");

    // Collect workspace crate names (hyphens → underscores) for filtering
    let metadata = MetadataCommand::new().exec()?;
    let crate_names: Vec<String> = metadata
        .workspace_packages()
        .iter()
        .map(|p| p.name.replace('-', "_"))
        .collect();

    // From the JSON export: workspace functions with zero execution count.
    // Hash-mismatched functions appear in the export (from the binary's coverage
    // mapping) but their profdata counters aren't applied, leaving count == 0.
    let zero_coverage = collect_zero_coverage_from_json(&json_str, &crate_names)?;

    // From profdata: workspace functions that were actually executed.
    let llvm_cov_target_dir = metadata
        .target_directory
        .as_std_path()
        .join("llvm-cov-target");
    let Some(profdata_path) = find_latest_profdata(llvm_cov_target_dir) else {
        println!("    (skipping function identification: no profdata file found)");
        return Ok(());
    };

    // Locate llvm-profdata via rustc sysroot
    let sysroot = cmd!(sh, "rustc --print sysroot").read()?;
    let rustc_info = cmd!(sh, "rustc -vV").read()?;
    let host_triple = rustc_info
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or_else(|| eyre::eyre!("could not determine host triple from rustc -vV"))?;
    let llvm_profdata = format!("{sysroot}/lib/rustlib/{host_triple}/bin/llvm-profdata");

    let profdata_str = profdata_path.display().to_string();
    let profdata_output = cmd!(sh, "{llvm_profdata} show --all-functions {profdata_str}").read()?;

    let executed = collect_executed_from_profdata(&profdata_output, &crate_names);

    // Intersection: executed in profdata ∩ zero coverage in export = hash mismatch.
    // Both sources use mangled symbol names from the coverage mapping.
    let mut mismatched: Vec<&String> = executed.intersection(&zero_coverage).collect();
    mismatched.sort();

    if mismatched.is_empty() {
        println!("    No workspace functions affected (mismatches are in dependencies).");
    } else {
        println!(
            "  Affected workspace functions ({} of {mismatch_count}):",
            mismatched.len()
        );
        let log_path = PathBuf::from("target/llvm-cov/mismatched-functions.txt");
        let mut file = fs::File::create(&log_path)?;
        for (i, func) in mismatched.iter().enumerate() {
            writeln!(file, "{func}")?;
            if i < 30 {
                println!("    {func}");
            }
        }
        if mismatched.len() > 30 {
            println!(
                "    ... and {} more (see {})",
                mismatched.len() - 30,
                log_path.display()
            );
        }
        println!("  Full list: {}", log_path.display());
        println!("  Tip: pipe through `rustfilt` to demangle function names");
    }

    Ok(())
}

/// Env vars that Ring's build.rs emits rerun conditions for, many of which are
/// absent under a regular `cargo check`, causing unnecessary rebuilds when
/// alternating between `cargo check` and xtask commands.
// TODO: remove once briansmith/ring#2454 is resolved and released; that issue
// tracks spurious rebuilds caused by ring's build.rs rerun conditions
const RING_ENV_VARS: &[&str] = &[
    "CARGO_MANIFEST_DIR",
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_MANIFEST_LINKS",
    "RING_PREGENERATE_ASM",
    // "OUT_DIR",
    "CARGO_CFG_TARGET_ARCH",
    "CARGO_CFG_TARGET_OS",
    "CARGO_CFG_TARGET_ENV",
    "CARGO_CFG_TARGET_ENDIAN",
    // "DEBUG",
];

fn remove_ring_env_vars(cmd: Cmd<'_>) -> Cmd<'_> {
    let mut c = cmd;
    for k in RING_ENV_VARS {
        c = c.env_remove(k);
    }
    c
}

pub trait CmdExt {
    fn remove_and_run(self) -> Result<(), xshell::Error>;
    fn remove_and_read(self) -> Result<String, xshell::Error>;
}

impl CmdExt for Cmd<'_> {
    fn remove_and_run(self) -> Result<(), xshell::Error> {
        remove_ring_env_vars(self).run()
    }

    fn remove_and_read(self) -> Result<String, xshell::Error> {
        remove_ring_env_vars(self).read()
    }
}

#[cfg(test)]
mod coverage_mismatch_tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;

    #[test]
    fn test_parse_mismatch_count_present() {
        let stderr = "some preamble\nwarning: 135 functions have mismatched data\nother line\n";
        assert_eq!(parse_mismatch_count_from_stderr(stderr), 135);
    }

    #[test]
    fn test_parse_mismatch_count_absent() {
        assert_eq!(
            parse_mismatch_count_from_stderr("no relevant lines here"),
            0
        );
        assert_eq!(parse_mismatch_count_from_stderr(""), 0);
    }

    #[test]
    fn test_parse_mismatch_count_zero() {
        let stderr = "warning: 0 functions have mismatched data\n";
        assert_eq!(parse_mismatch_count_from_stderr(stderr), 0);
    }

    #[test]
    fn test_collect_zero_coverage_from_json_basic() {
        let json = r#"{
            "data": [{
                "functions": [
                    {"name": "my_crate::foo", "count": 0},
                    {"name": "my_crate::bar", "count": 5},
                    {"name": "other_crate::baz", "count": 0},
                    {"name": "my_crate::qux", "count": 0}
                ]
            }]
        }"#;
        let crate_names = vec!["my_crate".to_owned()];
        let result = collect_zero_coverage_from_json(json, &crate_names).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains("my_crate::foo"));
        assert!(result.contains("my_crate::qux"));
        assert!(!result.contains("my_crate::bar")); // count > 0
        assert!(!result.contains("other_crate::baz")); // not in workspace
    }

    #[test]
    fn test_collect_zero_coverage_from_json_empty_functions() {
        let json = r#"{"data": [{"functions": []}]}"#;
        let crate_names = vec!["my_crate".to_owned()];
        let result = collect_zero_coverage_from_json(json, &crate_names).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_zero_coverage_from_json_missing_path() {
        let json = r#"{"data": []}"#;
        let crate_names = vec!["my_crate".to_owned()];
        let result = collect_zero_coverage_from_json(json, &crate_names).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_latest_profdata_missing_dir() {
        let result = find_latest_profdata(PathBuf::from("/nonexistent/path/xyz"));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_latest_profdata_empty_dir() {
        let tmp = TempDirBuilder::new().build();
        let result = find_latest_profdata(tmp.path().to_path_buf());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_latest_profdata_picks_newest() {
        let tmp = TempDirBuilder::new().build();
        let old = tmp.path().join("old.profdata");
        let new = tmp.path().join("new.profdata");
        std::fs::write(&old, b"").unwrap();
        let old_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        // Poll until new's mtime is strictly newer than old's — filesystem timestamp
        // granularity varies and a fixed sleep is unreliable.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::fs::write(&new, b"").unwrap();
            let new_mtime = std::fs::metadata(&new).unwrap().modified().unwrap();
            if new_mtime > old_mtime {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for filesystem mtime to advance"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let result = find_latest_profdata(tmp.path().to_path_buf()).unwrap();
        assert_eq!(result, new);
    }

    #[test]
    fn test_find_latest_profdata_ignores_non_profdata() {
        let tmp = TempDirBuilder::new().build();
        std::fs::write(tmp.path().join("coverage.txt"), b"").unwrap();
        std::fs::write(tmp.path().join("data.json"), b"").unwrap();
        let result = find_latest_profdata(tmp.path().to_path_buf());
        assert!(result.is_none());
    }

    #[test]
    fn test_collect_executed_from_profdata_basic() {
        let output = "  my_crate::foo:
    Hash: 0xabc
    Counters: 3
    Function count: 7
  my_crate::bar:
    Hash: 0xdef
    Counters: 1
    Function count: 0
  other_crate::baz:
    Hash: 0x111
    Counters: 2
    Function count: 4
";
        let crate_names = vec!["my_crate".to_owned()];
        let result = collect_executed_from_profdata(output, &crate_names);
        assert!(result.contains("my_crate::foo"));
        assert!(!result.contains("my_crate::bar")); // count == 0
        assert!(!result.contains("other_crate::baz")); // not in workspace
    }

    #[test]
    fn test_collect_executed_from_profdata_empty() {
        let result = collect_executed_from_profdata("", &["my_crate".to_owned()]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_executed_from_profdata_no_workspace_funcs() {
        let output = "\
  std::foo:\n\
    Hash: 0xabc\n\
    Function count: 10\n";
        let crate_names = vec!["my_crate".to_owned()];
        let result = collect_executed_from_profdata(output, &crate_names);
        assert!(result.is_empty());
    }
}
