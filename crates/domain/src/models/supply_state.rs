use atomic_write_file::AtomicWriteFile;
use eyre::{Context as _, Result};
use irys_types::{NodeConfig, U256};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;
use tracing::warn;

const FILE_NAME: &str = "supply_state.dat";
const STATE_SIZE: usize = 50;

fn write_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(h) => {
            bytes.push(1);
            bytes.extend_from_slice(&h.to_le_bytes());
        }
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&0_u64.to_le_bytes());
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistedSupplyState {
    pub backfill_height: Option<u64>,
    pub backfill_value: U256,
    pub first_migration_height: Option<u64>,
}

impl PersistedSupplyState {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(STATE_SIZE);
        write_optional_u64(&mut bytes, self.backfill_height);
        bytes.extend_from_slice(&self.backfill_value.to_le_bytes());
        write_optional_u64(&mut bytes, self.first_migration_height);
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

        let has_backfill = bytes[0] != 0;
        let backfill_height = if has_backfill {
            Some(u64::from_le_bytes(
                bytes[1..9].try_into().expect("slice is exactly 8 bytes"),
            ))
        } else {
            None
        };
        let backfill_value =
            U256::from_le_bytes(bytes[9..41].try_into().expect("slice is exactly 32 bytes"));
        let has_first_migration = bytes[41] != 0;
        let first_migration_height = if has_first_migration {
            Some(u64::from_le_bytes(
                bytes[42..50].try_into().expect("slice is exactly 8 bytes"),
            ))
        } else {
            None
        };

