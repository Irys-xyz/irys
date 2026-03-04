//! Monitoring and synchronization utilities for external integration tests
//!
//! # Test Environment Assumptions
//!
//! These utilities are designed for multi-node test environments with specific
//! synchronization and monitoring requirements:
//!
//! - **Stability Checks**: 3 consecutive checks required to confirm stable state
//! - **Slot Configuration**: Tests check 2 slots (Publish and Submit ledgers)
//! - **Height Sync**: Nodes are considered synced within 2 blocks of each other
//! - **Retry Policy**: 30 retries with 2-second intervals for node readiness
//! - **Sync Intervals**: 5-second checks for storage sync, 2-second for height sync
//!
//! These thresholds are tuned for local/CI test environments and may need adjustment
//! for different network conditions.

use super::{
    api::{
        fetch_genesis_info, get_all_assignments, get_chain_height, get_ledger_summary,
        get_storage_intervals,
    },
    client::RemoteNodeClient,
    types::*,
    utils::count_chunks_in_intervals,
};
use eyre::Result;
use futures::future::join_all;
use irys_types::DataLedger;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

const STABILITY_THRESHOLD: usize = 3;
const DEFAULT_SLOT_CHECK_COUNT: usize = 2;
const HEIGHT_SYNC_TOLERANCE: u64 = 2;
const NODE_READY_MAX_RETRIES: u32 = 30;
const SYNC_CHECK_INTERVAL_SECS: u64 = 5;
const HEIGHT_CHECK_INTERVAL_SECS: u64 = 2;

pub(crate) async fn get_node_storage_status(
    client: &RemoteNodeClient,
    node_name: &str,
) -> Result<StorageStatus> {
    // Check Publish(0)
    let publish_data = get_storage_intervals(client, DataLedger::Publish, 0, "Data")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    let publish_packed = get_storage_intervals(client, DataLedger::Publish, 0, "Entropy")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    // Check Submit(0)
    let submit0_data = get_storage_intervals(client, DataLedger::Submit, 0, "Data")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    let submit0_packed = get_storage_intervals(client, DataLedger::Submit, 0, "Entropy")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    // Check Submit(1)
    let submit1_packed = get_storage_intervals(client, DataLedger::Submit, 1, "Entropy")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    Ok(StorageStatus {
        node_name: node_name.to_string(),
        publish_data,
        publish_packed,
        submit0_data,
        submit0_packed,
        submit1_packed,
    })
}

pub(crate) async fn check_node_storage(
    client: &RemoteNodeClient,
    _node_name: &str,
) -> Result<NodeStorageCounts> {
    let mut publish_slots = Vec::new();
    let mut submit_slots = Vec::new();

    // Check Publish slots
    for slot_index in 0..DEFAULT_SLOT_CHECK_COUNT {
        let data_intervals = get_storage_intervals(client, DataLedger::Publish, slot_index, "Data")
            .await
            .ok();
        let packed_intervals =
            get_storage_intervals(client, DataLedger::Publish, slot_index, "Entropy")
                .await
                .ok();

        let data_chunks = data_intervals
            .map(|r| count_chunks_in_intervals(&r.intervals))
            .unwrap_or(0);
        let packed_chunks = packed_intervals
            .map(|r| count_chunks_in_intervals(&r.intervals))
            .unwrap_or(0);

        if data_chunks > 0 || packed_chunks > 0 {
            publish_slots.push(SlotCounts {
                slot_index,
                data_chunks,
                packed_chunks,
            });
        }
    }

    // Check Submit slots
    for slot_index in 0..DEFAULT_SLOT_CHECK_COUNT {
        let data_intervals = get_storage_intervals(client, DataLedger::Submit, slot_index, "Data")
            .await
            .ok();
        let packed_intervals =
            get_storage_intervals(client, DataLedger::Submit, slot_index, "Entropy")
                .await
                .ok();

        let data_chunks = data_intervals
            .map(|r| count_chunks_in_intervals(&r.intervals))
            .unwrap_or(0);
        let packed_chunks = packed_intervals
            .map(|r| count_chunks_in_intervals(&r.intervals))
            .unwrap_or(0);

        if data_chunks > 0 || packed_chunks > 0 {
            submit_slots.push(SlotCounts {
                slot_index,
                data_chunks,
                packed_chunks,
            });
        }
    }

    Ok(NodeStorageCounts {
        publish_slots,
        submit_slots,
        node_url: client.url.clone(),
    })
}

