use crate::{
    ConsensusConfig, H256, IrysAddress, MempoolConfig, PeerAddress, RethPeerInfo, VdfConfig,
    irys::IrysSigner,
    storage_pricing::{
        Amount,
        phantoms::{IrysPrice, Usd},
    },
};
use crate::{ConsensusOptions, serde_utils};
#[cfg(any(test, feature = "test-utils"))]
use alloy_genesis::GenesisAccount;

use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::{env, path::PathBuf, time::Duration};

/// Informational label for the node's intended operating context.
/// Does **not** drive any subsystem behavior — individual config fields control durability,
/// caching, CPU pinning, etc. Used only for startup warnings and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RunMode {
    #[default]
    Production,
    Test,
}

impl RunMode {
    pub fn is_test(&self) -> bool {
        matches!(self, Self::Test)
    }
}

/// Database sync mode controlling durability vs write performance.
/// Mirrors `reth_db::mdbx::SyncMode` which doesn't derive Serialize/Deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbSyncMode {
    /// fsync after every transaction — safest, slowest.
    Durable,
    /// No fsync, but DB integrity preserved on crash (uncommitted txns lost).
    SafeNoSync,
    /// No fsync, no crash safety — fastest, for tests only.
    UtterlyNoSync,
}

#[cfg(feature = "db")]
impl From<DbSyncMode> for reth_db::mdbx::SyncMode {
    fn from(mode: DbSyncMode) -> Self {
        match mode {
            DbSyncMode::Durable => Self::Durable,
            DbSyncMode::SafeNoSync => Self::SafeNoSync,
            DbSyncMode::UtterlyNoSync => Self::UtterlyNoSync,
        }
    }
}

/// Per-chunk submodule index engine. The default stays MDBX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
// MDBX-SUBMODULE-INDEX: make RocksDB the only submodule index engine, then remove this enum and the submodule_index key. The consensus database stays MDBX. DatabaseConfig denies unknown fields, so delete the key only after configs no longer contain it.
pub enum SubmoduleIndexEngine {
    #[default]
    Mdbx,
    Rocksdb,
}

/// Block cache shared by every RocksDB submodule index in the process.
///
/// Partitioned filters stay in this cache. A 64 MiB cache has 1 MiB shards,
/// and a full filter for a 4 TB partition is about 20 MB, so that block is
/// loaded and dropped on every lookup. 1 GiB is 16 MiB per shard. The
/// write-buffer manager charges memtables to this cache, so this value must
/// be at least [`DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES`].
///
/// Inline data-path and tx-path blocks live here too. There is no separate
/// blob cache for new values. A directory that still has blob files reads
/// them through this same cache until compaction inlines them.
pub const DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
/// Memtable budget for every RocksDB submodule index in the process.
///
/// An idle column family holds about 1 MiB. Forty databases and seven
/// families (the empty default family plus six user families) are about
/// 280 MiB, which fits in 512 MiB. The cache must be at least this large
/// because the charge lives in the block cache. A path value sits in the
/// memtable until flush whether or not blob files are enabled, so this
/// budget does not grow when values stay inline.
pub const DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES: u64 = 512 * 1024 * 1024;
/// High-priority Env threads. Flushes use this pool. The pool is process-wide.
///
/// Two threads let one flush run while another disk flushes. One thread per
/// database is not required: the pool is shared.
pub const DEFAULT_ROCKSDB_FLUSH_THREADS: u32 = 2;
/// Low-priority Env threads. Compactions use this pool. The pool is process-wide.
///
/// Four threads let several of about ten HDD spindles compact at once. The
/// optional rate limit is the byte cap.
pub const DEFAULT_ROCKSDB_COMPACTION_THREADS: u32 = 4;
/// `0` leaves the flush and compaction rate limiter off.
pub const DEFAULT_ROCKSDB_RATE_LIMIT_BYTES_PER_SEC: u64 = 0;

/// Database durability and sync settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Sync mode for the main Irys consensus database.
    #[serde(default = "default_db_sync_mode")]
    pub sync_mode: DbSyncMode,

    /// Sync mode for the cache database.
    #[serde(default = "default_cache_db_sync_mode")]
    pub cache_sync_mode: DbSyncMode,

    /// Optional override for the MDBX geometry upper bound — the max DB size
    /// *and* the virtual address-space reservation made at open — of irys-owned
    /// databases (consensus + storage submodules). `None` = production defaults
    /// (consensus inherits reth's 8 TiB; submodules use 2 TiB), harmless as sparse
    /// reservations on real disks. Test configs set a small value so many
    /// concurrent node databases don't exhaust the process's ~128 TiB user
    /// address space. See `TEST_DB_GEOMETRY_MAX_SIZE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_max_size: Option<usize>,

    /// Engine for each storage submodule's per-chunk index.
    /// `mdbx` opens `db/`. `rocksdb` opens a sibling `rocks/` directory.
    #[serde(default)]
    // MDBX-SUBMODULE-INDEX: remove this field once every config is on RocksDB and the key has been retired. An omitted key selects MDBX.
    pub submodule_index: SubmoduleIndexEngine,

    /// Block cache shared by every RocksDB submodule index. Ignored for MDBX.
    /// See [`DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES`].
    #[serde(default = "default_rocksdb_block_cache_bytes")]
    pub rocksdb_block_cache_bytes: u64,

    /// Memtable budget shared by every RocksDB submodule index and charged to
    /// the block cache. Ignored for MDBX. Writers are not stalled. RocksDB
    /// schedules flushes once mutable memtables pass 7/8 of this budget.
    #[serde(default = "default_rocksdb_write_buffer_bytes")]
    pub rocksdb_write_buffer_bytes: u64,

    /// Process-wide RocksDB flush threads. Ignored for MDBX.
    #[serde(default = "default_rocksdb_flush_threads")]
    pub rocksdb_flush_threads: u32,

    /// Process-wide RocksDB compaction threads. Ignored for MDBX.
    #[serde(default = "default_rocksdb_compaction_threads")]
    pub rocksdb_compaction_threads: u32,

    /// Flush and compaction write cap, bytes per second, per submodule
    /// database. `0` leaves the limiter off. Ignored for MDBX.
    ///
    /// One limiter per physical disk is the right scope. Each open builds its
    /// own limiter, and the open does not know which disk the directory is
    /// on. N submodules on one disk then get N times this rate. Set the value
    /// to the disk budget divided by the submodule count on that disk.
    #[serde(default = "default_rocksdb_rate_limit_bytes_per_sec")]
    pub rocksdb_rate_limit_bytes_per_sec: u64,
}

fn default_db_sync_mode() -> DbSyncMode {
    DbSyncMode::Durable
}
fn default_cache_db_sync_mode() -> DbSyncMode {
    DbSyncMode::SafeNoSync
}
fn default_rocksdb_block_cache_bytes() -> u64 {
    DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
}
fn default_rocksdb_write_buffer_bytes() -> u64 {
    DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES
}
fn default_rocksdb_flush_threads() -> u32 {
    DEFAULT_ROCKSDB_FLUSH_THREADS
}
fn default_rocksdb_compaction_threads() -> u32 {
    DEFAULT_ROCKSDB_COMPACTION_THREADS
}
fn default_rocksdb_rate_limit_bytes_per_sec() -> u64 {
    DEFAULT_ROCKSDB_RATE_LIMIT_BYTES_PER_SEC
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            sync_mode: default_db_sync_mode(),
            cache_sync_mode: default_cache_db_sync_mode(),
            geometry_max_size: None,
            // MDBX-SUBMODULE-INDEX: delete this assignment with the field. The default is MDBX until then.
            submodule_index: SubmoduleIndexEngine::Mdbx,
            rocksdb_block_cache_bytes: default_rocksdb_block_cache_bytes(),
            rocksdb_write_buffer_bytes: default_rocksdb_write_buffer_bytes(),
            rocksdb_flush_threads: default_rocksdb_flush_threads(),
            rocksdb_compaction_threads: default_rocksdb_compaction_threads(),
            rocksdb_rate_limit_bytes_per_sec: default_rocksdb_rate_limit_bytes_per_sec(),
        }
    }
}

/// Controls whether the local VDF thread computes its own steps.
///
/// Not a switch for the VDF itself: the thread and its step buffer are always
/// needed, because block validation uses the local buffer as a fast path and
/// falls back to recomputing from a block's own seeds when it misses. What this
/// selects is whether the thread *free-runs* — produces fresh steps — or only
/// drains the validated fast-forward steps validation hands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VdfFreeRun {
    /// Free-run only if the startup throughput benchmark says it pays off.
    #[default]
    Auto,
    /// Always free-run, whatever the benchmark measured.
    ///
    /// This bypasses the benchmark's safety cutoff. On a machine that measures
    /// fast when idle but slows under validation load, a step can outlast
    /// `progress_timeout_secs`, which freezes the step counter and panics the
    /// process by the never-mislabel rule. An escape hatch, not a tuning knob.
    Enabled,
    /// Never free-run; follow the chain from validated fast-forward steps only.
    ///
    /// Always safe: the paused loop drains fast-forward every 200ms, well inside
    /// the stall budget. Rejected for a mining `node_mode` — mining seeds are
    /// broadcast only from the free-run path, so such a node could never mine.
    Disabled,
}

/// Controls whether and how the VDF thread is pinned to a CPU core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CorePinning {
    /// Do not pin the VDF thread to any core.
    Disabled,
    /// Pin to the first available core (current production behavior).
    #[default]
    Auto,
    /// Pin to a specific core by ID.
    Core(usize),
}

/// # Node Configuration
///
/// The main configuration for an Irys node, containing all settings needed
/// to participate in the network. This includes network mode, consensus rules,
/// pricing parameters, and system resource allocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Determines how the node joins and interacts with the network
    pub node_mode: NodeMode,

    /// The synchronization mode for the node
    pub sync_mode: SyncMode,

    /// The base directory where to look for artifact data
    #[serde(default = "default_irys_path")]
    pub base_directory: PathBuf,

    /// Private key used for mining operations
    /// This key identifies the node and receives mining rewards
    #[serde(
        deserialize_with = "serde_utils::signing_key_from_hex",
        serialize_with = "serde_utils::serializes_signing_key"
    )]
    pub mining_key: k256::ecdsa::SigningKey,

    /// The initial list of peers to contact for block sync
    pub trusted_peers: Vec<PeerAddress>,

    /// Initial whitelist of miner who can post stake and pledge transaction. To be removed on a
    /// later date. If this field is empty, all peers are allowed to stake and pledge.
    /// This has effect only on the genesis node, as all other nodes will get this parameter
    /// from their trusted peers.
    #[serde(default)]
    pub initial_stake_and_pledge_whitelist: Vec<IrysAddress>,

    /// Initial whitelist of peers to connect to. If you're joining the network as a peer in a
    /// trusted-only or trusted-and-handshake mode, you'll be supplied one during the handshake
    /// with the trusted peers. For the original trusted peer that has to be set.
    #[serde(default)]
    pub initial_whitelist: Vec<SocketAddr>,

    /// Controls how the node filters peer interactions
    #[serde(default = "default_peer_filter_mode")]
    pub peer_filter_mode: PeerFilterMode,

    pub reward_address: IrysAddress,

    // whether we should try to stake & pledge our local drives
    #[serde(default)]
    pub stake_pledge_drives: bool,

    // TODO: move into SyncConfig so all sync-related tunables live together.
    #[serde(default = "default_genesis_peer_discovery_timeout_millis")]
    pub genesis_peer_discovery_timeout_millis: u64,

    /// Default network configuration used by all services unless overridden
    #[serde(default = "default_network_defaults")]
    pub network_defaults: NetworkDefaults,

    /// Peer-to-peer network communication settings
    pub gossip: GossipConfig,

    /// HTTP API server configuration
    pub http: HttpConfig,

    /// Reth node configuration
    pub reth: RethConfig,

    /// StorageModule configuration
    #[serde(default)]
    pub storage: StorageSyncConfig,

    /// DataSyncService configuration
    #[serde(default)]
    pub data_sync: DataSyncServiceConfig,

    /// Data packing and compression settings
    #[serde(default)]
    pub packing: PackingConfig,

    /// Cache management configuration
    #[serde(default)]
    pub cache: CacheConfig,

    /// Settings for the price oracle system (list).
    #[serde(default)]
    pub oracles: Vec<OracleConfig>,

    #[serde(default)]
    pub vdf: VdfNodeConfig,

    #[serde(default)]
    pub mempool: MempoolNodeConfig,

    /// Metrics endpoint configuration. Default keeps the Reth Prometheus
    /// listener on the host loopback; deployments expose it via an
    /// mTLS-terminating reverse proxy rather than binding `0.0.0.0`.
    #[serde(default = "default_metrics_config")]
    pub metrics: MetricsConfig,

    /// Specifies which consensus rules the node follows
    pub consensus: ConsensusOptions,

    /// P2P handshake parameters
    #[serde(default)]
    pub p2p_handshake: P2PHandshakeConfig,

    /// Gossip/broadcast parameters
    #[serde(default)]
    pub p2p_gossip: P2PGossipConfig,

    /// P2P pull/request parameters
    #[serde(default)]
    pub p2p_pull: P2PPullConfig,

    /// Maximum bytes of one peer HTTP response body, for the gossip client
    /// and the API client. A body past this is dropped and not parsed.
    /// `0` is rejected by [`crate::Config::validate`].
    #[serde(default = "default_max_peer_response_bytes")]
    pub max_peer_response_bytes: u64,

    /// Sync parameters - how many blocks to pull in parallel, timeouts, etc
    #[serde(default)]
    pub sync: SyncConfig,

    /// Informational label indicating whether the node was configured for test or production use.
    /// Does not propagate to subsystem configs — durability, caching, and resource allocation
    /// are controlled by their own fields (e.g. `database.sync_mode`, `reth.db_sync_mode`).
    #[serde(default)]
    pub run_mode: RunMode,

    /// Database durability settings.
    #[serde(default)]
    pub database: DatabaseConfig,
}

