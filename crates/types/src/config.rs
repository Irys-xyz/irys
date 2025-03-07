use crate::{
    irys::IrysSigner,
    storage_pricing::{
        phantoms::{IrysPrice, Percentage, Usd},
        Amount,
    },
};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

// $44.0 comes from the following doc:
// https://docs.google.com/spreadsheets/d/1VMcMgguVRBcUUXIRwlUY7FgSTicasvzg_OWJZqk8rrs/edit?gid=0#gid=0
pub(crate) const ANNUALIZED_COST_OF_OPERATING_16TB: Decimal = dec!(44.0);
pub(crate) const MINER_PERCENTAGE_FEE: Decimal = dec!(0.05);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Block time in seconds
    pub block_time: u64,
    pub max_data_txs_per_block: u64,
    pub difficulty_adjustment_interval: u64,
    pub max_difficulty_adjustment_factor: Decimal,
    pub min_difficulty_adjustment_factor: Decimal,
    pub chunk_size: u64,
    pub num_chunks_in_partition: u64,
    pub num_chunks_in_recall_range: u64,
    pub vdf_reset_frequency: usize,
    pub vdf_parallel_verification_thread_limit: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_sha_1s: u64,
    pub entropy_packing_iterations: u32,
    pub chain_id: u64,
    /// Scaling factor for the capacity projection curve
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_partitions_per_slot: u64,
    pub num_writes_before_sync: u64,
    /// If `true`, the ledger will be persisted on disk when the node restarts. Otherwise the
    /// entire state of the node will reset to genesis upon restart.
    pub reset_state_on_restart: bool,
    // Longest chain consensus
    /// Number of block confirmations required before considering data final.
    ///
    /// In Nakamoto consensus, finality is probabilistic based on chain depth:
    /// - 6 confirmations protects against attackers with <25% hashpower
    /// - 20 confirmations protects against attackers with <40% hashpower
    /// - No number of confirmations is secure against attackers with >50% hashpower
    pub chunk_migration_depth: u32,
    #[serde(
        deserialize_with = "serde_utils::signing_key_from_hex",
        serialize_with = "serde_utils::serializes_signing_key"
    )]
    pub mining_key: k256::ecdsa::SigningKey,
    // TODO: enable this after fixing option in toml
    pub num_capacity_partitions: Option<u64>,
    pub port: u16,
    /// the number of block a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub anchor_expiry_depth: u8,
    /// defines for how long the protocol should use the geneses token price for (expressed in epoch count)
    pub genesis_price_valid_for_n_epochs: u8,
    /// defines the genesis price of the $IRYS, expressed in $USD
    #[serde(deserialize_with = "serde_utils::token_amount")]
    pub genesis_token_price: Amount<(IrysPrice, Usd)>,
    /// defines the range of how much can the token fluctuate since the last EMA price for it to be accepted
    #[serde(deserialize_with = "serde_utils::percentage_amount")]
    pub token_price_safe_range: Amount<Percentage>,
    /// number of blocks cache cleaning will lag behind block finalization
    pub cache_clean_lag: u8,
    /// number of packing threads
    pub cpu_packing_concurrency: u16,
    /// GPU kernel batch size
    pub gpu_packing_batch_size: u32,
    pub oracle_config: OracleConfig,
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

impl Config {
    pub fn irys_signer(&self) -> IrysSigner {
        IrysSigner::from_config(&self)
    }

