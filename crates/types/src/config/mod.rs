use eyre::ensure;
use serde::{Deserialize, Serialize};
use std::{ops::Deref, sync::Arc};

pub mod consensus;
pub mod node;
pub use consensus::*;
pub use node::*;

use crate::irys::IrysSigner;

/// Ergonomic and cheaply copyable Configuration that has the consensus and user-defined configs extracted out
#[derive(Debug, Clone)]
pub struct Config(Arc<CombinedConfigInner>);

impl Config {
    pub fn new(node_config: NodeConfig) -> Self {
        let consensus = node_config.consensus_config();
        Self(Arc::new(CombinedConfigInner {
            consensus,
            mempool: node_config.mempool(),
            vdf: node_config.vdf(),
            node_config,
        }))
    }

    pub fn irys_signer(&self) -> IrysSigner {
        IrysSigner {
            signer: self.node_config.mining_key.clone(),
            chain_id: self.consensus.chain_id,
            chunk_size: self.consensus.chunk_size,
        }
    }

    // validate configuration invariants
    // TODO: expand this!
    pub fn validate(&self) -> eyre::Result<()> {
        // ensures the block tree is able to contain all unmigrated blocks
        ensure!((self.consensus.block_migration_depth as u64) <= self.consensus.block_tree_depth);

        // ensure that txs aren't removed from the mempool due to expired anchors before a block migrates
        // TODO: once anchor maturity is enforced, apply that value here
        ensure!(
            std::convert::TryInto::<u8>::try_into(self.consensus.block_migration_depth)?
                <= (self.consensus.mempool.anchor_expiry_depth + 4)
        );

        Ok(())
    }
}

impl Deref for Config {
    type Target = CombinedConfigInner;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl From<NodeConfig> for Config {
    fn from(val: NodeConfig) -> Self {
        Self::new(val)
    }
}

#[derive(Debug)]
pub struct CombinedConfigInner {
    pub consensus: ConsensusConfig,
    pub node_config: NodeConfig,
    // composite configs - here to amortize the creation cost
    pub vdf: VdfConfig,
    pub mempool: MempoolConfig,
}

impl From<&NodeConfig> for VdfConfig {
    fn from(value: &NodeConfig) -> Self {
        let consensus = value.consensus_config().vdf;
        Self {
            parallel_verification_thread_limit: value.vdf.parallel_verification_thread_limit,
            reset_frequency: consensus.reset_frequency,
            num_checkpoints_in_vdf_step: consensus.num_checkpoints_in_vdf_step,
            max_allowed_vdf_fork_steps: consensus.max_allowed_vdf_fork_steps,
            sha_1s_difficulty: consensus.sha_1s_difficulty,
        }
    }
}

impl From<&NodeConfig> for MempoolConfig {
    fn from(value: &NodeConfig) -> Self {
        let consensus = value.consensus_config().mempool;
        Self {
            max_pending_pledge_items: value.mempool.max_pending_pledge_items,
            max_pledges_per_item: value.mempool.max_pledges_per_item,
            max_pending_chunk_items: value.mempool.max_pending_chunk_items,
            max_chunks_per_item: value.mempool.max_chunks_per_item,
            max_preheader_chunks_per_item: value.mempool.max_preheader_chunks_per_item,
            max_preheader_data_path_bytes: value.mempool.max_preheader_data_path_bytes,
            max_valid_items: value.mempool.max_valid_items,
            max_invalid_items: value.mempool.max_invalid_items,
            max_valid_chunks: value.mempool.max_valid_chunks,
            // consensus
            max_data_txs_per_block: consensus.max_data_txs_per_block,
            max_commitment_txs_per_block: consensus.max_commitment_txs_per_block,
            anchor_expiry_depth: consensus.anchor_expiry_depth,
            commitment_fee: consensus.commitment_fee,
        }
    }
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VdfConfig {
    /// VDF reset frequency in global steps
    /// Formula: blocks_between_resets × vdf_steps_per_block
    /// Example: 50 blocks × 12 steps = 600 global steps
    /// At 12s/block target, resets occur every ~10 minutes
    pub reset_frequency: usize,

    /// Maximum number of threads to use for parallel VDF verification
    pub parallel_verification_thread_limit: usize,

    /// Number of checkpoints to include in each VDF step
    pub num_checkpoints_in_vdf_step: usize,

    /// Minimum number of steps to store in FIFO VecDeque to allow for network forks
    pub max_allowed_vdf_fork_steps: u64,

    /// Target number of SHA-1 operations per second for VDF calibration
    pub sha_1s_difficulty: u64,
}

impl VdfConfig {
    /// Returns the number of iterations per checkpoint,
    /// computed as the floor of (step difficulty ÷ number of checkpoints in a step).
    pub fn num_iterations_per_checkpoint(&self) -> u64 {
        self.sha_1s_difficulty / self.num_checkpoints_in_vdf_step as u64
    }
}

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MempoolConfig {
    /// Maximum number of data transactions that can be included in a single block
    pub max_data_txs_per_block: u64,

    /// Maximum number of commitment transactions allowed in a single block
    pub max_commitment_txs_per_block: u64,

    /// The number of blocks a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub anchor_expiry_depth: u8,

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
    #[serde(default)]
    pub max_preheader_chunks_per_item: usize,

    /// Maximum allowed pre-header data_path bytes for chunk proofs
    /// Mitigates DoS on speculative chunk storage before header arrival
    #[serde(default)]
    pub max_preheader_data_path_bytes: usize,

    /// Maximum number of valid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_valid_items: usize,

    /// Maximum number of invalid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_invalid_items: usize,

    /// Fee required for commitment transactions (stake, unstake, pledge, unpledge)
    pub commitment_fee: u64,

    /// Maximum number of valid chunk hashes to keep track of
    /// Prevents re-processing and re-gossipping of recently seen chunks
    pub max_valid_chunks: usize,
}

pub mod serde_utils {