/// # Node Operation Mode
///
/// Defines how the node participates in the network - either as a genesis node
/// that starts a new network or as a node that joins an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NodeMode {
    /// Start a new blockchain network as the first node
    Genesis,

    /// Join an existing network by connecting to trusted peers, and mine.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Miner,

    /// Join an existing network and follow it without mining. Skips partition
    /// mining and default submodule creation. The startup VDF throughput check
    /// still runs, but failing it disables the local free-run instead of
    /// aborting startup. The local VDF otherwise still runs: a step count that
    /// tracks the chain reduces the parallel VDF work block validation has to do.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Observer,
}

/// Hand-written so the retired `Peer` spelling produces a migration error.
///
/// `Peer` used to mean "join the network and mine". The name was removed rather
/// than reused for the non-mining mode: a reused name would leave every
/// deployed config parsing without error while meaning the opposite, and every
/// miner would stop mining on its next restart with nothing to signal it.
impl<'de> Deserialize<'de> for NodeMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const VARIANTS: &[&str] = &["Genesis", "Miner", "Observer"];
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "Genesis" => Ok(Self::Genesis),
            "Miner" => Ok(Self::Miner),
            "Observer" => Ok(Self::Observer),
            "Peer" => Err(serde::de::Error::custom(
                "node_mode = \"Peer\" no longer exists: use \"Miner\" to keep the previous \
                 behavior (join the network and mine), or \"Observer\" to join and follow the \
                 chain without mining",
            )),
            other => Err(serde::de::Error::unknown_variant(other, VARIANTS)),
        }
    }
}

impl NodeMode {
    /// Whether this mode participates in mining. Gates partition mining, default
    /// submodule creation, and whether failing the startup VDF throughput check
    /// aborts startup — submodules exist to hold packed partitions, which only a
    /// mining node is assigned.
    ///
    /// Written as an exhaustive `match`, not `matches!`: a new variant must be
    /// a compile error here rather than a silent `false`.
    pub const fn mines(self) -> bool {
        match self {
            Self::Genesis | Self::Miner => true,
            Self::Observer => false,
        }
    }

    /// Whether this mode joins a network that already exists. Rules that
    /// govern joining — a pinned genesis hash, a working periodic sync
    /// check — must cover every such mode, not just `Miner`.
    ///
    /// Exhaustive for the same reason as [`Self::mines`].
    pub const fn joins_existing_network(self) -> bool {
        match self {
            Self::Miner | Self::Observer => true,
            Self::Genesis => false,
        }
    }
}

/// # Node Synchronization Mode
///
/// Defines the method the node uses to synchronize with the network.
/// Trusted mode allows for faster sync by relying on trusted peers,
/// while Full mode ensures complete validation of all blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SyncMode {
    /// Fast sync mode, downloads index from the trusted peers and skips
    /// heavy parts of the block validation
    Trusted,
    /// Full sync mode, fully validates all blocks
    Full,
}

/// # Peer Filter Mode
///
/// Defines how the node filters which peers it will interact with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerFilterMode {
    /// No restrictions - interact with any discovered peers (default behavior)
    Unrestricted,

    /// Only interact with peers specified in the `trusted_peers` list
    TrustedOnly,

    /// Interact with trusted peers and additional peers they return during handshake
    /// The combination of trusted peers + handshake peers forms the whitelist
    TrustedAndHandshake,
}

/// # Oracle Configuration
///
/// Defines how the node obtains and processes external price information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum OracleConfig {
    /// A simulated price oracle for testing and development
    #[serde(rename = "mock")]
    Mock {
        /// Starting price for the token in USD
        #[serde(
            deserialize_with = "serde_utils::token_amount",
            serialize_with = "serde_utils::serializes_token_amount"
        )]
        initial_price: Amount<(IrysPrice, Usd)>,

        /// How much the price can change between updates
        #[serde(
            deserialize_with = "serde_utils::token_amount",
            serialize_with = "serde_utils::serializes_token_amount"
        )]
        incremental_change: Amount<(IrysPrice, Usd)>,

        /// Number of blocks between price updates
        smoothing_interval: u64,
        /// Initial direction of price movement. When true the mock increases first;
        /// when false it decreases first.
        #[serde(default = "super::default_oracle_initial_direction_up")]
        initial_direction_up: bool,
        /// Poll interval in milliseconds for refreshing the mock oracle price snapshots.
        #[serde(default = "default_mock_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
    /// CoinMarketCap-backed price oracle
    #[serde(rename = "coinmarketcap", alias = "coin_market_cap")]
    CoinMarketCap {
        /// API key for the CoinMarketCap Pro API
        api_key: String,
        /// CoinMarketCap coin id (e.g., "1" for Bitcoin).
        /// Retrieve ids from https://api.coinmarketcap.com/data-api/v3/map/all?listing_status=active
        id: String,
        /// Poll interval in milliseconds.
        /// Free tier is limited to 10k requests/month, so a 5 minute (300_000 ms) interval is a safe default.
        #[serde(default = "default_price_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
    /// CoinGecko-backed price oracle
    #[serde(rename = "coingecko", alias = "coin_gecko")]
    CoinGecko {
        /// API key for the CoinGecko Pro/Demo API
        api_key: String,
        /// CoinGecko coin id (e.g., "bitcoin", "ethereum").
        /// Retrieve ids from https://docs.coingecko.com/reference/coins-list
        /// Or from the official spreadsheet: https://docs.google.com/spreadsheets/d/1wTTuxXt8n9q7C4NDXqQpI3wpKu1_5bGVmP9Xz0XGSyU/edit?gid=0#gid=0
        coin_id: String,
        /// Set to true when using a CoinGecko demo API key (free, not paid version)
        #[serde(default)]
        demo_api_key: bool,
        /// Poll interval in milliseconds.
        /// Free tier is limited to 10k requests/month, so a 5 minute (300_000 ms) interval is a safe default.
        #[serde(default = "default_price_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
}

const fn default_price_oracle_poll_interval_ms() -> u64 {
    300_000
}

const fn default_mock_oracle_poll_interval_ms() -> u64 {
    10_000
}

pub(crate) const fn default_oracle_initial_direction_up() -> bool {
    true
}

impl OracleConfig {
    /// Returns a default mock oracle config suitable for development/testing.
    pub fn default_mock() -> Self {
        Self::Mock {
            initial_price: Amount::token(dec!(1)).expect("valid token amount"),
            incremental_change: Amount::token(dec!(0.00000000000001)).expect("valid token amount"),
            smoothing_interval: 15,
            initial_direction_up: true,
            poll_interval_ms: default_mock_oracle_poll_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageSyncConfig {
    /// Pending chunk count that lets a non-force flush write one submodule.
    ///
    /// This count does not set the `fdatasync` gap. That gap is
    /// [`Self::min_durable_interval_ms`]. Reaching the count also releases
    /// every short packed run. A busy disk no longer waits out the reorder
    /// grace for a neighbor. The value also sizes the pending-byte ceiling
    /// and the packing sync stride. `0` is rejected.
    pub num_writes_before_sync: u64,
    /// Ceiling, in bytes, on chunk bodies one storage module may hold in memory
    /// awaiting flush before background body migration pauses writing to it.
    /// `None` derives `max(2 × num_writes_before_sync, 256) × chunk_size × submodules`
    /// per module: one flush batch in flight plus one being filled, per disk,
    /// floored at 256 chunks so a small `num_writes_before_sync` cannot stall
    /// the worker on the storage service's idle flush.
    pub max_pending_write_bytes: Option<u64>,
    /// Parsed so an older config that set this key still loads. A queued
    /// entropy sweep runs when the disk is free. The disk lane does not read
    /// this value, and it is not written back.
    #[serde(default = "default_entropy_sweep_interval_millis", skip_serializing)]
    pub entropy_sweep_interval_millis: u64,
    /// Max bytes of one entropy pread. A hold is at least one chunk.
    /// A coalesced packed `pwrite` uses its own cap. A non-coalesced pwrite
    /// still uses this one.
    #[serde(default = "default_entropy_sweep_max_bytes")]
    pub entropy_sweep_max_bytes: u64,
    /// Max unread gap, in bytes, a sweep may read through and discard.
    /// Zero bridges no hole.
    #[serde(default = "default_entropy_coalesce_hole_bytes")]
    pub entropy_coalesce_hole_bytes: u64,
    /// Milliseconds a short entropy span waits for a neighboring chunk.
    ///
    /// The wait runs while the disk is idle. The oldest chunk in the span
    /// starts it, so a neighbor does not restart it. A span that already
    /// reaches [`Self::entropy_read_batch_bytes`] does not wait. A packing
    /// span does not wait. A lone chunk is issued when this ends. `0` issues
    /// every formed span at once.
    #[serde(default = "default_entropy_read_grace_ms")]
    pub entropy_read_grace_ms: u64,
    /// Bytes a span must cover before it is read without
    /// [`Self::entropy_read_grace_ms`].
    ///
    /// [`Self::entropy_sweep_max_bytes`] still limits the pread. A config
    /// file that sets this key still loads, and the loaded value is the
    /// sweep cap. Tests and the bench assign the field in code. `0` on that
    /// path treats every span as large enough to issue at once.
    #[serde(
        default = "default_entropy_read_batch_bytes",
        skip_serializing,
        deserialize_with = "freeze_entropy_bytes_at_sweep_cap"
    )]
    pub entropy_read_batch_bytes: u64,
    /// Extra bytes a short ingress span may read past its last queued chunk.
    ///
    /// The whole pread still stops at [`Self::entropy_sweep_max_bytes`], at
    /// the transaction's last chunk in this module, and at the submodule.
    /// A config file that sets this key still loads, and the loaded value is
    /// the sweep cap. Tests and the bench assign the field in code. `0` on
    /// that path does not read ahead.
    #[serde(
        default = "default_entropy_readahead_bytes",
        skip_serializing,
        deserialize_with = "freeze_entropy_bytes_at_sweep_cap"
    )]
    pub entropy_readahead_bytes: u64,
    /// Cap, in bytes, on the per-module entropy read-ahead cache.
    ///
    /// The oldest pad is dropped first. `0` keeps nothing. Reset, a
    /// partition-hash change, and a pause drop the cache. A write drops
    /// that offset.
    #[serde(default = "default_entropy_readahead_cache_bytes")]
    pub entropy_readahead_cache_bytes: u64,
    /// Seconds between completion rewrites of each submodule's `intervals.json`.
    ///
    /// A write-ahead transition is still stored immediately: `Interrupted`
    /// before chunk bytes change, and `Uninitialized` on reset or rollback.
    /// Each of those rewrites is atomic (`fsync` of the temp file, then
    /// `fsync` of the directory). A completion (`Data` or `Entropy` after the
    /// chunk `fdatasync` and the presence commit) stays in memory and shares
    /// one rewrite at most this often, or sooner when a write-ahead rewrite
    /// already has to hit the disk. Clean shutdown writes the live map.
    /// `0` rewrites completions on every sync.
    #[serde(default = "default_intervals_persist_secs")]
    pub intervals_persist_secs: u64,
    /// Milliseconds a RocksDB submodule index may leave a non-durable write
    /// unsynced.
    ///
    /// The write is in the WAL page cache. It becomes durable at the next
    /// synced write on that database, when this backstop syncs the WAL, or
    /// on clean shutdown. `0` makes the backstop thread sync as soon as it
    /// wakes. MDBX ignores this key. Open leaves group commit off, so each
    /// MDBX registration is durable before the write returns.
    #[serde(default = "default_index_wal_backstop_ms")]
    pub index_wal_backstop_ms: u64,
    /// Threads that prefetch tx headers when a data solution has no stored tx path.
    ///
    /// The derive starts when the solution is found. Block production awaits it
    /// when it builds the PoA, so parent selection, mempool selection, and the
    /// Reth payload build run while the headers are read. `1` reads headers on
    /// the derive thread. `0` is raised to 1. The pool is
    /// `min(configured, available_parallelism)` and at least 1. The default
    /// is 16.
    #[serde(default = "default_tx_path_derive_threads")]
    pub tx_path_derive_threads: usize,
    /// Minimum milliseconds between durable points.
    ///
    /// The default is 250. On HDD storage modules that cap keeps a
    /// 16–28 MiB/s ingest, at one recall per second, under drive saturation
    /// (about 7–19 points of drive time) and adds about 100 ms to durable
    /// p99. `0` syncs on every gap.
    ///
    /// A durable point is the `chunks.dat` `fdatasync` and the synced presence
    /// commit that follows it. A positive value defers a pair that would land
    /// sooner than this after the previous pair. Pwrites keep running. Recall
    /// does not wait on a deferred pair.
    ///
    /// These still run the pair immediately: `force_sync_pending_chunks`,
    /// shutdown, drop, pending bytes over [`Self::durable_headroom_permille`]
    /// of the ceiling, and a pending-byte level above the ceiling
    /// (`max_pending_write_bytes`, or
    /// `max(2 × num_writes_before_sync, 256) × chunk_size × submodules`).
    /// Reset drops pending bytes instead of waiting. Headroom loss ends the
    /// wait sooner. The interval is not a bound while a mining recall holds
    /// the disk: the lane does not start the point, and the `fdatasync`
    /// waits until that `pread` returns.
    #[serde(default = "default_min_durable_interval_ms")]
    pub min_durable_interval_ms: u64,
    /// Pending bytes, in parts per thousand of the memory ceiling, above
    /// which a durable point runs at once.
    ///
    /// The lane defers only while pending bytes (queued, written ahead, and
    /// unpacked) stay at or under `ceiling * durable_headroom_permille / 1000`,
    /// and only until [`Self::min_durable_interval_ms`]. Crossing the line
    /// closes deferral. It stays closed until pending bytes are at or under
    /// half that line, so a full queue cannot re-arm a large sync by dipping
    /// just under the line. An owed recall flush still defers on this line:
    /// that recall is waiting on the flush and must not wait on the fdatasync.
    ///
    /// The default is 400. `1000` puts the line on the ceiling. `0` runs a
    /// point as soon as any byte is pending, except while a recall flush is owed.
    #[serde(default = "default_durable_headroom_permille")]
    pub durable_headroom_permille: u64,
    /// Accepted so an older config that set this key still loads.
    /// The value is ignored and is not written back. Recall pages stay cached.
    #[serde(default, skip_serializing)]
    pub drop_recall_page_cache: bool,
    /// When true, mining recall reads `chunks.dat` through a second read-only
    /// descriptor opened with `O_DIRECT`.
    ///
    /// A recall range is read once per step. A buffered read of that range
    /// fills the page cache with pages nothing else will hit, and that cache
    /// is also where RocksDB SST pages and other reused file data live.
    /// `O_DIRECT` keeps the recall bytes out of that cache.
    ///
    /// The default is true: buffered recall reads put about 100 MiB of
    /// use-once pages per partition per step into the page cache and evict
    /// everything else (measured on devnet HDDs: page cache full within
    /// minutes; serving p50 5–8 ms buffered vs 0.8 ms direct, recall p50
    /// ~12% lower direct). `testing()` keeps it off. An open that fails with
    /// `EINVAL` or `EOPNOTSUPP` logs one warning and keeps the buffered
    /// descriptor. Any other errno fails module open. Set this flag false
    /// to skip the direct descriptor. Serving, the entropy sweep, and packing
    /// validation stay on the buffered descriptor either way.
    ///
    /// A direct read of a range that still has dirty buffered pages returns
    /// the new bytes. Linux writes that range back before the direct read
    /// (XFS and ext4 iomap).
    #[serde(default = "default_recall_direct_io")]
    pub recall_direct_io: bool,
    /// When true, a mining recall holds the module disk while it reads:
    /// the disk lane submits no packed write or entropy read, and non-lane
    /// reads, writes, and durable points wait for the recall to finish.
    ///
    /// Set false when other IO should take priority: packing, data sync,
    /// and serving then share the disk with the recall read instead of
    /// waiting for it. The in-flight service-time budget still bounds the
    /// lane's submitted writes and entropy reads; recall preads are not
    /// counted against it and may run alongside a full admitted budget.
    /// Read at module open; a change needs a restart.
    #[serde(default = "default_recall_io_priority")]
    pub recall_io_priority: bool,
}

