use base58::ToBase58;
use irys_actors::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    broadcast_mining_service::BroadcastMiningSeed,
    mempool_service::MempoolServiceMessage,
    vdf_service::VdfServiceMessage,
};
use irys_p2p::PeerListServiceFacade;
use irys_types::block::CombinedBlockHeader;
use irys_p2p::fast_forward_vdf_steps_from_block;
use irys_api_client::{ApiClient, IrysApiClient};
pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};
use irys_types::block::CombinedBlockHeader;
use irys_types::{
    BlockIndexItem, CommitmentTransaction, IrysBlockHeader, IrysTransactionResponse, H256,
};
use std::net::SocketAddr;
use tracing::{info, warn};

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

pub async fn fetch_genesis_block(
    peer: &SocketAddr,
    client: &awc::Client,
) -> Option<IrysBlockHeader> {
    let url = format!("http://{}", peer);
    let mut result_genesis = block_index_endpoint_request(&url, 0, 1).await;

    let block_index_genesis = result_genesis
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");

    let fetched_genesis_block = fetch_block(peer, &client, &block_index_genesis.get(0).unwrap())
        .await
        .unwrap();
    Some(fetched_genesis_block)
}

pub async fn fetch_genesis_commitments(
    peer: &SocketAddr,
    irys_block_header: &IrysBlockHeader,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let api_client = IrysApiClient::new();
    let system_txs: Vec<H256> = irys_block_header
        .system_ledgers
        .iter()
        .flat_map(|ledger| ledger.tx_ids.0.clone())
        .collect();

    Ok(api_client
        .get_transactions(*peer, &system_txs)
        .await?
        .into_iter()
        .filter_map(|tx| match tx {
            IrysTransactionResponse::Commitment(commitment_tx) => Some(commitment_tx),
            IrysTransactionResponse::Storage(_) => None,
        })
        .collect())
}

pub async fn fetch_txn(
    peer: &SocketAddr,
    client: &awc::Client,
    txn_id: H256,
) -> Option<IrysTransactionResponse> {
    let url = format!("http://{}/v1/tx/{}", peer, txn_id.0.to_base58());

    match client.get(url.clone()).send().await {
        Ok(mut response) => {
            if response.status().is_success() {
                match response.json::<IrysTransactionResponse>().await {
                    Ok(txn) => Some(txn),
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
