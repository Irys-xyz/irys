//! API client utilities for external integration tests
//!
//! Provides wrapper functions for making HTTP requests to Irys node APIs.
//! All functions use the configured API version and handle common error cases.

use super::{
    client::RemoteNodeClient,
    types::{
        AnchorResponse, BlockHeightResponse, ChunkCountsResponse, EpochInfoResponse,
        GenesisResponse, LedgerSummary, NetworkConfigResponse, NodeInfo,
        PartitionAssignmentsResponse, PriceResponse, SlotReplica, SlotReplicaInfo,
        SlotReplicaSummary, StorageIntervalsResponse, TransactionStatusResponse,
    },
};
use eyre::Result;
use irys_api_server::API_VERSION;
use irys_types::{DataLedger, H256, U256};
use reqwest::Response;
use serde::de::DeserializeOwned;
use std::collections::HashMap;

/// Make a GET request to the API endpoint
async fn make_get_request(client: &RemoteNodeClient, endpoint: &str) -> Result<Response> {
    let url = format!("{}/{}/{}", client.url, API_VERSION, endpoint);
    client
        .http_client
        .get(&url)
        .send()
        .await
        .map_err(Into::into)
}

/// Make a GET request and parse the JSON response
async fn get_json<T: DeserializeOwned>(
    client: &RemoteNodeClient,
    endpoint: &str,
    error_msg: &str,
) -> Result<T> {
    let response = make_get_request(client, endpoint).await?;
    let status = response.status();

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "No response body".to_string());
        tracing::error!(
            "API error - endpoint: {}, status: {}, body: {}",
            endpoint,
            status,
            error_text
        );
        return Err(eyre::eyre!("{}: {} - {}", error_msg, status, error_text));
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

fn ledger_to_string(ledger: DataLedger) -> &'static str {
    match ledger {
        DataLedger::Publish => "Publish",
        DataLedger::Submit => "Submit",
        DataLedger::OneYear => "OneYear",
        DataLedger::ThirtyDay => "ThirtyDay",
    }
}

pub(crate) async fn fetch_genesis_info(client: &RemoteNodeClient) -> Result<GenesisResponse> {
    get_json(client, "genesis", "Failed to fetch genesis info").await
}

pub(crate) async fn fetch_network_config(
    client: &RemoteNodeClient,
) -> Result<NetworkConfigResponse> {
    get_json(client, "network/config", "Failed to fetch network config").await
}

pub(crate) async fn fetch_anchor(client: &RemoteNodeClient) -> Result<H256> {
    let anchor_resp: AnchorResponse = get_json(client, "anchor", "Failed to fetch anchor").await?;
    Ok(anchor_resp.block_hash)
}

pub(crate) async fn fetch_data_price(
    client: &RemoteNodeClient,
    ledger: DataLedger,
    data_size: usize,
) -> Result<(U256, U256)> {
    let ledger_id = ledger as u32;
    let endpoint = format!("price/{ledger_id}/{data_size}");
    tracing::info!("Fetching price from endpoint: {}", endpoint);
    let price_resp: PriceResponse = get_json(client, &endpoint, "Failed to fetch price").await?;

    // Parse the string prices to U256
    use std::str::FromStr as _;
    let perm_fee = U256::from_str(&price_resp.perm_fee)?;
    let term_fee = U256::from_str(&price_resp.term_fee)?;

    Ok((perm_fee, term_fee))
}

pub(crate) async fn get_chain_height(client: &RemoteNodeClient) -> Result<u64> {
    let height_resp: BlockHeightResponse =
        get_json(client, "block/latest", "Failed to get chain height").await?;
    Ok(height_resp.height)
}

pub(crate) async fn get_storage_intervals(
    client: &RemoteNodeClient,
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: &str,
) -> Result<StorageIntervalsResponse> {
    let ledger_str = ledger_to_string(ledger);
    let endpoint = format!("storage/intervals/{ledger_str}/{slot_index}/{chunk_type}");
    get_json(client, &endpoint, "Failed to get storage intervals").await
}

