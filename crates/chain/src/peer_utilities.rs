use crate::arbiter_handle::{ArbiterHandle, CloneableJoinHandle};
use crate::vdf::run_vdf;
use actix::{Actor, Addr, Arbiter, System, SystemRegistry};
use actix_web::dev::Server;
use base58::ToBase58;
use irys_actors::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    block_index_service::{BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerActor,
    block_tree_service::BlockTreeReadGuard,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    cache_service::ChunkCacheService,
    chunk_migration_service::ChunkMigrationService,
    ema_service::EmaService,
    epoch_service::{EpochServiceActor, EpochServiceConfig, GetPartitionAssignmentsGuardMessage},
    mempool_service::{MempoolService, TxIngressMessage},
    mining::PartitionMiningActor,
    packing::PackingConfig,
    packing::{PackingActor, PackingRequest},
    peer_list_service::PeerListService,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    validation_service::ValidationService,
    vdf_service::{GetVdfStateMessage, VdfService},
    ActorAddresses, BlockFinalizedMessage,
};
use irys_api_server::{create_listener, run_server, ApiState};
use irys_config::{IrysNodeConfig, StorageSubmodulesConfig};
use irys_database::{
    add_genesis_commitments, database, get_genesis_commitments, insert_commitment_tx,
    migration::check_db_version_and_run_migrations_if_needed, tables::IrysTables, BlockIndex,
    BlockIndexItem, DataLedger, Initialized,
};
use irys_gossip_service::{GossipResult, ServiceHandleWithShutdownSignal};
use irys_price_oracle::{mock_oracle::MockOracle, IrysPriceOracle};

pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};
use irys_storage::{
    irys_consensus_data_db::open_or_create_irys_consensus_data_db,
    reth_provider::{IrysRethProvider, IrysRethProviderInner},
    ChunkProvider, ChunkType, StorageModule,
};

use irys_types::{
    app_state::DatabaseProvider, block::CombinedBlockHeader, calculate_initial_difficulty,
    vdf_config::VDFStepsConfig, CommitmentTransaction, Config, DifficultyAdjustmentConfig,
    GossipData, IrysBlockHeader, IrysTransactionHeader, OracleConfig, PartitionChunkRange,
    StorageConfig, H256,
};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c_or_channel_message};
use reth_db::{Database as _, HasName, HasTableType};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::atomic::AtomicU64,
    sync::{mpsc, Arc, RwLock},
    thread::{self, sleep, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::{Handle, Runtime},
    sync::oneshot::{self},
    sync::Mutex,
    time::Duration,
};
use tracing::{debug, error, info, warn};

pub async fn fetch_txn(
    peer: &SocketAddr,
    client: &awc::Client,
    txn_id: H256,
) -> Option<IrysTransactionHeader> {
    let url = format!("http://{}/v1/tx/{}", peer, txn_id);

    match client.get(url.clone()).send().await {
        Ok(mut response) => {
            if response.status().is_success() {
                match response.json::<Vec<IrysTransactionHeader>>().await {
                    Ok(txn) => {
                        //info!("Synced txn {} from {}", txn_id, &url);
                        let txn_header = txn.first().expect("valid txnid").clone();
                        Some(txn_header)
                    }
                    Err(e) => {
                        let msg = format!("Error reading body from {}: {}", &url, e);
                        warn!(msg);
                        None
                    }
                }
            } else {
                let msg = format!("Non-success from {}: {}", &url, response.status());
                warn!(msg);
                None
            }
        }
        Err(e) => {
            warn!("Request to {} failed: {}", &url, e);
            None
        }
    }
}

//TODO spread requests across peers
pub async fn fetch_block(
    peer: &SocketAddr,
    client: &awc::Client,
    block_index_item: &BlockIndexItem,
) -> Option<IrysBlockHeader> {
    let url = format!(
        "http://{}/v1/block/{}",
        peer,
        block_index_item.block_hash.0.to_base58(),
    );

    match client.get(url.clone()).send().await {
        Ok(mut response) => {
            if response.status().is_success() {
                match response.json::<CombinedBlockHeader>().await {
                    Ok(block) => {
                        info!("Got block from {}", &url);
                        let irys_block_header = block.irys.clone();
                        Some(irys_block_header)
                    }
                    Err(e) => {
                        let msg = format!("Error reading body from {}: {}", &url, e);
                        warn!(msg);
                        None
                    }
                }
            } else {
                let msg = format!("Non-success from {}: {}", &url, response.status());
                warn!(msg);
                None
            }
        }
        Err(e) => {
            warn!("Request to {} failed: {}", &url, e);
            None
        }
    }
}

/// Fetches a slice starting at `height` of size `limit` of the block index from a remote peer over HTTP.
pub async fn fetch_block_index(
    peer: &SocketAddr,
    client: &awc::Client,
    block_index: Arc<Mutex<VecDeque<BlockIndexItem>>>,
    height: u64,
    limit: u32,
) -> u64 {
    let url = format!(
        "http://{}/v1/block_index?height={}&limit={}",
        peer, height, limit
    );

    match client.get(url.clone()).send().await {
        Ok(mut response) => {
            if response.status().is_success() {
                match response.json::<Vec<BlockIndexItem>>().await {
                    Ok(remote_block_index) => {
                        info!("Got block_index {},{} from {}", height, limit, &url);
                        let new_block_count = remote_block_index
                            .len()
                            .try_into()
                            .expect("try into should succeed as u64");
                        let mut index = block_index.lock().await;
                        index.extend(remote_block_index.into_iter());
                        return new_block_count;
                    }
                    Err(e) => {
                        warn!("Error reading body from {}: {}", &url, e);
                    }
                }
            } else {
                warn!(
                    "fetch_block_index Non-success from {}: {}",
                    &url,
                    response.status()
                );
            }
        }
        Err(e) => {
            warn!("Request to {} failed: {}", &url, e);
        }
    }
    0
}

