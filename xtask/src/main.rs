use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write as _;
use xshell::{cmd, Cmd, Shell};

const CARGO_FLAKE_VERSION: &str = "0.0.5";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Test {
        #[clap(short, long, default_value_t = false)]
        coverage: bool,
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

fn run_command(command: Commands, sh: &Shell) -> eyre::Result<()> {
    match command {
        Commands::Test { args, coverage } => {
            println!("cargo test");
            let _ =
                cmd!(sh, "cargo install --locked --version 0.9.102 cargo-nextest").remove_and_run();

            if coverage {
                cmd!(sh, "cargo install  --locked --version 0.10.5 grcov").remove_and_run()?;
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

            cmd!(
                sh,
                "cargo nextest run --workspace --tests --all-targets {args...}"
            )
            .remove_and_run()?;

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
                let output_file = format!("target/flaky-test-output-{}.txt", timestamp);

                // Create output file and write header
                let mut file = fs::File::create(&output_file)?;
                writeln!(file, "=== Flaky Test Run - {} ===", timestamp)?;
                write!(file, "Command: cargo flake --iterations {}", iters)?;
                if let Some(thread_count) = threads {
                    write!(file, " --threads {}", thread_count)?;
                }
                if let Some(failures) = tolerable_failures {
                    write!(file, " --tolerable-failures {}", failures)?;
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
                println!("Streaming output to: {}", output_file);

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
