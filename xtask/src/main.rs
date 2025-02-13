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
    Check,
    Fmt,
    Doc,
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
            cmd!(sh, "cargo install cargo-nextest").run()?;

            if coverage {
                cmd!(sh, "cargo install grcov").run()?;
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
                "cargo nextest run --workspace --tests --all-targets --no-fail-fast {args...}"
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

        Commands::Check => {
            println!("cargo check");
            cmd!(sh, "cargo clippy --workspace --locked").run()?;
            cmd!(sh, "cargo fmt --all --check").run()?;
        }
        Commands::Fmt => {
            println!("cargo fix");
            cmd!(sh, "cargo fmt --all").run()?;
            cmd!(
                sh,
                "cargo fix --allow-dirty --allow-staged --workspace --all-features --tests"
            )
            .run()?;
            cmd!(
                sh,
                "cargo clippy --fix --allow-dirty --allow-staged --workspace --all-features --tests"
            )
            .run()?;
        }
        Commands::Doc => {
            println!("cargo doc");
            cmd!(sh, "cargo doc --workspace --no-deps --all-features").run()?;

            if std::option_env!("CI").is_none() {
                #[cfg(target_os = "macos")]
                cmd!(sh, "open target/doc/relayer/index.html").run()?;

                #[cfg(target_os = "linux")]
                cmd!(sh, "xdg-open target/doc/relayer/index.html").run()?;
            }
        }
        Commands::Typos => {
            println!("typos check");
            cmd!(sh, "cargo install typos-cli").run()?;
            cmd!(sh, "typos").run()?;
        }
        Commands::UnusedDeps => {
            println!("unused deps");
            cmd!(sh, "cargo install cargo-machete").run()?;
            cmd!(sh, "cargo-machete").run()?;
        }
    }

    Ok(())
}