    pub fn miner_address(&self) -> Address {
        Address::from_private_key(&self.mining_key)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet() -> Self {
        use k256::ecdsa::SigningKey;

        const DEFAULT_BLOCK_TIME: u64 = 5;

        Config {
            block_time: DEFAULT_BLOCK_TIME,
            max_data_txs_per_block: 100,
            difficulty_adjustment_interval: (24u64 * 60 * 60 * 1000).div_ceil(DEFAULT_BLOCK_TIME)
                * 14,
            max_difficulty_adjustment_factor: rust_decimal_macros::dec!(4),
            min_difficulty_adjustment_factor: rust_decimal_macros::dec!(0.25),
            chunk_size: 256 * 1024,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            vdf_reset_frequency: 10 * 120,
            vdf_parallel_verification_thread_limit: 4,
            num_checkpoints_in_vdf_step: 25,
            vdf_sha_1s: 7_000,
            entropy_packing_iterations: 1000,
            chain_id: 1270,
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            submit_ledger_epoch_length: 5,
            num_partitions_per_slot: 1,
            num_writes_before_sync: 1,
            reset_state_on_restart: false,
            chunk_migration_depth: 1,
            mining_key: SigningKey::from_slice(
                &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                    .expect("valid hex"),
            )
            .expect("valid key"),
            num_capacity_partitions: None,
            port: 8080,
            anchor_expiry_depth: 10,
            genesis_price_valid_for_n_epochs: 2,
            genesis_token_price: Amount::token(rust_decimal_macros::dec!(1))
                .expect("valid token amount"),
            token_price_safe_range: Amount::percentage(rust_decimal_macros::dec!(1))
                .expect("valid percentage"),
            cache_clean_lag: 2,
            cpu_packing_concurrency: 4,
            gpu_packing_batch_size: 1024,
            oracle_config: OracleConfig::Mock {
                initial_price: Amount::token(rust_decimal_macros::dec!(1))
                    .expect("valid token amount"),
                percent_change: Amount::percentage(rust_decimal_macros::dec!(0.01))
                    .expect("valid percentage"),
                smoothing_interval: 15,
            },
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
        amount_from_string(deserializer, |dec| Amount::<T>::token(dec))
    }

    /// deserialize the percentage amount from a string.
    ///
    /// The string is expected to be:
    /// - "0.1" (10%)
    /// - "1.0" (100%)
    pub fn percentage_amount<'de, T: std::fmt::Debug, D>(
        deserializer: D,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        amount_from_string(deserializer, |dec| Amount::<T>::percentage(dec))
    }

    fn amount_from_string<'de, T: std::fmt::Debug, D>(
        deserializer: D,
        dec_to_amount: impl Fn(Decimal) -> eyre::Result<Amount<T>>,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use core::str::FromStr as _;

        let raw_string = String::deserialize(deserializer)?;
        let decimal = Decimal::from_str(&raw_string).map_err(serde::de::Error::custom)?;
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
    use rust_decimal_macros::dec;
    use toml;

    #[test]
    fn test_deserialize_config_from_toml() {
        let toml_data = r#"
            block_time = 10
            max_data_txs_per_block = 20
            difficulty_adjustment_interval = 100
            max_difficulty_adjustment_factor = "4"
            min_difficulty_adjustment_factor = "0.25"
            chunk_size = 262144
            num_chunks_in_partition = 10
            num_chunks_in_recall_range = 2
            vdf_reset_frequency = 1200
            vdf_parallel_verification_thread_limit = 4
            num_checkpoints_in_vdf_step = 25
            vdf_sha_1s = 7000
            entropy_packing_iterations = 22500000
            chain_id = 1270
            capacity_scalar = 100
            num_blocks_in_epoch = 100
            submit_ledger_epoch_length = 5
            num_partitions_per_slot = 1
            num_writes_before_sync = 5
            reset_state_on_restart = false
            chunk_migration_depth = 1
            mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
            num_capacity_partitions = 16
            port = 8080
            anchor_expiry_depth = 10
            genesis_price_valid_for_n_epochs = 2
            genesis_token_price = "1.0"
            token_price_safe_range = "0.25"
            cpu_packing_concurrency = 4
            gpu_packing_batch_size = 1024   
            cache_clean_lag = 2

            [oracle_config]
            type = "mock"
            initial_price = "1"
            percent_change = "0.01"
            smoothing_interval = 15
            "#;

        // Attempt to deserialize the TOML string into a Config
        let config: Config =
            toml::from_str(toml_data).expect("Failed to deserialize Config from TOML");

        // Basic assertions to verify deserialization succeeded
        assert_eq!(config.block_time, 10);
        assert_eq!(config.max_data_txs_per_block, 20);
        assert_eq!(config.difficulty_adjustment_interval, 100);
        assert_eq!(config.reset_state_on_restart, false);
        assert_eq!(
            config.genesis_token_price,
            Amount::token(dec!(1.0)).unwrap()
        );
        assert_eq!(config.port, 8080);
        assert_eq!(config.decay_params.safe_minimum_number_of_years, 200);
        assert_eq!(
            config.storage_fees.ingress_fee,
            rust_decimal_macros::dec!(0.01)
        );
    }
}