/// Mining recall reads `chunks.dat` with `O_DIRECT` unless a config says otherwise.
pub const fn default_recall_direct_io() -> bool {
    true
}

/// A mining recall holds the module disk for other IO unless a config says otherwise.
pub const fn default_recall_io_priority() -> bool {
    true
}

impl Default for StorageSyncConfig {
    fn default() -> Self {
        Self {
            num_writes_before_sync: 100,
            max_pending_write_bytes: None,
            entropy_sweep_interval_millis: default_entropy_sweep_interval_millis(),
            entropy_sweep_max_bytes: default_entropy_sweep_max_bytes(),
            entropy_coalesce_hole_bytes: default_entropy_coalesce_hole_bytes(),
            entropy_read_grace_ms: default_entropy_read_grace_ms(),
            entropy_read_batch_bytes: default_entropy_read_batch_bytes(),
            entropy_readahead_bytes: default_entropy_readahead_bytes(),
            entropy_readahead_cache_bytes: default_entropy_readahead_cache_bytes(),
            drop_recall_page_cache: false,
            recall_direct_io: default_recall_direct_io(),
            recall_io_priority: default_recall_io_priority(),
            intervals_persist_secs: default_intervals_persist_secs(),
            index_wal_backstop_ms: default_index_wal_backstop_ms(),
            tx_path_derive_threads: default_tx_path_derive_threads(),
            min_durable_interval_ms: default_min_durable_interval_ms(),
            durable_headroom_permille: default_durable_headroom_permille(),
        }
    }
}

/// Default gap between completion rewrites of `intervals.json`.
pub const DEFAULT_INTERVALS_PERSIST_SECS: u64 = 30;

/// Default RocksDB index WAL backstop. See [`StorageSyncConfig::index_wal_backstop_ms`].
pub const DEFAULT_INDEX_WAL_BACKSTOP_MS: u64 = 1000;

/// Default tx-header prefetch threads. See [`StorageSyncConfig::tx_path_derive_threads`].
pub const DEFAULT_TX_PATH_DERIVE_THREADS: usize = 16;

/// Default gap between durable points, in milliseconds. See
/// [`StorageSyncConfig::min_durable_interval_ms`].
pub const DEFAULT_MIN_DURABLE_INTERVAL_MS: u64 = 250;

/// Default headroom line, in parts per thousand of the pending-byte ceiling.
/// See [`StorageSyncConfig::durable_headroom_permille`].
pub const DEFAULT_DURABLE_HEADROOM_PERMILLE: u64 = 400;

fn default_intervals_persist_secs() -> u64 {
    DEFAULT_INTERVALS_PERSIST_SECS
}

fn default_index_wal_backstop_ms() -> u64 {
    DEFAULT_INDEX_WAL_BACKSTOP_MS
}

fn default_tx_path_derive_threads() -> usize {
    DEFAULT_TX_PATH_DERIVE_THREADS
}

fn default_min_durable_interval_ms() -> u64 {
    DEFAULT_MIN_DURABLE_INTERVAL_MS
}

fn default_durable_headroom_permille() -> u64 {
    DEFAULT_DURABLE_HEADROOM_PERMILLE
}

fn default_entropy_sweep_interval_millis() -> u64 {
    1000
}

/// Default cap on one entropy pread.
///
/// 4 MiB is about 22 ms at the quiet-drive rate the in-flight budget uses
/// (182 MiB/s), so one read stays inside a recall's margin. One 256 KiB
/// chunk is a random read on an HDD and does not fill that margin.
pub const DEFAULT_ENTROPY_SWEEP_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Default unread gap a sweep may read through. One 256 KiB chunk.
pub const DEFAULT_ENTROPY_COALESCE_HOLE_BYTES: u64 = 256 * 1024;

/// How long a short entropy span waits for a neighbor, including on an idle disk.
///
/// 20 ms collects a burst of concurrent chunk posts and stays well under the
/// 250 ms durable interval. A lone chunk is late by this much.
pub const DEFAULT_ENTROPY_READ_GRACE_MS: u64 = 20;

/// Bytes a span must reach before it skips [`DEFAULT_ENTROPY_READ_GRACE_MS`].
/// The pread is still bounded by [`DEFAULT_ENTROPY_SWEEP_MAX_BYTES`].
pub const DEFAULT_ENTROPY_READ_BATCH_BYTES: u64 = 4 * 1024 * 1024;

/// Extra bytes read past a short ingress span.
///
/// The same 4 MiB as [`DEFAULT_ENTROPY_SWEEP_MAX_BYTES`]. A one-chunk span
/// can grow to that cap. A larger value cannot be issued until the sweep
/// cap is raised, because one pread stays inside the recall margin.
pub const DEFAULT_ENTROPY_READAHEAD_BYTES: u64 = 4 * 1024 * 1024;

/// Per-module cap on cached read-ahead entropy.
///
/// 64 MiB is 256 chunks. That covers the in-flight window of a few
/// concurrent uploads on one HDD module. Older pads are dropped first.
pub const DEFAULT_ENTROPY_READAHEAD_CACHE_BYTES: u64 = 64 * 1024 * 1024;

fn default_entropy_sweep_max_bytes() -> u64 {
    DEFAULT_ENTROPY_SWEEP_MAX_BYTES
}

fn default_entropy_coalesce_hole_bytes() -> u64 {
    DEFAULT_ENTROPY_COALESCE_HOLE_BYTES
}

fn default_entropy_read_grace_ms() -> u64 {
    DEFAULT_ENTROPY_READ_GRACE_MS
}

fn default_entropy_read_batch_bytes() -> u64 {
    DEFAULT_ENTROPY_READ_BATCH_BYTES
}

fn default_entropy_readahead_bytes() -> u64 {
    DEFAULT_ENTROPY_READAHEAD_BYTES
}

/// Accept a configured read size and keep the sweep cap.
///
/// One pread cannot pass [`DEFAULT_ENTROPY_SWEEP_MAX_BYTES`]. A smaller
/// value is not a node-config knob. Tests and the bench assign the field
/// in code, which does not go through this function.
fn freeze_entropy_bytes_at_sweep_cap<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _configured = u64::deserialize(deserializer)?;
    Ok(DEFAULT_ENTROPY_SWEEP_MAX_BYTES)
}