    use std::time::Duration;

    use rust_decimal::Decimal;
    use serde::{Deserialize as _, Deserializer, Serializer};

    use crate::storage_pricing::Amount;

    /// deserialize the token amount from a float.
    /// The float is expected to be in a format of 1.42.
    pub fn token_amount<'de, T: std::fmt::Debug, D>(deserializer: D) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        amount_from_float(deserializer, |dec| Amount::<T>::token(dec))
    }

    /// deserialize the percentage amount from a string.
    ///
    /// The string is expected to be:
    /// - 0.1 (10%)
    /// - 1.0 (100%)
    pub fn percentage_amount<'de, T: std::fmt::Debug, D>(
        deserializer: D,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        amount_from_float(deserializer, |dec| Amount::<T>::percentage(dec))
    }

    fn amount_from_float<'de, T: std::fmt::Debug, D>(
        deserializer: D,
        dec_to_amount: impl Fn(Decimal) -> eyre::Result<Amount<T>>,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw_float = f64::deserialize(deserializer)?;
        let decimal = Decimal::try_from(raw_float).map_err(serde::de::Error::custom)?;
        let amount = dec_to_amount(decimal).map_err(serde::de::Error::custom)?;
        Ok(amount)
    }

    /// Deserialize a secp256k1 private key from a hex encoded string slice
    pub fn signing_key_from_hex<'de, D>(
        deserializer: D,
    ) -> Result<k256::ecdsa::SigningKey, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = String::deserialize(deserializer)?;
        let decoded = hex::decode(bytes.as_bytes()).map_err(serde::de::Error::custom)?;
        let key =
            k256::ecdsa::SigningKey::from_slice(&decoded).map_err(serde::de::Error::custom)?;
        Ok(key)
    }

    pub fn serializes_signing_key<S>(
        key: &k256::ecdsa::SigningKey,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Convert to bytes and then hex-encode
        let key_bytes = key.to_bytes();
        let hex_string = hex::encode(key_bytes);
        serializer.serialize_str(&hex_string)
    }

    pub fn serializes_token_amount<S, T>(
        amount: &Amount<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: std::fmt::Debug,
    {
        // Convert to bytes and then hex-encode
        let decimal = amount
            .token_to_decimal()
            .map_err(serde::ser::Error::custom)?;
        let float: f64 = decimal
            .try_into()
            .expect("decimal to be convertible to a f64");
        serializer.serialize_f64(float)
    }

    pub fn serializes_percentage_amount<S, T>(
        amount: &Amount<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: std::fmt::Debug,
    {
        // Convert to bytes and then hex-encode
        let decimal = amount
            .percentage_to_decimal()
            .map_err(serde::ser::Error::custom)?;
        let float: f64 = decimal
            .try_into()
            .expect("decimal to be convertible to a f64");
        serializer.serialize_f64(float)
    }

    /// Deserialize a timestamp drift value (stored as `u64` in TOML) into a `u128`
    pub fn u128_millis_from_u64<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = u64::deserialize(deserializer)?;
        Ok(val as u128)
    }

    /// Serialize a `u128` timestamp drift value as a `u64` so it can be encoded by `toml`
    /// As this stores time and only 15 seconds, the 128 bit -> 64bit conversion is not a concern
    pub fn u128_millis_to_u64<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // this type conversion is safe as time stores a value of 15 seconds, and not millions of years
        serializer.serialize_u64(*value as u64)
    }

    pub fn duration_from_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }

    pub fn duration_from_millis<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }

    pub fn duration_from_string<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_duration_string(&s).map_err(serde::de::Error::custom)
    }

    fn parse_duration_string(s: &str) -> Result<Duration, String> {
        if let Some(secs_str) = s.strip_suffix('s') {
            let secs: u64 = secs_str
                .parse()
                .map_err(|_| format!("Invalid duration number: {}", secs_str))?;
            Ok(Duration::from_secs(secs))
        } else if let Some(millis_str) = s.strip_suffix("ms") {
            let millis: u64 = millis_str
                .parse()
                .map_err(|_| format!("Invalid duration number: {}", millis_str))?;
            Ok(Duration::from_millis(millis))
        } else {
            Err(format!("Duration must end with 's' or 'ms': {}", s))
        }
    }

    pub fn serialize_duration_secs<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn serialize_duration_string<S>(
        duration: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = format!("{}s", duration.as_secs());
        serializer.serialize_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{PeerAddress, RethPeerInfo};

    use super::*;
    use pretty_assertions::assert_eq;
    use toml;

    #[test]
    fn test_deserialize_consensus_config_from_toml() {
        let toml_data = r#"
        chain_id = 1270
        token_price_safe_range = 1.0
        genesis_price = 1.0
        annual_cost_per_gb = 0.01
        decay_rate = 0.01
        chunk_size = 32
        block_migration_depth = 6
        block_tree_depth = 50
        num_chunks_in_partition = 10
        num_chunks_in_recall_range = 2
        num_partitions_per_slot = 1
        entropy_packing_iterations = 1000
        number_of_ingress_proofs_total = 1
        number_of_ingress_proofs_from_assignees = 0
        safe_minimum_number_of_years = 200
        stake_value = 20000.0
        pledge_base_value = 950.0
        pledge_decay = 0.9
        immediate_tx_inclusion_reward_percent = 0.05
        minimum_term_fee_usd = 0.01

        [genesis]
        miner_address = "0x0000000000000000000000000000000000000000"
        reward_address = "0x0000000000000000000000000000000000000000"
        last_epoch_hash = "11111111111111111111111111111111"
        vdf_seed = "11111111111111111111111111111111"
        # Optional: if omitted, defaults to vdf_seed
        # vdf_next_seed = "22222222222222222222222222222222"
        timestamp_millis = 0

        [reth]
        chain = 1270

        [reth.genesis]
        nonce = "0x0"
        timestamp = "0x0"
        extraData = "0x"
        gasLimit = "0x1c9c380"
        difficulty = "0x0"
        mixHash = "0x0000000000000000000000000000000000000000000000000000000000000000"
        coinbase = "0x0000000000000000000000000000000000000000"

        [reth.genesis.config]
        chainId = 1
        daoForkSupport = false
        terminalTotalDifficultyPassed = false

        [reth.genesis.alloc.0x64f1a2829e0e698c18e7792d6e74f67d89aa0a32]
        balance = "0x152cf4e72a974f1c0000"

        [reth.genesis.alloc.0xa93225cbf141438629f1bd906a31a1c5401ce924]
        balance = "0x152cf4e72a974f1c0000"

        [mempool]
        max_data_txs_per_block = 100
        max_commitment_txs_per_block = 100
        anchor_expiry_depth = 20
        commitment_fee = 100



        [difficulty_adjustment]
        block_time = 1
        difficulty_adjustment_interval = 1209600000
        max_difficulty_adjustment_factor = 4
        min_difficulty_adjustment_factor = 0.25

        [vdf]
        reset_frequency = 600
        max_allowed_vdf_fork_steps = 60000
        num_checkpoints_in_vdf_step = 25
        sha_1s_difficulty = 70000

        [block_reward_config]
        inflation_cap = 100000000
        half_life_secs = 126144000

        [epoch]
        capacity_scalar = 100
        num_blocks_in_epoch = 100
        submit_ledger_epoch_length = 5

        [ema]
        price_adjustment_interval = 10
        "#;

        // Create the expected config
        let expected_config = ConsensusConfig::testing();
        let expected_toml_data = toml::to_string(&expected_config).unwrap();
        // for debugging purposes
        println!("{}", expected_toml_data);

        // Deserialize the TOML string into a ConsensusConfig
        let config = toml::from_str::<ConsensusConfig>(toml_data)
            .expect("Failed to deserialize ConsensusConfig from TOML");

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }

    #[test]
    fn test_deserialize_config_from_toml() {
        let toml_data = r#"
        node_mode = "Genesis"
        sync_mode = "Full"
        base_directory = "~/.tmp/.irys"
        consensus = "Testing"
        mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
        reward_address = "0x64f1a2829e0e698c18e7792d6e74f67d89aa0a32"
        peer_filter_mode = "trusted_and_handshake"
        genesis_peer_discovery_timeout_millis = 10000
        stake_pledge_drives = false
        initial_whitelist = ["127.0.0.1:8080"]

        [[trusted_peers]]
        gossip = "127.0.0.1:8081"
        api = "127.0.0.1:8080"

        [trusted_peers.execution]
        peering_tcp_addr = "127.0.0.1:30303"
        peer_id = "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"

        [oracle]
        type = "mock"
        initial_price = 1.0
        incremental_change = 0.00000000000001
        smoothing_interval = 15

        [storage]
        num_writes_before_sync = 1

        [data_sync]
        max_pending_chunk_requests = 1000
        max_storage_throughput_bps = 209715200
        bandwidth_adjustment_interval = "5s"
        chunk_request_timeout = "10s"


        [gossip]
        bind_ip = "127.0.0.1"
        bind_port = 0
        public_ip = "127.0.0.1"
        public_port = 0

        [packing]
        cpu_packing_concurrency = 4
        gpu_packing_batch_size = 1024

        [cache]
        cache_clean_lag = 2

        [http]
        bind_ip = "127.0.0.1"
        bind_port = 0
        public_ip = "127.0.0.1"
        public_port = 0

        [reth.network]
        use_random_ports = true
        bind_ip = "0.0.0.0"
        bind_port = 0
        public_ip = "0.0.0.0"
        public_port = 0

        [vdf]
        parallel_verification_thread_limit = 4

        [mempool]
        max_pending_pledge_items = 100
        max_pledges_per_item = 100
        max_pending_chunk_items = 30
        max_chunks_per_item = 500
        max_preheader_chunks_per_item= 64
        max_preheader_data_path_bytes= 65536
        max_invalid_items = 10_000
        max_valid_items = 10_000
        max_valid_chunks = 10000
        "#;

        // Create the expected config
        let mut expected_config = NodeConfig::testing();
        expected_config.consensus = ConsensusOptions::Testing;
        expected_config.base_directory = PathBuf::from("~/.tmp/.irys");
        expected_config.peer_filter_mode = PeerFilterMode::TrustedAndHandshake;
        expected_config.trusted_peers = vec![PeerAddress {
            api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo {
                peering_tcp_addr: "127.0.0.1:30303".parse().unwrap(),
                peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap(),
            },
        }];
        expected_config.initial_whitelist = vec!["127.0.0.1:8080".parse().unwrap()];
        // for debugging purposes

        // let expected_toml_data = toml::to_string(&expected_config).unwrap();
        // println!("{}", expected_toml_data);

        // Deserialize the TOML string into a NodeConfig
        let config = toml::from_str::<NodeConfig>(toml_data)
            .expect("Failed to deserialize NodeConfig from TOML");

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }

    #[test]
    fn test_roundtrip_toml_serdes() {
        let cfg = NodeConfig::testing();
        let enc = toml::to_string_pretty(&cfg).unwrap();
        let dec: NodeConfig = toml::from_str(&enc).unwrap();
        assert_eq!(cfg, dec);
    }

    #[test]
    fn test_parse_testnet_config_template() {
        let template_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("config")
            .join("templates")
            .join("testnet_config.toml");

        println!("path: {:?}", template_path);

        let template_content = std::fs::read_to_string(&template_path)
            .expect("Failed to read testnet_config.toml template");

        let config = toml::from_str::<NodeConfig>(&template_content)
            .expect("Failed to parse testnet_config.toml template");

        // Basic sanity checks - just verify it parsed successfully
        assert_eq!(config.node_mode, NodeMode::Peer);

        // Check consensus config fields
        let consensus = config.consensus_config();
        assert_eq!(consensus.chain_id, 1270);
    }
}
