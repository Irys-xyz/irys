// MDBX-SUBMODULE-INDEX: remove this binary once the rollback window is closed. The verify-rocks command compares two RocksDB v1 directories and can be split out if that compare is still wanted.
//! Offline copy between an MDBX submodule index and RocksDB v1.
//!
//! Stop the node first. See `--help`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr as _;

use irys_database::submodule::{
    ConvertOptions, PackingRecord, convert_mdbx_to_rocks, convert_mdbx_to_rocks_and_verify,
    convert_rocks_to_mdbx, verify_mdbx_and_rocks, verify_rocks_records,
};
use irys_database::{consensus_db_args, submodule_db_args};
use irys_types::IrysAddress;
use irys_types::config::{ConsensusConfig, ConsensusOptions, DatabaseConfig, NodeConfig};
use irys_types::data_tree::Geometry;

const USAGE: &str = "\
usage:
  submodule-index-convert mdbx-to-rocks [flags] <db-dir> <rocks-dir>
  submodule-index-convert rocks-to-mdbx [flags] <rocks-dir> <db-dir>
  submodule-index-convert verify        [flags] <db-dir> <rocks-dir>
  submodule-index-convert verify-rocks  [flags] <rocks-left-dir> <rocks-right-dir>

verify-rocks compares two RocksDB v1 directories byte for byte. Every
column family is counted, including metadata identity keys. It does not
read the consensus database. The flags below are required for every other
command.

required flags:
  --consensus-db <dir>           node consensus database.
                                 This is <node>/.irys/irys_consensus_data
                                 (config.toml base_directory plus irys_consensus_data).
                                 The directory contains mdbx.dat.
  --ledger <id>                  data ledger id.
                                 Publish is 0, Submit is 1, OneYear is 10, ThirtyDay is 20.
  --slot <n>                     partition slot from packing_params.toml (`slot`)
  --config <path>                config.toml of the node that wrote this index.
                                 The tool reads num_chunks_in_partition, the chunk
                                 size and the data tree geometry from its consensus
                                 config. The geometry derives each tx's data
                                 commitment. Every node of one chain has the same
                                 consensus config, but a config for another chain
                                 or preset, or another custom consensus file,
                                 gives false mismatches. A relative `consensus`
                                 path in the file resolves from the current
                                 directory: run the tool from the node's working
                                 directory or use an absolute path.

optional:
  --rollback                     verify only: the <db-dir> is a rocks-to-mdbx copy.
                                 MDBX has no checksum column, so checksums the node
                                 wrote on RocksDB are counted as dropped_checksums
                                 and are not a mismatch.
  --replace-incomplete           delete a target only when it contains a CONVERTING
                                 file written by this tool, and none of chunks.dat,
                                 chunks.crc, packing_params.toml, intervals.json, a consensus
                                 database, an MDBX index, or a completed RocksDB v1 index.
                                 An older incomplete directory without CONVERTING
                                 is not deleted. Remove that directory by hand.
                                 Illegal on verify and verify-rocks.
  --dry-run                      validate arguments, scan the source, and print the
                                 same counts a convert would report, including
                                 orphans and a reset that would write an empty index.
                                 Does not create the target and does not write
                                 CONVERTING. Illegal on verify and verify-rocks.
                                 Illegal with --verify.
  --verify                       mdbx-to-rocks only. Warm and pin the MDBX source,
                                 convert, then verify in this process. Prints the convert counts and the
                                 verify report. Exits 1 when a compared value differs.
  --allow-unresolved             write the target even when the consensus database
                                 does not cover every placement. Those placements
                                 are omitted and counted as skipped_frontier.
                                 Without this flag the run fails and does not write
                                 the completion marker. An empty consensus block
                                 index always fails. Illegal on verify and
                                 verify-rocks. verify always counts skipped_frontier
                                 as a mismatch.
  --packing-address <addr>       packing address, base58 or hex. Together with
                                 --packing-iterations and --chain-id, mdbx-to-rocks
                                 writes the raw packing record.
  --packing-iterations <n>       entropy packing iterations stored in that record.
  --chain-id <n>                 chain id stored in that record.
                                 The three packing flags must be set together.
                                 Without them the node writes the record on open.

ledger offset = slot * chunks_per_partition + partition offset.
The tool derives the tx path at that offset and requires the stored bytes to
be identical. A mismatch aborts and prints the partition offset.
The source is never modified. A schema v1 MDBX index is read as v1.
rocks-to-mdbx reads RocksDB v1 only. It drops checksum-only rows and copies
the proof bytes from the offset record.

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

  submodule-index-convert mdbx-to-rocks \\
    --consensus-db <node>/.irys/irys_consensus_data \\
    --ledger <id> --slot <slot> \\
    --config <node>/config.toml \\
    <submodule>/db <submodule>/rocks

  submodule-index-convert verify \\
    --consensus-db <node>/.irys/irys_consensus_data \\
    --ledger <id> --slot <slot> \\
    --config <node>/config.toml \\
    <submodule>/db <submodule>/rocks

  submodule-index-convert verify-rocks <submodule>/rocks <other>/rocks

