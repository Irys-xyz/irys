use actix::prelude::*;
use irys_database::block_header_by_hash;
use irys_vdf::vdf_state::{AtomicVdfState, VdfState, VdfStepsReadGuard};
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

#[derive(Debug)]
pub struct VdfService {
    pub vdf_state: AtomicVdfState,
}

impl Default for VdfService {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl VdfService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn new(block_index: Option<BlockIndexReadGuard>, db: Option<DatabaseProvider>) -> Self {
        let vdf_state = Self::create_state(block_index, db);

        Self {
            vdf_state: Arc::new(RwLock::new(vdf_state)),
        }
    }

    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn from_atomic_state(vdf_state: AtomicVdfState) -> Self {
        Self { vdf_state }
    }

    pub fn create_state(
        block_index: Option<BlockIndexReadGuard>,
        db: Option<DatabaseProvider>,
    ) -> VdfState {
        // set up a minimum cache size of 10_000 steps for testing purposes, chunks number can be very low in testing setups so may need more cached steps than strictly efficient sampling needs.
        let capacity = std::cmp::max(
            10_000,
            (CONFIG.num_chunks_in_partition / CONFIG.num_chunks_in_recall_range)
                .try_into()
                .unwrap(),
        );

        let latest_block_hash = if let Some(bi) = block_index {
            bi.read().get_latest_item().map(|item| item.block_hash)
        } else {
            None
        };

        if let Some(block_hash) = latest_block_hash {
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
                info!(
                    "Initializing vdf service from block's info in step number {}",
                    global_step_number
                );
                VdfState {
                    global_step: global_step_number,
                    seeds,
                    max_seeds_num: capacity,
                }
            } else {
                panic!("Can't initialize VdfService without a DatabaseProvider");
            }
        } else {
            info!("No block index found, initializing VdfState from zero");
            VdfState {
                global_step: 0,
                seeds: VecDeque::with_capacity(capacity),
                max_seeds_num: capacity,
            }
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
