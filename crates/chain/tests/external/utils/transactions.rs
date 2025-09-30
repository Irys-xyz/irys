//! Transaction utilities for external integration tests
//!
//! # Test Environment Assumptions
//!
//! These utilities are designed for a 3-replica test environment with specific
//! configuration requirements:
//!
//! - **Fee Structure**: Permanent fees use 3x multiplier to cover ingress proof
//!   rewards (production uses different calculation)
//! - **Timing**: 2-second delays between operations to allow block production
//! - **Chunk Posting**: 10ms delays between chunks to prevent overwhelming nodes
//!
//! These assumptions are hardcoded as constants and should not be used in production.

use super::{
    api::{check_transaction_status, fetch_anchor, fetch_data_price, fetch_network_config},
    client::RemoteNodeClient,
    signer::TestSigner,
    utils::generate_test_data,
};
use eyre::Result;
use irys_api_client::ApiClient as _;
use irys_types::{Address, CommitmentTransaction, DataLedger, DataTransaction, PledgeDataProvider};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

const INGRESS_FEE_MULTIPLIER: u32 = 3;
const TX_PROCESSING_DELAY_SECS: u64 = 2;
const CHUNK_POST_DELAY_MS: u64 = 10;

pub(crate) async fn post_data_transaction(
    client: &RemoteNodeClient,
    signer: &TestSigner,
    data_size: usize,
) -> Result<DataTransaction> {
    info!(
        "Posting {} byte data transaction from {}",
        data_size, signer.name
    );

    let anchor = fetch_anchor(client).await?;
    debug!("Got anchor: {:?}", anchor);

    let (mut perm_fee, term_fee) = fetch_data_price(client, DataLedger::Publish, data_size)
        .await
        .map_err(|e| {
            warn!("Failed to fetch price: {}", e);
            e
        })?;

    // CRITICAL: Test environment requires 3x higher permanent fees to ensure
    // ingress proof rewards are covered. Production uses different calculation.
    perm_fee = perm_fee * INGRESS_FEE_MULTIPLIER;

    info!("Got fees - perm: {}, term: {}", perm_fee, term_fee);

    let data = generate_test_data(data_size);

    let tx =
        signer
            .irys_signer
            .create_publish_transaction(data.clone(), anchor, perm_fee, term_fee)?;

    let signed_tx = signer.irys_signer.sign_transaction(tx)?;
    info!("Created and signed transaction: {:?}", signed_tx.header.id);

    info!("About to post transaction header...");
    post_transaction_header(client, &signed_tx)
        .await
        .map_err(|e| {
            warn!("Failed to post transaction header: {}", e);
            e
        })?;
    info!("Successfully posted transaction header!");

    info!("Transaction has {} merkle nodes", signed_tx.chunks.len());
    post_transaction_chunks(client, &signed_tx)
        .await
        .map_err(|e| {
            warn!("Failed to post transaction chunks: {}", e);
            e
        })?;
    info!("Successfully posted all transaction chunks!");

    sleep(Duration::from_secs(TX_PROCESSING_DELAY_SECS)).await;
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

pub(crate) async fn post_stake_commitment(
    client: &RemoteNodeClient,
    signer: &TestSigner,
) -> Result<CommitmentTransaction> {
    info!("Posting stake commitment from {}", signer.name);

    let anchor = fetch_anchor(client).await.map_err(|e| {
        warn!("Failed to fetch anchor: {}", e);
        e
    })?;

    let config_resp = fetch_network_config(client).await.map_err(|e| {
        warn!("Failed to fetch network config: {}", e);
        e
    })?;

    let config = super::utils::create_consensus_config_from_response(&config_resp);

    let commitment = CommitmentTransaction::new_stake(&config, anchor);

    let signed_commitment = signer.irys_signer.sign_commitment(commitment)?;
    info!("Created stake commitment: {:?}", signed_commitment.id);

    post_commitment_transaction(client, &signed_commitment).await?;
    info!("Posted stake commitment successfully");

    Ok(signed_commitment)
}

pub(crate) async fn post_pledge_commitment(
    client: &RemoteNodeClient,
    signer: &TestSigner,
) -> Result<CommitmentTransaction> {
    post_pledge_commitment_with_count(client, signer, 0).await
}

pub(crate) async fn post_pledge_commitment_with_count(
    client: &RemoteNodeClient,
    signer: &TestSigner,
    pledge_count: u64,
) -> Result<CommitmentTransaction> {
    info!(
        "Posting pledge commitment {} from {}",
        pledge_count + 1,
        signer.name
    );

    let anchor = fetch_anchor(client).await.map_err(|e| {
        warn!("Failed to fetch anchor for pledge: {}", e);
        e
    })?;
    debug!("Got anchor: {:?}", anchor);

    let config_resp = fetch_network_config(client).await.map_err(|e| {
        warn!("Failed to fetch network config for pledge: {}", e);
        e
    })?;
    debug!("Got network config");
    let config = super::utils::create_consensus_config_from_response(&config_resp);

    let signer_address = signer.irys_signer.address();
    debug!("Using pledge count {} for testing", pledge_count);

    let commitment =
        CommitmentTransaction::new_pledge(&config, anchor, &pledge_count, signer_address).await;
    info!(
        "Created pledge with value: {} (count: {})",
        commitment.value, pledge_count
    );

    let signed_commitment = signer.irys_signer.sign_commitment(commitment)?;
    info!("Created pledge commitment: {:?}", signed_commitment.id);

    post_commitment_transaction(client, &signed_commitment).await?;
    info!("Posted pledge commitment successfully");

    Ok(signed_commitment)
}

async fn post_transaction_header(client: &RemoteNodeClient, tx: &DataTransaction) -> Result<()> {
    let socket_addr = client.socket_addr()?;

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

    client
        .api_client
        .post_commitment_transaction(socket_addr, commitment.clone())
        .await?;

    Ok(())
}

pub(crate) async fn post_chunk(
    client: &RemoteNodeClient,
    chunk: &irys_types::UnpackedChunk,
) -> Result<()> {
    let url = format!("{}/v1/chunk", client.url);

    let response = client.http_client.post(&url).json(&chunk).send().await?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!(
            "Failed to post chunk at offset {}: {}",
            chunk.tx_offset,
            error_text
        ));
    }

    Ok(())
}