rocks-to-mdbx writes an MDBX schema v2 index for this build. It reads
RocksDB v1 only. The node migrates a schema v1 db/ (release 4.0.x) to
MDBX schema v2 in place. Release 4.0.x cannot read the result.
There is no rollback from 4.1.0 to 4.0.x.

verify, verify-rocks, and mdbx-to-rocks --verify exit 1 when a compared value
differs or when a placement was skipped because the consensus database does not
cover it.
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
    match tool.command {
        Command::VerifyRocks => {
            let report = verify_rocks_records(&tool.left, &tool.right)?;
            print!("{report}");
            if report.has_mismatch() {
                eyre::bail!("read equivalence mismatch");
            }
        }
        command => {
            let options = consensus_options(&tool)?;
            match command {
                Command::MdbxToRocks if tool.verify_after => {
                    init_info_tracing();
                    let (stats, report) =
                        convert_mdbx_to_rocks_and_verify(&tool.left, &tool.right, &options)?;
                    print!("{stats}");
                    print!("{report}");
                    if report.has_mismatch() {
                        eyre::bail!("read equivalence mismatch");
                    }
                }
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
                Command::VerifyRocks => unreachable!("handled above"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Consensus flags for a command that derives tx paths.
/// `verify-rocks` does not call this.
fn required<T>(flag: &str, value: Option<T>) -> eyre::Result<T> {
    value.ok_or_else(|| eyre::eyre!("{flag} is required for this command"))
}

fn consensus_options(tool: &ToolArgs) -> eyre::Result<ConvertOptions> {
    let consensus_db = required("--consensus-db", tool.consensus_db.clone())?;
    let ledger_id = required("--ledger", tool.ledger_id)?;
    let slot_index = required("--slot", tool.slot_index)?;
    let config_path = required("--config", tool.config.clone())?;
    let (consensus, data_tree_geometry) = load_consensus(&config_path)?;
    let mut options = ConvertOptions::new(
        submodule_db_args(&DatabaseConfig::default())?,
        data_tree_geometry,
    );
    options.consensus = consensus_db_args(&DatabaseConfig::default())?;
    options.replace_incomplete = tool.replace_incomplete;
    options.dry_run = tool.dry_run;
    options.allow_unresolved = tool.allow_unresolved;
    options.rollback = tool.rollback;
    options.consensus_db = Some(consensus_db);
    options.ledger_id = ledger_id;
    options.slot_index = slot_index;
    options.chunks_per_partition = consensus.num_chunks_in_partition;
    options.packing_record = packing_record_from_flags(tool, data_tree_geometry.chunk_size())?;
    Ok(options)
}

/// The consensus config and data tree geometry of the node config at `config_path`.
///
/// `NodeConfig::consensus_config` panics when a `consensus = Path(..)` file is
/// missing or malformed. The tool reads that file itself so the operator gets an
/// error that names it. A relative path resolves from the current directory,
/// the same as in the node.
fn load_consensus(config_path: &Path) -> eyre::Result<(ConsensusConfig, Geometry)> {
    let contents = std::fs::read_to_string(config_path)
        .map_err(|err| eyre::eyre!("--config {}: {err}", config_path.display()))?;
    let node_config = toml::from_str::<NodeConfig>(&contents).map_err(|err| {
        eyre::eyre!(
            "--config {}: invalid node config: {err}",
            config_path.display()
        )
    })?;
    let consensus = match &node_config.consensus {
        ConsensusOptions::Path(consensus_path) => {
            let text = std::fs::read_to_string(consensus_path).map_err(|err| {
                eyre::eyre!(
                    "--config {}: consensus file {}: {err}. A relative path resolves from the current directory.",
                    config_path.display(),
                    consensus_path.display()
                )
            })?;
            toml::from_str::<ConsensusConfig>(&text).map_err(|err| {
                eyre::eyre!(
                    "--config {}: invalid consensus file {}: {err}",
                    config_path.display(),
                    consensus_path.display()
                )
            })?
        }
        _ => node_config.consensus_config(),
    };
    let data_tree_geometry = consensus.data_tree_geometry().map_err(|err| {
        eyre::eyre!(
            "--config {}: invalid data tree geometry: {err}",
            config_path.display()
        )
    })?;
    Ok((consensus, data_tree_geometry))
}

fn packing_record_from_flags(
    tool: &ToolArgs,
    chunk_size: u64,
) -> eyre::Result<Option<PackingRecord>> {
    match (tool.packing_address, tool.packing_iterations, tool.chain_id) {
        (None, None, None) => Ok(None),
        (Some(address), Some(iterations), Some(chain_id)) => Ok(Some(PackingRecord {
            packing_address: address.into(),
            iterations,
            chunk_size,
            chain_id,
        })),
        _ => eyre::bail!(
            "--packing-address, --packing-iterations, and --chain-id must be set together"
        ),
    }
}

#[derive(Clone, Copy)]
enum Command {
    MdbxToRocks,
    RocksToMdbx,
    Verify,
    VerifyRocks,
}

struct ToolArgs {
    command: Command,
    replace_incomplete: bool,
    dry_run: bool,
    verify_after: bool,
    allow_unresolved: bool,
    rollback: bool,
    consensus_db: Option<PathBuf>,
    ledger_id: Option<u32>,
    slot_index: Option<u64>,
    config: Option<PathBuf>,
    packing_address: Option<IrysAddress>,
    packing_iterations: Option<u32>,
    chain_id: Option<u64>,
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
        // MDBX-SUBMODULE-INDEX: delete this command.
        "mdbx-to-rocks" => Command::MdbxToRocks,
        // MDBX-SUBMODULE-INDEX: delete this command once the rollback window is closed.
        "rocks-to-mdbx" => Command::RocksToMdbx,
        // MDBX-SUBMODULE-INDEX: delete this command. It compares an MDBX index with RocksDB v1. Keep verify-rocks if a Rocks-only compare is still wanted.
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
    let mut verify_after = false;
    let mut allow_unresolved = false;
    let mut rollback = false;
    let mut consensus_db = None;
    let mut ledger_id = None;
    let mut slot_index = None;
    let mut config = None;
    let mut packing_address = None;
    let mut packing_iterations = None;
    let mut chain_id = None;
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
            "--verify" => {
                verify_after = true;
                index += 1;
            }
            "--allow-unresolved" => {
                allow_unresolved = true;
                index += 1;
            }
            // MDBX-SUBMODULE-INDEX: delete this flag with verify of a rocks-to-mdbx copy.
            "--rollback" => {
                rollback = true;
                index += 1;
            }
            "--consensus-db"
            | "--ledger"
            | "--slot"
            | "--config"
            | "--packing-address"
            | "--packing-iterations"
            | "--chain-id" => {
                let Some(value) = raw.get(index + 1) else {
                    eprintln!("error: {arg} needs a value");
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                match arg {
                    "--consensus-db" => consensus_db = Some(PathBuf::from(value)),
                    "--ledger" => ledger_id = Some(parse_u32_flag("--ledger", value)?),
                    "--slot" => slot_index = Some(parse_u64("--slot", value)?),
                    "--config" => config = Some(PathBuf::from(value)),
                    "--packing-address" => packing_address = Some(parse_address(value)?),
                    "--packing-iterations" => {
                        packing_iterations = Some(parse_u32_flag("--packing-iterations", value)?)
                    }
                    "--chain-id" => chain_id = Some(parse_u64("--chain-id", value)?),
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
    if verify_after && !matches!(command, Command::MdbxToRocks) {
        eprintln!("error: --verify is only valid for mdbx-to-rocks");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if verify_after && dry_run {
        eprintln!("error: --dry-run is not valid with --verify");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if (replace_incomplete || dry_run || allow_unresolved) && verify {
        eprintln!(
            "error: --replace-incomplete, --dry-run, and --allow-unresolved are not valid for verify or verify-rocks"
        );
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if rollback && !matches!(command, Command::Verify) {
        eprintln!("error: --rollback is only valid for verify");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    // verify-rocks compares two RocksDB v1 directories and needs no consensus flags.
    let structural = matches!(command, Command::VerifyRocks);
    if !structural
        && (consensus_db.is_none()
            || ledger_id.is_none()
            || slot_index.is_none()
            || config.is_none())
    {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    Ok(Some(ToolArgs {
        command,
        replace_incomplete,
        dry_run,
        verify_after,
        allow_unresolved,
        rollback,
        consensus_db,
        ledger_id,
        slot_index,
        config,
        packing_address,
        packing_iterations,
        chain_id,
        left: positional.remove(0),
        right: positional.remove(0),
    }))
}

fn parse_address(text: &str) -> eyre::Result<IrysAddress> {
    IrysAddress::from_str(text).map_err(|err| eyre::eyre!("--packing-address {text}: {err}"))
}

fn parse_u32_flag(flag: &str, text: &str) -> eyre::Result<u32> {
    text.parse::<u32>()
        .map_err(|err| eyre::eyre!("{flag} {text}: {err}"))
}

fn parse_u64(flag: &str, text: &str) -> eyre::Result<u64> {
    text.parse::<u64>()
        .map_err(|err| eyre::eyre!("{flag} {text}: {err}"))
}

fn init_info_tracing() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    // stderr: stdout carries the key: value report the switch play reads.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::TempDirBuilder;

    #[test]
    fn a_missing_consensus_file_is_an_error_not_a_panic() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().prefix("convert-config").build();
        let missing = dir.path().join("no-such-consensus.toml");
        let mut node_config = NodeConfig::testing();
        node_config.consensus = ConsensusOptions::Path(missing.clone());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml::to_string(&node_config)?)?;

        let err = load_consensus(&config_path).expect_err("missing consensus file");
        let message = format!("{err:#}");
        assert!(
            message.contains(&missing.display().to_string()),
            "{message}"
        );
        assert!(message.contains("--config"), "{message}");

        std::fs::write(&missing, "not = [valid")?;
        let err = load_consensus(&config_path).expect_err("malformed consensus file");
        assert!(
            format!("{err:#}").contains("invalid consensus file"),
            "{err:#}"
        );
        Ok(())
    }
}
