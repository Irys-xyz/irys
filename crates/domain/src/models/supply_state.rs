//! Tracks cumulative emitted supply for efficient O(1) lookups.
//!
//! The supply state is recalculated on node startup by iterating through
//! all blocks in the block index, and updated incrementally as new blocks
//! are migrated.

use atomic_write_file::AtomicWriteFile;
use eyre::Result;
use irys_types::{NodeConfig, U256};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

const FILE_NAME: &str = "supply_state.dat";
const STATE_SIZE: usize = std::mem::size_of::<u64>() + std::mem::size_of::<U256>();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupplyStateData {
    pub height: u64,
    pub cumulative_emitted: U256,
}

impl SupplyStateData {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(STATE_SIZE);
        bytes.extend_from_slice(&self.height.to_le_bytes());
        bytes.extend_from_slice(&self.cumulative_emitted.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != STATE_SIZE {
            eyre::bail!(
                "Invalid supply state data: expected exactly {} bytes, got {}",
                STATE_SIZE,
                bytes.len()
            );
        }

        let height = u64::from_le_bytes(bytes[0..8].try_into().expect("slice is exactly 8 bytes"));
        let cumulative_emitted =
            U256::from_le_bytes(bytes[8..40].try_into().expect("slice is exactly 32 bytes"));

        Ok(Self {
            height,
            cumulative_emitted,
        })
    }
}

#[derive(Debug)]
pub struct SupplyState {
    data: RwLock<SupplyStateData>,
    ready: AtomicBool,
    genesis_processed: AtomicBool,
    state_file: PathBuf,
}

impl SupplyState {
    /// Creates a new supply state, loading from disk if available.
    pub fn new(config: &NodeConfig) -> Result<Self> {
        let state_dir = config.block_index_dir();
        std::fs::create_dir_all(&state_dir)?;
        let state_file = state_dir.join(FILE_NAME);

        let data = load_from_file(&state_file).unwrap_or_default();
        let genesis_already_processed = data.height > 0 || data.cumulative_emitted > U256::zero();

        Ok(Self {
            data: RwLock::new(data),
            ready: AtomicBool::new(false),
            genesis_processed: AtomicBool::new(genesis_already_processed),
            state_file,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn get(&self) -> SupplyStateData {
        *self.data.read().expect("supply state read lock poisoned")
    }

    pub fn height(&self) -> u64 {
        self.data
            .read()
            .expect("supply state read lock poisoned")
            .height
    }

    pub fn cumulative_emitted(&self) -> U256 {
        self.data
            .read()
            .expect("supply state read lock poisoned")
            .cumulative_emitted
    }

    /// Adds a block's reward if height is sequential. Persists to disk.
    /// Callers should NOT gate on `is_ready()` - sequencing is handled internally.
    pub fn add_block_reward(&self, height: u64, reward_amount: U256) -> Result<()> {
        let data_to_write = {
            let mut data = self.data.write().expect("supply state write lock poisoned");

            let is_genesis = height == 0;
            let is_sequential = height == data.height.saturating_add(1);

            if is_genesis {
                if self.genesis_processed.swap(true, Ordering::AcqRel) {
                    eyre::bail!("Genesis block reward already processed");
                }
            } else if !is_sequential {
                eyre::bail!(
                    "Supply state height mismatch: expected {}, got {}",
                    data.height.saturating_add(1),
                    height
                );
            }

            data.height = height;
            data.cumulative_emitted = data.cumulative_emitted.saturating_add(reward_amount);
            *data
        };

        save_to_file(&self.state_file, &data_to_write)
    }

    /// Atomically sets the supply state and marks it as ready.
    /// This prevents race conditions by updating state and ready flag
    /// while holding the write lock.
    pub fn set_and_mark_ready(&self, height: u64, cumulative_emitted: U256) -> Result<()> {
        let data_to_write = {
            let mut data = self.data.write().expect("supply state write lock poisoned");

            data.height = height;
            data.cumulative_emitted = cumulative_emitted;

            if height > 0 || cumulative_emitted > U256::zero() {
                self.genesis_processed.store(true, Ordering::Release);
            }

            // Mark ready while holding write lock to ensure atomicity
            self.ready.store(true, Ordering::Release);

            *data
        };

        save_to_file(&self.state_file, &data_to_write)
    }

    pub fn mark_not_ready(&self) {
        self.ready.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub struct SupplyStateReadGuard {
    inner: Arc<SupplyState>,
}

impl SupplyStateReadGuard {
    pub fn new(supply_state: Arc<SupplyState>) -> Self {
        Self {
            inner: supply_state,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    pub fn get(&self) -> SupplyStateData {
        self.inner.get()
    }

    pub fn height(&self) -> u64 {
        self.inner.height()
    }

    pub fn cumulative_emitted(&self) -> U256 {
        self.inner.cumulative_emitted()
    }
}

fn load_from_file(path: &Path) -> Result<SupplyStateData> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    if bytes.is_empty() {
        return Ok(SupplyStateData::default());
    }

    SupplyStateData::from_bytes(&bytes)
}

fn save_to_file(path: &Path, data: &SupplyStateData) -> Result<()> {
    let bytes = data.to_bytes();
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::AtomicU64;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_supply_state() -> SupplyState {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("supply_state_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        SupplyState {
            data: RwLock::new(SupplyStateData::default()),
            ready: AtomicBool::new(false),
            genesis_processed: AtomicBool::new(false),
            state_file,
        }
    }

    fn arb_u256_bytes() -> impl Strategy<Value = [u8; 32]> {
        any::<[u8; 32]>()
    }

    proptest! {
        #[test]
        fn roundtrip_property(height: u64, emitted_bytes in arb_u256_bytes()) {
            let data = SupplyStateData {
                height,
                cumulative_emitted: U256::from_le_bytes(emitted_bytes),
            };
            let bytes = data.to_bytes();
            let decoded = SupplyStateData::from_bytes(&bytes).unwrap();
            prop_assert_eq!(decoded, data);
        }

        #[test]
        fn sequential_rewards_accumulate_correctly(rewards in prop::collection::vec(0_u64..1_000_000, 1..50)) {
            let state = create_test_supply_state();

            let mut expected_cumulative = U256::zero();
            for (height, &reward) in rewards.iter().enumerate() {
                let reward_u256 = U256::from(reward);
                state.add_block_reward(height as u64, reward_u256).unwrap();
                expected_cumulative = expected_cumulative.saturating_add(reward_u256);
            }

            prop_assert_eq!(state.cumulative_emitted(), expected_cumulative);
            prop_assert_eq!(state.height(), (rewards.len() - 1) as u64);
        }
    }

    #[test]
    fn from_bytes_rejects_short_input() {
        let short = vec![0_u8; STATE_SIZE - 1];
        assert!(SupplyStateData::from_bytes(&short).is_err());
    }

    #[test]
    fn from_bytes_rejects_long_input() {
        let long = vec![0_u8; STATE_SIZE + 1];
        assert!(SupplyStateData::from_bytes(&long).is_err());
    }

    #[test]
    fn add_block_reward_rejects_non_sequential() {
        let state = create_test_supply_state();
        state.add_block_reward(0, U256::from(100)).unwrap();

        // Height 2 should fail (expected 1)
        let result = state.add_block_reward(2, U256::from(100));
        assert!(result.is_err());
    }

    #[test]
    fn add_block_reward_rejects_duplicate_genesis() {
        let state = create_test_supply_state();
        state.add_block_reward(0, U256::from(100)).unwrap();

        // Second genesis should fail
        let result = state.add_block_reward(0, U256::from(100));
        assert!(result.is_err());
    }
}
