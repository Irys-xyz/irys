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
//! The source is never modified, including a schema v1 MDBX directory.
//! The completion marker is written last. A crash before that marker leaves
//! a `CONVERTING` file and no marker. A finished index removes `CONVERTING`
//! after the marker is durable. `--replace-incomplete` deletes only a
//! directory this tool marked with that file and that has no marker.
//!
//! See `--help` for the node steps.

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
  --consensus-db <dir>           node consensus database.
                                 This is <node>/.irys/irys_consensus_data
                                 (config.toml base_directory plus irys_consensus_data).
                                 The directory contains mdbx.dat.
  --ledger <id>                  data ledger id.
                                 Publish is 0, Submit is 1, OneYear is 10, ThirtyDay is 20.
  --slot <n>                     partition slot from packing_params.toml (`slot`)
  --chunks-per-partition <n>     consensus num_chunks_in_partition
  --chunk-size <bytes>           consensus chunk size (mainnet is 262144)

optional:
  --replace-incomplete           delete a target only when it contains a CONVERTING
                                 file written by this tool, and none of chunks.dat,
                                 chunks.crc, packing_params.toml, intervals.json, a consensus
                                 database, a v1 or v2 index, or a completed v3 index.
                                 An older incomplete directory without CONVERTING
                                 is not deleted. Remove that directory by hand.
                                 Illegal on verify and verify-rocks.
  --dry-run                      validate arguments, scan the source, and print the
                                 same counts a convert would report, including
                                 orphans and a reset that would write an empty index.
                                 Does not create the target and does not write
                                 CONVERTING. Illegal on verify and verify-rocks.
  --allow-unresolved             write the target even when the consensus database
                                 does not cover every placement. Those placements
                                 are omitted and counted as skipped_frontier.
                                 Without this flag the run fails and does not write
                                 the completion marker. An empty consensus block
                                 index always fails. Illegal on verify and
                                 verify-rocks. verify always counts skipped_frontier
                                 as a mismatch.

ledger offset = slot * chunks_per_partition + partition offset.
The tool derives the tx path at that offset and requires the stored bytes to
be identical. A mismatch aborts and prints the partition offset.
The source is never modified. A schema v1 MDBX index is read as v1.
rocks-to-mdbx accepts only schema v3. A schema v2 RocksDB index uses rocks-to-v3.

An empty submodule index succeeds and the report shows zero copied rows.
A placement the consensus database cannot resolve fails the run. The target
then has no completion marker. Pass --allow-unresolved only for a partial
index. verify and verify-rocks count skipped_frontier as a mismatch.

Stop the node first. The tool locks the index and refuses a held lock.
The completion marker is written last (rocks/SCHEMA, or the MDBX schema row).
A crash before that marker leaves CONVERTING and no completion marker.
Rerun the same command with --replace-incomplete. A finished run removes
CONVERTING. Do not start the node on an incomplete directory.

mdbx-to-rocks writes <submodule>/rocks. Set database.submodule_index to rocksdb
before the next start. Read `ledger` and `slot` from <submodule>/packing_params.toml.
Read num_chunks_in_partition and the chunk size from the chain config.

  submodule-index-convert mdbx-to-rocks \\
    --consensus-db <node>/.irys/irys_consensus_data \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/db <submodule>/rocks

  submodule-index-convert verify \\
    --consensus-db <node>/.irys/irys_consensus_data \\
    --ledger <id> --slot <slot> \\
    --chunks-per-partition <n> --chunk-size <bytes> \\
    <submodule>/db <submodule>/rocks

A RocksDB schema v2 index uses rocks-to-v3 with the same flags, then
verify-rocks. Replace rocks/ with the new directory only when verify-rocks
prints no mismatch. database.submodule_index stays rocksdb.

rocks-to-mdbx writes an MDBX schema v2 index. That index is readable by the
mdbx engine in this build. Release 4.0.8 cannot read it.
Rollback to 4.0.8 is: keep the original db/ directory. This tool does not
modify that directory. rocks-to-mdbx is only for a node whose live index is
already RocksDB schema v2 or v3 and that will keep running this build.

verify and verify-rocks exit 1 when a public read differs or when a placement
was skipped because the consensus database does not cover it.
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
    options.dry_run = tool.dry_run;
    options.allow_unresolved = tool.allow_unresolved;
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
    dry_run: bool,
    allow_unresolved: bool,
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
    let mut dry_run = false;
    let mut allow_unresolved = false;
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
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--allow-unresolved" => {
                allow_unresolved = true;
                index += 1;
            }
            "--consensus-db"
            | "--ledger"
            | "--slot"
            | "--chunks-per-partition"
            | "--chunk-size" => {
                let Some(value) = raw.get(index + 1) else {
                    eprintln!("error: {arg} needs a value");
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                match arg {
                    "--consensus-db" => consensus_db = Some(PathBuf::from(value)),
                    "--ledger" => ledger_id = Some(parse_u32(value)?),
                    "--slot" => slot_index = Some(parse_u64("--slot", value)?),
                    "--chunks-per-partition" => {
                        chunks_per_partition = Some(parse_u64("--chunks-per-partition", value)?)
                    }
                    "--chunk-size" => chunk_size = Some(parse_u64("--chunk-size", value)?),
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
    if (replace_incomplete || dry_run || allow_unresolved) && verify {
        eprintln!(
            "error: --replace-incomplete, --dry-run, and --allow-unresolved are not valid for verify or verify-rocks"
        );
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
        dry_run,
        allow_unresolved,
        consensus_db,
        ledger_id,
        slot_index,
        chunks_per_partition,
        chunk_size,
        left: positional.remove(0),
        right: positional.remove(0),
    }))
}

fn parse_u32(text: &str) -> eyre::Result<u32> {
    text.parse::<u32>()
        .map_err(|err| eyre::eyre!("--ledger {text}: {err}"))
}

fn parse_u64(flag: &str, text: &str) -> eyre::Result<u64> {
    text.parse::<u64>()
        .map_err(|err| eyre::eyre!("{flag} {text}: {err}"))
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
