use clap::{Parser, Subcommand};
use xshell::{cmd, Cmd, Shell};

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
}

fn run_command(command: Commands, sh: &Shell) -> eyre::Result<()> {
    match command {
        Commands::Test { args, coverage } => {
            println!("cargo test");
            let _ = cmd!(sh, "cargo install --locked cargo-nextest").remove_and_run();

            if coverage {
                cmd!(sh, "cargo install  --locked grcov").remove_and_run()?;
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
            let _ = cmd!(sh, "cargo install --locked bacon").remove_and_run();
            println!("bacon check-all ");
            cmd!(sh, "bacon check-all {args...}").remove_and_run()?;
        }
        Commands::Clippy { args } => {
            println!("cargo clippy");
            cmd!(sh, "cargo clippy --workspace --locked {args...}").remove_and_run()?;
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
            cmd!(sh, "cargo install --locked typos-cli").remove_and_run()?;
            cmd!(sh, "typos").remove_and_run()?;
        }
        Commands::UnusedDeps => {
            println!("unused deps");
            cmd!(sh, "cargo install --locked cargo-machete").remove_and_run()?;
            cmd!(sh, "cargo-machete").remove_and_run()?;
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
        ] {
            c = c.env_remove(k);
        }
        c.run()
    }
}
