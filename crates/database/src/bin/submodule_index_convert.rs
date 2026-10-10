//! Offline copy between a submodule index and RocksDB schema v3.
//!
//! Stop the node first. This tool takes the engine lock and refuses to run
//! when that lock is already held. The directory name does not need to contain
//! `copy`.
//!
//! `mdbx-to-rocks` and `rocks-to-v3` write schema v3. `rocks-to-mdbx` rebuilds
//! MDBX schema v2 from schema v3. Each stored tx path must equal the path
//! derived from the consensus database. A mismatch aborts and names the
//! partition offset. The ledger offset is
//! `slot * chunks_per_partition + partition_offset`.
//!
//! The source is not modified, except a schema v1 MDBX directory, which is
//! migrated to v2 in that same directory before the copy. The completion
//! marker is written last. A crash leaves no marker. Run the same command
//! again with `--replace-incomplete`.
//!
//! See `--help` for the devnet-node-2 steps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use irys_database::submodule::{
    ConvertOptions, convert_mdbx_to_rocks, convert_rocks_to_mdbx, convert_rocks_v2_to_v3,
    verify_mdbx_and_rocks, verify_rocks_v2_and_v3,
};
use irys_database::{consensus_db_args, submodule_db_args};
use irys_types::config::DatabaseConfig;

const USAGE: &str = "\
usage:
  submodule-index-convert mdbx-to-rocks [flags] <db-dir> <rocks-v3-dir>
  submodule-index-convert rocks-to-v3   [flags] <rocks-v2-dir> <rocks-v3-dir>
  submodule-index-convert rocks-to-mdbx [flags] <rocks-v3-dir> <db-dir>
  submodule-index-convert verify        [flags] <db-dir> <rocks-v3-dir>
  submodule-index-convert verify-rocks  [flags] <rocks-v2-dir> <rocks-v3-dir>

required flags:
  --consensus-db <dir>           node consensus database (the directory with mdbx.dat)
  --ledger <id>                  data ledger id. Publish is 0, Submit is 1.
  --slot <n>                     partition slot from packing_params.toml (`slot`)
  --chunks-per-partition <n>     consensus num_chunks_in_partition
  --chunk-size <bytes>           consensus chunk size (mainnet is 262144)

optional:
  --replace-incomplete           delete a target that has no completion marker.
                                 illegal on verify and verify-rocks.

ledger offset = slot * chunks_per_partition + partition offset.
The tool derives the tx path at that offset and requires the stored bytes to
be identical. A mismatch aborts and prints the partition offset.
The source is not modified, except a v1 MDBX index, which is migrated to v2
in place. rocks-to-mdbx accepts only schema v3. A schema v2 RocksDB index
uses rocks-to-v3.

Stop the node first. The tool locks the index and refuses a held lock.
The completion marker is written last (rocks/SCHEMA, or the MDBX schema row).
A crash leaves no marker. Rerun the same command with --replace-incomplete.
Do not start the node on an incomplete directory.

devnet-node-2 is RocksDB schema v2. For each submodule, after the node is stopped:

  Read `ledger` and `slot` from <submodule>/packing_params.toml.
  Read num_chunks_in_partition and the chunk size from the chain config.
  Keep <submodule>/rocks.

  submodule-index-convert rocks-to-v3 \\
    --consensus-db <node>/db \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/rocks <submodule>/rocks-v3

  submodule-index-convert verify-rocks \\
    --consensus-db <node>/db \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/rocks <submodule>/rocks-v3

  Replace rocks/ with rocks-v3 only when verify-rocks prints no mismatch.
  database.submodule_index stays rocksdb.

Rollback of schema v3 to MDBX v2, for each submodule, after the node is stopped:

  submodule-index-convert rocks-to-mdbx \\
    --consensus-db <node>/db \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/rocks <submodule>/db-v2

  submodule-index-convert verify \\
    --consensus-db <node>/db \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/db-v2 <submodule>/rocks

  Replace db/ with db-v2 only when verify prints no mismatch.
  Set database.submodule_index to mdbx.

MDBX v1 or v2 to RocksDB v3 uses mdbx-to-rocks with the same flags, then
verify. A v1 source is migrated in place first.

verify and verify-rocks exit 1 when a public read differs.
";

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
    options.consensus = consensus_db_args(&DatabaseConfig::default())?;
    options.replace_incomplete = tool.replace_incomplete;
    options.consensus_db = Some(tool.consensus_db);
    options.ledger_id = tool.ledger_id;
    options.slot_index = tool.slot_index;
    options.chunks_per_partition = tool.chunks_per_partition;
    options.chunk_size = tool.chunk_size;
    match tool.command {
        Command::MdbxToRocks => {
            let stats = convert_mdbx_to_rocks(&tool.left, &tool.right, &options)?;
            print!("{stats}");
        }
        Command::RocksToV3 => {
            let stats = convert_rocks_v2_to_v3(&tool.left, &tool.right, &options)?;
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
        Command::VerifyRocks => {
            let report = verify_rocks_v2_and_v3(&tool.left, &tool.right, &options)?;
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
    RocksToV3,
    RocksToMdbx,
    Verify,
    VerifyRocks,
}

struct ToolArgs {
    command: Command,
    replace_incomplete: bool,
    consensus_db: PathBuf,
    ledger_id: u32,
    slot_index: u64,
    chunks_per_partition: u64,
    chunk_size: u64,
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
        "rocks-to-v3" => Command::RocksToV3,
        "rocks-to-mdbx" => Command::RocksToMdbx,
        "verify" => Command::Verify,
        "verify-rocks" => Command::VerifyRocks,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let raw: Vec<String> = args.collect();
    let mut replace_incomplete = false;
    let mut consensus_db = None;
    let mut ledger_id = None;
    let mut slot_index = None;
    let mut chunks_per_partition = None;
    let mut chunk_size = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let arg = raw[index].as_str();
        match arg {
            "-h" | "--help" => return Ok(None),
            "--replace-incomplete" => {
                replace_incomplete = true;
                index += 1;
            }
            "--consensus-db"
            | "--ledger"
            | "--slot"
            | "--chunks-per-partition"
            | "--chunk-size" => {
                let Some(value) = raw.get(index + 1) else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                match arg {
                    "--consensus-db" => consensus_db = Some(PathBuf::from(value)),
                    "--ledger" => ledger_id = parse_u32(value),
                    "--slot" => slot_index = parse_u64(value),
                    "--chunks-per-partition" => chunks_per_partition = parse_u64(value),
                    "--chunk-size" => chunk_size = parse_u64(value),
                    _ => {
                        eprintln!("{USAGE}");
                        std::process::exit(2);
                    }
                }
                index += 2;
            }
            other if other.starts_with('-') => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            other => {
                positional.push(PathBuf::from(other));
                index += 1;
            }
        }
    }
    if positional.len() != 2 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let verify = matches!(command, Command::Verify | Command::VerifyRocks);
    if replace_incomplete && verify {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let Some(consensus_db) = consensus_db else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let Some(ledger_id) = ledger_id else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let Some(slot_index) = slot_index else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let Some(chunks_per_partition) = chunks_per_partition else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let Some(chunk_size) = chunk_size else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    Ok(Some(ToolArgs {
        command,
        replace_incomplete,
        consensus_db,
        ledger_id,
        slot_index,
        chunks_per_partition,
        chunk_size,
        left: positional.remove(0),
        right: positional.remove(0),
    }))
}

fn parse_u32(text: &str) -> Option<u32> {
    text.parse().ok()
}

fn parse_u64(text: &str) -> Option<u64> {
    text.parse().ok()
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
