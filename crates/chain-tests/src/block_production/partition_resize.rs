//! Scaled partition-resize transition on actual node services. No data transaction spans slots.
use crate::utils::IrysNodeTest;
use irys_types::{NodeConfig, UnixTimestamp, hardfork_config::Estuary};
use std::time::Duration;

#[test_log::test(tokio::test)]
async fn heavy_partition_resize_activation_and_restart() -> eyre::Result<()> {
    let resize = Estuary {
        activation_timestamp: UnixTimestamp::from_secs(0),
        partition_chunks: 8,
        recall_chunks: 2,
        target_ingress_bytes_per_second: 32,
        minimum_partition_ids: 0,
    };
    let mut config = NodeConfig::testing_with_epochs(4);
    let c = config.consensus.get_mut();
    c.num_chunks_in_partition = 20;
    c.num_chunks_in_recall_range = 10;
    c.difficulty_adjustment.difficulty_adjustment_interval = 6;
    c.block_migration_depth = 2;
    c.block_tree_depth = 6;
    c.epoch.submit_ledger_epoch_length = 100;
    c.hardforks.estuary = Some(resize);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("RESIZE", 30)
        .await;
    let capacity_url = format!(
        "{}/v1/storage/capacity",
        node.node_ctx.config.node_config.local_api_url()
    );
    let before: serde_json::Value = reqwest::get(&capacity_url)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(before["geometry"]["partitionChunks"], "20");
    assert!(before["targetIngressBytesPerSecond"].is_null());
    node.mine_blocks(3).await?;
    let parent = node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let inventory = parent.all_active_partitions.clone();
    assert!(parent.partition_resize.is_none());
    let previous = node.get_block_by_height(3).await?;
    let activation = node.mine_block().await?;
    let epoch = node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let state = epoch
        .partition_resize
        .expect("partition resize activates at height 4");
    assert_eq!(state.activation_height, 4);
    assert_eq!(state.activation_hash, activation.block_hash);
    // E is credited per partition-step: the weight its legacy sibling would get.
    let legacy_weight = irys_types::U256::MAX / (irys_types::U256::MAX - previous.diff);
    assert_eq!(
        activation.cumulative_diff - previous.cumulative_diff,
        legacy_weight
    );
    assert_eq!(
        state.sampling_origin,
        activation.vdf_limiter_info.global_step_number + 1
    );
    assert_eq!(
        &epoch.all_active_partitions[..inventory.len()],
        inventory.as_slice()
    );
    assert!(epoch.all_active_partitions.len() as u64 * 8 >= inventory.len() as u64 * 20);
    for (miner, pledges) in &parent.commitment_state.pledge_commitments {
        let retained = &epoch.commitment_state.pledge_commitments[miner];
        assert_eq!(retained.len(), pledges.len());
        for (before, after) in pledges.iter().zip(retained) {
            assert_eq!(
                (before.id, before.signer, before.partition_hash),
                (after.id, after.signer, after.partition_hash)
            );
        }
    }
    let (normal, _) = irys_types::calculate_difficulty(
        4,
        previous.last_diff_timestamp,
        activation.timestamp,
        previous.diff,
        &parent.config.consensus.difficulty_adjustment,
    );
    assert_eq!(activation.diff, resize.activation_difficulty(normal, 10));
    assert_eq!(activation.last_diff_timestamp, activation.timestamp);
    let after: serde_json::Value = reqwest::get(&capacity_url)
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(after["geometry"]["partitionChunks"], "8");
    assert_eq!(after["geometry"]["recallChunks"], "2");
    assert_eq!(
        after["geometry"]["samplingOrigin"],
        state.sampling_origin.to_string()
    );
    assert_eq!(after["targetIngressBytesPerSecond"], "32");
    assert_eq!(
        after["reserveChunksPerEpoch"],
        resize.reserve_chunks(&parent.config.consensus)?.to_string()
    );
    // The activation block's proof is still checked against the parent geometry.
    irys_actors::block_validation::recall_recall_range_is_valid(
        &activation,
        &parent.config.consensus,
        &node.node_ctx.vdf_steps_guard,
        &node.node_ctx.block_tree_guard,
        &node.node_ctx.db,
        false,
    )
    .await?;
    let child = node.mine_block().await?;
    assert!(u64::from(child.poa.partition_chunk_offset) < 8);
    assert_eq!(child.diff, activation.diff);
    assert_eq!(
        child.cumulative_diff - activation.cumulative_diff,
        legacy_weight
    );
    let initial_floor = state.preserved_id_floor;
    node.mine_blocks(7).await?; // E is finalized and later epoch tasks have run.
    let at_six = node.get_block_by_height_from_index(6, false)?;
    assert_eq!(at_six.diff, activation.diff);
    assert_eq!(at_six.last_diff_timestamp, activation.timestamp);
    let at_nine = node.get_block_by_height(9).await?;
    let at_ten = node.get_block_by_height(10).await?;
    let (expected, stats) = irys_types::calculate_difficulty(
        6,
        activation.timestamp,
        at_ten.timestamp,
        at_nine.diff,
        &parent.config.consensus.difficulty_adjustment,
    );
    assert!(stats.is_some());
    assert_eq!(at_ten.diff, expected);
    assert_eq!(at_ten.last_diff_timestamp, at_ten.timestamp);
    // Wait on actual disk truncation, not the earlier logical interval view.
    let module_infos = epoch.map_storage_modules_to_partition_assignments();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let complete = module_infos.iter().all(|m| {
                m.submodules.iter().all(|(_, path)| {
                    let path = node.cfg.base_directory.join(path);
                    path.join("partition_resize.json").exists()
                        && std::fs::metadata(path.join("chunks.dat"))
                            .is_ok_and(|m| m.len() <= 8 * 32)
                })
            });
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    assert_eq!(
        node.node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize
            .unwrap()
            .preserved_id_floor,
        initial_floor
    );
    let restarted = node.stop().await.start().await;
    assert_eq!(
        restarted
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize
            .unwrap()
            .preserved_id_floor,
        initial_floor
    );
    let after_restart = restarted.mine_block().await?;
    assert!(u64::from(after_restart.poa.partition_chunk_offset) < 8);
    // A historical entropy proof above the new bound remains reproducible
    // after its original disk suffix is gone. The same offset is invalid for children.
    let mut historical_poa = previous.poa.clone();
    historical_poa.partition_chunk_offset = 19;
    historical_poa.tx_path = None;
    historical_poa.data_path = None;
    let mut entropy = Vec::new();
    irys_packing::capacity_single::compute_entropy_chunk(
        previous.miner_address,
        19,
        historical_poa.partition_hash.into(),
        parent.config.consensus.entropy_packing_iterations,
        parent.config.consensus.chunk_size as usize,
        &mut entropy,
        parent.config.consensus.chain_id,
    );
    historical_poa.chunk = Some(irys_types::Base64::from(entropy));
    let validate = |snapshot: &irys_domain::EpochSnapshot| {
        irys_actors::block_validation::poa_is_valid(
            &historical_poa,
            &restarted.node_ctx.block_index_guard,
            &restarted.node_ctx.block_tree_guard,
            &restarted.node_ctx.db,
            previous.previous_block_hash,
            previous.height - 1,
            snapshot,
            &parent.config.consensus,
            &previous.miner_address,
        )
    };
    validate(&parent)?;
    assert!(matches!(
        validate(&epoch),
        Err(irys_actors::block_validation::PreValidationError::PoAChunkOffsetOutOfBlockBounds)
    ));

    // Cold sync reconstructs the activation baseline and validates old PoA
    // chunks from block storage, without the original partition tails.
    let peer = IrysNodeTest::new(restarted.testing_peer())
        .start_with_name("RESIZE_REPLAY")
        .await;
    peer.wait_until_height(after_restart.height, 30).await?;
    assert_eq!(
        peer.node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize,
        restarted
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize
    );
    peer.stop().await;

    // One two-chunk transaction at offset zero fits both ledgers' first slots.
    let signer = restarted.cfg.signer();
    let tx = restarted.post_publish_data_tx(&signer, vec![7; 64]).await?;
    restarted.mine_blocks(3).await?;
    restarted.upload_chunks(&tx).await?;
    restarted
        .wait_for_ingress_proofs_no_mining(vec![tx.header.id], 30)
        .await?;
    restarted.mine_blocks(3).await?;
    restarted.wait_for_promotion(&tx.header.id, 15).await?;
    let tip = restarted.get_max_difficulty_block();
    assert_eq!(tip.ledger_total_chunks(irys_types::DataLedger::Submit), 2);
    assert_eq!(tip.ledger_total_chunks(irys_types::DataLedger::Publish), 2);
    restarted.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_partition_resize_reorg_selects_branch_activation() -> eyre::Result<()> {
    use crate::utils::wait_for_block_event;
    use irys_actors::block_tree_service::ValidationResult;
    let mut config = NodeConfig::testing_with_epochs(2);
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);
    let c = config.consensus.get_mut();
    c.num_chunks_in_partition = 20;
    c.num_chunks_in_recall_range = 10;
    c.difficulty_adjustment.difficulty_adjustment_interval = 8;
    c.block_migration_depth = 10;
    c.block_tree_depth = 20;
    c.epoch.submit_ledger_epoch_length = 100;
    c.hardforks.estuary = Some(Estuary {
        activation_timestamp: UnixTimestamp::from_secs(0),
        partition_chunks: 8,
        recall_chunks: 2,
        target_ingress_bytes_per_second: 32,
        minimum_partition_ids: 0,
    });
    let resize = c.hardforks.estuary.take().unwrap();
    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("RESIZE_FORK", 30)
        .await;
    let peer = genesis.testing_peer_with_assignments(&signer).await?;
    let height = genesis.get_max_difficulty_block().height;
    genesis.mine_blocks((18 - height) as usize).await?;
    peer.wait_until_height(18, 30).await?;
    let persisted_height = genesis.node_ctx.block_index_guard.read().latest_height();
    let activation_timestamp = UnixTimestamp::from_secs(
        genesis
            .get_block_by_height(persisted_height)
            .await?
            .timestamp_secs()
            .as_secs()
            + 1,
    );
    let mut stopped_peer = peer.stop().await;
    let mut stopped_genesis = genesis.stop().await;
    for config in [&mut stopped_genesis.cfg, &mut stopped_peer.cfg] {
        config.consensus.get_mut().hardforks.estuary = Some(Estuary {
            activation_timestamp,
            ..resize
        });
    }
    let genesis = stopped_genesis.start().await;
    let peer = stopped_peer.start().await;
    assert_eq!(genesis.get_max_difficulty_block().height, persisted_height);
    assert!(persisted_height.is_multiple_of(2));
    let common = genesis.mine_block().await?;
    assert!(common.timestamp_secs() >= activation_timestamp);
    let split_height = common.height;
    peer.wait_until_height(split_height, 30).await?;
    assert!(
        genesis
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize
            .is_none()
    );
    genesis.gossip_disable();
    peer.gossip_disable();
    genesis.mine_blocks(2).await?;
    let old = genesis
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .partition_resize
        .unwrap();
    peer.mine_blocks(3).await?;
    // Normal adjustment can give the branches different weights. Extend only
    // as far as needed to make the alternative strictly heavier.
    for _ in 0..8 {
        if peer.get_max_difficulty_block().cumulative_diff
            > genesis.get_max_difficulty_block().cumulative_diff
        {
            break;
        }
        peer.mine_blocks(1).await?;
    }
    assert!(
        peer.get_max_difficulty_block().cumulative_diff
            > genesis.get_max_difficulty_block().cumulative_diff
    );
    let replacement = peer
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .partition_resize
        .unwrap();
    assert_ne!(old.activation_hash, replacement.activation_hash);
    let reorg = genesis.wait_for_reorg(60);
    for height in (split_height + 1)..=peer.get_max_difficulty_block().height {
        let header = peer.get_block_by_height(height).await?;
        let sealed = peer
            .node_ctx
            .block_tree_guard
            .read()
            .get_sealed_block(&header.block_hash)
            .unwrap();
        let payload = peer
            .node_ctx
            .block_pool
            .execution_payload_provider
            .get_sealed_block_from_cache(&header.evm_block_hash.0.into())
            .await
            .unwrap();
        genesis
            .node_ctx
            .block_pool
            .execution_payload_provider
            .add_payload_to_cache(payload)
            .await;
        let mut events = genesis
            .node_ctx
            .service_senders
            .subscribe_block_state_updates();
        peer.send_block_to_peer(&genesis, sealed).await?;
        wait_for_block_event(&mut events, 60, |event| {
            event.block_hash == header.block_hash
                && matches!(event.validation_result, ValidationResult::Valid)
        })
        .await?;
    }
    reorg.await?;
    assert_eq!(
        genesis
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize,
        Some(replacement)
    );
    let child = genesis.mine_block().await?;
    assert!(u64::from(child.poa.partition_chunk_offset) < 8);
    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

/// While activation is pending, data transactions wait in the mempool: the
/// producer leaves them out (validators reject a block carrying them), so E
/// always has empty ledgers and its first child includes the backlog.
#[test_log::test(tokio::test)]
async fn heavy_partition_resize_pending_blocks_keep_ledgers_empty() -> eyre::Result<()> {
    let mut config = NodeConfig::testing_with_epochs(4);
    let c = config.consensus.get_mut();
    c.num_chunks_in_partition = 20;
    c.num_chunks_in_recall_range = 10;
    c.epoch.submit_ledger_epoch_length = 100;
    c.hardforks.estuary = Some(Estuary {
        activation_timestamp: UnixTimestamp::from_secs(0),
        partition_chunks: 8,
        recall_chunks: 2,
        target_ingress_bytes_per_second: 32,
        minimum_partition_ids: 0,
    });
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("RESIZE_PENDING", 30)
        .await;
    let signer = node.cfg.signer();
    let tx = node.post_publish_data_tx(&signer, vec![7; 64]).await?;
    node.mine_blocks(3).await?;
    for height in 1..=3 {
        let block = node.get_block_by_height(height).await?;
        assert_eq!(
            block.ledger_total_chunks(irys_types::DataLedger::Submit),
            0,
            "height {height} is mined while activation is pending"
        );
    }
    let activation = node.mine_block().await?;
    assert_eq!(activation.height, 4);
    assert!(
        node.node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .partition_resize
            .is_some()
    );
    assert_eq!(
        activation.ledger_total_chunks(irys_types::DataLedger::Submit),
        0
    );
    let child = node.mine_block().await?;
    assert_eq!(child.ledger_total_chunks(irys_types::DataLedger::Submit), 2);
    assert!(
        child.data_ledgers[irys_types::DataLedger::Submit]
            .tx_ids
            .0
            .contains(&tx.header.id)
    );
    node.stop().await;
    Ok(())
}
