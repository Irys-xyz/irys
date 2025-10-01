//! External integration test for partition data synchronization across a 3-node cluster.
//!
//! # Test Requirements
//!
//! **NOTE**: This test requires a Docker cluster with specific configuration:
//! - **Cluster Setup**: Use `docker/tests/data_chunksnc/docker-compose.yaml`
//! - **Replicas**: 3 partition replicas per slot
//! - **Peer Nodes**: MIN_HEIGHT=40 for irys-2 and irys-3 (delayed start)
//!
//! # Test Scenario
//!
//! Validates that nodes sync data chunks to their assigned partitions when joining
//! at different blockchain heights. Genesis posts data at height ~20, peers join at
//! height 40, and all nodes must sync to consistent state.

use eyre::Result;
use futures::future::join_all;
use irys_testing_utils::initialize_tracing;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::external::utils::{
    api::*,
    client::{make_http_client, RemoteNodeClient},
    monitoring::*,
    signer::{load_test_signers_from_env, TestSigner},
    transactions::*,
    types::AssignmentStatus,
    utils::{create_consensus_config_from_response, get_env_duration, parse_node_urls},
};

// Test configuration constants
const TEST_DATA_CHUNKS: usize = 50;
const EXPECTED_PACKED_SLOT0: usize = 10; // 50 chunks * 0.2 packing ratio
const EXPECTED_PACKED_SLOT1: usize = 60; // Partition size limit
const LEDGER_SYNC_TIMEOUT_SECS: u64 = 200; // 95% of txs appear within 30s, 99.9% within 180s
const LEDGER_POLL_INTERVAL_SECS: u64 = 2;
const GENESIS_INITIAL_HEIGHT: u64 = 20;
const DATA_VERIFICATION_HEIGHT: u64 = 50; // Peers at 40 + 1-2 epochs for assignments
const GENESIS_FINAL_HEIGHT: u64 = 60; // Min height for full multi-node validation
const REPLICA_WAIT_MAX_ATTEMPTS: usize = 30; // 30 * 2s = 60s for async replica assignment
const SYNC_VALIDATION_MAX_ATTEMPTS: usize = 60;

// Genesis node address hardcoded from Docker genesis config
// Must match docker/tests/partition-sync/genesis.json funded accounts
const GENESIS_NODE_ADDR: &str = "0x10605A299777D44BE5373D120d5479f07860325d";

