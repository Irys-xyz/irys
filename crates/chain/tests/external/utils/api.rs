use super::{client::RemoteNodeClient, types::*};
use eyre::Result;
use irys_api_server::API_VERSION;
use irys_types::{DataLedger, H256, U256};
use reqwest::Response;
use serde::de::DeserializeOwned;


/// Make a GET request to the API endpoint
async fn make_get_request(client: &RemoteNodeClient, endpoint: &str) -> Result<Response> {
    let url = format!("{}/{}/{}", client.url, API_VERSION, endpoint);
    client.http_client.get(&url).send().await.map_err(Into::into)
}

/// Make a GET request and parse the JSON response
async fn get_json<T: DeserializeOwned>(
    client: &RemoteNodeClient,
    endpoint: &str,
    error_msg: &str,
) -> Result<T> {
    let response = make_get_request(client, endpoint).await?;

    if !response.status().is_success() {
        return Err(eyre::eyre!("{}: {}", error_msg, response.status()));
    }

    response.json().await.map_err(Into::into)
}

/// Make a GET request with optional default value on failure
async fn get_json_with_default<T: DeserializeOwned>(
    client: &RemoteNodeClient,
    endpoint: &str,
) -> Result<Option<T>> {
    let response = make_get_request(client, endpoint).await?;

    if response.status().is_success() {
        Ok(Some(response.json().await?))
    } else {
        Ok(None)
    }
}

/// Convert DataLedger to API string representation
fn ledger_to_string(ledger: DataLedger) -> &'static str {
    match ledger {
        DataLedger::Publish => "publish",
        DataLedger::Submit => "submit",
    }
}


pub async fn fetch_genesis_info(client: &RemoteNodeClient) -> Result<GenesisResponse> {
    get_json(client, "genesis", "Failed to fetch genesis info").await
}

pub async fn fetch_network_config(client: &RemoteNodeClient) -> Result<NetworkConfigResponse> {
    get_json(client, "network/config", "Failed to fetch network config").await
}

pub async fn fetch_anchor(client: &RemoteNodeClient) -> Result<H256> {
    let anchor_resp: AnchorResponse = get_json(client, "anchor", "Failed to fetch anchor").await?;
    Ok(anchor_resp.anchor)
}

pub async fn fetch_data_price(
    client: &RemoteNodeClient,
    ledger: DataLedger,
    data_size: usize,
) -> Result<(U256, U256)> {
    let endpoint = format!("price/{}/{}", ledger_to_string(ledger), data_size);
    let price_resp: PriceResponse = get_json(client, &endpoint, "Failed to fetch price").await?;

    // Parse the string prices to U256
    use std::str::FromStr;
    let perm_fee = U256::from_str(&price_resp.perm_fee)?;
    let term_fee = U256::from_str(&price_resp.term_fee)?;

    Ok((perm_fee, term_fee))
}

pub async fn get_chain_height(client: &RemoteNodeClient) -> Result<u64> {
    let height_resp: ChainHeightResponse =
        get_json(client, "chain/height", "Failed to get chain height").await?;
    Ok(height_resp.height)
}

pub async fn get_storage_intervals(
    client: &RemoteNodeClient,
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: &str,
) -> Result<StorageIntervalsResponse> {
    let endpoint = format!(
        "storage/intervals/{}/{}/{}",
        ledger_to_string(ledger),
        slot_index,
        chunk_type
    );
    get_json(client, &endpoint, "Failed to get storage intervals").await
}

pub async fn get_ledger_summary(
    client: &RemoteNodeClient,
    node_id: &str,
    ledger: DataLedger,
) -> Result<LedgerSummary> {
    let ledger_str = ledger_to_string(ledger);
    let endpoint = format!("ledger/{}/{}/summary", ledger_str, node_id);

    match get_json_with_default(client, &endpoint).await? {
        Some(summary) => Ok(summary),
        None => {
            // Return default if not found
            Ok(LedgerSummary {
                node_id: node_id.to_string(),
                ledger_type: ledger_str.to_string(),
                assignment_count: 0,
            })
        }
    }
}

pub async fn check_transaction_status(
    client: &RemoteNodeClient,
    tx_id: &H256,
) -> Result<TransactionStatusResponse> {
    let endpoint = format!("tx/{:?}", tx_id);

    match get_json_with_default(client, &endpoint).await? {
        Some(status) => Ok(status),
        None => Ok(TransactionStatusResponse {
            status: "not_found".to_string(),
            block_height: None,
            confirmations: None,
        }),
    }
}

pub async fn get_node_info(client: &RemoteNodeClient) -> Result<NodeInfo> {
    get_json(client, "info", "Failed to get node info").await
}
