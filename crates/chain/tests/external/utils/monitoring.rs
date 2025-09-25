use super::{
    api::{fetch_genesis_info, get_chain_height, get_ledger_summary, get_storage_intervals},
    client::RemoteNodeClient,
    types::*,
    utils::count_chunks_in_intervals,
};
use eyre::Result;
use futures::future::join_all;
use irys_types::DataLedger;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub(crate) async fn get_node_storage_status(
    client: &RemoteNodeClient,
    node_name: &str,
) -> Result<StorageStatus> {
    // Check Publish(0)
    let publish_data = get_storage_intervals(client, DataLedger::Publish, 0, "data")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    let publish_packed = get_storage_intervals(client, DataLedger::Publish, 0, "packed")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    // Check Submit(0)
    let submit0_data = get_storage_intervals(client, DataLedger::Submit, 0, "data")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    let submit0_packed = get_storage_intervals(client, DataLedger::Submit, 0, "packed")
        .await
        .map(|r| count_chunks_in_intervals(&r.intervals))
        .unwrap_or(0);

    // Check Submit(1)
    let submit1_packed = get_storage_intervals(client, DataLedger::Submit, 1, "packed")
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
    for slot_index in 0..2 {
        let data_intervals = get_storage_intervals(client, DataLedger::Publish, slot_index, "data")
            .await
            .ok();
        let packed_intervals =
            get_storage_intervals(client, DataLedger::Publish, slot_index, "packed")
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
    for slot_index in 0..2 {
        let data_intervals = get_storage_intervals(client, DataLedger::Submit, slot_index, "data")
            .await
            .ok();
        let packed_intervals =
            get_storage_intervals(client, DataLedger::Submit, slot_index, "packed")
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
        let heights_futures = clients.iter().map(|c| get_chain_height(c));
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

        sleep(Duration::from_secs(2)).await;
    }

    Err(eyre::eyre!(
        "Timeout waiting for chain to reach height {}",
        target_height
    ))
}

pub(crate) async fn wait_for_height_sync(clients: &[RemoteNodeClient], timeout: u64) -> Result<()> {
    let start = std::time::Instant::now();

    while start.elapsed().as_secs() < timeout {
        let heights_futures = clients.iter().map(|c| get_chain_height(c));
        let heights: Vec<u64> = join_all(heights_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        let max_height = heights.iter().max().copied().unwrap_or(0);
        let min_height = heights.iter().min().copied().unwrap_or(0);

        if max_height - min_height <= 2 {
            info!("Nodes synchronized at height ~{}", max_height);
            return Ok(());
        }

        info!(
            "Height sync in progress - min: {}, max: {}, diff: {}",
            min_height,
            max_height,
            max_height - min_height
        );

        sleep(Duration::from_secs(5)).await;
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
    let stability_threshold = 3; // Need 3 consecutive checks with no changes

    while start.elapsed().as_secs() < timeout_secs {
        let mut current_status_map = HashMap::new();
        let mut all_have_data = true;

        // Collect storage status from all nodes
        for (i, client) in clients.iter().enumerate() {
            let node_name = format!("Node{}", i);
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

            if stable_count >= stability_threshold {
                info!("Storage synchronization complete and stable!");
                return Ok(());
            }
        } else {
            stable_count = 0;
        }

        last_status_map = current_status_map;

        info!("---");
        sleep(Duration::from_secs(5)).await;
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
            "Publish data: {}/{}",
            publish_data, expected_data_chunks
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
        sync_details.push(format!(
            "Submit data: {}/{}",
            submit_data, expected_data_chunks
        ));
    }

    if submit_packed_0 != expected_packed_slot0 {
        is_fully_synced = false;
        sync_details.push(format!(
            "Submit slot0 packed: {}/{}",
            submit_packed_0, expected_packed_slot0
        ));
    }

    if submit_packed_1 != expected_packed_slot1 {
        is_fully_synced = false;
        sync_details.push(format!(
            "Submit slot1 packed: {}/{}",
            submit_packed_1, expected_packed_slot1
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
    let max_retries = 30;
    let mut retries = 0;

    while !client.is_ready().await {
        retries += 1;
        if retries > max_retries {
            return Err(eyre::eyre!("{} failed to become ready", node_name));
        }
        warn!("{} not ready, retry {}/{}", node_name, retries, max_retries);
        sleep(Duration::from_secs(2)).await;
    }

    info!("{} is ready", node_name);
    Ok(())
}

/// Check sync status of all nodes against expected chunk counts
pub(crate) async fn check_nodes_sync_status(
    clients: &[RemoteNodeClient],
    expected_data_chunks: usize,
    expected_packed_slot0: usize,
    expected_packed_slot1: usize,
) -> Result<Vec<(String, bool)>> {
    let mut node_sync_status = Vec::new();

    for (i, client) in clients.iter().enumerate() {
        let node_name = if i == 0 {
            "Node0(Genesis)".to_string()
        } else {
            format!("Node{}", i)
        };

        // Check node storage
        let storage = check_node_storage(client, &node_name).await?;

        // Check Publish(0)
        let publish_data: usize = storage
            .publish_slots
            .iter()
            .filter(|s| s.slot_index == 0)
            .map(|s| s.data_chunks)
            .sum();
        let publish_packed: usize = storage
            .publish_slots
            .iter()
            .filter(|s| s.slot_index == 0)
            .map(|s| s.packed_chunks)
            .sum();

        // Check Submit(0)
        let submit0_data: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == 0)
            .map(|s| s.data_chunks)
            .sum();
        let submit0_packed: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == 0)
            .map(|s| s.packed_chunks)
            .sum();

        // Check Submit(1)
        let submit1_data: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == 1)
            .map(|s| s.data_chunks)
            .sum();
        let submit1_packed: usize = storage
            .submit_slots
            .iter()
            .filter(|s| s.slot_index == 1)
            .map(|s| s.packed_chunks)
            .sum();

        // For remote test, we check if nodes have synced the expected data
        let is_synced = (publish_data == expected_data_chunks
            && publish_packed == expected_packed_slot0)
            && (submit0_data == expected_data_chunks && submit0_packed == expected_packed_slot0)
            && (submit1_data == 0 && submit1_packed == expected_packed_slot1);

        node_sync_status.push((node_name.clone(), is_synced));

        // Log sync status
        info!(
            "{} chunks - Publish(0): data={}, packed={} | Submit(0): data={}, packed={} | Submit(1): data={}, packed={}",
            node_name, publish_data, publish_packed, submit0_data, submit0_packed, submit1_data, submit1_packed
        );
    }

    Ok(node_sync_status)
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