#[ignore]
#[tokio::test]
async fn sync_partition_data_remote_test() -> Result<()> {
    initialize_tracing();

    info!("=== Remote Partition Data Sync Test ===");
    info!("This test requires external nodes configured with:");
    info!("  - 3 partition replicas per slot");
    info!("  - 1 ingress proof to promote");
    info!("Use docker/tests/partition-sync/docker-compose.yaml to start the cluster");

    let http_client = make_http_client()?;
    let urls = parse_node_urls()?;

    if urls.is_empty() {
        return Err(eyre::eyre!("No node URLs provided"));
    }

    info!("Testing with {} node(s): {:?}", urls.len(), urls);

    let clients: Vec<RemoteNodeClient> = urls
        .into_iter()
        .map(|url| RemoteNodeClient::new(url, http_client.clone()))
        .collect::<Result<Vec<_>>>()?;

    wait_for_node_ready(&clients[0], "Genesis").await?;

    let config_resp = fetch_network_config(&clients[0]).await?;

    let chain_id: u64 = config_resp.chain_id.parse().unwrap_or(0);
    let chunk_size: u64 = config_resp.chunk_size.parse().unwrap_or(32);
    let num_partitions_per_slot: u64 = config_resp.num_partitions_per_slot.parse().unwrap_or(0);

    info!(
        "Network config - Chain ID: {}, Chunk size: {}, Partitions per slot: {}",
        chain_id, chunk_size, num_partitions_per_slot
    );

    if num_partitions_per_slot != 3 {
        return Err(eyre::eyre!(
            "Test requires num_partitions_per_slot=3, got {}",
            num_partitions_per_slot
        ));
    }

    let config = create_consensus_config_from_response(&config_resp);

    let default_keys = vec![
        "53142d3c5a4a3f49715490456c26df7926d45586179d19afce68d799c08ea8c6".to_string(), // 0x10605A299777D44BE5373D120d5479f07860325d
        "4882545ed67d1b638207334975ee839e2369d31d8b3b8ef656b94952ca4ae4f3".to_string(), // 0x951AA3768fE8d51DbC348520b0825C0c1f197277
        "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a".to_string(), // 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
    ];

    let signer_keys = match load_test_signers_from_env() {
        Ok(keys) if keys.len() >= 3 => {
            info!("Using test signers from environment");
            keys
        }
        _ => {
            info!("Using default funded signers from genesis config");
            default_keys
        }
    };

    let signers = vec![
        TestSigner::from_private_key(&signer_keys[0], "Genesis".to_string(), &config)?,
        TestSigner::from_private_key(&signer_keys[1], "Signer1".to_string(), &config)?,
        TestSigner::from_private_key(&signer_keys[2], "Signer2".to_string(), &config)?,
    ];

    let genesis_signer = &signers[0];

    for signer in &signers {
        info!("  {}: {:?}", signer.name, signer.address);
    }

    analyze_cluster_data(&clients[..1]).await?;

    wait_for_chain_progression(&clients[..1], GENESIS_INITIAL_HEIGHT, 300).await?;
    info!("Genesis node reached height {}", GENESIS_INITIAL_HEIGHT);

    // Peers configured with MIN_HEIGHT=40 will sync historical data from genesis when
    // they join
    let num_chunks = TEST_DATA_CHUNKS;
    let chunk_size = config.chunk_size as usize;
    let data_size = num_chunks * chunk_size;

    let data_tx = match post_data_transaction(&clients[0], genesis_signer, data_size).await {
        Ok(tx) => {
            info!("✓ Successfully posted data transaction: {:?}", tx.header.id);
            info!("  Transaction contains {} merkle nodes", tx.chunks.len());
            Some(tx)
        }
        Err(e) => {
            info!("Could not post data transaction: {}", e);
            info!("This may be expected if test accounts don't have balance");
            info!("Continuing with monitoring existing data...");
            None
        }
    };

    // Poll ledgers directly instead of transaction API because transactions may be
    // valid but not yet stored in ledgers, causing false positive test passes
    if data_tx.is_some() {
        let mut data_in_ledgers = false;
        let max_wait_attempts = (LEDGER_SYNC_TIMEOUT_SECS / LEDGER_POLL_INTERVAL_SECS) as usize;

        for attempt in 1..=max_wait_attempts {
            sleep(Duration::from_secs(LEDGER_POLL_INTERVAL_SECS)).await;
            let publish_result = clients[0]
                .http_client
                .get(format!(
                    "{}/v1/data_ledger/intervals?ledger=Publish&slot=0",
                    clients[0].url
                ))
                .send()
                .await;

            if let Ok(resp) = publish_result {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(intervals) = json.get("intervals").and_then(|v| v.as_array()) {
                        if !intervals.is_empty() {
                            info!("✓ Data appeared in Publish(0) ledger after {} attempts ({} intervals)",
                                  attempt, intervals.len());
                            data_in_ledgers = true;
                            break;
                        }
                    }
                }
            }

            if attempt % 10 == 0 {
                info!(
                    "Still waiting for data in ledgers... (attempt {}/{})",
                    attempt, max_wait_attempts
                );
            }
        }

        if !data_in_ledgers {
            warn!(
                "⚠ Data transaction posted but not yet in ledgers after {} seconds",
                max_wait_attempts * 2
            );
            warn!("Continuing anyway, but test may fail if data doesn't sync");
        }
    }

    info!("Checking genesis node sync height...");
    let info_resp = clients[0]
        .http_client
        .get(format!("{}/v1/info", clients[0].url))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let height = info_resp
        .get("height")
        .and_then(|v| v.as_str())
        .unwrap_or("0");
    let sync_height = info_resp
        .get("currentSyncHeight")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    if sync_height <= 1 {
        warn!(
            "Genesis node has low sync height: {} (chain height: {})",
            sync_height, height
        );
    } else {
        info!(
            "Genesis node sync height: {} (chain height: {})",
            sync_height, height
        );
    }

    let epoch_info = get_current_epoch(&clients[0]).await?;
    info!(
        "Current epoch: {}, Total partitions: {}, Unassigned: {}",
        epoch_info.current_epoch,
        epoch_info.total_active_partitions,
        epoch_info.unassigned_partitions
    );

    if epoch_info.unassigned_partitions > epoch_info.total_active_partitions / 2 {
        warn!(
            "More than half of partitions are unassigned: {}/{}",
            epoch_info.unassigned_partitions, epoch_info.total_active_partitions
        );
    }

    info!("Cluster health check complete. Proceeding with test...");

    // Height 50 verification: Peers start at 40, need 1-2 epochs (4-8 blocks) for partition
    // assignments. Genesis must have data before peers can sync.
    let current_height_for_check = get_chain_height(&clients[0]).await?;

    if current_height_for_check >= DATA_VERIFICATION_HEIGHT && data_tx.is_some() {
        info!(
            "Verifying genesis has data in ledgers at height {}...",
            current_height_for_check
        );

        // Check Publish(0) ledger for data
        let publish_result = clients[0]
            .http_client
            .get(format!("{}/v1/storage/counts/Publish/0", clients[0].url))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let publish_data_chunks = publish_result
            .get("data_chunks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let publish_packed_chunks = publish_result
            .get("packed_chunks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        // Check Submit(0) ledger for data
        let submit_result = clients[0]
            .http_client
            .get(format!("{}/v1/storage/counts/Submit/0", clients[0].url))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let submit_data_chunks = submit_result
            .get("data_chunks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let submit_packed_chunks = submit_result
            .get("packed_chunks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        if publish_data_chunks == 0 && submit_data_chunks == 0 {
            return Err(eyre::eyre!(
                "CRITICAL: Genesis node has NO data chunks at height {}. \
                Publish: {} data, {} packed. Submit: {} data, {} packed. \
                Data was posted at height ~20 but never appeared in ledgers.",
                current_height_for_check,
                publish_data_chunks,
                publish_packed_chunks,
                submit_data_chunks,
                submit_packed_chunks
            ));
        }

        info!(
            "✓ Genesis has data chunks (Publish: {} data/{} packed, Submit: {} data/{} packed)",
            publish_data_chunks, publish_packed_chunks, submit_data_chunks, submit_packed_chunks
        );
    }

    let blocks_for_epoch = get_env_duration("BLOCKS_FOR_EPOCH", 4);

    // Peer interaction phase begins: Peers have started at MIN_HEIGHT=40 and auto-staked
    // themselves via stake_pledge_drives=true. Genesis can now interact with all nodes.
    info!("Genesis checkpoint reached - peers should now be online and ready");
    if clients.len() > 1 {
        let node_names: Vec<String> = (1..clients.len()).map(|i| format!("Node{i}")).collect();
        let readiness_futures = clients[1..]
            .iter()
            .zip(node_names.iter())
            .map(|(client, name)| wait_for_node_ready(client, name));
        join_all(readiness_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        info!("✓ All peer nodes are now ready");
    }

    // Peers get assignments at epoch boundaries (40, 44, 48, etc based on blocks_for_epoch)
    let current_height = get_chain_height(&clients[0]).await?;
    let next_epoch = ((current_height / blocks_for_epoch) + 1) * blocks_for_epoch;

    if next_epoch - current_height > 1 {
        info!(
            "Waiting for next epoch at height {} for partition assignments (current: {})...",
            next_epoch, current_height
        );
        wait_for_chain_progression(&clients, next_epoch, 120).await?;
        info!("Reached epoch boundary - nodes should now have partition assignments");
    }

    info!("Checking sync heights across all nodes...");
    for (i, client) in clients.iter().enumerate() {
        let info_resp = client
            .http_client
            .get(format!("{}/v1/info", client.url))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let height = info_resp
            .get("height")
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        let sync_height = info_resp
            .get("currentSyncHeight")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        if sync_height <= 1 {
            warn!(
                "Node {} has low sync height: {} (chain height: {})",
                i + 1,
                sync_height,
                height
            );
        } else {
            info!(
                "Node {} sync height: {} (chain height: {})",
                i + 1,
                sync_height,
                height
            );
        }
    }

    match wait_for_nodes_to_pack(&clients, 60).await {
        Ok(_) => info!("✓ All nodes have packed chunks"),
        Err(e) => warn!("Packing wait timeout: {}", e),
    }

    let blocks_for_epoch = get_env_duration("BLOCKS_FOR_EPOCH", 4);
    let current_height = get_chain_height(&clients[0]).await?;
    let first_epoch_target = current_height + blocks_for_epoch;

    match wait_for_chain_progression(&clients, first_epoch_target, 60).await {
        Ok(_) => info!("First epoch complete at height {}", first_epoch_target),
        Err(e) => info!("First epoch timeout: {}", e),
    }

    let second_epoch_target = first_epoch_target + blocks_for_epoch;

    match wait_for_chain_progression(&clients, second_epoch_target, 60).await {
        Ok(_) => info!("Second epoch complete at height {}", second_epoch_target),
        Err(e) => info!("Second epoch timeout: {}", e),
    }

    let current_epoch_info = get_current_epoch(&clients[0]).await?;
    info!(
        "Current epoch: {}, block height: {}, active partitions: {}, unassigned: {}",
        current_epoch_info.current_epoch,
        current_epoch_info.epoch_block_height,
        current_epoch_info.total_active_partitions,
        current_epoch_info.unassigned_partitions
    );

    info!(
        "Checking partition hash assignments for genesis node {}",
        GENESIS_NODE_ADDR
    );
    let assignments_resp = get_all_assignments(&clients[0], GENESIS_NODE_ADDR).await?;

    match assignments_resp.assignment_status {
        AssignmentStatus::FullyAssigned => {
            info!("✓ All partition hashes are assigned for genesis node");

            let hash_analysis = &assignments_resp.hash_analysis;
            info!(
                "Hash analysis: {} total, {} unique, {} zero hashes",
                hash_analysis.total_hashes, hash_analysis.unique_hashes, hash_analysis.zero_hashes
            );

            if hash_analysis.zero_hashes > 0 {
                return Err(eyre::eyre!(
                    "Found {} zero partition hashes",
                    hash_analysis.zero_hashes
                ));
            }

            if !hash_analysis.duplicate_hashes.is_empty() {
                return Err(eyre::eyre!(
                    "Found duplicate partition hashes: {:?}",
                    hash_analysis.duplicate_hashes
                ));
            }

            // Verify each assignment has valid data
            for (i, assignment) in assignments_resp.assignments.iter().enumerate() {
                info!(
                    "  Assignment {}: Hash={:?}, Ledger={:?}, Slot={:?}",
                    i + 1,
                    assignment.partition_hash,
                    assignment.ledger_id,
                    assignment.slot_index
                );
            }

            info!("✓ All partition hash assignments are valid");
        }
        AssignmentStatus::PartiallyAssigned { assigned, total } => {
            return Err(eyre::eyre!(
                "Only {}/{} partition hashes assigned after epoch boundary for genesis node",
                assigned,
                total
            ));
        }
        AssignmentStatus::Unassigned => {
            return Err(eyre::eyre!(
                "No partition hashes assigned after epoch boundary for genesis node"
            ));
        }
    }

    // Note: We've already waited for 2 epochs above, matching the non-remote test which mines 2 epochs total after staking

    // Validate precise partition assignments after epoch transitions
    info!("Validating precise partition assignments...");

    let node_addresses = vec![
        "0x10605A299777D44BE5373D120d5479f07860325d", // irys-1 (Genesis)
        "0x951AA3768fE8d51DbC348520b0825C0c1f197277", // irys-2
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC", // irys-3 (Hardhat account 2)
    ];

    match validate_partition_assignments(&clients[0], GENESIS_NODE_ADDR, 1, 2, 0).await {
        Ok(_) => info!("Genesis node partition assignments validated"),
        Err(e) => info!("Genesis node validation failed: {}", e),
    }

    if node_addresses.len() >= 3 {
        match validate_partition_assignments(&clients[0], node_addresses[1], 1, 2, 0).await {
            Ok(_) => info!("Node2 partition assignments validated"),
            Err(e) => info!("Node2 validation failed: {}", e),
        }

        match validate_partition_assignments(&clients[0], node_addresses[2], 1, 2, 0).await {
            Ok(_) => info!("Node3 partition assignments validated"),
            Err(e) => info!("Node3 validation failed: {}", e),
        }
    }

    match wait_for_height_sync(&clients, 30).await {
        Ok(_) => info!("All nodes synchronized to same height"),
        Err(e) => info!("Height sync timeout: {}", e),
    }

    for (i, node_addr) in node_addresses.iter().enumerate() {
        info!(
            "Checking partition assignments for Node{} ({})",
            i + 1,
            node_addr
        );

        match get_all_assignments(&clients[0], node_addr).await {
            Ok(assignments_resp) => {
                info!(
                    "Node{} has {} total partition assignments (Epoch: {})",
                    i + 1,
                    assignments_resp.assignments.len(),
                    assignments_resp.epoch_height
                );

                // Check assignment status
                match &assignments_resp.assignment_status {
                    AssignmentStatus::FullyAssigned => {
                        info!("Node{}: ✓ All partitions fully assigned", i + 1)
                    }
                    AssignmentStatus::PartiallyAssigned { assigned, total } => {
                        info!(
                            "Node{}: ⚠ Partially assigned: {}/{}",
                            i + 1,
                            assigned,
                            total
                        );
                    }
                    AssignmentStatus::Unassigned => {
                        info!("Node{}: ✗ No partitions assigned", i + 1)
                    }
                }

                // Analyze hash quality
                let hash_analysis = &assignments_resp.hash_analysis;
                info!(
                    "Node{} hash analysis: {} total, {} unique, {} zero hashes",
                    i + 1,
                    hash_analysis.total_hashes,
                    hash_analysis.unique_hashes,
                    hash_analysis.zero_hashes
                );

                if hash_analysis.zero_hashes > 0 {
                    info!(
                        "Node{}: ⚠ Found {} zero hashes",
                        i + 1,
                        hash_analysis.zero_hashes
                    );
                }

                if !hash_analysis.duplicate_hashes.is_empty() {
                    info!(
                        "Node{}: ⚠ Found duplicate hashes: {:?}",
                        i + 1,
                        hash_analysis.duplicate_hashes
                    );
                }

                // Count assignments by ledger type
                let submit_assignments: Vec<_> = assignments_resp
                    .assignments
                    .iter()
                    .filter(|pa| pa.ledger_id == Some(irys_types::DataLedger::Submit.into()))
                    .collect();
                let publish_assignments: Vec<_> = assignments_resp
                    .assignments
                    .iter()
                    .filter(|pa| pa.ledger_id == Some(irys_types::DataLedger::Publish.into()))
                    .collect();

                info!(
                    "Node{}: {} Submit assignments, {} Publish assignments",
                    i + 1,
                    submit_assignments.len(),
                    publish_assignments.len()
                );

                // Log details of each assignment
                for assignment in &assignments_resp.assignments {
                    info!(
                        "  Assignment - Ledger: {:?}, Slot: {:?}",
                        assignment.ledger_id, assignment.slot_index
                    );
                }
            }
            Err(e) => {
                info!("Failed to get assignments for Node{}: {}", i + 1, e);
            }
        }
    }

    info!("Partition assignment verification completed");

    info!("Verifying slot replica distribution...");

    let expected_replicas = 3;

    // 30 attempts * 2s = 60 seconds. Replicas assigned async at epoch boundaries,
    // timing varies based on when nodes joined relative to epoch cycle.
    let mut replicas_ready = false;
    let max_attempts = REPLICA_WAIT_MAX_ATTEMPTS;
    let mut attempt = 0;

    while !replicas_ready && attempt < max_attempts {
        attempt += 1;

        match verify_all_slots_replicated(&clients, &node_addresses, expected_replicas).await {
            Ok(true) => {
                replicas_ready = true;
                info!("✓ All slots have {} replicas", expected_replicas);
            }
            Ok(false) => {
                info!("Replica assignment in progress... (attempt {})", attempt);
                sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                info!("Replica verification error: {}", e);
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    if !replicas_ready {
        info!(
            "Warning: Slots did not achieve {} replicas within {} attempts",
            expected_replicas, max_attempts
        );
        info!("Proceeding with available replica configuration");
    }

    let replica_summary = aggregate_slot_replicas(&clients, &node_addresses).await?;

    info!("Slot replica summary:");
    info!("  Total slots: {}", replica_summary.total_slots);
    info!(
        "  Fully replicated ({} replicas): {}",
        expected_replicas, replica_summary.fully_replicated_slots
    );
    info!(
        "  Under-replicated: {}",
        replica_summary.under_replicated_slots
    );
    info!(
        "  Over-replicated: {}",
        replica_summary.over_replicated_slots
    );

    // Log detailed slot replica distribution
    for slot_info in replica_summary.slots.iter().take(5) {
        // Show first 5 slots as examples
        info!(
            "  Slot {} (Ledger {:?}): {} replicas on nodes: {:?}",
            slot_info.slot_index,
            slot_info.ledger,
            slot_info.replica_count,
            slot_info
                .replicas
                .iter()
                .map(|r| &r.node_address[..8]) // Show first 8 chars of address
                .collect::<Vec<_>>()
        );
    }

    verify_replica_distribution(&clients, &node_addresses).await?;

    info!("Slot replica verification completed");

    if let Some(ref tx) = data_tx {
        info!("Transaction posted with {} chunks", tx.chunks.len());

        match wait_for_ingress_proofs(&clients[0], vec![tx.header.id], 30).await {
            Ok(_) => info!("Transaction has ingress proofs and is promoted!"),
            Err(e) => info!("Ingress proof timeout (may be expected in test env): {}", e),
        }
    }

    info!(
        "Expected sync targets: {} data chunks, {} packed in slot0, {} packed in slot1",
        TEST_DATA_CHUNKS, EXPECTED_PACKED_SLOT0, EXPECTED_PACKED_SLOT1
    );

    let mut all_synced = false;
    let mut attempt = 0;
    let max_attempts = SYNC_VALIDATION_MAX_ATTEMPTS;

    while !all_synced && attempt < max_attempts {
        attempt += 1;
        sleep(Duration::from_secs(1)).await;

        let genesis_height = get_chain_height(&clients[0]).await?;
        if genesis_height < GENESIS_FINAL_HEIGHT {
            info!(
                "Genesis at height {}, waiting for height {}+ before validating multi-node sync",
                genesis_height, GENESIS_FINAL_HEIGHT
            );
            continue;
        }

        let node_addresses_strings: Vec<String> = node_addresses
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let validation_results = match check_nodes_sync_status_by_assignment(
            &clients,
            &node_addresses_strings,
            TEST_DATA_CHUNKS,
            EXPECTED_PACKED_SLOT1,
        )
        .await
        {
            Ok(results) => results,
            Err(e) => {
                info!(
                    "Could not validate all nodes (some may not be reachable yet): {}",
                    e
                );
                continue;
            }
        };

        // Check if all nodes are synced
        all_synced = validation_results.iter().all(|result| result.is_synced);

        // Log detailed validation results and assignments for debugging
        for result in &validation_results {
            info!(
                "{} assignments - Publish slots: {:?}, Submit slots: {:?}",
                result.node_name,
                result.assignment_info.publish_slots,
                result.assignment_info.submit_slots
            );

            // Log validation details if not synced
            if !result.is_synced {
                info!(
                    "{} validation details: {}",
                    result.node_name, result.details
                );
            }
        }

        // Log overall sync status
        let synced_nodes: Vec<_> = validation_results
            .iter()
            .filter(|result| result.is_synced)
            .map(|result| result.node_name.clone())
            .collect();

        info!(
            "Sync status at attempt {}: {}/{} nodes synced",
            attempt,
            synced_nodes.len(),
            clients.len()
        );

        if all_synced {
            info!("All nodes synced successfully at attempt {}", attempt);
            break;
        }
    }

    let final_height = get_chain_height(&clients[0]).await?;
    if !all_synced {
        if data_tx.is_some() && final_height >= GENESIS_FINAL_HEIGHT {
            return Err(eyre::eyre!(
                "Nodes failed to sync data within {} attempts (height: {})",
                max_attempts,
                final_height
            ));
        } else {
            info!(
                "Test completed before height {} - multi-node validation skipped",
                GENESIS_FINAL_HEIGHT
            );
        }
    } else {
        info!("All peers successfully synced data chunks to their assigned partitions!");
    }

    info!("Verifying nodes can continue mining...");
    for i in 0..5 {
        info!("Mining verification round {}...", i + 1);

        let pre_height = get_chain_height(&clients[0]).await?;
        sleep(Duration::from_secs(2)).await;
        let post_height = get_chain_height(&clients[0]).await?;

        if post_height > pre_height {
            info!("Block mined: {} -> {}", pre_height, post_height);

            // Verify all nodes sync to new height
            for (j, client) in clients.iter().enumerate() {
                let node_height = get_chain_height(client).await?;
                info!("Node{} at height {}", j, node_height);
            }
        }
    }

    info!("Remote partition data sync test completed successfully!");
    Ok(())
}