pub(crate) async fn post_transaction_chunks(
    client: &RemoteNodeClient,
    tx: &DataTransaction,
) -> Result<()> {
    let chunks = tx.data_chunks()?;

    info!(
        "Posting {} chunks for transaction {:?}",
        chunks.len(),
        tx.header.id
    );

    for (idx, chunk) in chunks.iter().enumerate() {
        debug!(
            "Posting chunk {} of {} (offset: {})",
            idx + 1,
            chunks.len(),
            chunk.tx_offset
        );
        post_chunk(client, chunk).await?;

        if idx < chunks.len() - 1 {
            sleep(Duration::from_millis(CHUNK_POST_DELAY_MS)).await;
        }
    }

    info!("Successfully posted all {} chunks", chunks.len());
    Ok(())
}

/// Response from pledge price endpoint
#[derive(Debug, Deserialize)]
struct PledgePriceResponse {
    pledge_count: u64,
    #[expect(dead_code)]
    price: String,
}

/// A pledge provider that queries the remote node for pledge counts
struct RemotePledgeProvider {
    client: RemoteNodeClient,
    address: Address,
}

impl RemotePledgeProvider {
    fn new(client: RemoteNodeClient, address: Address) -> Self {
        Self { client, address }
    }

    async fn fetch_pledge_count(&self) -> Result<u64> {
        let url = format!(
            "{}/price/commitment/pledge/{}",
            self.client.url,
            format!("{:?}", self.address)
        );

        let response = self.client.http_client.get(&url).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Failed to fetch pledge count: {}", error_text));
        }

        let resp: PledgePriceResponse = response.json().await?;
        Ok(resp.pledge_count)
    }
}

#[async_trait::async_trait]
impl PledgeDataProvider for RemotePledgeProvider {
    async fn pledge_count(&self, _user_address: Address) -> u64 {
        // Use the address provided at construction time
        self.fetch_pledge_count().await.unwrap_or(0)
    }
}

/// Post multiple pledges for a signer
pub(crate) async fn post_multiple_pledges(
    client: &RemoteNodeClient,
    signer: &TestSigner,
    count: usize,
) -> Result<Vec<CommitmentTransaction>> {
    let mut pledges = Vec::new();
    for i in 0..count {
        info!("Posting pledge {} of {} for {}", i + 1, count, signer.name);
        match post_pledge_commitment_with_count(client, signer, i as u64).await {
            Ok(pledge) => {
                info!("{} pledge {} posted: {:?}", signer.name, i + 1, pledge.id);
                pledges.push(pledge);
            }
            Err(e) => {
                warn!("{} pledge {} failed: {}", signer.name, i + 1, e);
                // Continue with next pledge instead of failing entirely
                // This handles the case where some pledges might already exist
            }
        }
    }
    if pledges.is_empty() {
        Err(eyre::eyre!(
            "Failed to post any pledges for {}",
            signer.name
        ))
    } else {
        Ok(pledges)
    }
}

/// Stake and pledge a signer with multiple pledges
pub(crate) async fn stake_and_pledge_signer(
    client: &RemoteNodeClient,
    signer: &TestSigner,
    pledge_count: usize,
) -> Result<()> {
    // First stake
    post_stake_commitment(client, signer).await?;

    // Then post multiple pledges
    post_multiple_pledges(client, signer, pledge_count).await?;

    Ok(())
}

/// Wait for ingress proofs for transactions
pub(crate) async fn wait_for_ingress_proofs(
    client: &RemoteNodeClient,
    tx_ids: Vec<irys_types::H256>,
    timeout_secs: u64,
) -> Result<()> {
    info!(
        "Waiting for ingress proofs for {} transactions",
        tx_ids.len()
    );
    let start = std::time::Instant::now();

    while start.elapsed().as_secs() < timeout_secs {
        let mut all_confirmed = true;

        for tx_id in &tx_ids {
            let status = check_transaction_status(client, tx_id).await?;
            debug!("Transaction {:?} status: {}", tx_id, status.status);

            // Check if transaction has been confirmed (has block height)
            if status.block_height.is_none() || status.status != "confirmed" {
                all_confirmed = false;
                break;
            }
        }

        if all_confirmed {
            info!("All transactions have ingress proofs!");
            return Ok(());
        }

        debug!(
            "Still waiting for ingress proofs... ({} seconds elapsed)",
            start.elapsed().as_secs()
        );
        sleep(Duration::from_secs(2)).await;
    }

    Err(eyre::eyre!(
        "Timeout waiting for ingress proofs after {} seconds",
        timeout_secs
    ))
}
