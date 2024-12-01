use std::{
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use nodit::{InclusiveInterval, Interval, IntervalType, NoditMap};
use serde::{Deserialize, Serialize};

use crate::CHUNK_SIZE;

pub const MEGABYTE: usize = 1024 * 1024;
pub const GIGABYTE: usize = MEGABYTE * 1024;
pub const TERABYTE: usize = GIGABYTE * 1024;

/// Partition relative chunk offsets
pub type PartitionChunkOffset = u32;

/// Partition relative chunk interval/ranges
#[derive(Debug, Copy, Clone)]
pub struct PartitionChunkRange(pub Interval<PartitionChunkOffset>);

impl Deref for PartitionChunkRange {
    type Target = Interval<PartitionChunkOffset>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Interval<u32>> for PartitionChunkRange {
    fn from(interval: Interval<PartitionChunkOffset>) -> Self {
        Self(interval)
    }
}

impl InclusiveInterval<u32> for PartitionChunkRange {
    fn start(&self) -> u32 {
        self.0.start()
    }

    fn end(&self) -> u32 {
        self.0.end()
    }
}

/// Ledger Relative chunk offsets
pub type LedgerChunkOffset = u64;

/// Ledger Relative chunk interval/ranges
#[derive(Debug, Copy, Clone)]
pub struct LedgerChunkRange(pub Interval<LedgerChunkOffset>);

impl Deref for LedgerChunkRange {
    type Target = Interval<LedgerChunkOffset>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Interval<u64>> for LedgerChunkRange {
    fn from(interval: Interval<LedgerChunkOffset>) -> Self {
        Self(interval)
    }
}

/// Add impl Into<Interval<u64>> for owned conversion
impl From<LedgerChunkRange> for Interval<u64> {
    fn from(range: LedgerChunkRange) -> Self {
        range.0
    }
}

// Add Deref implementation to convert to Interval<u64>
impl AsRef<Interval<u64>> for LedgerChunkRange {
    fn as_ref(&self) -> &Interval<u64> {
        &self.0
    }
}

impl InclusiveInterval<u64> for LedgerChunkRange {
    fn start(&self) -> u64 {
        self.0.start()
    }

    fn end(&self) -> u64 {
        self.0.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Storage provider config
pub struct PartitionStorageProviderConfig {
    /// vec of intervals to storage module configurations
    pub sm_paths_offsets: Vec<(Interval<u32>, StorageModuleConfig)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

pub struct StorageModuleConfig {
    pub directory_path: PathBuf,
    pub size_bytes: u64,
    // pub chunks_per_lock_segment: u32,
}

impl Default for StorageModuleConfig {
    fn default() -> Self {
        Self {
            directory_path: "/tmp".into(),
            size_bytes: 100 * CHUNK_SIZE,
            // chunks_per_lock_segment: 800, // 200MB
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalState {
    // common fields:
    pub chunk_state: ChunkState,
}

impl IntervalState {
    pub fn new(chunk_state: ChunkState) -> Self {
        Self { chunk_state }
    }

    pub fn packed() -> Self {
        Self {
            chunk_state: ChunkState::Packed,
        }
    }
}

// wrapper struct so we can "contain" the custom Eq impl that works off the state enum
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalStateWrapped {
    pub inner: IntervalState,
}

impl Deref for IntervalStateWrapped {
    type Target = IntervalState;

    fn deref(&self) -> &Self::Target {
        return &self.inner;
    }
}

impl DerefMut for IntervalStateWrapped {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return &mut self.inner;
    }
}

impl IntervalStateWrapped {
    pub fn new(state: IntervalState) -> Self {
        Self { inner: state }
    }
}

impl PartialEq for IntervalStateWrapped {
    fn eq(&self, other: &Self) -> bool {
        // compare the state enum variant, not inner state
        std::mem::discriminant(&self.inner.chunk_state)
            == std::mem::discriminant(&other.inner.chunk_state)
    }
}

impl Eq for IntervalStateWrapped {}

#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
pub enum ChunkState {
    Unpacked,
    Packed,
    Data,
    Writing,
}