fn default_entropy_readahead_cache_bytes() -> u64 {
    DEFAULT_ENTROPY_READAHEAD_CACHE_BYTES
}

fn default_pending_body_hold_timeout() -> Duration {
    Duration::from_secs(10 * 60)
}

fn default_max_local_cache_hits_per_tick() -> u64 {
    64
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DataSyncServiceConfig {
    pub max_pending_chunk_requests: u64,
    pub max_storage_throughput_bps: u64,
    #[serde(
        deserialize_with = "serde_utils::duration_from_string",
        serialize_with = "serde_utils::serialize_duration_string"
    )]
    pub bandwidth_adjustment_interval: Duration,
    #[serde(
        deserialize_with = "serde_utils::duration_from_string",
        serialize_with = "serde_utils::serialize_duration_string"
    )]
    pub chunk_request_timeout: Duration,
    /// How long data sync leaves an offset to a live body-migration job.
    ///
    /// The clock starts when the hold starts. It restarts when that job's row
    /// key moves forward. A job that makes no progress is fetched from peers
    /// after this. Monotonic; not wall-clock and not the job's attempt count.
    #[serde(
        deserialize_with = "serde_utils::duration_from_string",
        serialize_with = "serde_utils::serialize_duration_string",
        default = "default_pending_body_hold_timeout"
    )]
    pub pending_body_hold_timeout: Duration,
    /// Local-cache lookups one storage module may do in one data-sync tick.
    ///
    /// Each lookup reads the submodule index and the chunk cache. A hit also
    /// hashes the body. The cap keeps that work off an unbounded burst.
    /// Zero is raised to one so the popped offset is not left in front of
    /// every peer fetch.
    #[serde(default = "default_max_local_cache_hits_per_tick")]
    pub max_local_cache_hits_per_tick: u64,
}

impl Default for DataSyncServiceConfig {
    fn default() -> Self {
        Self {
            max_pending_chunk_requests: 1000,
            max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
            bandwidth_adjustment_interval: Duration::from_secs(5),
            chunk_request_timeout: Duration::from_secs(10),
            pending_body_hold_timeout: default_pending_body_hold_timeout(),
            max_local_cache_hits_per_tick: default_max_local_cache_hits_per_tick(),
        }
    }
}

/// # Network Defaults
///
/// Default IP addresses used across all services unless overridden.
/// This allows you to specify public_ip and bind_ip once instead of
/// repeating them for each service (http, gossip, reth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDefaults {
    /// Default public IP address advertised to other peers
    pub public_ip: String,
    /// Default bind IP address for all services
    pub bind_ip: String,
}

/// Network configuration with optional overrides
pub trait NetworkConfigWithDefaults {
    /// Get the optional public IP
    fn public_ip_option(&self) -> &Option<String>;

    /// Get the optional bind IP
    fn bind_ip_option(&self) -> &Option<String>;

    /// Get the public IP, falling back to network defaults if not set
    fn public_ip<'a>(&'a self, defaults: &'a NetworkDefaults) -> &'a str {
        self.public_ip_option()
            .as_deref()
            .unwrap_or(&defaults.public_ip)
    }

    /// Get the bind IP, falling back to network defaults if not set
    fn bind_ip<'a>(&'a self, defaults: &'a NetworkDefaults) -> &'a str {
        self.bind_ip_option()
            .as_deref()
            .unwrap_or(&defaults.bind_ip)
    }
}

/// Macro to implement NetworkConfigWithDefaults for types with public_ip and bind_ip fields
macro_rules! impl_network_config_with_defaults {
    ($type:ty) => {
        impl NetworkConfigWithDefaults for $type {
            fn public_ip_option(&self) -> &Option<String> {
                &self.public_ip
            }

            fn bind_ip_option(&self) -> &Option<String> {
                &self.bind_ip
            }
        }
    };
}

fn default_actix_workers() -> usize {
    std::thread::available_parallelism()
        .map(|count| core::cmp::max(1, count.get() / 2))
        .unwrap_or(1)
}

/// # Gossip Network Configuration
///
/// Settings for peer-to-peer communication between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GossipConfig {
    /// The IP address that's going to be announced to other peers
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address the gossip service binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port number the gossip service listens on
    pub bind_port: u16,
    /// Actix worker count. Defaults to half the available system threads.
    #[serde(default = "default_actix_workers")]
    pub actix_workers: usize,
}

impl_network_config_with_defaults!(GossipConfig);

/// # Reth Node Configuration
///
/// Settings that are passed to the reth node
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethConfig {
    pub network: RethNetworkConfig,

    /// Sync mode for the Reth embedded database.
    #[serde(default = "default_reth_db_sync_mode")]
    pub db_sync_mode: DbSyncMode,

    /// Override for Reth engine cross-block cache size in MB.
    /// None means use Reth's default.
    #[serde(default)]
    pub cross_block_cache_size_megabytes: Option<usize>,

    /// Number of additional transaction validation tasks.
    #[serde(default = "default_additional_validation_tasks")]
    pub additional_validation_tasks: usize,
}

fn default_reth_db_sync_mode() -> DbSyncMode {
    DbSyncMode::Durable
}
fn default_additional_validation_tasks() -> usize {
    2
}

/// # Reth network Configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethNetworkConfig {
    #[serde(default)]
    pub use_random_ports: bool,
    /// The IP address that's going to be announced to other peers
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address that Reth binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port number the Reth listens on
    pub bind_port: u16,
    // peer ID
    // WARNING: this gets overridden partway through the startup sequence with the correct value
    #[serde(default)]
    pub peer_id: alloy_primitives::B512,
}

impl_network_config_with_defaults!(RethNetworkConfig);

/// # Data Packing Configuration
///
/// Controls how data is compressed and packed for storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PackingConfig {
    #[serde(default)]
    pub local: LocalPackingConfig,
    #[serde(default)]
    pub remote: Vec<RemotePackingConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LocalPackingConfig {
    /// Number of CPU threads to use for data packing operations
    pub cpu_packing_concurrency: u16,

    /// Batch size for GPU-accelerated packing operations
    pub gpu_packing_batch_size: u32,
}

impl Default for LocalPackingConfig {
    fn default() -> Self {
        Self {
            cpu_packing_concurrency: 2, // TODO: default to something like numcpus - 4
            gpu_packing_batch_size: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePackingConfig {
    pub url: String,

    // This is the read (max time between streamed chunks) and connection timeout
    pub timeout: Option<Duration>,
}

/// # Cache Configuration
///
/// Settings for in-memory caching to improve performance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Number of blocks cache cleaning will lag behind block finalization
    /// Higher values keep more data in cache but use more memory
    pub cache_clean_lag: u8,

    pub max_cache_size_bytes: u64,

    /// Target capacity for chunk cache as a percentage of it's total capacity (0 -> 100%)
    /// Don't set this too low, or you won't be able to promote transactions
    pub prune_at_capacity_percent: f64,
}

/// Default maximum cache size: 10 GiB
pub const DEFAULT_MAX_CACHE_SIZE_BYTES: u64 = 10_737_418_240;

pub const DEFAULT_PRUNE_AT_CAPACITY_PERCENT: f64 = 80_f64;

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            cache_clean_lag: 0,
            max_cache_size_bytes: DEFAULT_MAX_CACHE_SIZE_BYTES,
            prune_at_capacity_percent: DEFAULT_PRUNE_AT_CAPACITY_PERCENT,
        }
    }
}

/// # HTTP API Configuration
///
/// Settings for the node's HTTP server that provides API access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// The IP address visible to the outside world
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port that is visible to the outside world
    pub public_port: u16,
    /// The IP address the HTTP service binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port that the Node's HTTP server should listen on. Set to 0 for randomization.
    pub bind_port: u16,
    /// Actix worker count. Defaults to half the available system threads.
    #[serde(default = "default_actix_workers")]
    pub actix_workers: usize,
    /// Whether to expose the node-internal block-follower stack: mount `/internal/*` HTTP routes
    /// (block-stream SSE, canonical block reads, unpacked chunk-range reads) **and** run the durable
    /// block-stream producer that appends `observed` / `finalized` / `reorged` frames to the
    /// consensus MDBX log. Off by default: routes carry no application-layer authentication, and
    /// the producer is unnecessary write load when nothing can subscribe. Enable only on a node
    /// whose HTTP bind is restricted to a trusted gateway at the network layer (firewall / reverse
    /// proxy / bind address).
    #[serde(default)]
    pub expose_internal_api: bool,
}

impl_network_config_with_defaults!(HttpConfig);

/// P2P handshake configuration with sensible defaults
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct P2PHandshakeConfig {
    pub max_concurrent_handshakes: usize,
    pub max_peers_per_response: usize,
    pub max_retries: u32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
    pub blocklist_ttl_secs: u64,
    pub server_peer_list_cap: usize,
}

impl Default for P2PHandshakeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_handshakes: 32,
            max_peers_per_response: 25,
            max_retries: 8,
            backoff_base_secs: 1,
            backoff_cap_secs: 60,
            blocklist_ttl_secs: 600,
            server_peer_list_cap: 25,
        }
    }
}

/// P2P gossip/broadcast configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct P2PGossipConfig {
    /// Maximum peers to target per broadcast step
    pub broadcast_batch_size: usize,
    /// Interval between broadcast steps in milliseconds
    pub broadcast_batch_throttle_interval: u64,
    /// Enable scoring of peers based on their behavior. Disabling this might help with reducing
    /// noise during debug, otherwise it's recommended to keep it enabled.
    pub enable_scoring: bool,
    /// Maximum concurrent chunk handler tasks on the gossip receiver.
    /// Limits memory and CPU pressure from inbound chunk processing.
    pub max_concurrent_gossip_chunks: usize,
    /// Maximum concurrent block-body serves (pull and get-data).
    /// Limits database reconstruction of historical bodies.
    /// `0` means unlimited.
    pub max_concurrent_block_body_serves: usize,
}

impl Default for P2PGossipConfig {
    fn default() -> Self {
        Self {
            broadcast_batch_size: 50,
            broadcast_batch_throttle_interval: 100,
            enable_scoring: true,
            max_concurrent_gossip_chunks: 50,
            max_concurrent_block_body_serves: 8,
        }
    }
}

/// P2P pull/request configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct P2PPullConfig {
    /// How many top active peers to consider before random sampling
    pub top_active_window: usize,
    /// Number of peers to randomly sample (truncate) per pull attempt batch
    pub sample_size: usize,
    /// Maximum number of attempts to iterate over the sampled set
    pub max_attempts: u32,
}

impl Default for P2PPullConfig {
    fn default() -> Self {
        Self {
            top_active_window: 10,
            sample_size: 5,
            max_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    /// How many blocks to fetch in parallel per batch during the sync
    pub block_batch_size: usize,
    /// How often to check if we're behind and need to sync
    pub periodic_sync_check_interval_secs: u64,
    /// Timeout for retry block pull/process
    pub retry_block_request_timeout_secs: u64,
    /// Whether to enable periodic sync checks
    pub enable_periodic_sync_check: bool,
    /// Timeout per attempt when waiting for a queue slot
    pub wait_queue_slot_timeout_secs: u64,
    /// Maximum consecutive timeout attempts when waiting for a queue slot with no active validations
    pub wait_queue_slot_max_attempts: usize,
    /// Minimum number of active+online peers required before chain sync proceeds.
    /// If unmet within `peer_wait_timeout_millis`, sync proceeds best-effort
    /// with however many peers are available.
    pub min_active_peers: usize,
    /// How long to wait for `min_active_peers` peers before proceeding
    /// best-effort. Does not apply to the genesis branch, which uses
    /// `NodeConfig::genesis_peer_discovery_timeout_millis`.
    pub peer_wait_timeout_millis: u64,
    /// Bounded wait used by `ExecutionPayloadCache::wait_for_sealed_block`
    /// to cap how long the validation pipeline blocks on a payload that
    /// may never arrive (peer advertised the block header but never
    /// served the EVM payload). Sized to exceed the ~50s budget of the
    /// payload-request retry loop so the request side completes first
    /// under normal conditions; tripping this timeout maps to the soft
    /// `ValidationError::ExecutionPayloadCacheEvicted` (block parks for
    /// retry, NOT a node fault).
    #[serde(default = "default_execution_payload_wait_timeout_millis")]
    pub execution_payload_wait_timeout_millis: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            block_batch_size: 50,
            // Check every 30 seconds if we're behind
            periodic_sync_check_interval_secs: 30,
            retry_block_request_timeout_secs: 30,
            enable_periodic_sync_check: true,
            wait_queue_slot_timeout_secs: 30,
            wait_queue_slot_max_attempts: 3,
            // Production: require 3 active+online peers before chain sync proceeds.
            min_active_peers: 3,
            peer_wait_timeout_millis: 20_000,
            execution_payload_wait_timeout_millis: default_execution_payload_wait_timeout_millis(),
        }
    }
}

