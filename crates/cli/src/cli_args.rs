use clap::{Parser, Subcommand};
use eyre::bail;
use irys_types::H256;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
pub(crate) struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum Commands {
    #[command(name = "dump-state")]
    DumpState {},
    #[command(name = "init-state")]
    InitState { state_path: PathBuf },
    #[command(name = "rollback-blocks")]
    RollbackBlocks {
        #[command(subcommand)]
        mode: RollbackMode,
    },
    #[command(
        name = "build-genesis",
        about = "Build a signed genesis block from miner keys or existing commitments",
        group = clap::ArgGroup::new("genesis-source").required(true).args(&["miners", "commitments"]),
    )]
    BuildGenesis {
        /// Path to genesis_miners.toml containing miner keys and pledge counts.
        /// Mutually exclusive with --commitments.
        #[arg(long, conflicts_with = "commitments")]
        miners: Option<PathBuf>,

        /// Path to a JSON file of pre-signed CommitmentTransaction objects.
        /// Requires --signing-key. Mutually exclusive with --miners.
        #[arg(long, conflicts_with = "miners")]
        commitments: Option<PathBuf>,

        /// Hex-encoded secp256k1 private key for signing the genesis block header.
        /// Required when using --commitments. With --miners, the first miner signs.
        ///
        /// Resolution order: --signing-key flag, IRYS_SIGNING_KEY env var,
        /// --signing-key-file / IRYS_SIGNING_KEY_FILE, then mining_key in config.toml.
        #[arg(long, env = "IRYS_SIGNING_KEY")]
        signing_key: Option<String>,

        /// Path to a file containing the hex-encoded signing key.
        /// The file should contain only the key (whitespace is trimmed).
        #[arg(long, env = "IRYS_SIGNING_KEY_FILE")]
        signing_key_file: Option<PathBuf>,

        /// Output directory for genesis block and commitments JSON files.
        /// Defaults to current directory.
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    #[command(
        name = "generate-miner-info",
        about = "Derive Irys and EVM addresses from a mining key"
    )]
    GenerateMinerInfo {
        /// Hex-encoded secp256k1 private key (with or without 0x prefix).
        ///
        /// Resolution order: --key flag, IRYS_SIGNING_KEY env var,
        /// --key-file / IRYS_SIGNING_KEY_FILE, then mining_key in config.toml.
        #[arg(long, env = "IRYS_SIGNING_KEY")]
        key: Option<String>,

        /// Path to a file containing the hex-encoded key.
        /// The file should contain only the key (whitespace is trimmed).
        #[arg(long, env = "IRYS_SIGNING_KEY_FILE")]
        key_file: Option<PathBuf>,
    },
    #[command(
        name = "configured-miner-info",
        about = "Print the miner address derived from mining_key in config.toml"
    )]
    ConfiguredMinerInfo {},
    #[command(
        name = "inspect-genesis",
        about = "Load genesis files and display partition assignments"
    )]
    InspectGenesis {
        /// Directory containing .irys_genesis.json and .irys_genesis_commitments.json
        #[arg(long, default_value = ".")]
        genesis_dir: PathBuf,
    },
    #[command(
        name = "import-genesis",
        about = "Import genesis files from disk into the node database"
    )]
    ImportGenesis {
        /// Directory containing .irys_genesis.json and .irys_genesis_commitments.json
        #[arg(long, default_value = ".")]
        genesis_dir: PathBuf,
    },
    #[command(
        name = "dump-commitments",
        about = "Export all commitment transactions from the database to JSON"
    )]
    DumpCommitments {
        /// Output file path. Defaults to .irys_genesis_commitments.json
        #[arg(long, default_value = ".irys_genesis_commitments.json")]
        output: PathBuf,
    },
    #[command(
        name = "compare-genesis",
        about = "Compare current network partition assignments with a target genesis"
    )]
    CompareGenesis {
        /// Directory containing .irys_genesis.json and .irys_genesis_commitments.json
        #[arg(long, default_value = ".")]
        genesis_dir: PathBuf,

        /// Print all partition hashes present in both current network and target genesis
        #[arg(long)]
        list_retained_partition_hashes: bool,
    },
    #[command(
        name = "snapshot",
        about = "Export or import a portable snapshot of Irys + Reth chain state"
    )]
    Snapshot {
        #[command(subcommand)]
        mode: SnapshotMode,
    },
    #[command(name = "tui", about = "Launch the Irys cluster monitoring TUI")]
    Tui {
        /// Node URLs to connect to
        #[arg(value_name = "NODE_URLS")]
        node_urls: Vec<String>,

        /// Configuration file path (contains both TUI settings and nodes list)
        #[arg(short, long)]
        config: Option<String>,

        /// Record node info to SQLite database (irys-tui-records.db)
        #[arg(short, long)]
        record: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum SnapshotMode {
    #[command(
        name = "export",
        about = "Capture a portable snapshot of chain state (excludes node identity)"
    )]
    Export {
        /// Path to the .irys data directory to snapshot.
        /// Defaults to the base_directory from config.toml.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Path to write the resulting `.tar.zst` archive.
        #[arg(long)]
        output: PathBuf,

        /// Include cache tables (CachedDataRoots, CachedChunksIndex, CachedChunks,
        /// IngressProofs). Off by default — caches are rebuildable from canonical data.
        #[arg(long)]
        include_caches: bool,

        /// Skip MDBX page compaction during copy. Produces a larger archive but
        /// completes faster. Compaction is on by default and reclaims free pages.
        #[arg(long)]
        no_compact: bool,

        /// Skip MVCC throttling during copy. Faster but may stall a busy writer.
        /// Throttling is on by default; safe when the source DB is busy.
        #[arg(long)]
        no_throttle_mvcc: bool,
    },
    #[command(
        name = "import",
        about = "Restore chain state from a snapshot archive into a fresh data directory"
    )]
    Import {
        /// Path to the snapshot `.tar.zst` archive.
        #[arg(long)]
        input: PathBuf,

        /// Target .irys data directory (must be empty unless --force is set).
        /// Defaults to the base_directory from config.toml.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Override schema-version mismatch check. Use with care.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum RollbackMode {
    #[command(
        name = "to-block",
        about = "Rollback to a specific block by height or hash. the provided height/hash will be the new tip of the block index"
    )]
    ToBlock {
        #[arg(help = "Block height (number) or block hash")]
        target: String,
    },
    #[command(name = "count", about = "Rollback a specific number of blocks")]
    Count {
        #[arg(help = "Number of blocks to rollback")]
        count: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RollbackTarget {
    Height(u64),
    Hash(H256),
}

pub(crate) fn parse_rollback_target(target: &str) -> eyre::Result<RollbackTarget> {
    if let Ok(height) = target.parse::<u64>() {
        return Ok(RollbackTarget::Height(height));
    }
    if let Ok(hash) = H256::from_base58_result(target) {
        return Ok(RollbackTarget::Hash(hash));
    }
    bail!(
        "Invalid target {} - could not parse as a height or a valid base58 irys block hash",
        target
    )
}

pub(crate) fn timestamp_millis_to_secs(millis: u128) -> eyre::Result<u64> {
    u64::try_from(millis / 1000)
        .map_err(|_| eyre::eyre!("timestamp seconds {} overflows u64", millis / 1000))
}