pub(crate) async fn wait_for_chain_progression(
    clients: &[RemoteNodeClient],
    target_height: u64,
    timeout_secs: u64,
) -> Result<()> {
    info!("Waiting for all nodes to reach height {}...", target_height);

    let start = std::time::Instant::now();

    while start.elapsed().as_secs() < timeout_secs {
        let heights_futures = clients.iter().map(get_chain_height);
        let heights: Vec<u64> = join_all(heights_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        let min_height = heights.iter().min().copied().unwrap_or(0);

        if min_height >= target_height {
            info!("All nodes reached height {}", target_height);
            return Ok(());
        }

        debug!(
            "Current heights: {:?} (min: {}, target: {})",
            heights, min_height, target_height
        );

        sleep(Duration::from_secs(HEIGHT_CHECK_INTERVAL_SECS)).await;
    }

    Err(eyre::eyre!(
        "Timeout waiting for chain to reach height {}",
        target_height
    ))
}

pub(crate) async fn wait_for_height_sync(clients: &[RemoteNodeClient], timeout: u64) -> Result<()> {
    let start = std::time::Instant::now();

    while start.elapsed().as_secs() < timeout {
        let heights_futures = clients.iter().map(get_chain_height);
        let heights: Vec<u64> = join_all(heights_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        let max_height = heights.iter().max().copied().unwrap_or(0);
        let min_height = heights.iter().min().copied().unwrap_or(0);

        if max_height - min_height <= HEIGHT_SYNC_TOLERANCE {
            info!("Nodes synchronized at height ~{}", max_height);
            return Ok(());
        }

        info!(
            "Height sync in progress - min: {}, max: {}, diff: {}",
            min_height,
            max_height,
            max_height - min_height
        );

        sleep(Duration::from_secs(SYNC_CHECK_INTERVAL_SECS)).await;
    }

    Err(eyre::eyre!("Timeout waiting for height synchronization"))
}

#[expect(dead_code)]
pub(crate) async fn monitor_storage_sync(
    clients: &[RemoteNodeClient],
    expected_data_chunks: Option<usize>,
    timeout_secs: u64,
) -> Result<()> {
    info!(
        "Monitoring storage synchronization across {} nodes",
        clients.len()
    );

    let start = std::time::Instant::now();
    let mut last_status_map: HashMap<String, StorageStatus> = HashMap::new();
    let mut stable_count = 0;

    while start.elapsed().as_secs() < timeout_secs {
        let mut current_status_map = HashMap::new();
        let mut all_have_data = true;

        // Collect storage status from all nodes
        for (i, client) in clients.iter().enumerate() {
            let node_name = format!("Node{i}");
            let status = get_node_storage_status(client, &node_name).await?;
            let total = status.total_chunks();

            if total == 0 {
                all_have_data = false;
            }

            info!(
                "{}: Publish({}/{}) Submit0({}/{}) Submit1(0/{}) Total:{}",
                node_name,
                status.publish_data,
                status.publish_packed,
                status.submit0_data,
                status.submit0_packed,
                status.submit1_packed,
                total
            );

            current_status_map.insert(node_name.clone(), status);
        }

        // Check if we've reached expected chunks (if specified)
        if let Some(expected) = expected_data_chunks {
            let all_match_expected = current_status_map
                .values()
                .all(|s| s.publish_data >= expected || s.submit0_data >= expected);

            if all_match_expected {
                info!("All nodes have reached expected data chunks: {}", expected);
                return Ok(());
            }
        }

        // Check if status is stable (no changes from last check)
        let status_unchanged = if last_status_map.is_empty() {
            false
        } else {
            current_status_map.iter().all(|(name, status)| {
                last_status_map
                    .get(name)
                    .map(|last| {
                        last.publish_data == status.publish_data
                            && last.publish_packed == status.publish_packed
                            && last.submit0_data == status.submit0_data
                            && last.submit0_packed == status.submit0_packed
                            && last.submit1_packed == status.submit1_packed
                    })
                    .unwrap_or(false)
            })
        };

        if status_unchanged && all_have_data {
            stable_count += 1;
            info!("Storage stable for {} checks", stable_count);

            if stable_count >= STABILITY_THRESHOLD {
                info!("Storage synchronization complete and stable!");
                return Ok(());
            }
        } else {
            stable_count = 0;
        }

        last_status_map = current_status_map;

        info!("---");
        sleep(Duration::from_secs(SYNC_CHECK_INTERVAL_SECS)).await;
    }

    warn!("Timeout reached, nodes may still be syncing");
    Ok(())
}

#[expect(dead_code)]
pub(crate) fn validate_detailed_sync(
    node_name: &str,
    storage: &NodeStorageCounts,
    expected_data_chunks: usize,
    expected_packed_slot0: usize,
    expected_packed_slot1: usize,
) -> DetailedSyncValidation {
    let mut sync_details = Vec::new();
    let mut is_fully_synced = true;

    // Check Publish slots
    let publish_data: usize = storage.publish_slots.iter().map(|s| s.data_chunks).sum();
    let _publish_packed: usize = storage.publish_slots.iter().map(|s| s.packed_chunks).sum();

    if publish_data != expected_data_chunks {
        is_fully_synced = false;
        sync_details.push(format!(
            "Publish data: {publish_data}/{expected_data_chunks}"
        ));
    }

    // Check Submit slots
    let submit_data: usize = storage.submit_slots.iter().map(|s| s.data_chunks).sum();
    let submit_packed_0 = storage
        .submit_slots
        .iter()
        .find(|s| s.slot_index == 0)
        .map(|s| s.packed_chunks)
        .unwrap_or(0);
    let submit_packed_1 = storage
        .submit_slots
        .iter()
        .find(|s| s.slot_index == 1)
        .map(|s| s.packed_chunks)
        .unwrap_or(0);

    if submit_data != expected_data_chunks {
        is_fully_synced = false;
        sync_details.push(format!("Submit data: {submit_data}/{expected_data_chunks}"));
    }

    if submit_packed_0 != expected_packed_slot0 {
        is_fully_synced = false;
        sync_details.push(format!(
            "Submit slot0 packed: {submit_packed_0}/{expected_packed_slot0}"
        ));
    }

    if submit_packed_1 != expected_packed_slot1 {
        is_fully_synced = false;
        sync_details.push(format!(
            "Submit slot1 packed: {submit_packed_1}/{expected_packed_slot1}"
        ));
    }

    DetailedSyncValidation {
        node_name: node_name.to_string(),
        expected_data_chunks,
        expected_packed_chunks_slot0: expected_packed_slot0,
        expected_packed_chunks_slot1: expected_packed_slot1,
        is_fully_synced,
        sync_details: if sync_details.is_empty() {
            "Fully synced".to_string()
        } else {
            sync_details.join(", ")
        },
    }
}

pub(crate) async fn wait_for_node_ready(client: &RemoteNodeClient, node_name: &str) -> Result<()> {
    let mut retries = 0;

    while !client.is_ready().await {
        retries += 1;
        if retries > NODE_READY_MAX_RETRIES {
            return Err(eyre::eyre!("{} failed to become ready", node_name));
        }
        warn!(
            "{} not ready, retry {}/{}",
            node_name, retries, NODE_READY_MAX_RETRIES
        );
        sleep(Duration::from_secs(HEIGHT_CHECK_INTERVAL_SECS)).await;
    }

    info!("{} is ready", node_name);
    Ok(())
}

pub(crate) async fn analyze_cluster_data(clients: &[RemoteNodeClient]) -> Result<()> {
    info!("=== Analyzing Cluster Data ===");

    for (i, client) in clients.iter().enumerate() {
        match fetch_genesis_info(client).await {
            Ok(genesis) => {
                info!(
                    "Node{}: Genesis hash: {}, Height: {}",
                    i, genesis.genesis_block_hash, genesis.height
                );
            }
            Err(e) => {
                info!("Node{}: Could not fetch genesis info: {}", i, e);
            }
        }

        // Try to get ledger summaries for known test addresses
        let test_addresses = vec![
            "0x10605A299777D44BE5373D120d5479f07860325d",
            "0x951AA3768fE8d51DbC348520b0825C0c1f197277",
            "0x852BB3678fE8d51DbC348520b0825C0c1f197288",
        ];

        for addr in &test_addresses {
            let publish_summary = get_ledger_summary(client, addr, DataLedger::Publish).await;
            let submit_summary = get_ledger_summary(client, addr, DataLedger::Submit).await;

            if let (Ok(pub_sum), Ok(sub_sum)) = (publish_summary, submit_summary) {
                if pub_sum.assignment_count > 0 || sub_sum.assignment_count > 0 {
                    info!(
                        "  {} has {} publish and {} submit assignments",
                        addr, pub_sum.assignment_count, sub_sum.assignment_count
                    );
                }
            }
        }
    }

    Ok(())
}

#[expect(dead_code)]
pub(crate) async fn test_mining_capabilities(clients: &[RemoteNodeClient]) -> Result<()> {
    info!("Checking if nodes can mine blocks...");

    for (i, client) in clients.iter().enumerate() {
        let initial_height = get_chain_height(client).await?;
        info!("Node{} initial height: {}", i, initial_height);

        // Wait a bit to see if height increases
        sleep(Duration::from_secs(10)).await;

        let final_height = get_chain_height(client).await?;
        if final_height > initial_height {
            info!("Node{} mined {} blocks", i, final_height - initial_height);
        } else {
            info!("Node{} did not mine blocks", i);
        }
    }

    Ok(())
}

pub(crate) async fn wait_for_nodes_to_pack(
    clients: &[RemoteNodeClient],
    timeout_secs: u64,
) -> Result<()> {
    use crate::external::utils::api::{get_all_assignments, get_chunk_counts};

    info!("Waiting for all nodes to have packed chunks...");

    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            return Err(eyre::eyre!(
                "Timeout waiting for nodes to pack after {} seconds",
                timeout_secs
            ));
        }

        let mut all_packed = true;

        for (i, client) in clients.iter().enumerate() {
            // Get node info to find its address
            let info_resp = client
                .http_client
                .get(format!("{}/v1/info", client.url))
                .send()
                .await?
                .json::<serde_json::Value>()
                .await?;

            let node_address = info_resp
                .get("node_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| eyre::eyre!("Node {} missing node_id", i))?;

            // Get assignments for this node
            let assignments = get_all_assignments(client, node_address).await?;

            if assignments.assignments.is_empty() {
                info!("Node {} has no assignments yet, waiting...", i);
                all_packed = false;
                break;
            }

            // Check each assignment to see if it has packed chunks
            let mut node_has_packed = false;
            for assignment in &assignments.assignments {
                if let (Some(ledger_id), Some(slot_index)) =
                    (assignment.ledger_id, assignment.slot_index)
                {
                    let ledger = if ledger_id == u32::from(DataLedger::Publish) {
                        "Publish"
                    } else {
                        "Submit"
                    };

                    match get_chunk_counts(client, ledger, slot_index).await {
                        Ok(counts) if counts.packed_chunks > 0 => {
                            node_has_packed = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }

            if !node_has_packed {
                info!("Node {} has no packed chunks yet, waiting...", i);
                all_packed = false;
                break;
            }
        }

        if all_packed {
            info!("✓ All nodes have packed chunks");
            return Ok(());
        }

        sleep(Duration::from_secs(HEIGHT_CHECK_INTERVAL_SECS)).await;
    }
}

// Assignment-based validation functions

pub(crate) async fn get_node_assignment_info(
    client: &RemoteNodeClient,
    node_address: &str,
) -> Result<NodeAssignmentInfo> {
    let assignments_resp = get_all_assignments(client, node_address).await?;

    let mut publish_slots = Vec::new();
    let mut submit_slots = Vec::new();

    for assignment in &assignments_resp.assignments {
        match assignment.ledger_id {
            Some(id) if id == u32::from(DataLedger::Publish) => {
                if let Some(slot_idx) = assignment.slot_index {
                    publish_slots.push(slot_idx);
                }
            }
            Some(id) if id == u32::from(DataLedger::Submit) => {
                if let Some(slot_idx) = assignment.slot_index {
                    submit_slots.push(slot_idx);
                }
            }
            None => {} // capacity assignment
            _ => warn!("Unknown ledger_id: {:?}", assignment.ledger_id),
        }
    }

    // Sort slot indices for consistent ordering
    publish_slots.sort();
    submit_slots.sort();

    Ok(NodeAssignmentInfo {
        publish_slots,
        submit_slots,
    })
}

pub(crate) fn calculate_expected_storage(
    assignment_info: &NodeAssignmentInfo,
    test_data_size: usize,    // 50 chunks of data in test
    packed_size_slot1: usize, // 60 packed chunks in slot 1
) -> NodeExpectedStorage {
    let mut expected_publish_data = BTreeMap::new();
    let mut expected_publish_packed = BTreeMap::new();
    let mut expected_submit_data = BTreeMap::new();
    let mut expected_submit_packed = BTreeMap::new();

    // For test scenario:
    // - Slot 0 has 50 data chunks + 10 packed chunks (from test data)
    // - Slot 1 has 0 data chunks + 60 packed chunks (full partition)

    for &slot in &assignment_info.publish_slots {
        if slot == 0 {
            expected_publish_data.insert(slot, test_data_size); // 50 data chunks
            expected_publish_packed.insert(slot, test_data_size / 5); // 10 packed chunks (5:1 ratio)
        } else if slot == 1 {
            expected_publish_data.insert(slot, 0); // No data chunks in slot 1
            expected_publish_packed.insert(slot, packed_size_slot1); // 60 packed chunks
        }
        // Other slots would have 0 chunks in this test
    }

    for &slot in &assignment_info.submit_slots {
        if slot == 0 {
            expected_submit_data.insert(slot, test_data_size); // 50 data chunks
            expected_submit_packed.insert(slot, test_data_size / 5); // 10 packed chunks
        } else if slot == 1 {
            expected_submit_data.insert(slot, 0); // No data chunks in slot 1
            expected_submit_packed.insert(slot, packed_size_slot1); // 60 packed chunks
        }
        // Other slots would have 0 chunks in this test
    }

    NodeExpectedStorage {
        expected_publish_data,
        expected_publish_packed,
        expected_submit_data,
        expected_submit_packed,
    }
}

pub(crate) async fn validate_node_storage_against_assignments(
    client: &RemoteNodeClient,
    node_name: &str,
    assignment_info: &NodeAssignmentInfo,
    expected_storage: &NodeExpectedStorage,
) -> Result<NodeSyncValidationResult> {
    // Get actual storage for this node
    let storage = check_node_storage(client, node_name).await?;

    let mut validation_details = Vec::new();
    let mut is_fully_synced = true;

    // Validate Publish slots
    for &slot_index in &assignment_info.publish_slots {
        let actual_data: usize = storage
            .publish_slots
            .iter()
            .filter(|s| s.slot_index == slot_index)
            .map(|s| s.data_chunks)
            .sum();
        let actual_packed: usize = storage
            .publish_slots
            .iter()
            .filter(|s| s.slot_index == slot_index)
            .map(|s| s.packed_chunks)
            .sum();

        let expected_data = expected_storage
            .expected_publish_data
            .get(&slot_index)
            .copied()
            .unwrap_or(0);
        let expected_packed = expected_storage
            .expected_publish_packed
            .get(&slot_index)
            .copied()
            .unwrap_or(0);

        let slot_synced = actual_data == expected_data && actual_packed == expected_packed;
        if !slot_synced {
            is_fully_synced = false;
        }

        validation_details.push(format!(
            "Publish({}): data={}/{}, packed={}/{} {}",
            slot_index,
            actual_data,
            expected_data,
            actual_packed,
            expected_packed,
            if slot_synced { "✓" } else { "✗" }
        ));
    }

    // Validate Submit slots
    for &slot_index in &assignment_info.submit_slots {
        let actual_data: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == slot_index)
            .map(|s| s.data_chunks)
            .sum();
        let actual_packed: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == slot_index)
            .map(|s| s.packed_chunks)
            .sum();

        let expected_data = expected_storage
            .expected_submit_data
            .get(&slot_index)
            .copied()
            .unwrap_or(0);
        let expected_packed = expected_storage
            .expected_submit_packed
            .get(&slot_index)
            .copied()
            .unwrap_or(0);

        let slot_synced = actual_data == expected_data && actual_packed == expected_packed;
        if !slot_synced {
            is_fully_synced = false;
        }

        validation_details.push(format!(
            "Submit({}): data={}/{}, packed={}/{} {}",
            slot_index,
            actual_data,
            expected_data,
            actual_packed,
            expected_packed,
            if slot_synced { "✓" } else { "✗" }
        ));
    }

    let details = if validation_details.is_empty() {
        "No assignments to validate".to_string()
    } else {
        validation_details.join(" | ")
    };

    Ok(NodeSyncValidationResult {
        node_name: node_name.to_string(),
        is_synced: is_fully_synced,
        details,
        assignment_info: assignment_info.clone(),
    })
}

pub(crate) async fn check_nodes_sync_status_by_assignment(
    clients: &[RemoteNodeClient],
    node_addresses: &[String],
    test_data_size: usize,
    packed_size_slot1: usize,
) -> Result<Vec<NodeSyncValidationResult>> {
    let mut validation_results = Vec::new();

    for (i, client) in clients.iter().enumerate() {
        let node_name = if i == 0 {
            "Node0(Genesis)".to_string()
        } else {
            format!("Node{i}")
        };

        let node_address = if i < node_addresses.len() {
            node_addresses[i].clone()
        } else {
            format!("unknown_address_{i}")
        };

        // Get assignment info for this node
        let assignment_info = get_node_assignment_info(client, &node_address).await?;

        // Calculate expected storage based on assignments
        let expected_storage =
            calculate_expected_storage(&assignment_info, test_data_size, packed_size_slot1);

        // Validate storage against assignments
        let validation_result = validate_node_storage_against_assignments(
            client,
            &node_name,
            &assignment_info,
            &expected_storage,
        )
        .await?;

        info!(
            "{}: {} - {}",
            validation_result.node_name,
            if validation_result.is_synced {
                "SYNCED ✓"
            } else {
                "NOT_SYNCED ✗"
            },
            validation_result.details
        );

        validation_results.push(validation_result);
    }

    Ok(validation_results)
}