/// Default `execution_payload_wait_timeout_millis`. 60s exceeds the ~50s
/// budget of the 10×5s payload-request retry loop in
/// `ExecutionPayloadCache::request_payload_from_the_network` so the
/// request side completes first under normal conditions; the timeout
/// only fires when a peer advertises a header but never serves the
/// payload.
fn default_execution_payload_wait_timeout_millis() -> u64 {
    60_000
}

/// Default ceiling for a single peer HTTP response body.
///
/// About twice a 30M-gas execution payload once JSON hex-encodes the
/// calldata. A gas limit past roughly 60M of zero-byte calldata needs
/// this constant raised.
pub const DEFAULT_MAX_PEER_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

fn default_max_peer_response_bytes() -> u64 {
    DEFAULT_MAX_PEER_RESPONSE_BYTES
}

/// Default for `peer_filter_mode` when the field is not present in the provided TOML.
/// This keeps legacy configurations working by defaulting to unrestricted mode.
fn default_peer_filter_mode() -> PeerFilterMode {
    PeerFilterMode::Unrestricted
}

/// Default genesis peer discovery timeout (20s)
fn default_genesis_peer_discovery_timeout_millis() -> u64 {
    20_000
}

/// Default network configuration when not specified in the config file.
/// Uses localhost for public_ip and binds to all interfaces.
fn default_network_defaults() -> NetworkDefaults {
    NetworkDefaults {
        public_ip: "127.0.0.1".to_string(),
        bind_ip: "0.0.0.0".to_string(),
    }
}

/// # Metrics Configuration
///
/// Settings for the Reth-internal Prometheus endpoint. Defaults constrain
/// the listener to localhost; production deployments front it with an
/// mTLS-terminating reverse proxy and never need to expose this port
/// directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Bind address for the Reth Prometheus endpoint (port 9001).
    pub bind_ip: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind_ip: "127.0.0.1".to_string(),
        }
    }
}

