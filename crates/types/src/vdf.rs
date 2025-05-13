use crate::{block_production::Seed, H256List};
use std::{collections::VecDeque, sync::Arc};
use tokio::{
    sync::{RwLock, RwLockReadGuard},
    time::{sleep, Duration, Interval},
};

#[derive(Debug, Clone, Default)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
    /// whether the VDF thread is mining or paused
    pub mining_state_sender: Option<tokio::sync::mpsc::Sender<bool>>,
}

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct VdfStepsReadGuard(AtomicVdfState);

impl VdfStepsReadGuard {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(state: Arc<RwLock<VdfState>>) -> Self {
        Self(state)
    }

    pub fn into_inner_cloned(&self) -> AtomicVdfState {
        self.0.clone()
    }

    /// Read access to internal steps queue
    pub fn read(&self) -> RwLockReadGuard<'_, VdfState> {
        self.0.read().unwrap()
    }

    /// Try to read steps interval pooling a max. of 10 times waiting for interval to be available
    /// TODO @ernius: remove this method usage after VDF validation is done async, vdf steps validation reads VDF steps blocking last steps pushes so the need of this pooling.
    pub async fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        const MAX_RETRIES: i32 = 10;
        for attempt in 0..MAX_RETRIES {
            match self.read().get_steps(i) {
                        Ok(c) => return Ok(c),
                        Err(e) =>
                            tracing::warn!("Requested vdf steps range {:?} still unavailable, attempt: {}, reason: {:?}, waiting ...", &i, attempt, e),
                    };
            // should be similar to a yield
            sleep(Duration::from_millis(200)).await;
        }
        Err(eyre::eyre!(
            "Max. retries reached while waiting to get VDF steps!"
        ))
    }
}
