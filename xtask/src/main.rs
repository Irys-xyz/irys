use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use xshell::{cmd, Cmd, Shell};

use xtask::failures::{
    self, generate_nextest_config, get_failures_file_path, get_stats_file_path, FailuresFile,
    RunResults,
};

const CARGO_FLAKE_VERSION: &str = "0.0.5";
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
        #[clap(short, long, default_value_t = false)]
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

/// Build the nextest-wrapper binary
fn build_wrapper(sh: &Shell) -> eyre::Result<PathBuf> {
    println!("Building nextest-wrapper...");
    cmd!(
        sh,
        "cargo build --package nextest-monitor --bin nextest-wrapper"
    )
    .remove_and_run()?;

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
        } => {
            println!("cargo test");
            let _ = cmd!(
                sh,
                "cargo install --locked --version {NEXTEST_VERSION} cargo-nextest"
            )
            .remove_and_run();

            if coverage {
                cmd!(sh, "cargo install --locked --version 0.10.5 grcov").remove_and_run()?;
                for (key, val) in [
                    ("CARGO_INCREMENTAL", "0"),
                    ("RUSTFLAGS", "-Cinstrument-coverage"),
                    ("LLVM_PROFILE_FILE", "target/coverage/%p-%m.profraw"),
                ] {
                    sh.set_var(key, val);
                }
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
            }

            // Build the wrapper binary and generate config
            let config_file = {
                let wrapper_path = build_wrapper(sh)?;
                let wrapper_path_str = wrapper_path.to_string_lossy().to_string();

                generate_nextest_config(&wrapper_path_str, failed_tests_filter.as_deref())?
            };

            // Build the nextest command
            let mut nextest_args = vec![
                "nextest".to_string(),
                "run".to_string(),
                "--workspace".to_string(),
                "--tests".to_string(),
                "--all-targets".to_string(),
            ];

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
            }

            // Add user-provided args
            nextest_args.extend(args);

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

            // Propagate the test result
            test_result?;

            if coverage {
                cmd!(sh, "mkdir -p target/coverage").remove_and_run()?;
                cmd!(sh, "grcov . --binary-path ./target/debug/deps/ -s . -t html,cobertura --branch --ignore-not-existing --ignore '../*' --ignore \"/*\" -o target/coverage/").remove_and_run()?;

                // Open the generated file
                if std::option_env!("CI").is_none() {
                    #[cfg(target_os = "macos")]
                    cmd!(sh, "open target/coverage/html/index.html").remove_and_run()?;

                    #[cfg(target_os = "linux")]
                    cmd!(sh, "xdg-open target/coverage/html/index.html").remove_and_run()?;
                }
            }
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
                println!("cargo fmt & fix & clippy fix");
                cmd!(sh, "cargo fmt --all").remove_and_run()?;
                let args_clone = args.clone();
                cmd!(
                    sh,
                    "cargo fix --allow-dirty --allow-staged --workspace --tests {args_clone...}"
                )
                .remove_and_run()?;
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

            if std::option_env!("CI").is_none() {
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
            {
                // push -D warnings for just this command to mimic CI
                let _rustflags_guard = sh.push_env("RUSTFLAGS", "-D warnings");
                run_command(
                    Commands::Check {
                        args: vec!["--tests".to_string()],
                    },
                    sh,
                )?;
            }
            run_command(Commands::Clippy { args: vec![] }, sh)?;
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
                    eprintln!("Warning: script command may not be available on this platform, progress bars may not display correctly");
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

pub trait CmdExt {
    fn remove_and_run(self) -> Result<(), xshell::Error>;
}

impl CmdExt for Cmd<'_> {
    /// removes a set of problematic env vars set by xtask being a cargo subcommand
    /// this is for ring, as their build.rs emits rerun conditions for the following env vars
    /// many of which are not present if you use a regular `cargo check`,
    /// which causes a re-run if you alternate between `cargo check` and an xtask command
    fn remove_and_run(self) -> Result<(), xshell::Error> {
        let mut c = self;
        // TODO: once ring releases  0.17.15+, we should no longer need this
        // these were taken from Ring's build.rs
        for k in [
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
        ] {
            c = c.env_remove(k);
        }
        c.run()
    }
}
