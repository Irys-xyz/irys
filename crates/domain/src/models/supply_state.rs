use atomic_write_file::AtomicWriteFile;
use eyre::{Context as _, Result};
use irys_types::{NodeConfig, U256};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

const FILE_NAME: &str = "supply_state.dat";
// Layout: height(8) + cumulative_emitted(32) + backfill_complete(1) + has_first_migration(1) + first_migration_height(8) = 50 bytes
const STATE_SIZE: usize = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupplyStateData {
    pub height: u64,
    pub cumulative_emitted: U256,
    pub backfill_complete: bool,
    pub first_migration_height: Option<u64>,
}

impl SupplyStateData {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(STATE_SIZE);
        bytes.extend_from_slice(&self.height.to_le_bytes());
        bytes.extend_from_slice(&self.cumulative_emitted.to_le_bytes());
        bytes.push(if self.backfill_complete { 1 } else { 0 });
        match self.first_migration_height {
            Some(h) => {
                bytes.push(1);
                bytes.extend_from_slice(&h.to_le_bytes());
            }
            None => {
                bytes.push(0);
                bytes.extend_from_slice(&0_u64.to_le_bytes());
            }
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != STATE_SIZE {
            eyre::bail!(
                "Invalid supply state data: expected {} bytes, got {}",
                STATE_SIZE,
                bytes.len()
            );
        }

        let height = u64::from_le_bytes(bytes[0..8].try_into().expect("slice is exactly 8 bytes"));
        let cumulative_emitted =
            U256::from_le_bytes(bytes[8..40].try_into().expect("slice is exactly 32 bytes"));
        let backfill_complete = bytes[40] != 0;
        let has_first_migration = bytes[41] != 0;
        let first_migration_height = if has_first_migration {
            Some(u64::from_le_bytes(
                bytes[42..50].try_into().expect("slice is exactly 8 bytes"),
            ))
        } else {
            None
        };

        Ok(Self {
            height,
            cumulative_emitted,
            backfill_complete,
            first_migration_height,
        })
    }
}

#[derive(Debug)]
pub struct SupplyState {
    inner: RwLock<SupplyStateData>,
    ready: AtomicBool,
    state_file: PathBuf,
}