fn default_metrics_config() -> MetricsConfig {
    MetricsConfig::default()
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VdfNodeConfig {
    /// Maximum number of threads to use for parallel VDF verification.
    ///
    /// This is a per-pool limit, not a node-wide one. Block discovery
    /// (prevalidation) and the validation service each build their own rayon
    /// pool of this size, so a node doing both at once can run up to twice
    /// this many verification threads. The pools are kept separate on purpose:
    /// prevalidation gates block forwarding and must not queue behind a
    /// validation batch, and rayon offers no way to prioritise within a pool.
    ///
    /// Defaults to `available_parallelism() - 2` (never below 1): one core held
    /// back for the VDF thread (which `core_pinning` may pin) and one for the
    /// rest of the node — tokio, reth and the API server. That reservation is
    /// sized for one pool at a time, so it only holds while a single pool is
    /// verifying. A fixed default silently wastes a large
    /// machine: this is the widest stage of block prevalidation
    /// (`last_step_checkpoints_is_valid` re-hashes a whole VDF step per block),
    /// so a host that grows more cores gets no faster unless the limit grows
    /// with it.
    ///
    /// Set it explicitly to override. Derived or explicit, it is clamped to
    /// `consensus.num_checkpoints_in_vdf_step` when `VdfConfig` is merged —
    /// a pass hands one checkpoint per thread. That also caps the floor
    /// `validation_batch_size` is raised to (`clamped_validation_batch_size`).
    #[serde(default = "default_parallel_verification_thread_limit")]
    pub parallel_verification_thread_limit: usize,

    /// Controls whether and how the VDF thread is pinned to a CPU core.
    #[serde(default)]
    pub core_pinning: CorePinning,

    /// Whether the local VDF thread computes its own steps, or only follows the
    /// chain from validated fast-forward steps. `Auto` defers to the startup
    /// throughput benchmark.
    #[serde(default)]
    pub free_run: VdfFreeRun,

    /// When true, the local step uses a 25ms floor instead of one second.
    /// Test configs set this so a tiny hash count can still produce many steps
    /// per second. Production leaves it false: the hash count stays the
    /// consensus difficulty, and a core that finishes early waits out the rest
    /// of the second.
    #[serde(default)]
    pub throttle: bool,

    /// Bail out of a VDF wait if the local `global_step` has not advanced for
    /// this many seconds. Detects a dead/stuck VDF thread without imposing a
    /// wall-clock cap on legitimate long waits.
    ///
    /// Used in three places, all in the validation pipeline. All three treat a
    /// trip as a local-infrastructure failure ("never mislabel a block as
    /// Valid or Invalid"; see design/docs/vdf-validation-stall-detection.md)
    /// and panic the process so the supervisor restarts it clean:
    ///   1. `wait_for_step` — `ensure_vdf_is_valid` calls it once at the start
    ///      (waiting for the previous VDF step) and once at the end (waiting
    ///      for `global_step` to reach the block's `global_step_number` after
    ///      all batches have been enqueued). A `WaitForStepError::Stalled`
    ///      from either site panics the validation task.
    ///   2. `fast_forward_validated_steps` — per-step send timeout (and the
    ///      receiver-closed arm) into the bounded `vdf_fast_forward` channel.
    ///      A consumer that stays stuck for this long, or has dropped the
    ///      receiver, panics — see the call site for why.
    ///   3. The pipeline watchdog — every 5s tick the validation loop force-
    ///      aborts a VDF task whose stage signal has been idle for this long,
    ///      panicking the service (and the process) on the assumption that
    ///      something is genuinely broken, not slow.
    #[serde(default = "default_vdf_progress_timeout_secs")]
    pub progress_timeout_secs: u64,

    /// Number of VDF steps to validate before fast-forwarding that validated
    /// prefix and waiting for local progress. Smaller batches surface liveness
    /// problems sooner; larger batches reduce coordination overhead.
    #[serde(default = "default_vdf_validation_batch_size")]
    pub validation_batch_size: usize,
}

fn default_vdf_progress_timeout_secs() -> u64 {
    15
}

/// Cores this node reserves for work other than parallel VDF verification: one
/// for the VDF thread itself, one for tokio/reth/the API server.
const VDF_VERIFICATION_RESERVED_CORES: usize = 2;

/// `available` minus [`VDF_VERIFICATION_RESERVED_CORES`], floored at 1 so a 1-
/// or 2-core host still verifies (single-threaded) rather than building a
/// zero-thread rayon pool.
///
/// Separate from the `available_parallelism()` call that feeds it, so the
/// reservation arithmetic is testable without depending on the core count of
/// the machine running the test.
const fn verification_threads_for(available: usize) -> usize {
    let reserved = available.saturating_sub(VDF_VERIFICATION_RESERVED_CORES);
    if reserved == 0 { 1 } else { reserved }
}

/// Falls back to 1 when the core count is unavailable: under-committing costs
/// throughput, while a guessed-high default oversubscribes a host we could not
/// measure.
fn default_parallel_verification_thread_limit() -> usize {
    std::thread::available_parallelism()
        .map(|count| verification_threads_for(count.get()))
        .unwrap_or(1)
}

fn default_vdf_validation_batch_size() -> usize {
    32
}

impl Default for VdfNodeConfig {
    fn default() -> Self {
        Self {
            parallel_verification_thread_limit: default_parallel_verification_thread_limit(),
            core_pinning: CorePinning::default(),
            free_run: VdfFreeRun::default(),
            throttle: false,
            progress_timeout_secs: default_vdf_progress_timeout_secs(),
            validation_batch_size: default_vdf_validation_batch_size(),
        }
    }
}

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MempoolNodeConfig {
    /// Maximum number of addresses in the LRU cache for out-of-order stakes and pledges
    /// Controls memory usage for tracking transactions that arrive before their dependencies
    pub max_pending_pledge_items: usize,

    /// Maximum number of pending pledge transactions allowed per address
    /// Limits the resources that can be consumed by a single address
    pub max_pledges_per_item: usize,

    /// Maximum number of transaction data roots to keep in the pending cache
    /// For transactions whose chunks arrive before the transaction header
    pub max_pending_chunk_items: usize,

    /// Maximum number of chunks that can be cached per data root
    /// Prevents memory exhaustion from excessive chunk storage for a single transaction
    pub max_chunks_per_item: usize,

    /// Maximum number of pre-header chunks to keep per data root before the header arrives
    /// Limits speculative storage window for out-of-order chunks
    pub max_preheader_chunks_per_item: usize,

    /// Maximum allowed pre-header data_path bytes for chunk proofs
    /// Mitigates DoS on speculative chunk storage before header arrival
    pub max_preheader_data_path_bytes: usize,

    /// Maximum number of valid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_valid_items: usize,

    /// Maximum number of invalid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_invalid_items: usize,

    /// Maximum number of valid chunk hashes to keep track of
    /// Prevents re-processing and re-gossipping of recently seen chunks
    pub max_valid_chunks: usize,

    /// Maximum number of data transactions to hold in mempool
    /// Prevents unbounded growth. Conservative: max_data_txs_per_block * block_migration_depth * 3
    pub max_valid_submit_txs: usize,

    /// Maximum number of addresses with pending commitment transactions
    /// Prevents unbounded growth. Conservative: num_staked_miners * 3
    pub max_valid_commitment_addresses: usize,

    /// Maximum commitment transactions per address
    /// Limits the resources that can be consumed by a single address
    pub max_commitments_per_address: usize,

    /// Maximum number of concurrent handlers for the mempool messages
    pub max_concurrent_mempool_tasks: usize,

    /// Maximum number of concurrent handlers for chunk ingress messages
    pub max_concurrent_chunk_ingress_tasks: usize,

    /// Reserved concurrency for control-plane messages (`IngestIngressProof`,
    /// `ProcessPendingChunks`) on the chunk ingress service. Carved out of
    /// `max_concurrent_chunk_ingress_tasks`, not added on top: the service's
    /// total peak concurrency stays at `max_concurrent_chunk_ingress_tasks`,
    /// but this many permits are reserved for control-plane work so chunk
    /// floods cannot starve them.
    pub max_control_plane_concurrent_tasks: usize,

    /// Backpressure channel capacity for the async chunk write-behind buffer.
    /// Controls how many chunk writes can be queued before the sender blocks.
    pub chunk_writer_buffer_size: usize,

    /// Max public `POST /v1/chunk` bodies admitted and not yet finished by ingress.
    pub max_http_chunk_admission: usize,
    /// Max decoded public POSTs waiting for an admission permit
    /// (`http_chunk_admission_timeout_millis`).
    pub max_http_chunk_waiters: usize,
    /// Monotonic wait for an admission permit, in milliseconds.
    pub http_chunk_admission_timeout_millis: u64,
}

impl Default for MempoolNodeConfig {
    fn default() -> Self {
        Self {
            max_pending_pledge_items: 1000,
            max_pledges_per_item: 10,
            max_pending_chunk_items: 500,
            max_chunks_per_item: 100,
            max_preheader_chunks_per_item: 50,
            max_preheader_data_path_bytes: 4096,
            max_valid_items: 10_000,
            max_invalid_items: 5_000,
            max_valid_chunks: 5_000,
            max_valid_submit_txs: 3_000,
            max_valid_commitment_addresses: 1_000,
            max_commitments_per_address: 5,
            max_concurrent_mempool_tasks: 30,
            max_concurrent_chunk_ingress_tasks: 30,
            // Backward-compat fallback for missing `max_control_plane_concurrent_tasks`
            // in operator configs that pre-date the field. `0` means "no
            // reserved control-plane lane" (legacy single-lane mode) so an
            // operator running with `max_concurrent_chunk_ingress_tasks = 1`
            // still validates after upgrade. Fresh testnet/mainnet configs
            // override this to a positive value to enable the carve-out.
            max_control_plane_concurrent_tasks: 0,
            chunk_writer_buffer_size: 4096,
            max_http_chunk_admission: 256,
            max_http_chunk_waiters: 32,
            http_chunk_admission_timeout_millis: 5000,
        }
    }
}

impl NodeConfig {
    pub fn consensus_config(&self) -> ConsensusConfig {
        // load the consensus config
        // todo: lazy load the consensus config, caching the result for subsequent calls

        match &self.consensus {
            ConsensusOptions::Path(path_buf) => std::fs::read_to_string(path_buf)
                .map(|consensus_cfg| {
                    toml::from_str::<ConsensusConfig>(&consensus_cfg)
                        .expect("invalid consensus file")
                })
                .expect("consensus cfg does not exist"),
            ConsensusOptions::Testnet => ConsensusConfig::testnet(),
            ConsensusOptions::Testing => ConsensusConfig::testing(),
            ConsensusOptions::Mainnet => ConsensusConfig::mainnet(),
            ConsensusOptions::Custom(consensus_config) => consensus_config.clone(),
        }
    }

    pub fn with_consensus<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ConsensusConfig),
    {
        f(self.consensus.get_mut());
        self
    }

    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.run_mode = run_mode;
        self
    }

    pub fn with_genesis_peer_discovery_timeout(mut self, timeout_millis: u64) -> Self {
        self.genesis_peer_discovery_timeout_millis = timeout_millis;
        self
    }

    pub fn miner_address(&self) -> IrysAddress {
        IrysAddress::from_private_key(&self.mining_key)
    }

    pub fn new_random_signer(&self) -> IrysSigner {
        IrysSigner::random_signer(&self.consensus_config())
    }

    pub fn signer(&self) -> IrysSigner {
        IrysSigner {
            signer: self.mining_key.clone(),
            chain_id: self.consensus_config().chain_id,
            chunk_size: self.consensus_config().chunk_size,
        }
    }

    pub fn local_api_url(&self) -> String {
        format!(
            "http://{}:{}",
            self.http.bind_ip(&self.network_defaults),
            self.http.bind_port
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn fund_genesis_accounts<'a>(
        &mut self,
        signers: impl IntoIterator<Item = &'a IrysSigner>,
    ) -> &mut Self {
        let mut accounts: Vec<(IrysAddress, GenesisAccount)> = Vec::new();
        for signer in signers {
            accounts.push((
                signer.address(),
                GenesisAccount {
                    balance: alloy_primitives::U256::from(99999000000000000000000_u128),
                    ..Default::default()
                },
            ))
        }
        self.consensus.extend_genesis_accounts(accounts);
        self
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing_with_signer(signer: &IrysSigner) -> Self {
        let mining_key = signer.signer.clone();
        let reward_address = signer.address();
        let mut consensus = ConsensusConfig::testing();
        consensus.genesis.miner_address = reward_address;
        consensus.genesis.reward_address = reward_address;
        Self {
            node_mode: NodeMode::Genesis,
            sync_mode: SyncMode::Full,
            consensus: ConsensusOptions::Custom(consensus),
            base_directory: default_irys_path(),

            oracles: vec![OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::token(dec!(0.00000000000001))
                    .expect("valid token amount"),
                smoothing_interval: 15,
                initial_direction_up: true,
                poll_interval_ms: default_mock_oracle_poll_interval_ms(),
            }],
            mining_key,
            reward_address,
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                entropy_sweep_max_bytes: 256 * 1024,
                entropy_coalesce_hole_bytes: 256 * 1024,
                // Tests drive the sweep themselves and must not sleep on a grace.
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: 256 * 1024,
                // Tests count one pread per span. Read-ahead would add bytes.
                entropy_readahead_bytes: 0,
                entropy_readahead_cache_bytes: 0,
                drop_recall_page_cache: false,
                recall_direct_io: false,
                // Tests rely on the recall holding the disk for other IO.
                recall_io_priority: true,
                // Tests read `intervals.json` as soon as `sync` returns.
                intervals_persist_secs: 0,
                index_wal_backstop_ms: DEFAULT_INDEX_WAL_BACKSTOP_MS,
                tx_path_derive_threads: DEFAULT_TX_PATH_DERIVE_THREADS,
                // Tests read durability as soon as `sync` returns. 0 does not defer.
                min_durable_interval_ms: 0,
                durable_headroom_permille: DEFAULT_DURABLE_HEADROOM_PERMILLE,
            },
            data_sync: DataSyncServiceConfig {
                max_pending_chunk_requests: 1000,
                max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
                bandwidth_adjustment_interval: Duration::from_secs(5),
                chunk_request_timeout: Duration::from_secs(10),
                pending_body_hold_timeout: default_pending_body_hold_timeout(),
                max_local_cache_hits_per_tick: default_max_local_cache_hits_per_tick(),
            },
            trusted_peers: vec![/* PeerAddress {
                api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
                gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
                execution: crate::RethPeerInfo::default(), // TODO: figure out how to pre-compute peer IDs
            }*/],
            initial_stake_and_pledge_whitelist: vec![],
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            network_defaults: NetworkDefaults {
                public_ip: "127.0.0.1".to_string(),
                bind_ip: "127.0.0.1".to_string(),
            },
            gossip: GossipConfig {
                public_ip: None,
                public_port: 0,
                bind_ip: None,
                bind_port: 0,
                actix_workers: default_actix_workers(),
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: true,
                    public_ip: Some("0.0.0.0".to_string()),
                    public_port: 0,
                    bind_ip: Some("0.0.0.0".to_string()),
                    bind_port: 0,
                    peer_id: Default::default(),
                },
                db_sync_mode: DbSyncMode::UtterlyNoSync,
                cross_block_cache_size_megabytes: Some(10),
                additional_validation_tasks: 0,
            },
            packing: PackingConfig {
                local: LocalPackingConfig {
                    cpu_packing_concurrency: 4,
                    gpu_packing_batch_size: 1024,
                },
                remote: Default::default(),
            },
            cache: CacheConfig {
                cache_clean_lag: 2,
                max_cache_size_bytes: DEFAULT_MAX_CACHE_SIZE_BYTES,
                prune_at_capacity_percent: DEFAULT_PRUNE_AT_CAPACITY_PERCENT,
            },
            http: HttpConfig {
                public_ip: None,
                public_port: 0,
                bind_ip: None,
                bind_port: 0,
                actix_workers: default_actix_workers(),
                // Test nodes bind locally and are trusted, so expose the internal block-follower API.
                expose_internal_api: true,
            },
            mempool: MempoolNodeConfig {
                max_pending_pledge_items: 100,
                max_pledges_per_item: 100,
                max_pending_chunk_items: 30,
                max_chunks_per_item: 500,
                max_preheader_chunks_per_item: 64,
                max_preheader_data_path_bytes: 64 * 1024,
                max_invalid_items: 10_000,
                max_valid_items: 10_000,
                max_valid_chunks: 10_000,
                max_valid_submit_txs: 3000,
                max_valid_commitment_addresses: 300,
                max_commitments_per_address: 20,
                max_concurrent_mempool_tasks: 30,
                max_concurrent_chunk_ingress_tasks: 30,
                max_control_plane_concurrent_tasks: 4,
                chunk_writer_buffer_size: 4096,
                max_http_chunk_admission: 256,
                max_http_chunk_waiters: 32,
                http_chunk_admission_timeout_millis: 5000,
            },
            metrics: MetricsConfig::default(),

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: 8,
                core_pinning: CorePinning::Disabled,
                // Test configs skip the throughput benchmark (low
                // sha_1s_difficulty), so `Auto` resolves to free-running.
                free_run: VdfFreeRun::Auto,
                throttle: true,
                progress_timeout_secs: default_vdf_progress_timeout_secs(),
                validation_batch_size: default_vdf_validation_batch_size(),
            },

            p2p_handshake: P2PHandshakeConfig::default(),
            p2p_gossip: P2PGossipConfig::default(),
            p2p_pull: P2PPullConfig::default(),
            max_peer_response_bytes: DEFAULT_MAX_PEER_RESPONSE_BYTES,
            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,
            sync: SyncConfig {
                min_active_peers: 1,
                // 2_000ms was still too short on CI: peer-branch tests where
                // genesis was restarted between phases skipped the initial
                // bootstrap because the handshake hadn't completed in time,
                // then had to wait the full 30s periodic-check window to
                // re-engage — leaving too little budget to sync a long
                // catch-up before per-test deadlines fired. 10_000ms matches
                // `genesis_peer_discovery_timeout_millis` so handshake has
                // the same headroom on both branches.
                peer_wait_timeout_millis: 10_000,
                // Short wait timeout for tests: the production default
                // (60s) would let `wait_for_sealed_block` hang a failing
                // test for nearly a minute before surfacing
                // `WaitTimeout`. 5s keeps the timeout long enough that
                // it does not fire on a healthy in-process fast path
                // while still bounding pathological waits.
                execution_payload_wait_timeout_millis: 5_000,
                ..SyncConfig::default()
            },
            run_mode: RunMode::Test,
            database: DatabaseConfig {
                sync_mode: DbSyncMode::UtterlyNoSync,
                cache_sync_mode: DbSyncMode::UtterlyNoSync,
                // Small MDBX geometry so many concurrent test node databases don't
                // exhaust virtual address space (see TEST_DB_GEOMETRY_MAX_SIZE).
                geometry_max_size: Some(crate::TEST_DB_GEOMETRY_MAX_SIZE),
                // MDBX-SUBMODULE-INDEX: point test configs at RocksDB when that is the only engine. This line selects MDBX.
                submodule_index: SubmoduleIndexEngine::Mdbx,
                ..DatabaseConfig::default()
            },
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing_with_epochs(num_blocks_in_epoch: usize) -> Self {
        let mut node_config = Self::testing();
        node_config.consensus.get_mut().epoch.num_blocks_in_epoch = num_blocks_in_epoch as u64;
        node_config
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing() -> Self {
        use k256::ecdsa::SigningKey;
        let mining_key = SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key");
        let signer = IrysSigner {
            signer: mining_key,
            chain_id: 0,
            chunk_size: 0,
        };

        Self::testing_with_signer(&signer)
    }

    pub fn testnet() -> Self {
        use k256::ecdsa::SigningKey;
        let mining_key = SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key");
        let mut consensus = ConsensusConfig::testnet();
        let signer = IrysSigner {
            signer: mining_key,
            chain_id: consensus.chain_id,
            chunk_size: consensus.chunk_size,
        };

        let mining_key = signer.signer.clone();
        let reward_address = signer.address();
        consensus.genesis.miner_address = reward_address;
        consensus.genesis.reward_address = reward_address;
        consensus.expected_genesis_hash = Some(H256::zero());
        Self {
            node_mode: NodeMode::Miner,
            sync_mode: SyncMode::Full,
            consensus: ConsensusOptions::Custom(consensus),
            base_directory: default_irys_path(),

            oracles: vec![OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::token(dec!(0.00000000000001))
                    .expect("valid token amount"),
                smoothing_interval: 15,
                initial_direction_up: true,
                poll_interval_ms: default_mock_oracle_poll_interval_ms(),
            }],
            mining_key,
            reward_address,
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 1000,
                entropy_sweep_max_bytes: DEFAULT_ENTROPY_SWEEP_MAX_BYTES,
                entropy_coalesce_hole_bytes: DEFAULT_ENTROPY_COALESCE_HOLE_BYTES,
                entropy_read_grace_ms: DEFAULT_ENTROPY_READ_GRACE_MS,
                entropy_read_batch_bytes: DEFAULT_ENTROPY_READ_BATCH_BYTES,
                entropy_readahead_bytes: DEFAULT_ENTROPY_READAHEAD_BYTES,
                entropy_readahead_cache_bytes: DEFAULT_ENTROPY_READAHEAD_CACHE_BYTES,
                drop_recall_page_cache: false,
                recall_direct_io: default_recall_direct_io(),
                recall_io_priority: default_recall_io_priority(),
                intervals_persist_secs: DEFAULT_INTERVALS_PERSIST_SECS,
                index_wal_backstop_ms: DEFAULT_INDEX_WAL_BACKSTOP_MS,
                tx_path_derive_threads: DEFAULT_TX_PATH_DERIVE_THREADS,
                min_durable_interval_ms: DEFAULT_MIN_DURABLE_INTERVAL_MS,
                durable_headroom_permille: DEFAULT_DURABLE_HEADROOM_PERMILLE,
            },
            data_sync: DataSyncServiceConfig {
                max_pending_chunk_requests: 1000,
                max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
                bandwidth_adjustment_interval: Duration::from_secs(5),
                chunk_request_timeout: Duration::from_secs(10),
                pending_body_hold_timeout: default_pending_body_hold_timeout(),
                max_local_cache_hits_per_tick: default_max_local_cache_hits_per_tick(),
            },
            trusted_peers: vec![],
            // trusted_peers: vec![PeerAddress {
            //     api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
            //     gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
            //     execution: reth_peer_info, // TODO: figure out how to pre-compute peer IDs
            // }],
            initial_stake_and_pledge_whitelist: vec![],
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            network_defaults: NetworkDefaults {
                public_ip: "127.0.0.1".to_string(),
                bind_ip: "0.0.0.0".to_string(),
            },
            gossip: GossipConfig {
                public_ip: None,
                public_port: 8081,
                bind_ip: None,
                bind_port: 8081,
                actix_workers: default_actix_workers(),
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: false,
                    public_ip: None,
                    public_port: 9009,
                    bind_ip: Some("127.0.0.1".to_string()),
                    bind_port: 9009,
                    peer_id: Default::default(),
                },
                db_sync_mode: DbSyncMode::Durable,
                cross_block_cache_size_megabytes: None,
                additional_validation_tasks: 2,
            },
            packing: PackingConfig {
                local: LocalPackingConfig {
                    cpu_packing_concurrency: 4,
                    gpu_packing_batch_size: 1024,
                },
                remote: Default::default(),
            },
            cache: CacheConfig {
                cache_clean_lag: 2,
                max_cache_size_bytes: DEFAULT_MAX_CACHE_SIZE_BYTES,
                prune_at_capacity_percent: DEFAULT_PRUNE_AT_CAPACITY_PERCENT,
            },
            http: HttpConfig {
                public_ip: None,
                public_port: 8080,
                bind_ip: None,
                bind_port: 8080,
                actix_workers: default_actix_workers(),
                // Unauthenticated internal endpoints stay off by default; a gateway deployment opts
                // in via `expose_internal_api` once its HTTP bind is network-restricted.
                expose_internal_api: false,
            },

            mempool: MempoolNodeConfig {
                max_pending_pledge_items: 100,
                max_pledges_per_item: 100,
                max_pending_chunk_items: 30,
                max_chunks_per_item: 500,
                max_preheader_chunks_per_item: 64,
                max_preheader_data_path_bytes: 64 * 1024,
                max_invalid_items: 10_000,
                max_valid_items: 10_000,
                max_valid_chunks: 10_000,
                max_valid_submit_txs: 3000,
                max_valid_commitment_addresses: 300,
                max_commitments_per_address: 20,
                max_concurrent_mempool_tasks: 30,
                max_concurrent_chunk_ingress_tasks: 30,
                max_control_plane_concurrent_tasks: 4,
                chunk_writer_buffer_size: 4096,
                max_http_chunk_admission: 256,
                max_http_chunk_waiters: 32,
                http_chunk_admission_timeout_millis: 5000,
            },
            metrics: MetricsConfig::default(),

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: default_parallel_verification_thread_limit(),
                core_pinning: CorePinning::Auto,
                free_run: VdfFreeRun::Auto,
                throttle: false,
                progress_timeout_secs: default_vdf_progress_timeout_secs(),
                validation_batch_size: default_vdf_validation_batch_size(),
            },

            p2p_handshake: P2PHandshakeConfig::default(),
            p2p_gossip: P2PGossipConfig::default(),
            p2p_pull: P2PPullConfig::default(),
            max_peer_response_bytes: DEFAULT_MAX_PEER_RESPONSE_BYTES,

            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,

            sync: SyncConfig::default(),
            run_mode: RunMode::Production,
            database: DatabaseConfig::default(),
        }
    }

    /// get the storage module directory path
    pub fn storage_module_dir(&self) -> PathBuf {
        self.base_directory.join("storage_modules")
    }
    /// get the irys consensus data directory path
    pub fn irys_consensus_data_dir(&self) -> PathBuf {
        self.base_directory.join("irys_consensus_data")
    }

    /// get the irys mempool persistence path
    pub fn mempool_dir(&self) -> PathBuf {
        self.base_directory.join("mempool")
    }
    /// get the reth data directory path
    pub fn reth_data_dir(&self) -> PathBuf {
        self.base_directory.join("reth")
    }
    /// get the reth log directory path
    pub fn reth_log_dir(&self) -> PathBuf {
        self.reth_data_dir().join("logs")
    }
    /// get the `block_index` directory path
    pub fn block_index_dir(&self) -> PathBuf {
        self.base_directory.join("block_index")
    }

    /// get the peer info directory path
    pub fn peer_info_dir(&self) -> PathBuf {
        self.base_directory.join("peer_info")
    }

    /// Get the PeerAddress for this node configuration
    pub fn peer_address(&self) -> PeerAddress {
        PeerAddress {
            api: format!(
                "{}:{}",
                self.http.public_ip(&self.network_defaults),
                self.http.public_port
            )
            .parse()
            .expect("valid SocketAddr expected"),
            gossip: format!(
                "{}:{}",
                self.gossip.public_ip(&self.network_defaults),
                self.gossip.public_port
            )
            .parse()
            .expect("valid SocketAddr expected"),
            execution: RethPeerInfo {
                peering_tcp_addr: format!(
                    "{}:{}",
                    self.reth.network.public_ip(&self.network_defaults),
                    self.reth.network.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                peer_id: self.reth.network.peer_id,
            },
        }
    }

    /// Check if the node should only interact with trusted peers
    pub fn is_trusted_peers_only(&self) -> bool {
        matches!(self.peer_filter_mode, PeerFilterMode::TrustedOnly)
    }

    /// Check if the node should interact with trusted peers and their handshake peers
    pub fn is_trusted_and_handshake_mode(&self) -> bool {
        matches!(self.peer_filter_mode, PeerFilterMode::TrustedAndHandshake)
    }

    /// Check if the node has peer filtering enabled (not unrestricted)
    pub fn has_peer_filtering(&self) -> bool {
        !matches!(self.peer_filter_mode, PeerFilterMode::Unrestricted)
    }

    pub fn vdf(&self) -> VdfConfig {
        self.into()
    }

    pub fn mempool(&self) -> MempoolConfig {
        self.into()
    }
}