//TODO url paths as ENUMS? Could update external api tests too
//#[tracing::instrument(err)]
pub async fn sync_state_from_peers(
    trusted_peers: Vec<SocketAddr>,
    block_discovery_addr: Addr<BlockDiscoveryActor>,
    mempool_addr: Addr<MempoolService>,
) -> eyre::Result<()> {
    let client = awc::Client::default();
    let peers = Arc::new(Mutex::new(trusted_peers.clone()));

    // lets give the local api a few second to load...
    sleep(Duration::from_millis(15000));

    //initialize queue
    let block_queue: Arc<tokio::sync::Mutex<VecDeque<BlockIndexItem>>> =
        Arc::new(Mutex::new(VecDeque::new()));

    info!("Discovering peers...");
    if let Some(new_peers_found) =
        fetch_and_update_peers(peers.clone(), &client, trusted_peers).await
    {
        info!("Discovered {new_peers_found} new peers");
    }

    info!("Downloading block index...");
    let peers_guard = peers.lock().await;
    let peer = peers_guard.first().expect("at least one peer");
    let mut height = 1; // start at height 1 as we already have the genesis block
    let limit = 50;
    loop {
        let fetched = fetch_block_index(peer, &client, block_queue.clone(), height, limit).await;
        if fetched == 0 {
            break; // no more blocks
        } else {
            info!("fetched {fetched} block index items");
        }
        height += fetched;
    }

    info!("Fetching latest blocks...and corresponding txns");
    let peer = peers_guard.first().expect("at least one peer");
    while let Some(block_index_item) = block_queue.lock().await.pop_front() {
        if let Some(irys_block) = fetch_block(peer, &client, &block_index_item).await {
            let block = Arc::new(irys_block);
            let block_discovery_addr = block_discovery_addr.clone();
            //TODO: temporarily introducing a 2 second pause to allow vdf steps to be created. otherwise vdf steps try to be included that do not exist locally. This helps prevent against the following type of error:
            //      Error sending BlockDiscoveredMessage for block 3Yy6zT8as2P4n4A4xYtVL4oMfwsAzgBpFMdoUJ6UYKoy: Block validation error Unavailable requested range (6..=10). Stored steps range is (1..=8)
            sleep(Duration::from_millis(2000));

            //add txns from block to txn db
            for tx in block.data_ledgers[DataLedger::Submit].tx_ids.iter() {
                let tx_ingress_msg = TxIngressMessage(
                    fetch_txn(peer, &client, *tx)
                        .await
                        .expect("valid txn from http GET"),
                );
                if let Err(e) = mempool_addr.send(tx_ingress_msg).await {
                    error!("Error sending txn {:?} to mempool: {}", tx, e);
                }
            }

            if let Err(e) = block_discovery_addr
                .send(BlockDiscoveredMessage(block.clone()))
                .await?
            {
                error!(
                    "Error sending BlockDiscoveredMessage for block {}: {:?}\nOFFENDING BLOCK evm_block_hash: {}",
                    block_index_item.block_hash.0.to_base58(),
                    e,
                    block.clone().evm_block_hash,
                );
            }
        }
    }

    info!("Sync complete.");
    Ok(())
}

/// Fetches `peers` list from each `peers_to_ask` via http. Adds new entries to `peers`
pub async fn fetch_and_update_peers(
    peers: Arc<tokio::sync::Mutex<Vec<SocketAddr>>>,
    client: &awc::Client,
    peers_to_ask: Vec<SocketAddr>,
) -> Option<u64> {
    let futures = peers_to_ask.into_iter().map(|peer| {
        let client = client.clone();
        let peers = peers.clone();
        let url = format!("http://{}/v1/peer_list", peer);

        async move {
            match client.get(url.clone()).send().await {
                Ok(mut response) => {
                    if response.status().is_success() {
                        let Ok(new_peers) = response.json::<Vec<SocketAddr>>().await else {
                            warn!("Error reading json body from {}", &url);
                            return 0;
                        };

                        let mut peers_guard = peers.lock().await;
                        let existing: HashSet<_> = peers_guard.iter().cloned().collect();
                        let mut added = 0;
                        for p in new_peers {
                            if existing.contains(&p) {
                                continue;
                            }
                            peers_guard.push(p);
                            added += 1;
                        }
                        info!("Got {} peers from {}", &added, peer);
                        return added;
                    } else {
                        warn!(
                            "fetch_and_update_peers Non-success from {}: {}",
                            &url,
                            response.status()
                        );
                    }
                }
                Err(e) => {
                    warn!("Request to {} failed: {}", &url, e);
                }
            }
            0
        }
    });
    let results = futures::future::join_all(futures).await;
    Some(results.iter().sum())
}
