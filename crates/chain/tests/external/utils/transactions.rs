use super::{
    api::{check_transaction_status, fetch_anchor, fetch_data_price, fetch_network_config},
    client::RemoteNodeClient,
    signer::TestSigner,
    utils::generate_test_data,
};
use eyre::Result;
use irys_api_client::ApiClient;
use irys_types::{CommitmentTransaction, DataLedger, DataTransaction, H256};
use serde::Serialize;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub async fn post_data_transaction(
    client: &RemoteNodeClient,
    signer: &TestSigner,
    data_size: usize,
) -> Result<DataTransaction> {
    info!(
        "Posting {} byte data transaction from {}",
        data_size, signer.name
    );

    // 1. Get anchor
    let anchor = fetch_anchor(client).await?;
    debug!("Got anchor: {:?}", anchor);

    // 2. Get price
    let (perm_fee, term_fee) = fetch_data_price(client, DataLedger::Publish, data_size).await?;
    debug!("Got fees - perm: {}, term: {}", perm_fee, term_fee);

    // 3. Create data
    let data = generate_test_data(data_size);

    // 4. Create and sign transaction using IrysSigner
    let tx =
        signer
            .irys_signer
            .create_publish_transaction(data.clone(), anchor, perm_fee, term_fee)?;

    let signed_tx = signer.irys_signer.sign_transaction(tx)?;
    info!("Created transaction: {:?}", signed_tx.header.id);

    // 5. Post transaction header
    post_transaction_header(client, &signed_tx).await?;
    info!("Posted transaction header");

    // 6. TODO: post chunks
    info!("Transaction has {} merkle nodes", signed_tx.chunks.len());

    // 7. Wait briefly for transaction to be processed
    sleep(Duration::from_secs(2)).await;

    // 8. Check transaction status
    match check_transaction_status(client, &signed_tx.header.id).await {
        Ok(status) => {
            info!("Transaction status: {}", status.status);
            if let Some(height) = status.block_height {
                info!("Transaction included in block {}", height);
            }
        }
        Err(e) => {
            warn!("Could not check transaction status: {}", e);
        }
    }

    Ok(signed_tx)
}

pub async fn post_stake_commitment(
    client: &RemoteNodeClient,
    signer: &TestSigner,
) -> Result<CommitmentTransaction> {
    info!("Posting stake commitment from {}", signer.name);

    // 1. Get anchor
    let anchor = fetch_anchor(client).await?;

    // 2. Get consensus config
    let config_resp = fetch_network_config(client).await?;

    // Convert response to ConsensusConfig for stake transaction
    let config = super::utils::create_consensus_config_from_response(&config_resp);

    // 3. Create stake transaction
    let commitment = CommitmentTransaction::new_stake(&config, anchor);

    // 4. Sign it
    let signed_commitment = signer.irys_signer.sign_commitment(commitment)?;
    info!("Created stake commitment: {:?}", signed_commitment.id);

    // 5. Post to node
    post_commitment_transaction(client, &signed_commitment).await?;
    info!("Posted stake commitment successfully");

    Ok(signed_commitment)
}

// TODO: Post pledge functionality
pub async fn post_pledge_commitments(
    _client: &RemoteNodeClient,
    signer: &TestSigner,
    count: usize,
) -> Result<Vec<CommitmentTransaction>> {
    warn!("Posting pledges isn't implemented yet ");
    info!("Skipping {} pledge commitments for {}", count, signer.name);
    Ok(Vec::new())
}

async fn post_transaction_header(client: &RemoteNodeClient, tx: &DataTransaction) -> Result<()> {
    let socket_addr = client.socket_addr()?;

    // Post transaction header via API client
    client
        .api_client
        .post_transaction(socket_addr, tx.header.clone())
        .await?;

    Ok(())
}

async fn post_commitment_transaction(
    client: &RemoteNodeClient,
    commitment: &CommitmentTransaction,
) -> Result<()> {
    let socket_addr = client.socket_addr()?;

    // Post commitment via API client
    client
        .api_client
        .post_commitment_transaction(socket_addr, commitment.clone())
        .await?;

    Ok(())
}

// TODO:  Use to post individual chunks
#[allow(dead_code)]
async fn post_chunk(
    client: &RemoteNodeClient,
    tx_id: &H256,
    index: usize,
    chunk_data: Vec<u8>,
) -> Result<()> {
    let url = format!("{}/v1/chunk", client.url);

    // Create chunk submission format
    #[derive(Serialize)]
    struct ChunkSubmission {
        tx_id: String,
        index: usize,
        data: Vec<u8>,
    }

    let submission = ChunkSubmission {
        tx_id: format!("{:?}", tx_id),
        index,
        data: chunk_data,
    };

    let response = client
        .http_client
        .post(&url)
        .json(&submission)
        .send()
        .await?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!(
            "Failed to post chunk {}: {}",
            index,
            error_text
        ));
    }

    Ok(())
}
