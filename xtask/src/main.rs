use clap::{Parser, Subcommand};
use xshell::{cmd, Shell};

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
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let sh = Shell::new()?;
    let args = Args::parse();

    match args.command {
        Commands::Test { args, coverage } => {
            println!("cargo test");
            let _ = cmd!(sh, "cargo install --locked cargo-nextest").run();

            if coverage {
                cmd!(sh, "cargo install  --locked grcov").run()?;
                for (key, val) in [
                    ("CARGO_INCREMENTAL", "0"),
                    ("RUSTFLAGS", "-Cinstrument-coverage"),
                    ("LLVM_PROFILE_FILE", "target/coverage/%p-%m.profraw"),
                ] {
                    sh.set_var(key, val);
                }
            }
            cmd!(
                sh,
                "cargo nextest run --workspace --tests --all-targets {args...}"
            )
            .run()?;

            if coverage {
                cmd!(sh, "mkdir -p target/coverage").run()?;
                cmd!(sh, "grcov . --binary-path ./target/debug/deps/ -s . -t html,cobertura --branch --ignore-not-existing --ignore '../*' --ignore \"/*\" -o target/coverage/").run()?;

                // Open the generated file
                if std::option_env!("CI").is_none() {
                    #[cfg(target_os = "macos")]
                    cmd!(sh, "open target/coverage/html/index.html").run()?;

                    #[cfg(target_os = "linux")]
                    cmd!(sh, "xdg-open target/coverage/html/index.html").run()?;
                }
            }
        }

        Commands::Check { args } => {
            println!("cargo check");
            cmd!(sh, "cargo check {args...}").run()?;
        }
        Commands::FullCheck { args } => {
            println!("cargo check --all-features --all-targets");
            cmd!(sh, "cargo check --all-features --all-targets {args...}").run()?;
        }
        Commands::FullBacon { args } => {
            let _ = cmd!(sh, "cargo install --locked bacon").run();
            println!("bacon check-all ");
            cmd!(sh, "bacon check-all {args...}").run()?;
        }
        Commands::Clippy { args } => {
            println!("cargo clippy");
            cmd!(sh, "cargo clippy --workspace --locked {args...}").run()?;
        }
        Commands::Fmt {
            check_only: only_check,
            args,
        } => {
            if only_check {
                cmd!(sh, "cargo fmt --check {args...}").run()?;
            } else {
                println!("cargo fmt & fix & clippy fix");
                cmd!(sh, "cargo fmt --all").run()?;
                let args_clone = args.clone();
                cmd!(
                    sh,
                    "cargo fix --allow-dirty --allow-staged --workspace --tests {args_clone...}"
                )
                .run()?;
                cmd!(
                    sh,
                    "cargo clippy --fix --allow-dirty --allow-staged --workspace --tests {args...}"
                )
                .run()?;
            }
        }
        Commands::Doc { args } => {
            println!("cargo doc");
            cmd!(sh, "cargo doc --workspace --no-deps {args...}").run()?;

            if std::option_env!("CI").is_none() {
                #[cfg(target_os = "macos")]
                cmd!(sh, "open target/doc/relayer/index.html").run()?;

                #[cfg(target_os = "linux")]
                cmd!(sh, "xdg-open target/doc/relayer/index.html").run()?;
            }
        }
        Commands::Typos => {
            println!("typos check");
            cmd!(sh, "cargo install --locked typos-cli").run()?;
            cmd!(sh, "typos").run()?;
        }
        Commands::UnusedDeps => {
            println!("unused deps");
            cmd!(sh, "cargo install --locked cargo-machete").run()?;
            cmd!(sh, "cargo-machete").run()?;
        }
    }

    Ok(())
}
