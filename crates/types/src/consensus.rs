/// Block time in milliseconds
pub const BLOCK_TIME: u64 = 30_000;

/// 2 weeks worth of blocks
pub const DIFFICULTY_ADJUSTMENT_INTERVAL: u64 = (24u64 * 60 * 60 * 1000).div_ceil(BLOCK_TIME) * 14;

/// A difficulty adjustment can be 4x larger or 1/4th the current difficulty
pub const DIFFICULTY_ADJUSTMENT_FACTOR: f64 = 4.0;

pub const CHUNK_SIZE: u64 = 256 * 1024;

pub const NUM_CHUNKS_IN_PARTITION: u64 = 10;

pub const PARTITION_SIZE: u64 = CHUNK_SIZE * NUM_CHUNKS_IN_PARTITION;

pub const NUM_CHUNKS_IN_RECALL_RANGE: u64 = 2;

pub const NUM_RECALL_RANGES_IN_PARTITION: u64 =
    NUM_CHUNKS_IN_PARTITION / NUM_CHUNKS_IN_RECALL_RANGE;

// Reset the nonce limiter (vdf) once every 1200 steps/seconds or every ~20 min
pub const NONCE_LIMITER_RESET_FREQUENCY: usize = 10 * 120;

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
