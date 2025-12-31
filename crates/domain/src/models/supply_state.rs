use irys_database::{db::IrysDatabaseExt as _, highest_block_reward_height, sum_all_block_rewards};
use irys_types::{DatabaseProvider, U256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupplyStateData {
    pub height: Option<u64>,
    pub cumulative_emitted: U256,
}

impl Default for SupplyStateData {
    fn default() -> Self {
        Self {
            height: None,
            cumulative_emitted: U256::zero(),
        }
    }
}

#[derive(Debug)]
pub struct SupplyState {
    data: RwLock<SupplyStateData>,
    ready: AtomicBool,
    failed: AtomicBool,
}

impl Default for SupplyState {
    fn default() -> Self {
        Self::new()
    }
}

impl SupplyState {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(SupplyStateData::default()),
            ready: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    pub fn get(&self) -> SupplyStateData {
        *self.data.read().expect("supply state read lock poisoned")
    }

    pub fn height(&self) -> Option<u64> {
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

    pub fn add_block_reward(&self, height: u64, reward_amount: U256) -> eyre::Result<()> {
        let mut data = self.data.write().expect("supply state write lock poisoned");

        let expected_height = match data.height {
            None => 0,
            Some(h) => h.saturating_add(1),
        };

        if height != expected_height {
            eyre::bail!(
                "Supply state height mismatch: expected {}, got {}",
                expected_height,
                height
            );
        }

        data.height = Some(height);
        data.cumulative_emitted = data.cumulative_emitted.saturating_add(reward_amount);
        Ok(())
    }

    pub fn recalculate_and_mark_ready(&self, db: &DatabaseProvider) -> eyre::Result<()> {
        let (cumulative_emitted, height) = db.view_eyre(|tx| {
            let sum = sum_all_block_rewards(tx)?;
            let h = highest_block_reward_height(tx)?;
            Ok((sum, h))
        })?;

        let mut data = self.data.write().expect("supply state write lock poisoned");
        data.height = height;
        data.cumulative_emitted = cumulative_emitted;
        self.failed.store(false, Ordering::Release);
        self.ready.store(true, Ordering::Release);
        Ok(())
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
}

impl std::ops::Deref for SupplyStateReadGuard {
    type Target = SupplyState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn sequential_rewards_accumulate_correctly(rewards in prop::collection::vec(0_u64..1_000_000, 1..50)) {
            let state = SupplyState::new();

            let mut expected_cumulative = U256::zero();
            for (height, &reward) in rewards.iter().enumerate() {
                let reward_u256 = U256::from(reward);
                state.add_block_reward(height as u64, reward_u256).unwrap();
                expected_cumulative = expected_cumulative.saturating_add(reward_u256);
            }

            prop_assert_eq!(state.cumulative_emitted(), expected_cumulative);
            prop_assert_eq!(state.height(), Some((rewards.len() - 1) as u64));
        }
    }

    #[test]
    fn add_block_reward_rejects_non_sequential() {
        let state = SupplyState::new();
        state.add_block_reward(0, U256::from(100)).unwrap();

        let result = state.add_block_reward(2, U256::from(100));
        assert!(result.is_err());
    }

    #[test]
    fn add_block_reward_rejects_duplicate_genesis() {
        let state = SupplyState::new();
        state.add_block_reward(0, U256::from(100)).unwrap();

        let result = state.add_block_reward(0, U256::from(100));
        assert!(result.is_err());
    }
}
