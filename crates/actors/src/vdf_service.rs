use actix::prelude::*;
use futures::future::join_all;
use irys_database::block_header_by_hash;
use irys_vdf::vdf_state::{AtomicVdfState, VdfState, VdfStepsReadGuard};
use rayon::prelude::*;
use reth::rpc::api::eth::helpers::block;
use reth_db::Database;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use tokio::task::{self, JoinHandle};
use tracing::info;

use crate::block_index_service::BlockIndexReadGuard;
use crate::services::Stop;
use irys_types::{block_production::Seed, Config, DatabaseProvider, IrysBlockHeader};

#[derive(Debug, Default)]
pub struct VdfService {
    pub vdf_state: AtomicVdfState,
}

impl VdfService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn new(block_index: BlockIndexReadGuard, db: DatabaseProvider, config: &Config) -> Self {
        let vdf_state = create_state(block_index, db, &config);

        Self {
            vdf_state: Arc::new(RwLock::new(vdf_state)),
        }
    }

    #[cfg(any(feature = "test-utils", test))]
    pub fn from_capacity(capacity: usize) -> Self {
        VdfService {
            vdf_state: Arc::new(RwLock::new(VdfState {
                global_step: 0,
                max_seeds_num: capacity,
                seeds: VecDeque::with_capacity(capacity),
            })),
        }
    }
}

fn create_state(
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
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
            max_seeds_num: capacity,
        };
    };

    info!("No block index found, initializing VdfState from zero");
    VdfState {
        global_step: 0,
        seeds: VecDeque::with_capacity(capacity),
        max_seeds_num: capacity,
    }
}

#[allow(dead_code)]
async fn create_state_parallel(
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);

    let mut height = block_index.read().latest_height();
    let mut steps_remaining = capacity;

    let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
    let mut global_step_number: u64 = 0;
    let n = 50; // number of block headers read in parallel

    while steps_remaining > 0 && height > 0 {
        // get in parallel n blocks
        info!("reading from {} to {}", height.saturating_sub(n), height);
        let mut blocks_handles: Vec<JoinHandle<IrysBlockHeader>> = Vec::new();
        for block in height.saturating_sub(n)..height {
            let db = db.clone();
            let block_index = block_index.clone();
            let join = task::spawn_blocking(move || {
                let block_hash = block_index
                    .read()
                    .get_item(block.try_into().expect("usize overflow"))
                    .map(|item| item.block_hash)
                    .expect("Error getting block hash");
                db.view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))
                    .expect("Db error reading block header by hash")
                    .expect("Block hash not found")
            });
            blocks_handles.push(join);
        }

        // Wait for all tasks to complete
        let results = join_all(blocks_handles).await;

        'outer: for block in results.iter().rev() {
            let block = block.as_ref().expect("Error getting block");
            if global_step_number == 0 {
                global_step_number = block.vdf_limiter_info.global_step_number;
            }
            for step in block.vdf_limiter_info.steps.0.iter().rev() {
                seeds.push_front(Seed(*step));
                steps_remaining -= 1;
                if steps_remaining == 0 {
                    break 'outer;
                }
            }
        }
        height = height.saturating_sub(n + 1);
    }
    info!(
        "Initializing vdf service from block's info in step number {} to {}",
        global_step_number,
        global_step_number - capacity as u64
    );
    return VdfState {
        global_step: global_step_number,
        seeds,
        max_seeds_num: capacity,
    };
}

pub fn calc_capacity(config: &Config) -> usize {
    const DEFAULT_CAPACITY: usize = 10_000;
    let capacity = std::cmp::max(
        DEFAULT_CAPACITY,
        (config.num_chunks_in_partition / config.num_chunks_in_recall_range)
            .try_into()
            .unwrap(),
    );

    capacity
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

impl Handler<Stop> for VdfService {
    type Result = ();

    fn handle(&mut self, _msg: Stop, ctx: &mut Context<Self>) -> Self::Result {
        ctx.stop();
    }
}

// Tests
#[cfg(test)]
mod tests {
    use std::time::Instant;

    use actix::SystemRegistry;
    use irys_config::IrysNodeConfig;
    use irys_database::{
        open_or_create_db, tables::IrysTables, BlockIndex, DataLedger, Initialized,
    };
    use irys_storage::ii;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{H256List, IrysBlockHeader, StorageConfig, H256};

    use crate::{
        block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
        BlockFinalizedMessage,
    };

    use super::*;

    #[actix_rt::test]
    async fn test_vdf() {
        let testnet_config = Config::testnet();
        let service = VdfService::from_capacity(calc_capacity(&testnet_config));
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

    #[actix_rt::test]
    async fn test_create_state_performance() {
        let testnet_config = Config {
            num_chunks_in_partition: 51_872_000, // testnet.toml numbers
            num_chunks_in_recall_range: 800,
            ..Config::testnet()
        };

        // Create a storage config for testing
        let storage_config = StorageConfig::new(&testnet_config);

        let tmp_dir = setup_tracing_and_temp_dir(Some("create_state_test"), false);
        let base_path = tmp_dir.path().to_path_buf();

        let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let database_provider = DatabaseProvider(Arc::new(db));

        let arc_config = Arc::new(IrysNodeConfig {
            base_directory: base_path.clone(),
            ..IrysNodeConfig::default()
        });

        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

        let now = Instant::now();
        // index and store in db blocks
        let mut height = 0;
        let capacity = calc_capacity(&testnet_config) as u64;
        while height <= capacity {
            new_epoch_block.height = height;
            new_epoch_block.previous_block_hash = new_epoch_block.block_hash;
            new_epoch_block.block_hash = H256::random();

            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => (), // debug!("block indexed"),
                Err(err) => panic!("Failed to index block {:?}", err),
            }

            database_provider
                .update_eyre(|tx| irys_database::insert_block_header(tx, &new_epoch_block))
                .unwrap();
            height += 1;
        }
        let elapsed = now.elapsed();
        println!("Indexed: {} blocks in {:.2?}", height, elapsed);

        let now = Instant::now();
        let vdf_state = create_state(
            block_index_guard.clone(),
            database_provider.clone(),
            &testnet_config,
        );
        let elapsed = now.elapsed();
        println!("VdfService vdf steps initialization time: {:.2?}", elapsed);

        let now = Instant::now();
        let vdf_state_parallel =
            create_state_parallel(block_index_guard, database_provider, &testnet_config).await;
        let elapsed = now.elapsed();
        println!(
            "VdfService vdf steps initialization time in parallel: {:.2?}",
            elapsed
        );

        assert_eq!(vdf_state.max_seeds_num, vdf_state_parallel.max_seeds_num);
        assert_eq!(vdf_state.global_step, vdf_state_parallel.global_step);
        assert_eq!(vdf_state.seeds, vdf_state_parallel.seeds);
    }
}
