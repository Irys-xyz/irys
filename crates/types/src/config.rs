use once_cell::unsync::Lazy;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Block time in seconds
    pub block_time: u64,
    pub difficulty_adjustment_interval: u64,
    pub max_difficulty_adjustment_factor: Decimal,
    pub min_difficulty_adjustment_factor: Decimal,
    pub chunk_size: u64,
    pub num_chunks_in_partition: u64,
    pub partition_size: u64,
    pub num_chunks_in_recall_range: u64,
    pub num_recall_ranges_in_partition: u64,
    pub nonce_limiter_reset_frequency: usize,
    pub vdf_parallel_verification_thread_limit: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_sha_1s: u64,
    pub packing_sha_1_5_s: u32,
    pub irys_chain_id: u64,
    /// Scaling factor for the capacity projection curve
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_partitions_per_slot: u64,
    pub num_writes_before_sync: u64,
}

pub const DEFAULT_CONFIG: Config = {
    const BLOCK_TIME: u64 = 30;
    const CHUNK_SIZE: u64 = 256 * 1024;
    const NUM_CHUNKS_IN_PARTITION: u64 = 10;
    const NUM_CHUNKS_IN_RECALL_RANGE: u64 = 2;
    Config {
        block_time: BLOCK_TIME,
        difficulty_adjustment_interval: (24u64 * 60 * 60 * 1000).div_ceil(BLOCK_TIME) * 14, // 2 weeks worth of blocks
        max_difficulty_adjustment_factor: dec!(4), // A difficulty adjustment can be 4x larger or 1/4th the current difficulty
        min_difficulty_adjustment_factor: dec!(0.1), // A 10% change must be required before a difficulty adjustment will occur
        chunk_size: CHUNK_SIZE,
        num_chunks_in_partition: NUM_CHUNKS_IN_PARTITION,
        partition_size: CHUNK_SIZE * NUM_CHUNKS_IN_PARTITION,
        num_chunks_in_recall_range: NUM_CHUNKS_IN_RECALL_RANGE,
        num_recall_ranges_in_partition: NUM_CHUNKS_IN_PARTITION / NUM_CHUNKS_IN_RECALL_RANGE,
        nonce_limiter_reset_frequency: 10 * 120, // Reset the nonce limiter (vdf) once every 1200 steps/seconds or every ~20 min
        vdf_parallel_verification_thread_limit: 4,
        num_checkpoints_in_vdf_step: 25, // 25 checkpoints 40 ms each = 1000 ms
        vdf_sha_1s: 530_000,
        packing_sha_1_5_s: 22_500_000,
        irys_chain_id: 69727973, // "irys" in ascii
        capacity_scalar: 100,
        num_blocks_in_epoch: 100,
        submit_ledger_epoch_length: 5,
        num_partitions_per_slot: 1,
        num_writes_before_sync: 5,
    }
};

thread_local! {
    pub static CONFIG: Lazy<Config> = Lazy::new(|| {
        // TODO: load from env
        DEFAULT_CONFIG.clone()
    });
}