impl SupplyState {
    pub fn new(config: &NodeConfig) -> Result<Self> {
        let state_dir = config.block_index_dir();
        std::fs::create_dir_all(&state_dir)?;
        let state_file = state_dir.join(FILE_NAME);

        let data = load_from_file(&state_file).unwrap_or_default();

        Ok(Self {
            inner: RwLock::new(data),
            ready: AtomicBool::new(data.backfill_complete),
            state_file,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn get(&self) -> SupplyStateData {
        *self.inner.read().expect("supply state read lock poisoned")
    }

    pub fn first_migration_height(&self) -> Option<u64> {
        self.get().first_migration_height
    }

    pub fn height(&self) -> u64 {
        self.get().height
    }

    pub fn cumulative_emitted(&self) -> U256 {
        self.get().cumulative_emitted
    }

    pub fn add_block_reward(&self, height: u64, reward_amount: U256) -> Result<()> {
        let mut data = self
            .inner
            .write()
            .expect("supply state write lock poisoned");

        let first_migration = if data.first_migration_height.is_none() {
            Some(height)
        } else {
            let expected = data.height.saturating_add(1);
            if height != expected {
                eyre::bail!(
                    "Supply state height mismatch: expected {}, got {}",
                    expected,
                    height
                );
            }
            data.first_migration_height
        };

        let new_data = SupplyStateData {
            height,
            cumulative_emitted: data.cumulative_emitted.saturating_add(reward_amount),
            backfill_complete: data.backfill_complete,
            first_migration_height: first_migration,
        };

        save_to_file(&self.state_file, &new_data)?;
        *data = new_data;
        Ok(())
    }

    pub fn add_historical_sum_and_mark_ready(&self, historical_sum: U256) -> Result<()> {
        let mut data = self
            .inner
            .write()
            .expect("supply state write lock poisoned");

        if data.backfill_complete {
            return Ok(());
        }

        let new_data = SupplyStateData {
            height: data.height,
            cumulative_emitted: data.cumulative_emitted.saturating_add(historical_sum),
            backfill_complete: true,
            first_migration_height: data.first_migration_height,
        };

        save_to_file(&self.state_file, &new_data)?;
        *data = new_data;
        self.ready.store(true, Ordering::Release);
        Ok(())
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
}

fn load_from_file(path: &Path) -> Result<SupplyStateData> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("Failed to open supply state file: {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .context("Failed to read supply state file")?;

    if bytes.is_empty() {
        return Ok(SupplyStateData::default());
    }

    SupplyStateData::from_bytes(&bytes).context("Failed to parse supply state data")
}

fn save_to_file(path: &Path, data: &SupplyStateData) -> Result<()> {
    let bytes = data.to_bytes();
    let mut file = AtomicWriteFile::open(path).with_context(|| {
        format!(
            "Failed to open supply state file for writing: {}",
            path.display()
        )
    })?;
    file.write_all(&bytes)
        .context("Failed to write supply state data")?;
    file.commit()
        .context("Failed to commit supply state file")?;
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
            inner: RwLock::new(SupplyStateData::default()),
            ready: AtomicBool::new(false),
            state_file,
        }
    }

    fn arb_u256_bytes() -> impl Strategy<Value = [u8; 32]> {
        any::<[u8; 32]>()
    }

    proptest! {
        #[test]
        fn roundtrip_property(height: u64, emitted_bytes in arb_u256_bytes(), backfill_complete: bool, first_migration in proptest::option::of(any::<u64>())) {
            let data = SupplyStateData {
                height,
                cumulative_emitted: U256::from_le_bytes(emitted_bytes),
                backfill_complete,
                first_migration_height: first_migration,
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
    fn from_bytes_rejects_wrong_size() {
        let short = vec![0_u8; STATE_SIZE - 1];
        assert!(SupplyStateData::from_bytes(&short).is_err());

        let long = vec![0_u8; STATE_SIZE + 1];
        assert!(SupplyStateData::from_bytes(&long).is_err());
    }

    #[test]
    fn add_block_reward_rejects_non_sequential() {
        let state = create_test_supply_state();
        state.add_block_reward(0, U256::from(100)).unwrap();

        let result = state.add_block_reward(2, U256::from(100));
        assert!(result.is_err());
    }

    #[test]
    fn add_block_reward_rejects_duplicate_height() {
        let state = create_test_supply_state();
        state.add_block_reward(0, U256::from(100)).unwrap();

        let result = state.add_block_reward(0, U256::from(100));
        assert!(result.is_err());
    }

    #[test]
    fn first_migration_can_be_any_height() {
        let state = create_test_supply_state();

        state.add_block_reward(100, U256::from(500)).unwrap();

        assert_eq!(state.height(), 100);
        assert_eq!(state.cumulative_emitted(), U256::from(500));
        assert_eq!(state.first_migration_height(), Some(100));

        state.add_block_reward(101, U256::from(300)).unwrap();
        assert_eq!(state.height(), 101);
        assert_eq!(state.cumulative_emitted(), U256::from(800));
    }

    #[test]
    fn add_historical_sum_adds_to_current_value() {
        let state = create_test_supply_state();

        state.add_block_reward(50, U256::from(100)).unwrap();
        state.add_block_reward(51, U256::from(200)).unwrap();

        assert_eq!(state.cumulative_emitted(), U256::from(300));
        assert!(!state.is_ready());

        state
            .add_historical_sum_and_mark_ready(U256::from(1000))
            .unwrap();

        assert_eq!(state.cumulative_emitted(), U256::from(1300));
        assert!(state.is_ready());
        assert_eq!(state.height(), 51);
    }

    #[test]
    fn first_migration_at_genesis_needs_no_backfill() {
        let state = create_test_supply_state();

        state.add_block_reward(0, U256::from(100)).unwrap();

        assert_eq!(state.first_migration_height(), Some(0));
        assert_eq!(state.height(), 0);
        assert_eq!(state.cumulative_emitted(), U256::from(100));

        state
            .add_historical_sum_and_mark_ready(U256::zero())
            .unwrap();

        assert!(state.is_ready());
        assert_eq!(state.cumulative_emitted(), U256::from(100));
    }

    #[test]
    fn restart_with_persisted_state_continues_sequentially() {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("supply_state_restart_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        {
            let state = SupplyState {
                inner: RwLock::new(SupplyStateData::default()),
                ready: AtomicBool::new(false),
                state_file: state_file.clone(),
            };

            state.add_block_reward(10, U256::from(100)).unwrap();
            state.add_block_reward(11, U256::from(200)).unwrap();
            state
                .add_historical_sum_and_mark_ready(U256::from(500))
                .unwrap();
        }

        let data = load_from_file(&state_file).unwrap();
        assert_eq!(data.height, 11);
        assert_eq!(data.cumulative_emitted, U256::from(800));
        assert!(data.backfill_complete);
        assert_eq!(data.first_migration_height, Some(10));

        let state = SupplyState {
            inner: RwLock::new(data),
            ready: AtomicBool::new(data.backfill_complete),
            state_file,
        };

        state.add_block_reward(12, U256::from(300)).unwrap();
        assert_eq!(state.height(), 12);
        assert_eq!(state.cumulative_emitted(), U256::from(1100));

        let result = state.add_block_reward(14, U256::from(100));
        assert!(result.is_err());
    }

    #[test]
    fn is_ready_state_transitions() {
        let state = create_test_supply_state();

        assert!(!state.is_ready());

        state.add_block_reward(5, U256::from(100)).unwrap();
        assert!(!state.is_ready());

        state
            .add_historical_sum_and_mark_ready(U256::from(50))
            .unwrap();
        assert!(state.is_ready());
    }

    #[test]
    fn add_historical_sum_is_idempotent() {
        let state = create_test_supply_state();

        state.add_block_reward(10, U256::from(100)).unwrap();
        state
            .add_historical_sum_and_mark_ready(U256::from(500))
            .unwrap();

        assert_eq!(state.cumulative_emitted(), U256::from(600));
        assert!(state.is_ready());

        state
            .add_historical_sum_and_mark_ready(U256::from(500))
            .unwrap();

        assert_eq!(state.cumulative_emitted(), U256::from(600));
    }

    #[test]
    fn restart_sets_ready_from_backfill_complete_flag() {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir =
            std::env::temp_dir().join(format!("supply_state_ready_restart_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        {
            let state = SupplyState {
                inner: RwLock::new(SupplyStateData::default()),
                ready: AtomicBool::new(false),
                state_file: state_file.clone(),
            };

            state.add_block_reward(5, U256::from(100)).unwrap();
            state
                .add_historical_sum_and_mark_ready(U256::from(200))
                .unwrap();
            assert!(state.is_ready());
        }

        let data = load_from_file(&state_file).unwrap();
        assert!(data.backfill_complete);
        assert_eq!(data.first_migration_height, Some(5));

        let state = SupplyState {
            inner: RwLock::new(data),
            ready: AtomicBool::new(data.backfill_complete),
            state_file,
        };

        assert!(state.is_ready());
        assert_eq!(state.cumulative_emitted(), U256::from(300));
    }

    #[test]
    fn restart_not_ready_if_backfill_incomplete() {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir =
            std::env::temp_dir().join(format!("supply_state_incomplete_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        {
            let state = SupplyState {
                inner: RwLock::new(SupplyStateData::default()),
                ready: AtomicBool::new(false),
                state_file: state_file.clone(),
            };

            state.add_block_reward(5, U256::from(100)).unwrap();
            state.add_block_reward(6, U256::from(200)).unwrap();
            // Simulate crash: backfill never completed
        }

        let data = load_from_file(&state_file).unwrap();
        assert!(!data.backfill_complete);
        assert_eq!(data.cumulative_emitted, U256::from(300));
        assert_eq!(data.first_migration_height, Some(5));

        let state = SupplyState {
            inner: RwLock::new(data),
            ready: AtomicBool::new(data.backfill_complete),
            state_file,
        };

        assert!(!state.is_ready());
    }

    #[test]
    fn concurrent_migration_and_backfill() {
        use std::sync::Arc;
        use std::thread;

        let state = Arc::new(create_test_supply_state());
        let first_migration_height = 50_u64;
        let migration_count = 50_u64;
        let reward_per_block = 100_u64;

        let state_for_migration = Arc::clone(&state);
        let migration_handle = thread::spawn(move || {
            for i in 0..migration_count {
                let height = first_migration_height + i;
                state_for_migration
                    .add_block_reward(height, U256::from(reward_per_block))
                    .unwrap();
                thread::yield_now();
            }
        });

        let state_for_backfill = Arc::clone(&state);
        let backfill_handle = thread::spawn(move || {
            loop {
                if state_for_backfill.first_migration_height().is_some() {
                    break;
                }
                thread::yield_now();
            }

            let historical_sum = U256::from(first_migration_height * reward_per_block);
            state_for_backfill
                .add_historical_sum_and_mark_ready(historical_sum)
                .unwrap();
        });

        migration_handle.join().unwrap();
        backfill_handle.join().unwrap();

        let total_blocks = first_migration_height + migration_count;
        let expected_total = U256::from(total_blocks * reward_per_block);

        assert!(state.is_ready());
        assert_eq!(state.cumulative_emitted(), expected_total);
        assert_eq!(state.height(), first_migration_height + migration_count - 1);
    }
}