fn default_irys_path() -> PathBuf {
    env::current_dir()
        .expect("Unable to determine working dir, aborting")
        .join(".irys")
}

#[cfg(test)]
mod run_mode_tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn omitted_intervals_persist_secs_uses_the_node_default() {
        let parsed: StorageSyncConfig = toml::from_str("num_writes_before_sync = 7").unwrap();
        assert_eq!(parsed.num_writes_before_sync, 7);
        assert_eq!(
            parsed.intervals_persist_secs,
            DEFAULT_INTERVALS_PERSIST_SECS
        );
        let immediate: StorageSyncConfig = toml::from_str("intervals_persist_secs = 0").unwrap();
        assert_eq!(immediate.intervals_persist_secs, 0);
        assert_eq!(parsed.index_wal_backstop_ms, DEFAULT_INDEX_WAL_BACKSTOP_MS);
        let backstop: StorageSyncConfig = toml::from_str("index_wal_backstop_ms = 0").unwrap();
        assert_eq!(backstop.index_wal_backstop_ms, 0);
        assert_eq!(
            parsed.tx_path_derive_threads,
            DEFAULT_TX_PATH_DERIVE_THREADS
        );
        let threads: StorageSyncConfig = toml::from_str("tx_path_derive_threads = 4").unwrap();
        assert_eq!(threads.tx_path_derive_threads, 4);
        assert_eq!(
            parsed.min_durable_interval_ms,
            DEFAULT_MIN_DURABLE_INTERVAL_MS
        );
        assert_eq!(
            parsed.durable_headroom_permille,
            DEFAULT_DURABLE_HEADROOM_PERMILLE
        );
        assert_eq!(NodeConfig::testing().storage.min_durable_interval_ms, 0);
        assert_eq!(
            NodeConfig::testnet().storage.min_durable_interval_ms,
            DEFAULT_MIN_DURABLE_INTERVAL_MS
        );
        // An explicit 0 still overrides the production default.
        let every_gap: StorageSyncConfig =
            toml::from_str("min_durable_interval_ms = 0\ndurable_headroom_permille = 400").unwrap();
        assert_eq!(every_gap.min_durable_interval_ms, 0);
        assert_eq!(every_gap.durable_headroom_permille, 400);
        // Absent key: O_DIRECT recall is the production default.
        assert!(parsed.recall_direct_io);
        let buffered: StorageSyncConfig = toml::from_str("recall_direct_io = false").unwrap();
        assert!(!buffered.recall_direct_io);
        assert!(StorageSyncConfig::default().recall_direct_io);
        assert!(NodeConfig::testnet().storage.recall_direct_io);
        assert!(!NodeConfig::testing().storage.recall_direct_io);
        // Absent key: the recall keeps the disk hold.
        assert!(parsed.recall_io_priority);
        let shared: StorageSyncConfig = toml::from_str("recall_io_priority = false").unwrap();
        assert!(!shared.recall_io_priority);
        assert!(StorageSyncConfig::default().recall_io_priority);
        assert!(NodeConfig::testnet().storage.recall_io_priority);
        assert!(NodeConfig::testing().storage.recall_io_priority);
    }

    #[test]
    fn run_mode_defaults_to_production() {
        assert_eq!(RunMode::default(), RunMode::Production);
        assert!(!RunMode::default().is_test());
        assert!(RunMode::Test.is_test());
    }

    #[rstest]
    #[case(DbSyncMode::Durable)]
    #[case(DbSyncMode::SafeNoSync)]
    #[case(DbSyncMode::UtterlyNoSync)]
    fn db_sync_mode_serde_round_trip(#[case] mode: DbSyncMode) {
        let json = serde_json::to_string(&mode).unwrap();
        let deserialized: DbSyncMode = serde_json::from_str(&json).unwrap();
        assert_eq!(mode, deserialized);
    }

    #[rstest]
    #[case(CorePinning::Disabled)]
    #[case(CorePinning::Auto)]
    #[case(CorePinning::Core(42))]
    fn core_pinning_serde_round_trip(#[case] pinning: CorePinning) {
        let json = serde_json::to_string(&pinning).unwrap();
        let deserialized: CorePinning = serde_json::from_str(&json).unwrap();
        assert_eq!(pinning, deserialized);
    }

    /// The runtime `VdfConfig` merge must carry `core_pinning` through from the
    /// node source struct — it was previously dropped (read separately in chain.rs).
    #[rstest]
    #[case(CorePinning::Disabled)]
    #[case(CorePinning::Auto)]
    #[case(CorePinning::Core(7))]
    fn vdf_config_merge_propagates_core_pinning(#[case] pinning: CorePinning) {
        let mut node_config = NodeConfig::testing();
        node_config.vdf.core_pinning = pinning;
        let vdf = node_config.vdf();
        assert_eq!(vdf.core_pinning, pinning);
    }

    /// The reservation keeps two cores back on any host wide enough to spare
    /// them, and never yields a zero-thread pool on a narrow one.
    #[rstest]
    #[case::single_core(1, 1)]
    #[case::two_cores(2, 1)]
    #[case::three_cores(3, 1)]
    #[case::eight_cores(8, 6)]
    #[case::twenty_two_cores(22, 20)]
    #[case::ninety_six_cores(96, 94)]
    fn verification_threads_reserve_two_cores_with_a_floor_of_one(
        #[case] available: usize,
        #[case] expected: usize,
    ) {
        assert_eq!(verification_threads_for(available), expected);
    }

    /// A zero core count cannot come from `available_parallelism()`, but the
    /// floor must hold for it regardless — a zero-thread rayon pool panics at
    /// build time, which would turn a bad measurement into a dead node.
    #[test]
    fn verification_threads_never_returns_zero() {
        assert_eq!(verification_threads_for(0), 1);
    }

    /// `[vdf]` without the key must parse and take the derived default — the
    /// commented-out line in the shipped templates depends on this.
    #[test]
    fn vdf_thread_limit_is_derived_when_the_key_is_absent() {
        #[derive(Deserialize)]
        struct VdfDoc {
            vdf: VdfNodeConfig,
        }

        let cfg: VdfDoc = toml::from_str("[vdf]\n").expect("[vdf] without the key must parse");
        assert_eq!(
            cfg.vdf.parallel_verification_thread_limit,
            default_parallel_verification_thread_limit()
        );
    }

    /// An explicit value still wins: this is a performance knob an operator who
    /// knows their host must be able to pin.
    #[test]
    fn vdf_thread_limit_honours_an_explicit_value() {
        #[derive(Deserialize)]
        struct VdfDoc {
            vdf: VdfNodeConfig,
        }

        let cfg: VdfDoc = toml::from_str("[vdf]\nparallel_verification_thread_limit = 3\n")
            .expect("explicit value must parse");
        assert_eq!(cfg.vdf.parallel_verification_thread_limit, 3);
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete the submodule_index assertion in this test when the field is removed. The other assertions stay.
    fn database_config_defaults() {
        let config = DatabaseConfig::default();
        assert_eq!(config.sync_mode, DbSyncMode::Durable);
        assert_eq!(config.cache_sync_mode, DbSyncMode::SafeNoSync);
        assert_eq!(config.submodule_index, SubmoduleIndexEngine::Mdbx);
        assert_eq!(
            config.rocksdb_block_cache_bytes,
            DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
        );
        assert_eq!(
            config.rocksdb_write_buffer_bytes,
            DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES
        );
        assert_eq!(config.rocksdb_flush_threads, DEFAULT_ROCKSDB_FLUSH_THREADS);
        assert_eq!(
            config.rocksdb_compaction_threads,
            DEFAULT_ROCKSDB_COMPACTION_THREADS
        );
        assert_eq!(
            config.rocksdb_rate_limit_bytes_per_sec,
            DEFAULT_ROCKSDB_RATE_LIMIT_BYTES_PER_SEC
        );
        assert_eq!(DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES, 1024 * 1024 * 1024);
        assert_eq!(DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES, 512 * 1024 * 1024);
        assert_eq!(DEFAULT_ROCKSDB_FLUSH_THREADS, 2);
        assert_eq!(DEFAULT_ROCKSDB_COMPACTION_THREADS, 4);
        assert_eq!(DEFAULT_ROCKSDB_RATE_LIMIT_BYTES_PER_SEC, 0);

        let omitted: DatabaseConfig = toml::from_str("").expect("empty toml");
        assert_eq!(omitted, DatabaseConfig::default());
        let omitted_json: DatabaseConfig = serde_json::from_str("{}").expect("empty json");
        assert_eq!(omitted_json, DatabaseConfig::default());

        let tuned: DatabaseConfig = toml::from_str(
            "
            rocksdb_block_cache_bytes = 8388608
            rocksdb_write_buffer_bytes = 4194304
            rocksdb_flush_threads = 1
            rocksdb_compaction_threads = 1
            rocksdb_rate_limit_bytes_per_sec = 1048576
            ",
        )
        .expect("rocks resource toml");
        assert_eq!(tuned.rocksdb_block_cache_bytes, 8 * 1024 * 1024);
        assert_eq!(tuned.rocksdb_write_buffer_bytes, 4 * 1024 * 1024);
        assert_eq!(tuned.rocksdb_flush_threads, 1);
        assert_eq!(tuned.rocksdb_compaction_threads, 1);
        assert_eq!(tuned.rocksdb_rate_limit_bytes_per_sec, 1024 * 1024);
        assert_eq!(tuned.sync_mode, DbSyncMode::Durable);
        assert_eq!(tuned.submodule_index, SubmoduleIndexEngine::Mdbx);
    }

    /// A config with no `submodule_index` stays on MDBX. `rocksdb` selects the
    /// sibling engine. Any other string is rejected.
    #[test]
    // MDBX-SUBMODULE-INDEX: rewrite this test when the key is removed. It pins an omitted key to MDBX.
    fn submodule_index_engine_serde() {
        let missing_toml: DatabaseConfig = toml::from_str(
            r#"
            sync_mode = "Durable"
            cache_sync_mode = "SafeNoSync"
            "#,
        )
        .expect("toml without submodule_index");
        assert_eq!(missing_toml.submodule_index, SubmoduleIndexEngine::Mdbx);

        let missing_json: DatabaseConfig =
            serde_json::from_str("{}").expect("json without submodule_index");
        assert_eq!(missing_json.submodule_index, SubmoduleIndexEngine::Mdbx);

        let mdbx: DatabaseConfig = toml::from_str("submodule_index = \"mdbx\"").expect("mdbx toml");
        assert_eq!(mdbx.submodule_index, SubmoduleIndexEngine::Mdbx);

        let rocks_toml: DatabaseConfig =
            toml::from_str("submodule_index = \"rocksdb\"").expect("rocksdb toml");
        assert_eq!(rocks_toml.submodule_index, SubmoduleIndexEngine::Rocksdb);

        let rocks_json: DatabaseConfig =
            serde_json::from_str(r#"{"submodule_index":"rocksdb"}"#).expect("rocksdb json");
        assert_eq!(rocks_json.submodule_index, SubmoduleIndexEngine::Rocksdb);

        assert!(
            toml::from_str::<DatabaseConfig>("submodule_index = \"lmdb\"").is_err(),
            "unknown toml engine must fail"
        );
        assert!(
            serde_json::from_str::<DatabaseConfig>(r#"{"submodule_index":"other"}"#).is_err(),
            "unknown json engine must fail"
        );

        assert_eq!(
            NodeConfig::testing().database.submodule_index,
            SubmoduleIndexEngine::Mdbx
        );
    }

    /// Regression test: a legacy TOML config that predates the `run_mode`,
    /// `database`, `reth.db_sync_mode`, and `vdf.core_pinning` fields must
    /// still deserialize successfully, with every omitted field receiving its
    /// intended default value.
    #[test]
    fn legacy_config_deserialization_regression() {
        // Minimal legacy TOML — every new field is intentionally absent.
        let legacy_toml = r#"
            node_mode = "Genesis"
            sync_mode = "Full"
            consensus = "Testing"
            mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
            reward_address = "0x64f1a2829e0e698c18e7792d6e74f67d89aa0a32"

            [[trusted_peers]]
            gossip = "127.0.0.1:8081"
            api = "127.0.0.1:8080"
            [trusted_peers.execution]
            peering_tcp_addr = "127.0.0.1:30303"
            peer_id = "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"

            [gossip]
            bind_port = 0
            public_port = 0

            [http]
            bind_port = 0
            public_port = 0

            # Legacy reth section — no db_sync_mode field
            [reth.network]
            bind_port = 0
            public_port = 0

            # Legacy vdf section — no core_pinning field
            [vdf]
            parallel_verification_thread_limit = 4
        "#;

        let config: NodeConfig = toml::from_str(legacy_toml).expect("legacy TOML must deserialize");

        // run_mode defaults to Production
        assert_eq!(config.run_mode, RunMode::Production);
        assert_eq!(config.run_mode, RunMode::default());

        // database defaults
        assert_eq!(config.database, DatabaseConfig::default());
        assert_eq!(config.database.sync_mode, DbSyncMode::Durable);
        assert_eq!(config.database.cache_sync_mode, DbSyncMode::SafeNoSync);

        // reth.db_sync_mode defaults to Durable
        assert_eq!(config.reth.db_sync_mode, DbSyncMode::Durable);

        // vdf.core_pinning defaults to Auto
        assert_eq!(config.vdf.core_pinning, CorePinning::Auto);
        assert_eq!(config.vdf.core_pinning, CorePinning::default());
    }

    #[test]
    fn testing_node_config_overrides_sync_min_active_peers() {
        let cfg = super::NodeConfig::testing();
        assert_eq!(cfg.sync.min_active_peers, 1, "testing override expected");
        assert_eq!(
            cfg.sync.peer_wait_timeout_millis, 10_000,
            "testing override: matches genesis_peer_discovery_timeout_millis to give handshake the same headroom"
        );
    }

    #[test]
    fn sync_config_defaults_match_spec() {
        let cfg = super::SyncConfig::default();
        assert_eq!(cfg.min_active_peers, 3, "production default expected");
        assert_eq!(cfg.peer_wait_timeout_millis, 20_000);
    }

    #[test]
    fn test_config_uses_short_payload_wait_timeout() {
        let cfg = super::NodeConfig::testing();
        assert_eq!(
            cfg.sync.execution_payload_wait_timeout_millis, 5_000,
            "test config must use the short 5s payload wait so integration tests don't hang 60s on the prod default"
        );
    }

    #[test]
    fn block_body_serve_cap_defaults_to_eight() {
        assert_eq!(
            P2PGossipConfig::default().max_concurrent_block_body_serves,
            8
        );
    }

    #[test]
    fn block_body_serve_cap_is_eight_when_the_key_is_absent() {
        #[derive(Deserialize)]
        struct Doc {
            p2p_gossip: P2PGossipConfig,
        }

        let cfg: Doc =
            toml::from_str("[p2p_gossip]\n").expect("[p2p_gossip] without the key must parse");
        assert_eq!(cfg.p2p_gossip.max_concurrent_block_body_serves, 8);
    }

    #[test]
    fn block_body_serve_cap_honours_an_explicit_zero() {
        #[derive(Deserialize)]
        struct Doc {
            p2p_gossip: P2PGossipConfig,
        }

        let cfg: Doc = toml::from_str("[p2p_gossip]\nmax_concurrent_block_body_serves = 0\n")
            .expect("explicit zero must parse");
        assert_eq!(cfg.p2p_gossip.max_concurrent_block_body_serves, 0);
    }
}

#[cfg(test)]
mod node_mode_tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn mines_is_true_for_genesis_and_miner_only() {
        assert!(NodeMode::Genesis.mines());
        assert!(NodeMode::Miner.mines());
        assert!(!NodeMode::Observer.mines());
    }

    #[test]
    fn joins_existing_network_excludes_genesis() {
        assert!(!NodeMode::Genesis.joins_existing_network());
        assert!(NodeMode::Miner.joins_existing_network());
        assert!(NodeMode::Observer.joins_existing_network());
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct ModeDoc {
        node_mode: NodeMode,
    }

    #[test]
    fn deserialize_peer_reports_the_rename() {
        let err = toml::from_str::<ModeDoc>("node_mode = \"Peer\"")
            .expect_err("\"Peer\" must not deserialize")
            .to_string();
        assert!(
            err.contains("no longer exists"),
            "error must explain that Peer was retired, not just list variants: {err}"
        );
        assert!(err.contains("Miner"), "error must name Miner: {err}");
        assert!(err.contains("Observer"), "error must name Observer: {err}");
    }

    #[test]
    fn deserialize_unknown_mode_lists_the_variants() {
        let err = toml::from_str::<ModeDoc>("node_mode = \"Nonsense\"")
            .expect_err("unknown variants must not deserialize")
            .to_string();
        assert!(err.contains("Genesis"), "error must list Genesis: {err}");
        assert!(err.contains("Miner"), "error must list Miner: {err}");
        assert!(err.contains("Observer"), "error must list Observer: {err}");
    }

    #[rstest]
    #[case(NodeMode::Genesis)]
    #[case(NodeMode::Miner)]
    #[case(NodeMode::Observer)]
    fn serde_round_trip(#[case] mode: NodeMode) {
        let doc = ModeDoc { node_mode: mode };
        let encoded = toml::to_string(&doc).expect("serializes");
        let decoded: ModeDoc = toml::from_str(&encoded).expect("deserializes");
        assert_eq!(doc, decoded);
    }
}
