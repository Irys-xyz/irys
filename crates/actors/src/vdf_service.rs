use actix::prelude::*;
use irys_database::block_header_by_hash;
use irys_types::{block_production::Seed, Config, DatabaseProvider};
use irys_vdf::vdf_state::{AtomicVdfState, VdfState, VdfStepsReadGuard};
use reth_db::Database;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use tracing::{info, warn};

use crate::{block_index_service::BlockIndexReadGuard, services::Stop};

#[derive(Debug)]
pub struct VdfService {
    pub vdf_state: AtomicVdfState,
}

impl Default for VdfService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation of the `VdfService`");
    }
}

impl VdfService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn new(
        block_index: BlockIndexReadGuard,
        db: DatabaseProvider,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
        config: &Config,
    ) -> Self {
        let vdf_state = create_state(block_index, db, vdf_mining_state_sender, &config);
        Self {
            vdf_state: Arc::new(RwLock::new(vdf_state)),
        }
    }

    #[cfg(any(feature = "test-utils", test))]
    pub fn from_capacity(capacity: usize) -> Self {
        VdfService {
            vdf_state: Arc::new(RwLock::new(VdfState {
                global_step: 0,
                capacity,
                seeds: VecDeque::with_capacity(capacity),
                mining_state_sender: None,
            })),
        }
    }
}

/// create VDF state using the latest block in db
fn create_state(
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
    vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);

    if let Some(block_hash) = block_index
        .read()
        .get_latest_item()
        .map(|item| item.block_hash)
    {
        let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
        let tx = db.tx().unwrap();

        let mut block = block_header_by_hash(&tx, &block_hash, false)
            .unwrap()
            .unwrap();
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
            block = block_header_by_hash(&tx, &block.previous_block_hash, false)
                .unwrap()
                .unwrap();
        }
        info!(
            "Initializing vdf service from block's info in step number {}",
            global_step_number
        );
        return VdfState {
            global_step: global_step_number,
            seeds,
            capacity,
            mining_state_sender: Some(vdf_mining_state_sender),
        };
    };

    info!("No block index found, initializing VdfState from zero");
    VdfState {
        global_step: 0,
        seeds: VecDeque::with_capacity(capacity),
        capacity,
        mining_state_sender: Some(vdf_mining_state_sender),
    }
}

/// return the larger of MINIMUM_CAPACITY or number of seeds required for (chunks in partition / chunks in recall range)
/// This ensure the capacity of VecDeqeue is large enough for the partition.
pub fn calc_capacity(config: &Config) -> usize {
    const MINIMUM_CAPACITY: u64 = 10_000;
    let capacity_from_config: u64 =
        config.consensus.num_chunks_in_partition / config.consensus.num_chunks_in_recall_range;
    let capacity = if capacity_from_config < MINIMUM_CAPACITY {
        warn!(
            "capacity in config: {} set too low. Overridden with {}",
            capacity_from_config, MINIMUM_CAPACITY
        );
        MINIMUM_CAPACITY
    } else {
        std::cmp::max(MINIMUM_CAPACITY, capacity_from_config)
    };

    capacity.try_into().expect("expected u64 to cast to u32")
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
        self.vdf_state.write().unwrap().increment_step(msg.0);
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

/// pause the VDF thread via mpsc
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct StopMiningMessage;

impl Handler<StopMiningMessage> for VdfService {
    type Result = ();

    fn handle(&mut self, _msg: StopMiningMessage, _ctx: &mut Self::Context) -> Self::Result {
        let sender = self
            .vdf_state
            .read()
            .expect("expected to get read lock on vdf state")
            .mining_state_sender
            .clone()
            .expect("expected valid mining_state_sender");

        tokio::spawn(async move {
            let _ = sender.send(false).await;
        });
    }
}

/// start/resume the VDF thread via mpsc
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct StartMiningMessage;

impl Handler<StartMiningMessage> for VdfService {
    type Result = ();

    fn handle(&mut self, _msg: StartMiningMessage, _ctx: &mut Self::Context) -> Self::Result {
        let sender = self
            .vdf_state
            .read()
            .expect("expected to get read lock on vdf state")
            .mining_state_sender
            .clone()
            .expect("expected valid mining_state_sender");

        tokio::spawn(async move {
            let _ = sender.send(true).await;
        });
    }
}

impl Handler<Stop> for VdfService {
    type Result = ();

    fn handle(&mut self, _msg: Stop, ctx: &mut Context<Self>) -> Self::Result {
        ctx.stop();
    }
}

// Tests
#[cfg(test)]
mod tests {
    use irys_storage::ii;
    use irys_types::{H256List, NodeConfig, H256};

    use super::*;

    #[actix_rt::test]
    async fn test_vdf() {
        let testnet_config = NodeConfig::testnet().into();
        let service = VdfService::from_capacity(calc_capacity(&testnet_config));
        service.vdf_state.write().unwrap().seeds = VecDeque::with_capacity(4);
        service.vdf_state.write().unwrap().capacity = 4;
        let addr = service.start();

        // Send 8 seeds 1,2..,8 (capacity is 4)
        for i in 0..8 {
            addr.send(VdfSeed(Seed(H256([(i + 1) as u8; 32]))))
                .await
                .unwrap();
        }

        let state = addr.send(GetVdfStateMessage).await.unwrap();

        let steps = state.read().seeds.iter().cloned().collect::<Vec<_>>();

        // Should only contain last 4 seeds
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