pub(crate) async fn get_ledger_summary(
    client: &RemoteNodeClient,
    node_id: &str,
    ledger: DataLedger,
) -> Result<LedgerSummary> {
    let ledger_str = ledger_to_string(ledger);
    let endpoint = format!("ledger/{ledger_str}/{node_id}/summary");

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

pub(crate) async fn check_transaction_status(
    client: &RemoteNodeClient,
    tx_id: &H256,
) -> Result<TransactionStatusResponse> {
    let endpoint = format!("tx/{tx_id:?}");

    match get_json_with_default(client, &endpoint).await? {
        Some(status) => Ok(status),
        None => Ok(TransactionStatusResponse {
            status: "not_found".to_string(),
            block_height: None,
            confirmations: None,
        }),
    }
}

#[expect(dead_code)]
pub(crate) async fn get_node_info(client: &RemoteNodeClient) -> Result<NodeInfo> {
    get_json(client, "info", "Failed to get node info").await
}

pub(crate) async fn get_partition_assignments(
    client: &RemoteNodeClient,
    node_id: &str,
    ledger: Option<DataLedger>,
) -> Result<PartitionAssignmentsResponse> {
    let endpoint = match ledger {
        Some(ledger) => {
            let ledger_str = ledger_to_string(ledger);
            format!("ledger/{ledger_str}/{node_id}/assignments")
        }
        None => format!("ledger/{node_id}/assignments"),
    };

    get_json(client, &endpoint, "Failed to get partition assignments").await
}

pub(crate) async fn get_all_assignments(
    client: &RemoteNodeClient,
    node_id: &str,
) -> Result<PartitionAssignmentsResponse> {
    get_partition_assignments(client, node_id, None).await
}

pub(crate) async fn get_current_epoch(client: &RemoteNodeClient) -> Result<EpochInfoResponse> {
    get_json(client, "epoch/current", "Failed to get current epoch").await
}

// Slot replica verification functions using existing APIs
pub(crate) async fn aggregate_slot_replicas(
    clients: &[RemoteNodeClient],
    node_addresses: &[&str],
) -> Result<SlotReplicaSummary> {
    use std::collections::HashMap;

    // Collect all partition assignments from all nodes
    let mut all_assignments = Vec::new();
    for node_addr in node_addresses.iter() {
        match get_all_assignments(&clients[0], node_addr).await {
            Ok(response) => {
                for assignment in response.assignments {
                    if let Some(slot_idx) = assignment.slot_index {
                        all_assignments.push((
                            (*node_addr).to_string(),
                            assignment
                                .ledger_id
                                .and_then(|id| match id {
                                    0 => Some(irys_types::DataLedger::Publish),
                                    1 => Some(irys_types::DataLedger::Submit),
                                    _ => None,
                                })
                                .unwrap_or(irys_types::DataLedger::Submit),
                            slot_idx,
                            assignment.partition_hash,
                        ));
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get assignments for node {}: {}", node_addr, e);
            }
        }
    }

    // Group by ledger and slot to count replicas
    let mut slot_map: HashMap<(irys_types::DataLedger, usize), Vec<(String, irys_types::H256)>> =
        HashMap::new();
    for (node_addr, ledger, slot_idx, hash) in all_assignments {
        slot_map
            .entry((ledger, slot_idx))
            .or_default()
            .push((node_addr, hash));
    }

    // Analyze slot replicas
    let expected_replicas = 3; // Standard for the test environment
    let mut slots = Vec::new();
    let mut fully_replicated = 0;
    let mut under_replicated = 0;
    let mut over_replicated = 0;

    for ((ledger, slot_idx), replicas) in slot_map {
        let replica_count = replicas.len();
        let is_fully_replicated = replica_count == expected_replicas;

        if is_fully_replicated {
            fully_replicated += 1;
        } else if replica_count < expected_replicas {
            under_replicated += 1;
        } else {
            over_replicated += 1;
        }

        let slot_replicas: Vec<SlotReplica> = replicas
            .into_iter()
            .map(|(node_addr, _hash)| SlotReplica {
                node_address: node_addr,
            })
            .collect();

        slots.push(SlotReplicaInfo {
            slot_index: slot_idx,
            ledger,
            replica_count,
            replicas: slot_replicas,
        });
    }

    // Sort slots for consistent output
    slots.sort_by_key(|s| (s.ledger as u8, s.slot_index));

    Ok(SlotReplicaSummary {
        total_slots: slots.len(),
        fully_replicated_slots: fully_replicated,
        under_replicated_slots: under_replicated,
        over_replicated_slots: over_replicated,
        expected_replicas,
        slots,
    })
}

pub(crate) async fn verify_all_slots_replicated(
    clients: &[RemoteNodeClient],
    node_addresses: &[&str],
    expected_replicas: usize,
) -> Result<bool> {
    let summary = aggregate_slot_replicas(clients, node_addresses).await?;

    if summary.expected_replicas != expected_replicas {
        tracing::warn!(
            "Expected {} replicas per slot, but network is configured for {}",
            expected_replicas,
            summary.expected_replicas
        );
    }

    if summary.under_replicated_slots > 0 {
        tracing::info!(
            "Under-replicated slots: {} out of {}",
            summary.under_replicated_slots,
            summary.total_slots
        );
        return Ok(false);
    }

    if summary.over_replicated_slots > 0 {
        tracing::warn!(
            "Over-replicated slots: {} out of {}",
            summary.over_replicated_slots,
            summary.total_slots
        );
    }

    if summary.fully_replicated_slots == summary.total_slots {
        tracing::info!(
            "✓ All {} slots have {} replicas",
            summary.total_slots,
            expected_replicas
        );
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) async fn verify_replica_distribution(
    clients: &[RemoteNodeClient],
    node_addresses: &[&str],
) -> Result<()> {
    let summary = aggregate_slot_replicas(clients, node_addresses).await?;

    // Check for slots with replicas on the same node (should not happen)
    for slot_info in &summary.slots {
        let mut node_set = std::collections::HashSet::new();
        for replica in &slot_info.replicas {
            if !node_set.insert(&replica.node_address) {
                return Err(eyre::eyre!(
                    "Slot {} on ledger {:?} has multiple replicas on node {}",
                    slot_info.slot_index,
                    slot_info.ledger,
                    replica.node_address
                ));
            }
        }
    }

    // Log replica distribution
    tracing::info!("Replica distribution analysis:");
    tracing::info!("  Total slots: {}", summary.total_slots);
    tracing::info!("  Fully replicated: {}", summary.fully_replicated_slots);
    tracing::info!("  Under replicated: {}", summary.under_replicated_slots);
    tracing::info!("  Over replicated: {}", summary.over_replicated_slots);

    // Count replicas per node
    let mut node_replica_count: HashMap<String, usize> = HashMap::new();
    for slot in &summary.slots {
        for replica in &slot.replicas {
            *node_replica_count
                .entry(replica.node_address.clone())
                .or_insert(0) += 1;
        }
    }

    for (node, count) in node_replica_count {
        tracing::info!("  Node {}: {} replicas", node, count);
    }

    Ok(())
}

/// Validate precise partition assignments for a node address
pub(crate) async fn validate_partition_assignments(
    client: &RemoteNodeClient,
    node_address: &str,
    expected_publish: usize,
    expected_submit: usize,
    expected_capacity: usize,
) -> Result<()> {
    let assignments_resp = get_all_assignments(client, node_address).await?;

    let publish_count = assignments_resp
        .assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(DataLedger::Publish.into()))
        .count();
    let submit_count = assignments_resp
        .assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(DataLedger::Submit.into()))
        .count();
    let capacity_count = assignments_resp
        .assignments
        .iter()
        .filter(|pa| pa.ledger_id.is_none())
        .count();

    tracing::info!(
        "Node {} assignments - Publish: {}/{}, Submit: {}/{}, Capacity: {}/{}",
        &node_address[..8], // Show first 8 chars
        publish_count,
        expected_publish,
        submit_count,
        expected_submit,
        capacity_count,
        expected_capacity
    );

    if publish_count != expected_publish {
        return Err(eyre::eyre!(
            "Publish assignment mismatch for {}: expected {}, got {}",
            node_address,
            expected_publish,
            publish_count
        ));
    }

    if submit_count != expected_submit {
        return Err(eyre::eyre!(
            "Submit assignment mismatch for {}: expected {}, got {}",
            node_address,
            expected_submit,
            submit_count
        ));
    }

    if capacity_count != expected_capacity {
        return Err(eyre::eyre!(
            "Capacity assignment mismatch for {}: expected {}, got {}",
            node_address,
            expected_capacity,
            capacity_count
        ));
    }

    tracing::info!(
        "✓ Partition assignments validated for {}",
        &node_address[..8]
    );
    Ok(())
}

pub(crate) async fn get_chunk_counts(
    client: &RemoteNodeClient,
    ledger: &str,
    slot_index: usize,
) -> Result<ChunkCountsResponse> {
    get_json(
        client,
        &format!("storage/counts/{}/{}", ledger, slot_index),
        "Failed to get chunk counts",
    )
    .await
}