        Ok(Self {
            backfill_height,
            backfill_value,
            first_migration_height,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupplyStateData {
    pub height: u64,
    pub cumulative_emitted: U256,
    pub first_migration_height: Option<u64>,
}

#[derive(Debug)]
pub struct SupplyState {
    inner: RwLock<SupplyStateData>,
    ready: AtomicBool,
    first_migration_notify: Notify,
    persisted_backfill_height: Option<u64>,
    persisted_backfill_value: U256,
    state_file: PathBuf,
}

impl SupplyState {
    pub fn new(config: &NodeConfig) -> Result<Self> {
        let state_dir = config.block_index_dir();
        std::fs::create_dir_all(&state_dir)?;
        let state_file = state_dir.join(FILE_NAME);

        // Load persisted backfill point (if any), defaulting only if file doesn't exist
        let persisted = match load_from_file(&state_file) {
            Ok(state) => state,
            Err(e) => {
                if is_not_found(&e) {
                    PersistedSupplyState::default()
                } else {
                    return Err(e);
                }
            }
        };

        // Runtime state starts fresh - will be reconstructed
        Ok(Self {
            inner: RwLock::new(SupplyStateData::default()),
            ready: AtomicBool::new(false),
            first_migration_notify: Notify::new(),
            persisted_backfill_height: persisted.backfill_height,
            persisted_backfill_value: persisted.backfill_value,
            state_file,
        })
    }

    pub fn persisted_backfill_height(&self) -> Option<u64> {
        self.persisted_backfill_height
    }

    pub fn persisted_backfill_value(&self) -> U256 {
        self.persisted_backfill_value
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

    /// Waits until `first_migration_height` is set and returns it.
    ///
    /// If already set, returns immediately. Otherwise waits for notification
    /// from `add_block_reward` when the first block is processed.
    pub async fn wait_for_first_migration(&self) -> u64 {
        loop {
            // Create the notified future BEFORE checking condition to avoid TOCTOU race.
            // Using enable() ensures the permit isn't lost if notification arrives
            // between checking the condition and awaiting.
            let notified = self.first_migration_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(height) = self.first_migration_height() {
                return height;
            }
            notified.await;
        }
    }

    pub fn height(&self) -> u64 {
        self.get().height
    }

    pub fn cumulative_emitted(&self) -> U256 {
        self.get().cumulative_emitted
    }

    pub fn add_block_reward(&self, height: u64, reward_amount: U256) -> Result<()> {
        let is_first_migration = {
            let mut data = self
                .inner
                .write()
                .expect("supply state write lock poisoned");

            let is_first = data.first_migration_height.is_none();
            if is_first
                && let Some(persisted_height) = self.persisted_backfill_height
                && height <= persisted_height
            {
                eyre::bail!(
                    "First migration height {} overlaps persisted backfill height {}",
                    height,
                    persisted_height
                );
            }
            let first_migration = if is_first {
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

            // Update runtime state only - no persistence needed here
            *data = SupplyStateData {
                height,
                cumulative_emitted: data.cumulative_emitted.saturating_add(reward_amount),
                first_migration_height: first_migration,
            };
            is_first
        };

        if is_first_migration {
            self.first_migration_notify.notify_one();
        }

        Ok(())
    }

    pub fn add_historical_sum_and_mark_ready(&self, historical_sum: U256) -> Result<()> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }

        // Calculate values under read lock
        let (persisted, new_backfill_value) = {
            let data = self.inner.read().expect("supply state read lock poisoned");
            let new_backfill_value = self.persisted_backfill_value.saturating_add(historical_sum);
            let new_backfill_height = data.first_migration_height.and_then(|h| h.checked_sub(1));

            let persisted = PersistedSupplyState {
                backfill_height: new_backfill_height,
                backfill_value: new_backfill_value,
                first_migration_height: data.first_migration_height,
            };
            (persisted, new_backfill_value)
        };

        {
            let mut data = self
                .inner
                .write()
                .expect("supply state write lock poisoned");
            // Double-check: another thread may have called this concurrently.
            // Persist only when we're the winning thread to ensure consistency
            // between persisted and runtime state. Holds write lock during I/O,
            // acceptable for rare backfill completion.
            if !self.ready.load(Ordering::Acquire) {
                save_to_file(&self.state_file, &persisted)?;
                data.cumulative_emitted =
                    data.cumulative_emitted.saturating_add(new_backfill_value);
                self.ready.store(true, Ordering::Release);
            }
        }
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

fn is_not_found(err: &eyre::Report) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|io_err| io_err.kind() == std::io::ErrorKind::NotFound)
}

fn load_from_file(path: &Path) -> Result<PersistedSupplyState> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("Failed to open supply state file: {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .context("Failed to read supply state file")?;

    if bytes.is_empty() {
        warn!(
            path = %path.display(),
            "Supply state file exists but is empty, using defaults. \
             This may indicate a previous incomplete write."
        );
        return Ok(PersistedSupplyState::default());
    }

    PersistedSupplyState::from_bytes(&bytes).context("Failed to parse supply state data")
}

fn save_to_file(path: &Path, data: &PersistedSupplyState) -> Result<()> {
    let bytes = data.to_bytes();
    let mut file = AtomicWriteFile::open(path).with_context(|| {
        format!(
            "Failed to open supply state file for writing: {}",
            path.display()
        )
    })?;
    file.write_all(&bytes)
        .with_context(|| format!("Failed to write supply state to {}", path.display()))?;
    file.commit()
        .with_context(|| format!("Failed to commit supply state to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::AtomicU64;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_supply_state() -> SupplyState {
        create_test_supply_state_with_backfill(None, U256::zero())
    }

    fn create_test_supply_state_with_backfill(
        backfill_height: Option<u64>,
        backfill_value: U256,
    ) -> SupplyState {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("supply_state_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        SupplyState {
            inner: RwLock::new(SupplyStateData::default()),
            ready: AtomicBool::new(false),
            first_migration_notify: Notify::new(),
            persisted_backfill_height: backfill_height,
            persisted_backfill_value: backfill_value,
            state_file,
        }
    }

    fn arb_u256_bytes() -> impl Strategy<Value = [u8; 32]> {
        any::<[u8; 32]>()
    }

    proptest! {
        #[test]
        fn persisted_state_roundtrip(
            backfill_height in proptest::option::of(any::<u64>()),
            value_bytes in arb_u256_bytes(),
            first_migration in proptest::option::of(any::<u64>())
        ) {
            let data = PersistedSupplyState {
                backfill_height,
                backfill_value: U256::from_le_bytes(value_bytes),
                first_migration_height: first_migration,
            };
            let bytes = data.to_bytes();
            let decoded = PersistedSupplyState::from_bytes(&bytes).unwrap();
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
        assert!(PersistedSupplyState::from_bytes(&short).is_err());

        let long = vec![0_u8; STATE_SIZE + 1];
        assert!(PersistedSupplyState::from_bytes(&long).is_err());
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
    fn add_block_reward_rejects_overlap_with_persisted_backfill() {
        let state = create_test_supply_state_with_backfill(Some(49), U256::from(5000));

        // First migration at height 49 (equal to persisted backfill) should fail
        let result = state.add_block_reward(49, U256::from(100));
        assert!(result.is_err());

        // First migration at height 48 (below persisted backfill) should fail
        let state = create_test_supply_state_with_backfill(Some(49), U256::from(5000));
        let result = state.add_block_reward(48, U256::from(100));
        assert!(result.is_err());

        // First migration at height 50 (above persisted backfill) should succeed
        let state = create_test_supply_state_with_backfill(Some(49), U256::from(5000));
        let result = state.add_block_reward(50, U256::from(100));
        assert!(result.is_ok());
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

        // historical_sum is 1000, persisted_backfill_value is 0
        // Total added = 0 + 1000 = 1000
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
    fn restart_resets_runtime_state_but_loads_backfill_point() {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!("supply_state_restart_test_{}", counter));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state_file = temp_dir.join(FILE_NAME);

        // First run: complete backfill
        {
            let state = SupplyState {
                inner: RwLock::new(SupplyStateData::default()),
                ready: AtomicBool::new(false),
                first_migration_notify: Notify::new(),
                persisted_backfill_height: None,
                persisted_backfill_value: U256::zero(),
                state_file: state_file.clone(),
            };

            state.add_block_reward(10, U256::from(100)).unwrap();
            state.add_block_reward(11, U256::from(200)).unwrap();
            state
                .add_historical_sum_and_mark_ready(U256::from(500))
                .unwrap();

            assert!(state.is_ready());
            assert_eq!(state.cumulative_emitted(), U256::from(800)); // 100 + 200 + 500
        }

        // Verify persisted state
        let persisted = load_from_file(&state_file).unwrap();
        assert_eq!(persisted.backfill_height, Some(9)); // first_migration(10) - 1
        assert_eq!(persisted.backfill_value, U256::from(500));
        assert_eq!(persisted.first_migration_height, Some(10));

        // Second run: simulate restart - runtime resets but backfill point loaded
        let state = SupplyState {
            inner: RwLock::new(SupplyStateData::default()), // Reset runtime
            ready: AtomicBool::new(false),                  // Reset ready
            first_migration_notify: Notify::new(),
            persisted_backfill_height: persisted.backfill_height,
            persisted_backfill_value: persisted.backfill_value,
            state_file,
        };

        // Runtime state is reset
        assert!(!state.is_ready());
        assert_eq!(state.height(), 0);
        assert_eq!(state.cumulative_emitted(), U256::zero());
        assert_eq!(state.first_migration_height(), None);

        // But backfill point is loaded
        assert_eq!(state.persisted_backfill_height(), Some(9));
        assert_eq!(state.persisted_backfill_value(), U256::from(500));
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

        // Second call should be a no-op
        state
            .add_historical_sum_and_mark_ready(U256::from(999))
            .unwrap();

        assert_eq!(state.cumulative_emitted(), U256::from(600));
    }

    #[test]
    fn backfill_with_previous_persisted_value() {
        // Simulate restart where we already have some backfill completed
        let state = create_test_supply_state_with_backfill(Some(49), U256::from(5000));

        // New migration comes in at height 100
        state.add_block_reward(100, U256::from(100)).unwrap();

        // Backfill adds sum for heights 50-99 (new historical_sum)
        let new_backfill_sum = U256::from(5000); // heights 50-99
        state
            .add_historical_sum_and_mark_ready(new_backfill_sum)
            .unwrap();

        // Total = migration(100) + persisted_backfill(5000) + new_backfill(5000) = 10100
        assert_eq!(state.cumulative_emitted(), U256::from(10100));
        assert!(state.is_ready());
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
