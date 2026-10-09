//! Shared helpers used across xtask commands.

use crate::prune;
use cargo_metadata::{Message, MetadataCommand, TargetKind};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use xshell::{Cmd, Shell, cmd};

/// Pinned cargo-nextest version, shared by `xtask test` and `xtask flaky` so the
/// two install the same binary whenever the version changes.
pub const NEXTEST_VERSION: &str = "0.9.124";

/// Env vars that Ring's build.rs emits rerun conditions for, many of which are
/// absent under a regular `cargo check`, causing unnecessary rebuilds when
/// alternating between `cargo check` and xtask commands.
// TODO: remove once briansmith/ring#2454 is resolved and released; that issue
// tracks spurious rebuilds caused by ring's build.rs rerun conditions
pub const RING_ENV_VARS: &[&str] = &[
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

pub fn remove_ring_env_vars(cmd: Cmd<'_>) -> Cmd<'_> {
    let mut c = cmd;
    for k in RING_ENV_VARS {
        c = c.env_remove(k);
    }
    c
}

/// Append `-- -D warnings` so clippy fails on rustc/clippy warnings without
/// putting that flag in `RUSTFLAGS` (Cargo fingerprints rustflags on every
/// crate, including git deps).
pub fn with_deny_warnings(mut args: Vec<String>) -> Vec<String> {
    if !args.iter().any(|a| a == "--") {
        args.push("--".into());
    }
    let already = args
        .iter()
        .any(|a| a == "-Dwarnings" || a == "-D=warnings" || a == "--deny=warnings")
        || args
            .windows(2)
            .any(|w| (w[0] == "-D" || w[0] == "--deny") && w[1] == "warnings");
    if !already {
        args.push("-D".into());
        args.push("warnings".into());
    }
    args
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

/// Quote a string for safe inclusion in a `bash -c` command line.
/// Wraps in single quotes; embedded single quotes become `'\''`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What one cargo invocation reported building or reusing.
#[derive(Default)]
pub struct Collected {
    /// Test executables, bin targets left out: a bin's unit-test executable shares its crate
    /// root with the bin itself, which the run still needs.
    pub test_executables: Vec<PathBuf>,
    /// Every artifact file and build-script output dir.
    pub artifacts: Vec<PathBuf>,
    /// Cargo reported a successful build. A failed build lists only the units it reached, so its
    /// artifacts must not replace a complete usage record.
    pub build_succeeded: bool,
}

/// Run a cargo command with its JSON on stdout: echo every other stdout line, and collect what
/// cargo reports.
pub fn run_collecting(cmd: Cmd<'_>) -> eyre::Result<(Collected, eyre::Result<()>)> {
    let mut command: std::process::Command = remove_ring_env_vars(cmd).into();
    let display = format!("{command:?}");
    eprintln!("$ {display}");
    let mut child = command.stdout(Stdio::piped()).spawn()?;
    let mut reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut collected = Collected::default();
    let mut out = std::io::stdout().lock();
    // Lines are bytes, not strings: test output under `--no-capture` need not be UTF-8. After a
    // failed echo (stdout closed, as under `| head`) keep draining, so nextest never hits a full
    // or broken pipe.
    let mut echo_ok = true;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                // Nothing drains the pipe any more, so waiting alone could block forever.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.into());
            }
        }
        match serde_json::from_slice::<Message>(&line) {
            Ok(Message::CompilerArtifact(artifact)) => {
                let test_exe = artifact.profile.test && !artifact.target.is_kind(TargetKind::Bin);
                if let Some(exe) = artifact.executable {
                    if test_exe {
                        collected
                            .test_executables
                            .push(exe.clone().into_std_path_buf());
                    }
                    collected.artifacts.push(exe.into_std_path_buf());
                }
                collected.artifacts.extend(
                    artifact
                        .filenames
                        .into_iter()
                        .map(cargo_metadata::camino::Utf8PathBuf::into_std_path_buf),
                );
            }
            Ok(Message::BuildScriptExecuted(script)) => {
                collected.artifacts.push(script.out_dir.into_std_path_buf());
            }
            Ok(Message::BuildFinished(finished)) => {
                collected.build_succeeded = finished.success;
            }
            Ok(Message::TextLine(_)) | Err(_) => {
                echo_ok = echo_ok && out.write_all(&line).is_ok();
            }
            Ok(_) => {}
        }
    }
    let status = child.wait()?;
    let result = if status.success() {
        Ok(())
    } else {
        Err(eyre::eyre!("command exited with {status}: {display}"))
    };
    Ok((collected, result))
}

/// Run a cargo build command through [`run_collecting`] and record what it used, so the unit
/// prune keeps it. A failed build keeps the previous record; a failed record only warns.
pub fn run_recorded(cmd: Cmd<'_>, invocation: &str) -> eyre::Result<()> {
    let (collected, result) = run_collecting(cmd)?;
    if collected.build_succeeded {
        record_run(invocation, &collected.artifacts);
    }
    result
}

pub fn record_run(invocation: &str, artifacts: &[PathBuf]) {
    if artifacts.is_empty() {
        return;
    }
    let recorded = MetadataCommand::new()
        .no_deps()
        .exec()
        .map_err(eyre::Report::from)
        .and_then(|m| {
            prune::record_usage(m.target_directory.as_std_path(), invocation, artifacts)
                .map_err(eyre::Report::from)
        });
    if let Err(e) = recorded {
        eprintln!("Warning: artifact usage not recorded: {e}");
    }
}

/// Build the nextest-wrapper binary, optionally with additional features.
/// Returns the path to the built binary.
pub fn build_wrapper(sh: &Shell, features: Option<&str>) -> eyre::Result<PathBuf> {
    println!("Building nextest-wrapper...");
    let mut build_args = vec![
        "build".to_string(),
        "--message-format".to_string(),
        "json-render-diagnostics".to_string(),
        "--package".to_string(),
        "nextest-monitor".to_string(),
        "--bin".to_string(),
        "nextest-wrapper".to_string(),
    ];
    if let Some(feat) = features {
        build_args.push("--features".to_string());
        build_args.push(feat.to_string());
    }
    let invocation = format!("nextest-wrapper {}", features.unwrap_or_default());
    run_recorded(cmd!(sh, "cargo {build_args...}"), &invocation)?;

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

#[cfg(test)]
mod tests {
    use super::with_deny_warnings;

    #[test]
    fn adds_deny_when_empty() {
        assert_eq!(with_deny_warnings(vec![]), ["--", "-D", "warnings"]);
    }

    #[test]
    fn keeps_cargo_args_before_dashdash() {
        assert_eq!(
            with_deny_warnings(vec!["--all-features".into()]),
            ["--all-features", "--", "-D", "warnings"]
        );
    }

    #[test]
    fn does_not_duplicate_existing_deny() {
        assert_eq!(
            with_deny_warnings(vec![
                "--all-features".into(),
                "--".into(),
                "-Dwarnings".into()
            ]),
            ["--all-features", "--", "-Dwarnings"]
        );
    }
}
