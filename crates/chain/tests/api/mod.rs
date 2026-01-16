use irys_types::DataLedger;
mod api;
mod client;
mod external_api;
mod hardfork_tests;
mod pd_pricing_endpoint;
mod pricing_endpoint;
mod supply_endpoint;
mod tx;
mod tx_commitments;
mod tx_duplicates;

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

pub async fn price_endpoint_request(
    address: &str,
    ledger: DataLedger,
    data_size_bytes: u64,
) -> reqwest::Response {
    let ledger = u32::from(ledger);
    client_request(&format!("{address:}/v1/price/{ledger:}/{data_size_bytes:}")).await
}

pub async fn pd_fee_history_request(address: &str, block_count: u64) -> reqwest::Response {
    client_request(&format!(
        "{}/v1/price/pd/fee-history?blockCount={}",
        address, block_count
    ))
    .await
}

pub async fn network_config_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/network/config")).await
}

pub async fn peer_list_endpoint_request(address: &str) -> reqwest::Response {
    client_request(&format!("{}{}", &address, "/v1/peer-list")).await
}
