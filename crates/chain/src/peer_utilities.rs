use irys_api_client::{ApiClient as _, IrysApiClient};
pub use irys_reth_node_bridge::node::{RethNode, RethNodeAddOns, RethNodeHandle, RethNodeProvider};
use irys_types::block::CombinedBlockHeader;
use irys_types::{
    BlockIndexItem, CommitmentTransaction, IrysBlockHeader, IrysTransactionResponse, H256,
};
use std::net::SocketAddr;
use tracing::warn;

pub async fn client_request(url: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    client.get(url).send().await.expect("client request")
}

pub async fn info_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/info")).await
}

pub async fn block_index_endpoint_request(
    address: &str,
    height: u64,
    limit: u64,
) -> reqwest::Response {
    client_request(&format!(
        "{}{}?height={}&limit={}",
        &address, "/v1/block-index", &height, &limit
    ))
    .await
}

pub async fn chunk_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/chunk/ledger/0/0")).await
}

pub async fn network_config_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/network/config")).await
}

pub async fn peer_list_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/peer-list")).await
}

pub async fn fetch_genesis_block(
    peer: &SocketAddr,
    client: &reqwest::Client,
) -> Option<IrysBlockHeader> {
    let url = format!("http://{}", peer);
    let response = block_index_endpoint_request(&url, 0, 1).await;

    let block_index_genesis = response
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");

    fetch_block(peer, client, block_index_genesis.first().unwrap()).await
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

#[tracing::instrument(level = "trace", skip_all, fields(peer.address = %peer, tx.id = %txn_id))]
pub async fn fetch_txn(
    peer: &SocketAddr,
    client: &reqwest::Client,
    txn_id: H256,
) -> Option<IrysTransactionResponse> {
    let url = format!("http://{}/v1/tx/{}", peer, txn_id);

    match client.get(&url).send().await {
        Ok(response) => match response.error_for_status() {
            Ok(ok) => match ok.json::<IrysTransactionResponse>().await {
                Ok(txn) => Some(txn),
                Err(e) => {
                    let msg = format!("Error reading body from {}: {}", &url, e);
                    warn!(msg);
                    None
                }
            },
            Err(e) => {
                let msg = format!(
                    "Non-success from {}: {}",
                    &url,
                    e.status().unwrap_or_default()
                );
                warn!(msg);
                None
            }
        },
        Err(e) => {
            warn!("Request to {} failed: {}", &url, e);
            None
        }
    }
}

//TODO spread requests across peers
#[tracing::instrument(level = "trace", skip_all, fields(peer.address = %peer, block.hash = %block_index_item.block_hash))]
pub async fn fetch_block(
    peer: &SocketAddr,
    client: &reqwest::Client,
    block_index_item: &BlockIndexItem,
) -> Option<IrysBlockHeader> {
    let url = format!("http://{}/v1/block/{}", peer, block_index_item.block_hash);
    match client.get(&url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => match ok.json::<CombinedBlockHeader>().await {
                Ok(combined) => Some(combined.irys),
                Err(e) => {
                    warn!("Error parsing block response {}: {}", &url, e);
                    None
                }
            },
            Err(e) => {
                warn!(
                    "Non-success from {}: {}",
                    &url,
                    e.status().unwrap_or_default()
                );
                None
            }
        },
        Err(e) => {
            warn!("Request to {} failed: {}", &url, e);
            None
        }
    }
}
