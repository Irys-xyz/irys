use actix::prelude::*;
use irys_database::block_header_by_hash;
use nodit::{interval::ii, InclusiveInterval, Interval};
use reth_db::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
    time::Duration,
};
use tokio::time::sleep;
use tracing::{info, warn};

use irys_types::{block_production::Seed, DatabaseProvider, H256List, CONFIG, H256};

use crate::block_index_service::BlockIndexReadGuard;

const FILE_NAME: &str = "vdf.dat";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    pub max_seeds_num: usize,
    pub seeds: VecDeque<Seed>,
}

impl VdfState {
    pub fn get_last_step_and_seed(&self) -> (u64, Option<Seed>) {
        (self.global_step, self.seeds.back().cloned())
    }

    /// Push new seed, and removing oldest one if is full
    pub fn push_step(&mut self, seed: Seed) {
        if self.seeds.len() >= self.max_seeds_num {
            self.seeds.pop_front();
        }

        self.global_step += 1;
        self.seeds.push_back(seed);
        info!(
            "Received seed: {:?} global step: {}",
            self.seeds.back().unwrap(),
            self.global_step
        );
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        let vdf_steps_len = self.seeds.len() as u64;

        let last_global_step = self.global_step;
        let first_global_step = last_global_step - vdf_steps_len + 1;

        if first_global_step > last_global_step {
            return Err(eyre::eyre!("No steps stored!"));
        }

        if !ii(first_global_step, last_global_step).contains_interval(&i) {
            return Err(eyre::eyre!(
                "Unavailable requested range ({}..={}). Stored steps range is ({}..={})",
                i.start(),
                i.end(),
                first_global_step,
                last_global_step
            ));
        }

        let start: usize = (i.start() - first_global_step).try_into()?;
        let end: usize = (i.end() - first_global_step).try_into()?;

        Ok(H256List(
            self.seeds
                .range(start..=end)
                .map(|seed| seed.0)
                .collect::<Vec<H256>>(),
        ))
    }
}

#[derive(Debug)]
pub struct VdfService {
    pub vdf_state: Arc<RwLock<VdfState>>,
}

impl Default for VdfService {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl VdfService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn new(block_index: Option<BlockIndexReadGuard>, db: Option<DatabaseProvider>) -> Self {
        let capacity = (CONFIG.num_chunks_in_partition / CONFIG.num_chunks_in_recall_range)
            .try_into()
            .unwrap();

        let latest_block_hash = if let Some(bi) = block_index {
            bi.read().get_latest_item().map(|item| item.block_hash)
        } else {
            None
        };

        let vdf_state = if let Some(block_hash) = latest_block_hash {
            if let Some(db) = db {
                let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
                let tx = db.tx().unwrap();

                let mut block = block_header_by_hash(&tx, &block_hash).unwrap().unwrap();
                let global_step_number = block.vdf_limiter_info.global_step_number;
                let mut steps_remaining = capacity;

                while steps_remaining > 0 && block.height > 0 {
                    // get all the steps out of the block
                    for step in block.vdf_limiter_info.steps.0.iter().rev() {
                        seeds.push_front(Seed(*step));
                        steps_remaining -= 1;
                        if steps_remaining == 0 {
                            break;
                        }
                    }
                    // get the previous block
                    block = block_header_by_hash(&tx, &block.previous_block_hash)
                        .unwrap()
                        .unwrap();
                }

                VdfState {
                    global_step: global_step_number,
                    seeds,
                    max_seeds_num: capacity,
                }
            } else {
                panic!("Can't initialize VdfService without a DatabaseProvider");
            }
        } else {
            VdfState {
                global_step: 0,
                seeds: VecDeque::with_capacity(capacity),
                max_seeds_num: capacity,
            }
        };

        Self {
            vdf_state: Arc::new(RwLock::new(vdf_state)),
        }
    }
}

impl Supervised for VdfService {}

impl SystemService for VdfService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {}
}

// Actor implementation
impl Actor for VdfService {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Vdf service started!");
    }
}

/// Send the most recent mining step to all the `PartitionMiningActors`
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct VdfSeed(pub Seed);

// Handler for SeedMessage
impl Handler<VdfSeed> for VdfService {
    type Result = ();

    fn handle(&mut self, msg: VdfSeed, _ctx: &mut Context<Self>) -> Self::Result {
        self.vdf_state.write().unwrap().push_step(msg.0);
    }
}

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct VdfStepsReadGuard(Arc<RwLock<VdfState>>);

impl VdfStepsReadGuard {
    /// Creates a new `ReadGard` for Ledgers
    pub const fn new(state: Arc<RwLock<VdfState>>) -> Self {
        Self(state)
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
                            warn!("Requested vdf steps range {:?} still unavailable, attempt: {}, reason: {:?}, waiting ...", &i, attempt, e),
                    };
            // should be similar to a yield
            sleep(Duration::from_millis(200)).await;
        }
        Err(eyre::eyre!(
            "Max. retries reached while waiting to get VDF steps!"
        ))
    }
}

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "VdfStepsReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetVdfStateMessage;

impl Handler<GetVdfStateMessage> for VdfService {
    type Result = VdfStepsReadGuard; // Return guard directly

    fn handle(&mut self, _msg: GetVdfStateMessage, _ctx: &mut Self::Context) -> Self::Result {
        VdfStepsReadGuard::new(self.vdf_state.clone())
    }
}

// Tests
#[cfg(test)]
mod tests {
    use super::*;

    #[actix_rt::test]
    async fn test_vdf() {
        let service = VdfService::new(None, None);
        service.vdf_state.write().unwrap().seeds = VecDeque::with_capacity(4);
        service.vdf_state.write().unwrap().max_seeds_num = 4;
        let addr = service.start();

        // Send 8 seeds 1,2..,8 (capacity is 4)
        for i in 0..8 {
            addr.send(VdfSeed(Seed(H256([(i + 1) as u8; 32]))))
                .await
                .unwrap();
        }

        let state = addr.send(GetVdfStateMessage).await.unwrap();

        let steps = state.read().seeds.iter().cloned().collect::<Vec<_>>();

        // Should only contain last 3 messages
        assert_eq!(steps.len(), 4);

        // Check last 4 seeds are stored
        for i in 0..4 {
            assert_eq!(steps[i], Seed(H256([(i + 5) as u8; 32])));
        }

        // range not stored
        let get_error = state.read().get_steps(ii(3, 5));
        assert!(get_error.is_err());

        // ok inner range
        let get = state.read().get_steps(ii(6, 7)).unwrap();
        assert_eq!(H256List(vec![H256([6; 32]), H256([7; 32])]), get);

        // complete stored range
        let get_all = state.read().get_steps(ii(5, 8)).unwrap();
        assert_eq!(
            H256List(vec![
                H256([5; 32]),
                H256([6; 32]),
                H256([7; 32]),
                H256([8; 32])
            ]),
            get_all
        );
    }
}
