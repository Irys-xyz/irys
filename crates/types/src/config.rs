use once_cell::unsync::Lazy;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
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
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_partitions_per_slot: u64,
    pub num_writes_before_sync: u64,
}

pub const DEFAULT_CONFIG: Config = Config {
    block_time: 30,
    difficulty_adjustment_interval: (24u64 * 60 * 60 * 1000).div_ceil(BLOCK_TIME) * 14,
    max_difficulty_adjustment_factor: dec!(4),
    min_difficulty_adjustment_factor: dec!(0.1),
    chunk_size: 256 * 1024,
    num_chunks_in_partition: 10,
    partition_size: todo!(),
    num_chunks_in_recall_range: todo!(),
    num_recall_ranges_in_partition: todo!(),
    nonce_limiter_reset_frequency: todo!(),
    vdf_parallel_verification_thread_limit: todo!(),
    num_checkpoints_in_vdf_step: todo!(),
    vdf_sha_1s: todo!(),
    packing_sha_1_5_s: todo!(),
    irys_chain_id: todo!(),
    capacity_scalar: todo!(),
    num_blocks_in_epoch: todo!(),
    submit_ledger_epoch_length: todo!(),
    num_partitions_per_slot: todo!(),
    num_writes_before_sync: todo!(),
};

thread_local! {
    pub static CONFIG: Lazy<Config> = Lazy::new(|| Config {
        block_time: 30,
        difficulty_adjustment_interval: DIFFICULTY_ADJUSTMENT_INTERVAL,
        max_difficulty_adjustment_factor: MAX_DIFFICULTY_ADJUSTMENT_FACTOR.into(),
        min_difficulty_adjustment_factor: MIN_DIFFICULTY_ADJUSTMENT_FACTOR,
        chunk_size: CHUNK_SIZE,
        num_chunks_in_partition: NUM_CHUNKS_IN_PARTITION,
        partition_size: PARTITION_SIZE,
        num_chunks_in_recall_range: NUM_CHUNKS_IN_RECALL_RANGE,
        num_recall_ranges_in_partition: NUM_RECALL_RANGES_IN_PARTITION,
        nonce_limiter_reset_frequency: NONCE_LIMITER_RESET_FREQUENCY,
        vdf_parallel_verification_thread_limit: VDF_PARALLEL_VERIFICATION_THREAD_LIMIT,
        num_checkpoints_in_vdf_step: NUM_CHECKPOINTS_IN_VDF_STEP,
        vdf_sha_1s: VDF_SHA_1S,
        packing_sha_1_5_s: PACKING_SHA_1_5_S,
        irys_chain_id: IRYS_CHAIN_ID,
        capacity_scalar: CAPACITY_SCALAR,
        num_blocks_in_epoch: NUM_BLOCKS_IN_EPOCH,
        submit_ledger_epoch_length: SUBMIT_LEDGER_EPOCH_LENGTH,
        num_partitions_per_slot: NUM_PARTITIONS_PER_SLOT,
        num_writes_before_sync: NUM_WRITES_BEFORE_SYNC,
    });
}

/// Block time in seconds
pub const BLOCK_TIME: u64 = 30;

/// 2 weeks worth of blocks
pub const DIFFICULTY_ADJUSTMENT_INTERVAL: u64 = (24u64 * 60 * 60 * 1000).div_ceil(BLOCK_TIME) * 14;

/// A difficulty adjustment can be 4x larger or 1/4th the current difficulty
pub const MAX_DIFFICULTY_ADJUSTMENT_FACTOR: u64 = 4;

// A 10% change must be required before a difficulty adjustment will occur
pub const MIN_DIFFICULTY_ADJUSTMENT_FACTOR: Decimal = dec!(0.1);

pub const CHUNK_SIZE: u64 = 256 * 1024;

pub const NUM_CHUNKS_IN_PARTITION: u64 = 10;

pub const PARTITION_SIZE: u64 = CHUNK_SIZE * NUM_CHUNKS_IN_PARTITION;

pub const NUM_CHUNKS_IN_RECALL_RANGE: u64 = 2;

pub const NUM_RECALL_RANGES_IN_PARTITION: u64 =
    NUM_CHUNKS_IN_PARTITION / NUM_CHUNKS_IN_RECALL_RANGE;

// Reset the nonce limiter (vdf) once every 1200 steps/seconds or every ~20 min
pub const NONCE_LIMITER_RESET_FREQUENCY: usize = 10 * 120;

pub const VDF_PARALLEL_VERIFICATION_THREAD_LIMIT: usize = 4;

// 25 checkpoints 40 ms each = 1000 ms
pub const NUM_CHECKPOINTS_IN_VDF_STEP: usize = 25;

pub const VDF_SHA_1S: u64 = 530_000;
pub const PACKING_SHA_1_5_S: u32 = 22_500_000;

pub const IRYS_CHAIN_ID: u64 = 69727973; // "irys" in ascii

// Epoch and capacity projection parameters
pub const CAPACITY_SCALAR: u64 = 100; // Scaling factor for the capacity projection curve
pub const NUM_BLOCKS_IN_EPOCH: u64 = 100;
pub const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 5;
pub const NUM_PARTITIONS_PER_SLOT: u64 = 1;
pub const NUM_WRITES_BEFORE_SYNC: u64 = 5;
