//! Offline copy between a submodule MDBX index (`db/`) and a RocksDB index (`rocks/`).
//!
//! Stop the node first. This tool takes the engine lock and refuses to run
//! when that lock is already held. The directory name does not need to contain
//! `copy`.
//!
//! The source is not modified, except a schema v1 MDBX directory, which is
//! migrated to v2 in that same directory before the copy. The completion
//! marker is written last. A crash leaves no marker. Run the same command
//! again with `--replace-incomplete`.
//!
//! Keep the old directory. If the node writes new rows after the switch,
//! rollback is the opposite command, then `verify`. See
//! `docs/60-indexes/06-storage-module-indexes.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use irys_database::submodule::{
    ConvertOptions, convert_mdbx_to_rocks, convert_rocks_to_mdbx, verify_mdbx_and_rocks,
};
use irys_database::submodule_db_args;
use irys_types::config::DatabaseConfig;

const USAGE: &str = "\
usage: submodule-index-convert mdbx-to-rocks [--replace-incomplete] <db-dir> <rocks-dir>
       submodule-index-convert rocks-to-mdbx [--replace-incomplete] <rocks-dir> <db-dir>
       submodule-index-convert verify <db-dir> <rocks-dir>
stop the node first. the tool locks the index and refuses a held lock.
the directory name does not need to contain \"copy\".
the source is not modified, except a v1 MDBX index, which is migrated in place.
the completion marker is written last. a crash leaves none.
--replace-incomplete deletes a non-empty target that has no completion marker.
a completed target is never replaced.
verify exits 1 when a public read differs.";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> eyre::Result<ExitCode> {
    let Some(tool) = parse_args()? else {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    };
    if paths_are_same(&tool.left, &tool.right) {
        eyre::bail!("source and target are the same path");
    }
    let mut options = ConvertOptions::new(submodule_db_args(&DatabaseConfig::default())?);
    options.replace_incomplete = tool.replace_incomplete;
    match tool.command {
        Command::MdbxToRocks => {
            let stats = convert_mdbx_to_rocks(&tool.left, &tool.right, &options)?;
            print!("{stats}");
        }
        Command::RocksToMdbx => {
            let stats = convert_rocks_to_mdbx(&tool.left, &tool.right, &options)?;
            print!("{stats}");
        }
        Command::Verify => {
            let report = verify_mdbx_and_rocks(&tool.left, &tool.right, &options)?;
            print!("{report}");
            if report.has_mismatch() {
                eyre::bail!("read equivalence mismatch");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

enum Command {
    MdbxToRocks,
    RocksToMdbx,
    Verify,
}

struct ToolArgs {
    command: Command,
    replace_incomplete: bool,
    left: PathBuf,
    right: PathBuf,
}

fn parse_args() -> eyre::Result<Option<ToolArgs>> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    if command == "-h" || command == "--help" {
        return Ok(None);
    }
    let command = match command.as_str() {
        "mdbx-to-rocks" => Command::MdbxToRocks,
        "rocks-to-mdbx" => Command::RocksToMdbx,
        "verify" => Command::Verify,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let mut replace_incomplete = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--replace-incomplete" => replace_incomplete = true,
            other if other.starts_with('-') => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            other => positional.push(PathBuf::from(other)),
        }
    }
    if positional.len() != 2 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if replace_incomplete && matches!(command, Command::Verify) {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    Ok(Some(ToolArgs {
        command,
        replace_incomplete,
        left: positional.remove(0),
        right: positional.remove(0),
    }))
}

fn paths_are_same(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
