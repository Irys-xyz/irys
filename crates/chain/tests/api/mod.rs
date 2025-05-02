use base58::ToBase58;
use irys_database::DataLedger;
use irys_types::CommitmentTransaction;
use tracing::info;

mod api;
mod client;
mod external_api;
mod pricing_endpoint;
mod tx;
mod tx_commitments;

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

pub async fn price_endpoint_request(
    address: &str,
    ledger: DataLedger,
    data_size_bytes: u64,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    let ledger = u32::from(ledger);
    client_request(&format!("{address:}/v1/price/{ledger:}/{data_size_bytes:}")).await
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

pub async fn post_commitment_tx_request(address: &str, commitment_tx: &CommitmentTransaction) {
    info!("Posting Commitment TX: {}", commitment_tx.id.0.to_base58());

    let client = awc::Client::default();
    let response = client
        .post(format!("{}/v1/commitment_tx", address))
        .send_json(commitment_tx) // Send the commitment_tx as JSON in the request body
        .await
        .expect("client post failed");

    info!(
        "Response status: {}\n{}",
        response.status(),
        serde_json::to_string_pretty(&commitment_tx).unwrap()
    );
}
