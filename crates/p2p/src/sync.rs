use crate::peer_list::PeerList;
use crate::{GossipError, GossipResult};
use base58::ToBase58 as _;
use irys_api_client::ApiClient;
use irys_types::{BlockIndexItem, BlockIndexQuery, NodeMode};
use rand::prelude::SliceRandom as _;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

const MAX_PROCESSING_BLOCKS_QUEUE_SIZE: usize = 100;

#[derive(Clone, Debug, Default)]
pub struct SyncState {
    syncing: Arc<AtomicBool>,
    sync_target_height: Arc<AtomicUsize>,
    highest_processed_block: Arc<AtomicUsize>,
}

impl SyncState {
    /// Creates a new SyncState with given syncing flag and sync_height = 0
    pub fn new(is_syncing: bool) -> Self {
        Self {
            syncing: Arc::new(AtomicBool::new(is_syncing)),
            sync_target_height: Arc::new(AtomicUsize::new(0)),
            highest_processed_block: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_is_syncing(&self, is_syncing: bool) {
        self.syncing.store(is_syncing, Ordering::Relaxed);
    }

    pub fn set_syncing_from(&self, height: usize) {
        self.set_is_syncing(true);
        self.set_sync_target_height(height);
    }

    pub fn finish_sync(&self) {
        self.set_is_syncing(false);
    }

    /// Returns whether the gossip service is currently syncing
    pub fn is_syncing(&self) -> bool {
        self.syncing.load(Ordering::Relaxed)
    }

    /// Waits for the sync flag to be set to false.
    #[must_use]
    pub async fn wait_for_sync(&self) {
        // If already synced, return immediately
        if !self.is_syncing() {
            return;
        }

        // Create a future that polls the sync state
        let syncing = Arc::clone(&self.syncing);
        tokio::spawn(async move {
            while syncing.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Sync checking task failed");
    }

    /// Sets the current sync height. During syncing, the gossip won't
    /// accept blocks higher than this height
    pub fn set_sync_target_height(&self, height: usize) {
        self.sync_target_height.store(height, Ordering::Relaxed);
    }

    /// Returns the current sync height
    pub fn sync_target_height(&self) -> usize {
        self.sync_target_height.load(Ordering::Relaxed)
    }

    /// Increments sync height by 1 and returns the new height
    pub fn increment_sync_target_height(&self) -> usize {
        self.sync_target_height.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// [`crate::block_pool::BlockPool`] marks block as processed once the
    /// BlockDiscovery finished the pre-validation and scheduled the block for full validation
    pub fn mark_processed(&self, height: usize) {
        let current_height = self.highest_processed_block.load(Ordering::Relaxed);
        if height > current_height {
            self.highest_processed_block
                .store(height, Ordering::Relaxed);
        }
    }

    /// Highest pre-validated block height. Set by the [`crate::block_pool::BlockPool`]
    pub fn highest_processed_block(&self) -> usize {
        self.highest_processed_block.load(Ordering::Relaxed)
    }

    /// Checks if more blocks can be scheduled for validation by checking the
    /// number of blocks scheduled for validation so far versus the highest block
    /// marked by [`crate::block_pool::BlockPool`] after pre-validation
    pub fn is_queue_full(&self) -> bool {
        // We already past the sync target height, so there's nothing in the queue
        //  scheduled by the sync task specifically (gossip still can schedule blocks)
        if self.highest_processed_block() > self.sync_target_height() {
            return false;
        }

        self.sync_target_height() - self.highest_processed_block()
            >= MAX_PROCESSING_BLOCKS_QUEUE_SIZE
    }

    /// Waits until the length of the validation queue is less than the maximum
    /// allowed size.
    pub async fn wait_for_an_empty_queue_slot(&self) {
        while self.is_queue_full() {
            tokio::time::sleep(Duration::from_millis(100)).await
        }
    }

    /// Waits for the highest pre-validated block to reach target sync height
    pub async fn wait_for_processed_block_to_reach_target(&self) {
        // If already synced, return immediately
        if !self.is_syncing() {
            return;
        }

        // Create a future that polls the sync state
        let target = Arc::clone(&self.sync_target_height);
        let highest_processed_block = Arc::clone(&self.highest_processed_block);
        tokio::spawn(async move {
            while target.load(Ordering::Relaxed) > highest_processed_block.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Sync checking task failed");
    }
}

pub async fn sync_chain(
    sync_state: SyncState,
    api_client: impl ApiClient,
    peer_list: impl PeerList,
    node_mode: &NodeMode,
    start_sync_from_height: usize,
    genesis_peer_discovery_timeout_millis: u64,
) -> Result<(), GossipError> {
    sync_state.set_syncing_from(start_sync_from_height);
    let is_in_genesis_mode = matches!(node_mode, NodeMode::Genesis);

    debug!("Sync task: Starting a chain sync task, waiting for active peers. Mode: {:?}, starting from height: {}", node_mode, sync_state.sync_target_height());

    if is_in_genesis_mode && sync_state.sync_target_height() <= 1 {
        debug!("Sync task: The node is a genesis node with no blocks, skipping the sync task");
        sync_state.finish_sync();
        return Ok(());
    }

    let fetch_index_from_the_trusted_peer = !is_in_genesis_mode;
    if is_in_genesis_mode {
        warn!("Because the node is a genesis node, waiting for active peers for {}, and if no peers are added, then skipping the sync task", genesis_peer_discovery_timeout_millis);
        match timeout(
            Duration::from_millis(genesis_peer_discovery_timeout_millis),
            peer_list.wait_for_active_peers(),
        )
        .await
        {
            Ok(peer_list_result) => {
                peer_list_result?;
            }
            Err(elapsed) => {
                warn!("Sync task: Due to the node being in genesis mode, after waiting for active peers for {} and no peers showing up, skipping the sync task", elapsed);
                sync_state.finish_sync();
                return Ok(());
            }
        };
    } else {
        peer_list.wait_for_active_peers().await?;
    }

    debug!("Sync task: Syncing started");

    let limit = 10;

    let mut block_queue = VecDeque::new();
    let block_index = get_block_index(
        &peer_list,
        &api_client,
        sync_state.sync_target_height(),
        limit,
        5,
        fetch_index_from_the_trusted_peer,
    )
    .await?;

    let mut blocks_left_to_process = block_index.len();
    block_queue.extend(block_index);
    let no_new_blocks_to_process = block_queue.is_empty();

    while let Some(block) = block_queue.pop_front() {
        if sync_state.is_queue_full() {
            debug!("Sync task: Block queue is full, waiting for an empty slot");
            sync_state.wait_for_an_empty_queue_slot().await;
        }

        debug!(
            "Sync task: Requesting block {} (sync height is {}) from the network",
            block.block_hash.0.to_base58(),
            sync_state.sync_target_height()
        );
        match peer_list
            .request_block_from_the_network(block.block_hash)
            .await
        {
            Ok(()) => {
                sync_state.increment_sync_target_height();
                info!(
                    "Successfully requested block {} (sync height is {}) from the network",
                    block.block_hash.0.to_base58(),
                    sync_state.sync_target_height()
                );
            }
            Err(err) => {
                error!(
                    "Failed to request block {} (height {}) from the network: {}",
                    block.block_hash.0.to_base58(),
                    sync_state.sync_target_height(),
                    err
                );
            }
        }

        blocks_left_to_process -= 1;
        if blocks_left_to_process == 0 {
            block_queue.extend(
                get_block_index(
                    &peer_list,
                    &api_client,
                    sync_state.sync_target_height(),
                    limit,
                    5,
                    fetch_index_from_the_trusted_peer,
                )
                .await?,
            );
            blocks_left_to_process = block_queue.len();
            if blocks_left_to_process == 0 {
                break;
            }
        }
    }

    // If no new blocks were added to the index, nothing is going to mark
    //  the tip as processed
    if !no_new_blocks_to_process {
        sync_state.mark_processed(sync_state.sync_target_height());
    }
    sync_state.wait_for_processed_block_to_reach_target().await;
    sync_state.finish_sync();
    info!("Gossip service sync completed");
    Ok(())
}

async fn get_block_index(
    peer_list: &impl PeerList,
    api_client: &impl ApiClient,
    start: usize,
    limit: usize,
    retries: usize,
    fetch_from_the_trusted_peer: bool,
) -> GossipResult<Vec<BlockIndexItem>> {
    let peers_to_fetch_index_from = if fetch_from_the_trusted_peer {
        peer_list.top_trusted_peer().await?
    } else {
        peer_list.top_active_peers(Some(5), None).await?
    };

    if peers_to_fetch_index_from.is_empty() {
        return Err(GossipError::Network("No peers available".to_string()));
    }

    for _ in 0..retries {
        let (miner_address, top_peer) =
            peers_to_fetch_index_from
                .choose(&mut rand::thread_rng())
                .ok_or(GossipError::Network("No peers available".to_string()))?;
        match api_client
            .get_block_index(
                top_peer.address.api,
                BlockIndexQuery {
                    height: start,
                    limit,
                },
            )
            .await
            .map_err(|network_error| GossipError::Network(network_error.to_string()))
        {
            Ok(index) => {
                debug!(
                    "Fetched block index from peer {:?}: {:?}",
                    miner_address, index
                );
                return Ok(index);
            }
            Err(error) => {
                error!(
                    "Failed to fetch block index from peer {:?}: {:?}",
                    miner_address, error
                );
                continue;
            }
        }
    }

    Err(GossipError::Network(
        "Failed to fetch block index from peer".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::util::{ApiClientStub, FakeGossipServer, MockRethServiceActor};
    use irys_types::BlockHash;

    mod catch_up_task {
        use super::*;
        use crate::peer_list::PeerListServiceWithClient;
        use actix::Actor as _;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::{
            Address, Config, DatabaseProvider, NodeConfig, PeerAddress, PeerListItem, PeerScore,
        };
        use std::sync::{Arc, Mutex};

        #[actix_web::test]
        async fn should_sync_and_change_status() -> eyre::Result<()> {
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let start_from = 10;
            let sync_state = SyncState::new(true);

            let db = DatabaseProvider(Arc::new(
                open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                    .expect("can't open temp dir"),
            ));

            let block_requests = Arc::new(Mutex::new(vec![]));
            let block_requests_clone = block_requests.clone();
            let fake_gossip_server = FakeGossipServer::new();
            let sync_state_clone = sync_state.clone();
            fake_gossip_server.set_on_block_data_request(move |block_hash| {
                let mut block_requests = block_requests.lock().unwrap();
                let requests_len = block_requests.len();
                block_requests.push(block_hash);

                // Simulating one false response so the block gets requested again
                if requests_len == 0 {
                    false
                } else {
                    sync_state_clone.mark_processed(start_from + requests_len);
                    true
                }
            });
            let fake_gossip_address = fake_gossip_server.spawn();
            let fake_peer_address = PeerAddress {
                gossip: fake_gossip_address,
                api: fake_gossip_address,
                execution: Default::default(),
            };

            let mut node_config = NodeConfig::testnet();
            node_config.trusted_peers = vec![fake_peer_address];
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub::new();
            let calls = Arc::new(Mutex::new(vec![]));
            let block_index_requests = calls.clone();
            api_client_stub.set_block_index_handler(move |query| {
                let mut calls_ref = calls.lock().unwrap();
                let calls_len = calls_ref.len();
                calls_ref.push(query);

                // Simulate process needing to make two calls
                if calls_len == 0 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(1),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else if calls_len == 1 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(2),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else {
                    Ok(vec![])
                }
            });

            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();
            let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
                db,
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
            );
            let peer_list = peer_list_service.start();
            peer_list
                .add_peer(
                    Address::repeat_byte(2),
                    PeerListItem {
                        reputation_score: PeerScore::new(100),
                        response_time: 0,
                        address: fake_peer_address,
                        last_seen: 0,
                        is_online: true,
                    },
                )
                .await
                .expect("to add peer");

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                api_client_stub.clone(),
                peer_list,
                &NodeMode::PeerSync,
                10,
                10,
            )
            .await
            .expect("to finish catching up");

            // There should be three calls total: two that got items and one that didn't
            let data_requests = block_index_requests.lock().unwrap();
            assert_eq!(data_requests.len(), 3);
            assert_eq!(data_requests[0].height, 10);
            assert_eq!(data_requests[1].height, 11);
            assert_eq!(data_requests[0].limit, 10);
            assert_eq!(data_requests[1].limit, 10);
            assert_eq!(data_requests[2].height, 12);
            assert_eq!(data_requests[2].limit, 10);

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            let block_requests = block_requests_clone.lock().unwrap();
            assert_eq!(block_requests.len(), 3);
            assert_eq!(block_requests[0], BlockHash::repeat_byte(1));
            // As the first call didn't return anything, the peer tries to fetch it once again
            assert_eq!(block_requests[1], BlockHash::repeat_byte(1));
            assert_eq!(block_requests[2], BlockHash::repeat_byte(2));

            Ok(())
        }
    }
}
