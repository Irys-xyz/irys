use actix::Addr;
use base58::ToBase58;
use irys_actors::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    mempool_service::{MempoolService, TxIngressMessage},
    peer_list_service::{AddPeer, PeerListService},
};
use irys_database::{BlockIndexItem, DataLedger};
use irys_types::Address;

pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};

use irys_types::{
    block::CombinedBlockHeader, IrysBlockHeader, IrysTransactionHeader, PeerAddress, PeerListItem,
    H256,
};
use std::{
    collections::{HashSet, VecDeque},
    net::SocketAddr,
    sync::Arc,
    thread::sleep,
};
use tokio::{sync::Mutex, time::Duration};
use tracing::{error, info, warn};

pub async fn client_request(
    url: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    let client = awc::Client::default();
    client.get(url).send().await.expect("client request")
}

pub async fn info_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/info")).await
}

pub async fn block_index_endpoint_request(
    address: &str,
    height: u64,
    limit: u64,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!(
        "{}{}?height={}&limit={}",
        &address, "/v1/block_index", &height, &limit
    ))
    .await
}

pub async fn chunk_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/chunk/ledger/0/0")).await
}

pub async fn network_config_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/network/config")).await
}

pub async fn peer_list_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/peer_list")).await
}

pub async fn version_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/version")).await
}

pub async fn fetch_genesis_block(
    peer: &SocketAddr,
    client: &awc::Client,
) -> Option<Arc<IrysBlockHeader>> {
    let url = format!("http://{}", peer);
    let mut result_genesis = block_index_endpoint_request(&url, 0, 1).await;

    let block_index_genesis = result_genesis
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");

    let fetched_genesis_block = fetch_block(peer, &client, &block_index_genesis.get(0).unwrap())
        .await
        .unwrap();
    let fetched_genesis_block = Arc::new(fetched_genesis_block);
    Some(fetched_genesis_block)
}

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
    trusted_peers: Vec<PeerAddress>,
    block_discovery_addr: Addr<BlockDiscoveryActor>,
    mempool_addr: Addr<MempoolService>,
    peer_list_service_addr: Addr<PeerListService>,
) -> eyre::Result<()> {
    let client = awc::Client::default();
    let peers = Arc::new(Mutex::new(trusted_peers.clone()));

    // lets give the local api a few second to load...
    sleep(Duration::from_millis(15000));

    //initialize queue
    let block_queue: Arc<tokio::sync::Mutex<VecDeque<BlockIndexItem>>> =
        Arc::new(Mutex::new(VecDeque::new()));

    info!("Discovering peers...");
    if let Some(new_peers_found) = fetch_and_update_peers(
        peers.clone(),
        &client,
        trusted_peers,
        peer_list_service_addr.clone(),
    )
    .await
    {
        info!("Discovered {new_peers_found} new peers");
    }

    info!("Downloading block index...");
    let peers_guard = peers.lock().await;
    let peer = peers_guard.first().expect("at least one peer");
    let mut height = 1; // start at height 1 as we already have the genesis block
    let limit = 50;
    loop {
        let fetched =
            fetch_block_index(&peer.api, &client, block_queue.clone(), height, limit).await;
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
        if let Some(irys_block) = fetch_block(&peer.api, &client, &block_index_item).await {
            let block = Arc::new(irys_block);
            let block_discovery_addr = block_discovery_addr.clone();
            //TODO: temporarily introducing a 2 second pause to allow vdf steps to be created. otherwise vdf steps try to be included that do not exist locally. This helps prevent against the following type of error:
            //      Error sending BlockDiscoveredMessage for block 3Yy6zT8as2P4n4A4xYtVL4oMfwsAzgBpFMdoUJ6UYKoy: Block validation error Unavailable requested range (6..=10). Stored steps range is (1..=8)
            sleep(Duration::from_millis(2000));

            //add txns from block to txn db
            for tx in block.data_ledgers[DataLedger::Submit].tx_ids.iter() {
                let tx_ingress_msg = TxIngressMessage(
                    fetch_txn(&peer.api, &client, *tx)
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
    peers: Arc<tokio::sync::Mutex<Vec<PeerAddress>>>,
    client: &awc::Client,
    peers_to_ask: Vec<PeerAddress>,
    peer_list_service_addr: Addr<PeerListService>,
) -> Option<u64> {
    let futures = peers_to_ask.into_iter().map(|peer| {
        let client = client.clone();
        let peers = peers.clone();
        let url = format!("http://{}/v1/peer_list", peer.api);
        let peer_list_service_addr = peer_list_service_addr.clone();

        async move {
            match client.get(url.clone()).send().await {
                Ok(mut response) if response.status().is_success() => {
                    let Ok(new_peers) = response.json::<Vec<PeerAddress>>().await else {
                        warn!("Invalid JSON {:?} from {}", response.body().await, &url);
                        return 0;
                    };

                    info!("fetched {:?} peers from {url}: ", new_peers);

                    let mut peers_guard = peers.lock().await;
                    let existing: HashSet<_> = peers_guard.iter().cloned().collect();
                    let mut added = 0;
                    for p in new_peers {
                        if existing.contains(&p) {
                            continue;
                        }
                        peers_guard.push(p);
                        let peer_list_entry = PeerListItem {
                            address: p,
                            ..Default::default()
                        };
                        if let Err(e) = peer_list_service_addr
                            .send(AddPeer {
                                mining_addr: Address::random(),
                                peer: peer_list_entry,
                            })
                            .await
                        {
                            error!("Unable to send AddPeerMessage message {e}");
                        };
                        added += 1;
                    }
                    error!("Got {} peers from {}", &added, peer.api);
                    added
                }
                Ok(response) => {
                    warn!(
                        "fetch_and_update_peers Non-success from {}: {}",
                        &url,
                        response.status()
                    );
                    0
                }
                Err(e) => {
                    warn!("Request to {} failed: {}", &url, e);
                    0
                }
            }
        }
    });
    let results = futures::future::join_all(futures).await;
    Some(results.iter().sum())
}
