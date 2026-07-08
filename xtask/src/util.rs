//! Shared helpers used across xtask commands.

use cargo_metadata::MetadataCommand;
use std::path::PathBuf;
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

/// Build the nextest-wrapper binary, optionally with additional features.
/// Returns the path to the built binary.
pub fn build_wrapper(sh: &Shell, features: Option<&str>) -> eyre::Result<PathBuf> {
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
