use std::{
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use nodit::{Interval, NoditMap};
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalState {
    // common fields:
    pub chunk_state: ChunkState,
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
}
