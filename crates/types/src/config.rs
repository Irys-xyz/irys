use crate::{
    irys::IrysSigner,
    storage_pricing::{
        phantoms::{CostPerGb, DecayRate, IrysPrice, Percentage, Usd},
        Amount,
    },
    PeerAddress,
};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::{env, net::SocketAddr, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusConfig {
    pub chain_id: u64,

    /// defines the range of how much can Oracle token price fluctuate between subsequent blocks
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub token_price_safe_range: Amount<Percentage>,

    /// defines the range of how much can Oracle token price fluctuate between subsequent blocks
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub genesis_price: Amount<(IrysPrice, Usd)>,

    /// The annual cost per storing a single GB of data on Irys
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub annual_cost_per_gb: Amount<(CostPerGb, Usd)>,

    /// A percentage value used in pricing calculations. Accounts for storage hardware getting cheaper as time goes on
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub decay_rate: Amount<DecayRate>,

    pub vdf: VdfConfig,
    pub chunk_size: u64,
    pub num_chunks_in_partition: u64,
    pub num_chunks_in_recall_range: u64,
    pub num_partitions_per_slot: u64,

    /// How many repliceas are needed for data to be considered as "upgraded to perm storage"
    pub number_of_ingerss_proofs: u64,

    /// The amount of years for a piece of data to be considered as permanent
    pub safe_minimum_number_of_years: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfig {
    pub mode: NodeMode,

    pub consensus_cfg: ConsensusCfg,

    pub difficulty_adjustment: DifficultyAdjustmentConfig,

    pub mempool: MempoolConfig,

    pub ema: EmaConfig,

    pub oracle: OracleConfig,

    #[serde(
        deserialize_with = "serde_utils::signing_key_from_hex",
        serialize_with = "serde_utils::serializes_signing_key"
    )]
    pub mining_key: k256::ecdsa::SigningKey,

    pub storage: StorageConfig,
    pub pricing: PricingConfig,
    pub gossip: GossipConfig,

    pub packing: PackingConfig,
    pub cache: CacheConfig,
    pub http: HttpConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeMode {
    Genesis,
    PeerSync {
        /// The initial list of peers to contact for block sync
        trusted_peers: Vec<PeerAddress>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsensusCfg {
    Path(PathBuf),
    Testnet,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingConfig {
    /// Used in priding calculations. Extra fee percentage toped up by nodes
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub fee_percentage: Amount<Percentage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum OracleConfig {
    Mock {
        #[serde(deserialize_with = "serde_utils::token_amount")]
        initial_price: Amount<(IrysPrice, Usd)>,
        #[serde(deserialize_with = "serde_utils::percentage_amount")]
        percent_change: Amount<Percentage>,
        smoothing_interval: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmaConfig {
    /// Defines how frequently the Irys EMA price should be adjusted
    pub price_adjustment_interval: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdfConfig {
    pub vdf_reset_frequency: usize,
    pub vdf_parallel_verification_thread_limit: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_sha_1s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochConfig {
    /// Scaling factor for the capacity projection curve
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_capacity_partitions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConfig {
    /// The base directory where to look for artifact data
    #[serde(default = "default_irys_path")]
    pub base_directory: PathBuf,
    pub chunk_migration_depth: u32,
    pub num_writes_before_sync: u64,
    pub entropy_packing_iterations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolConfig {
    pub max_data_txs_per_block: u64,
    /// the number of block a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub anchor_expiry_depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GossipConfig {
    /// The IP address of the gossip service
    pub gossip_service_bind_ip: SocketAddr,
    /// The port of the gossip service
    pub gossip_service_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackingConfig {
    /// number of packing threads
    pub cpu_packing_concurrency: u16,
    /// GPU kernel batch size
    pub gpu_packing_batch_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// number of blocks cache cleaning will lag behind block finalization
    pub cache_clean_lag: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpConfig {
    /// The port that the Node's HTTP server should listen on. Set to 0 for randomisation.
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifficultyAdjustmentConfig {
    /// Block time in seconds
    pub block_time: u64,
    pub difficulty_adjustment_interval: u64,
    pub max_difficulty_adjustment_factor: Decimal,
    pub min_difficulty_adjustment_factor: Decimal,
}

fn default_irys_path() -> PathBuf {
    env::current_dir()
        .expect("Unable to determine working dir, aborting")
        .join(".irys")
}

impl ConsensusConfig {
    pub fn testnet() -> Self {
        Self {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(), // 0.01$
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),    // 1%
            safe_minimum_number_of_years: 200,
            number_of_ingerss_proofs: 10,
            genesis_price: Amount::token(rust_decimal_macros::dec!(1)).expect("valid token amount"),
            token_price_safe_range: Amount::percentage(rust_decimal_macros::dec!(1))
                .expect("valid percentage"),
            vdf: VdfConfig {
                vdf_reset_frequency: 10 * 120,
                vdf_parallel_verification_thread_limit: 4,
                num_checkpoints_in_vdf_step: 25,
                vdf_sha_1s: 7_000,
            },
            chunk_size: 256 * 1024,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
        }
    }
}

impl NodeConfig {
    pub fn irys_signer(&self) -> IrysSigner {
        IrysSigner::from_config(&self)
    }

    pub fn miner_address(&self) -> Address {
        Address::from_private_key(&self.mining_key)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet() -> Self {
        use k256::ecdsa::SigningKey;
        use rust_decimal_macros::dec;

        const DEFAULT_BLOCK_TIME: u64 = 1;
        Self {
            mode: NodeMode::Genesis,
            consensus_cfg: ConsensusCfg::Testnet,
            difficulty_adjustment: DifficultyAdjustmentConfig {
                block_time: DEFAULT_BLOCK_TIME,
                difficulty_adjustment_interval: (24u64 * 60 * 60 * 1000)
                    .div_ceil(DEFAULT_BLOCK_TIME)
                    * 14,
                max_difficulty_adjustment_factor: dec!(4),
                min_difficulty_adjustment_factor: dec!(0.25),
            },
            mempool: MempoolConfig {
                max_data_txs_per_block: 100,
                anchor_expiry_depth: 10,
            },
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            oracle: OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                percent_change: Amount::percentage(dec!(0.01)).expect("valid percentage"),
                smoothing_interval: 15,
            },
            mining_key: SigningKey::from_slice(
                &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                    .expect("valid hex"),
            )
            .expect("valid key"),
            storage: StorageConfig {
                base_directory: default_irys_path(),
                chunk_migration_depth: 1,
                num_writes_before_sync: 1,
                entropy_packing_iterations: 1000,
            },
            pricing: PricingConfig {
                fee_percentage: Amount::percentage(dec!(0.01)).expect("valid percentage"),
            },
            gossip: GossipConfig {
                gossip_service_bind_ip: "127.0.0.1".parse().expect("valid IP address"),
                gossip_service_port: 0,
            },
            packing: PackingConfig {
                cpu_packing_concurrency: 4,
                gpu_packing_batch_size: 1024,
            },
            cache: CacheConfig { cache_clean_lag: 2 },
            http: HttpConfig { port: 0 },
        }
    }
}

pub mod serde_utils {

    use rust_decimal::Decimal;
    use serde::{Deserialize as _, Deserializer, Serializer};

    use crate::storage_pricing::Amount;

    /// deserialize the token amount from a string.
    /// The string is expected to be in a format of "1.42".
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
        let raw_string = f64::deserialize(deserializer)?;
        let decimal = Decimal::try_from(raw_string).map_err(serde::de::Error::custom)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;
    use rust_decimal_macros::dec;
    use toml;

    #[test]
    fn test_deserialize_consensus_config_from_toml() {
        let toml_data = r#"
            chain_id = 1270
            annual_cost_per_gb = 0.01
            decay_rate = 0.01
            safe_minimum_number_of_years = 200
            number_of_ingerss_proofs = 10
            genesis_price = 1.0
            token_price_safe_range = 1.0
            chunk_size = 262144
            num_chunks_in_partition = 10
            num_chunks_in_recall_range = 2
            num_partitions_per_slot = 1

            [vdf]
            vdf_reset_frequency = 1200
            vdf_parallel_verification_thread_limit = 4
            num_checkpoints_in_vdf_step = 25
            vdf_sha_1s = 7000
        "#;

        // Deserialize the TOML string into a ConsensusConfig
        let config: ConsensusConfig =
            toml::from_str(toml_data).expect("Failed to deserialize ConsensusConfig from TOML");

        // Create the expected config
        let expected_config = ConsensusConfig {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(),
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),
            safe_minimum_number_of_years: 200,
            number_of_ingerss_proofs: 10,
            genesis_price: Amount::token(dec!(1)).unwrap(),
            token_price_safe_range: Amount::percentage(dec!(1)).unwrap(),
            vdf: VdfConfig {
                vdf_reset_frequency: 1200,
                vdf_parallel_verification_thread_limit: 4,
                num_checkpoints_in_vdf_step: 25,
                vdf_sha_1s: 7000,
            },
            chunk_size: 262144,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
        };

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }

    #[test]
    fn test_deserialize_config_from_toml() {
        let toml_data = r#"
            mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"

            [mode]
            type = "genesis"

            [consensus_cfg]
            type = "testnet"

            [difficulty_adjustment]
            max_adjustment_factor = 4
            min_adjustment_factor = 0.25
            adjustment_interval = 100

            [mempool]
            max_data_txs_per_block = 20

            [ema]
            price_adjustment_interval = 10

            [oracle]
            type = "mock"
            initial_price = 1
            percent_change = 0.01
            smoothing_interval = 15

            [storage]
            base_directory = "~/.irys"
            chunk_size = 262144
            num_chunks_in_partition = 10
            num_chunks_in_recall_range = 2
            num_partitions_per_slot = 1
            number_of_ingerss_proofs = 10
            safe_minimum_number_of_years = 200

            [pricing]
            fee_percentage = 0.05

            [gossip]
            service_bind_ip = "127.0.0.1"
            service_port = 8081

            [packing]
            cpu_concurrency = 4
            gpu_batch_size = 1024

            [cache]
            clean_lag = 2

            [http]
            port = 8080

        "#;

        // Deserialize the TOML string into a NodeConfig
        let config: NodeConfig =
            toml::from_str(toml_data).expect("Failed to deserialize NodeConfig from TOML");

        // Create the expected config
        let expected_config = NodeConfig {
            mode: NodeMode::Genesis,
            consensus_cfg: ConsensusCfg::Testnet,
            difficulty_adjustment: DifficultyAdjustmentConfig {
                block_time: 1,
                difficulty_adjustment_interval: 100,
                max_difficulty_adjustment_factor: dec!(4),
                min_difficulty_adjustment_factor: dec!(0.25),
            },
            mempool: MempoolConfig {
                max_data_txs_per_block: 20,
                anchor_expiry_depth: 10,
            },
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            oracle: OracleConfig::Mock {
                initial_price: Amount::token(dec!(1.0)).unwrap(),
                percent_change: Amount::percentage(dec!(0.01)).unwrap(),
                smoothing_interval: 15,
            },
            mining_key: k256::ecdsa::SigningKey::from_slice(
                &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                    .expect("valid hex"),
            )
            .expect("valid private key"),
            storage: StorageConfig {
                base_directory: "~/.irys".into(),
                chunk_migration_depth: 1,
                num_writes_before_sync: 1,
                entropy_packing_iterations: 1000,
            },
            pricing: PricingConfig {
                fee_percentage: Amount::percentage(dec!(0.05)).unwrap(),
            },
            gossip: GossipConfig {
                gossip_service_bind_ip: "127.0.0.1".parse().unwrap(),
                gossip_service_port: 8081,
            },
            packing: PackingConfig {
                cpu_packing_concurrency: 4,
                gpu_packing_batch_size: 1024,
            },
            cache: CacheConfig { cache_clean_lag: 2 },
            http: HttpConfig { port: 8080 },
        };

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }
}
