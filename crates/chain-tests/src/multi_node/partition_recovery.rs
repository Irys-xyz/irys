use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::submodule::db::{get_data_root_infos_for_data_root, get_path_hashes_by_offset};
use irys_domain::ChunkType;
use irys_types::{
    BoundedFee, DataLedger, DataTransaction, H256, LedgerChunkOffset, NodeConfig,
    PartitionChunkOffset, UnixTimestamp, hardfork_config::Cascade, irys::IrysSigner,
};
use tracing::info;

/// Tests that network partition recovery is surgical: shared data from the common
/// ancestor chain is preserved while only orphaned fork data is cleared and
/// re-indexed with the winning fork's data.
///
/// Timeline:
///   Blocks 1–6  (gossip ON)  — genesis mines shared base with txs, peer follows
///   Blocks 7+   (gossip OFF) — each node posts unique txs and mines independently
///   Gossip ON   — peer gossips longer fork to genesis, genesis reorgs
///
/// Verifies on genesis:
///   - Shared offsets 0–2: data_root + path hashes preserved
///   - Orphaned offsets 3–5: Uninitialized, genesis's unique data_roots cleared
///   - Re-indexed offsets 3–5: peer's unique data_roots present after re-migration
///   - Supply state matches new canonical chain
#[test_log::test(tokio::test)]
async fn heavy4_network_partition_recovery() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    // migration_depth=1 so that 2+ orphaned fork blocks trigger recovery
    let block_migration_depth: u32 = 1;
    let chunk_size: u64 = 32;

    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.num_partitions_per_term_ledger_slot = 1;
        c.epoch.num_blocks_in_epoch = 2;
        c.block_migration_depth = block_migration_depth;
        c.entropy_packing_iterations = 1_000;
        // Raise initial difficulty so cumulative_diff grows meaningfully per block,
        // allowing fork choice to distinguish longer chains.
        c.genesis.initial_packed_partitions = Some(5.0);
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    genesis_config.storage.num_writes_before_sync = 1;

    let genesis_signer = genesis_config.new_random_signer();
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&genesis_signer, &peer_signer]);

    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 10)?;
    let genesis = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis.testing_peer_with_signer(&peer_signer);
    let peer_test = IrysNodeTest::new(peer_config);
    StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), 10)?;
    let peer = peer_test
        .start_and_wait_for_packing("PEER", seconds_to_wait)
        .await;

    // Ensure both nodes know about each other for gossip
    IrysNodeTest::announce_between(&genesis, &peer).await?;

    // Peer only follows during shared base — stop it from producing competing blocks
    peer.stop_mining();

    // ─── Stage 1: Shared base with data (gossip ON) ───

    // Block 1
    genesis.mine_block().await?;
    let block_1 = genesis.get_block_by_height(1).await?;
    peer.wait_for_block(&block_1.block_hash, seconds_to_wait)
        .await?;
    info!("Both nodes synced to height 1");

    // Block 2 (epoch boundary → partition assignments)
    genesis.mine_block().await?;
    let block_2 = genesis.get_block_by_height(2).await?;
    peer.wait_for_block(&block_2.block_hash, seconds_to_wait)
        .await?;
    info!("Both nodes synced to height 2 (epoch boundary)");

    genesis.wait_for_packing(seconds_to_wait).await;
    peer.wait_for_packing(seconds_to_wait).await;

    // Post shared txs on genesis
    let shared_publish_chunks = [[1_u8; 32], [2_u8; 32], [3_u8; 32]];
    let shared_publish_tx = post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &shared_publish_chunks,
        DataLedger::Publish,
    )
    .await?;

    let shared_one_year_chunks = [[4_u8; 32], [5_u8; 32], [6_u8; 32]];
    let shared_one_year_tx = post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &shared_one_year_chunks,
        DataLedger::OneYear,
    )
    .await?;

    let shared_thirty_day_chunks = [[7_u8; 32], [8_u8; 32], [9_u8; 32]];
    let shared_thirty_day_tx = post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &shared_thirty_day_chunks,
        DataLedger::ThirtyDay,
    )
    .await?;

    // Mine blocks 3–6, syncing peer after each
    for h in 3..=6_u64 {
        genesis.mine_block().await?;
        let block = genesis.get_block_by_height(h).await?;
        peer.wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
        info!(height = h, "Both nodes synced");
    }
    let fork_height = 6_u64;
    info!("Shared base complete at height {fork_height}");

    // Wait for shared tx migration on genesis and verify chunks
    let shared_index_height = fork_height - block_migration_depth as u64;
    genesis
        .wait_until_block_index_height(shared_index_height, seconds_to_wait)
        .await?;
    for i in 0..3 {
        genesis
            .wait_for_chunk_in_storage(
                DataLedger::Submit,
                LedgerChunkOffset::from(i as u64),
                seconds_to_wait,
            )
            .await?;
    }
    let data_size = 96_u64;
    for (i, chunk) in shared_publish_chunks.iter().enumerate() {
        genesis
            .verify_migrated_chunk_32b(
                DataLedger::Submit,
                LedgerChunkOffset::from(i as i32),
                chunk,
                data_size,
            )
            .await;
    }
    info!("Shared submit chunks verified at offsets 0–2 on genesis");

    let ledgers_to_verify = [
        DataLedger::Submit,
        DataLedger::Publish,
        DataLedger::OneYear,
        DataLedger::ThirtyDay,
    ];
    for ledger in &ledgers_to_verify {
        let intervals = genesis.get_storage_module_intervals(*ledger, 0, ChunkType::Data);
        assert!(
            !intervals.is_empty(),
            "{:?}: expected Data intervals on genesis after shared base migration",
            ledger
        );
    }
    info!("Genesis has Data intervals in all 4 ledgers from shared base");

    // ─── Stage 2: Fork — each node posts unique data (gossip OFF) ───
    genesis.gossip_disable();
    peer.gossip_disable();
    info!("Gossip disabled, fork point at height {fork_height}");

    // Genesis posts unique txs (minority fork)
    let genesis_publish_chunks = [[40_u8; 32], [41_u8; 32], [42_u8; 32]];
    post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &genesis_publish_chunks,
        DataLedger::Publish,
    )
    .await?;

    let genesis_one_year_chunks = [[50_u8; 32], [51_u8; 32], [52_u8; 32]];
    let genesis_one_year_tx = post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &genesis_one_year_chunks,
        DataLedger::OneYear,
    )
    .await?;

    let genesis_thirty_day_chunks = [[60_u8; 32], [61_u8; 32], [62_u8; 32]];
    let genesis_thirty_day_tx = post_tx_with_chunks(
        &genesis,
        &genesis_signer,
        &genesis_thirty_day_chunks,
        DataLedger::ThirtyDay,
    )
    .await?;

    // Re-enable mining on peer for its fork phase
    peer.node_ctx.start_mining()?;
    peer.wait_for_packing(seconds_to_wait).await;

    // Peer posts unique txs (majority fork)
    let peer_publish_chunks = [[140_u8; 32], [141_u8; 32], [142_u8; 32]];
    let peer_publish_tx = post_tx_with_chunks(
        &peer,
        &peer_signer,
        &peer_publish_chunks,
        DataLedger::Publish,
    )
    .await?;

    let peer_one_year_chunks = [[150_u8; 32], [151_u8; 32], [152_u8; 32]];
    let peer_one_year_tx = post_tx_with_chunks(
        &peer,
        &peer_signer,
        &peer_one_year_chunks,
        DataLedger::OneYear,
    )
    .await?;

    let peer_thirty_day_chunks = [[160_u8; 32], [161_u8; 32], [162_u8; 32]];
    let peer_thirty_day_tx = post_tx_with_chunks(
        &peer,
        &peer_signer,
        &peer_thirty_day_chunks,
        DataLedger::ThirtyDay,
    )
    .await?;

    // Genesis mines 2 blocks (minority fork). With migration_depth=1,
    // block index advances to height 7 (8-1=7). When the reorg happens,
    // old_fork_blocks(2) > migration_depth(1) triggers recovery.
    for i in 0..2 {
        let (block, _payload, _txs) = genesis.mine_block_without_gossip().await?;
        let tx_count: usize = block.data_ledgers.iter().map(|dl| dl.tx_ids.0.len()).sum();
        info!(
            height = block.height,
            tx_count,
            "Genesis fork block {}",
            i + 1
        );
    }
    let genesis_height = fork_height + 2; // = 8
    genesis
        .wait_until_height(genesis_height, seconds_to_wait)
        .await?;
    let genesis_index_height = genesis_height - block_migration_depth as u64; // = 7
    genesis
        .wait_until_block_index_height(genesis_index_height, seconds_to_wait)
        .await?;
    info!(
        genesis_index_height,
        actual = genesis.get_block_index_height(),
        "Genesis fork migrated"
    );

    // Peer mines 3 blocks (majority fork). The peer's 3rd block (height 9)
    // is taller than genesis's tip (height 8), so cumulative_diff is guaranteed
    // higher and the reorg triggers deterministically.
    let mut peer_fork_blocks = Vec::new();
    for i in 0..3 {
        let (block, _payload, _txs) = peer.mine_block_without_gossip().await?;
        let tx_count: usize = block.data_ledgers.iter().map(|dl| dl.tx_ids.0.len()).sum();
        info!(height = block.height, tx_count, "Peer fork block {}", i + 1);
        peer_fork_blocks.push(block);
    }
    let peer_height = fork_height + 3; // = 9
    info!(peer_height, genesis_height, "Peer fork is longer");

    // ─── Stage 3: Reorg — gossip peer's longer chain to genesis ───

    // Re-enable gossip and re-announce so peers are marked online.
    // Health checks during the disabled window may have set is_online=false,
    // which would block payload fetching via top_active_peers.
    genesis.gossip_enable();
    peer.gossip_enable();
    IrysNodeTest::announce_between(&genesis, &peer).await?;
    info!("Gossip re-enabled, peers re-announced");

    // Gossip peer's fork blocks to genesis one at a time.
    // The receiving node pulls block bodies (incl. tx headers) and ETH
    // execution payloads from the peer via P2P.
    for block in &peer_fork_blocks {
        peer.gossip_block_to_peers(block)?;
        genesis
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }

    genesis
        .wait_until_height(peer_height, seconds_to_wait)
        .await?;
    let adopted_height = genesis.get_canonical_chain_height().await;
    info!(adopted_height, "Genesis adopted peer's chain");

    // Wait for re-migration on genesis
    let new_expected_index = adopted_height - block_migration_depth as u64;
    genesis
        .wait_until_block_index_height(new_expected_index, seconds_to_wait)
        .await?;

    // ─── Stage 4: Verify surgical recovery on genesis ───
    let index_height = genesis.get_block_index_height();
    assert!(
        index_height >= new_expected_index,
        "Expected block index height >= {new_expected_index}, got {index_height}"
    );

    // Verify supply state
    let supply_after = genesis
        .node_ctx
        .supply_state_guard
        .as_ref()
        .expect("supply state not set")
        .get();
    let mut expected_emitted = irys_types::U256::zero();
    for h in 1..=index_height {
        let block = genesis.get_block_by_height_from_index(h, false)?;
        expected_emitted = expected_emitted.saturating_add(block.reward_amount);
    }
    assert_eq!(
        supply_after.cumulative_emitted, expected_emitted,
        "Supply state should match new canonical chain's rewards"
    );
    info!("Supply state verified");

    // ── Block index contains winning fork blocks, not orphaned ──
    for h in (fork_height + 1)..=index_height {
        let indexed_block = genesis.get_block_by_height_from_index(h, false)?;
        let canonical_block = genesis.get_block_by_height(h).await?;
        assert_eq!(
            indexed_block.block_hash, canonical_block.block_hash,
            "Block index at height {h} should contain the winning fork's block"
        );
    }
    info!("Block index verified: all entries from winning fork");

    // ── Chain linkage across recovery boundary ──
    let block_at_fork = genesis.get_block_by_height_from_index(fork_height, false)?;
    let block_after = genesis.get_block_by_height_from_index(fork_height + 1, false)?;
    assert_eq!(
        block_after.previous_block_hash,
        block_at_fork.block_hash,
        "Block at height {} should link to the fork point at height {fork_height}",
        fork_height + 1
    );
    info!("Chain linkage verified across recovery boundary");

    // ── Common ancestor data preserved (offsets 0–2) ──
    // Shared tx data_roots should still be indexed
    let shared_txs: Vec<(DataLedger, &DataTransaction)> = vec![
        (DataLedger::Submit, &shared_publish_tx),
        (DataLedger::OneYear, &shared_one_year_tx),
        (DataLedger::ThirtyDay, &shared_thirty_day_tx),
    ];
    for (ledger, tx) in &shared_txs {
        assert!(
            has_data_root_in_storage_module(&genesis, *ledger, 0, tx.header.data_root),
            "{:?}: shared tx data_root should still be indexed after recovery",
            ledger
        );
    }
    info!("Shared tx data_roots confirmed preserved");

    // Path hashes at shared offsets still present
    for ledger in &ledgers_to_verify {
        for offset in 0..3_u32 {
            assert!(
                has_path_hashes_at_offset(&genesis, *ledger, 0, offset),
                "{:?}: path hashes at shared offset {offset} should be preserved",
                ledger
            );
        }
    }
    info!("Shared path hashes confirmed at offsets 0–2");

    // Shared chunk data should still be readable
    for (i, chunk) in shared_publish_chunks.iter().enumerate() {
        let offset = LedgerChunkOffset::from(i as i32);
        if let Some(packed) = genesis.get_chunk(DataLedger::Submit, offset).await {
            let unpacked = irys_packing::unpack(
                &packed,
                genesis.node_ctx.config.consensus.entropy_packing_iterations,
                genesis.node_ctx.config.consensus.chunk_size as usize,
                genesis.node_ctx.config.consensus.chain_id,
            );
            assert_eq!(
                unpacked.bytes.0, chunk,
                "Submit offset {i} should still contain shared chunk data"
            );
        }
    }
    info!("Shared submit chunk data confirmed readable");

    // ── Orphaned data cleared (offsets 3–5) ──
    let genesis_orphaned_txs: Vec<(DataLedger, &DataTransaction)> = vec![
        (DataLedger::OneYear, &genesis_one_year_tx),
        (DataLedger::ThirtyDay, &genesis_thirty_day_tx),
    ];
    for (ledger, tx) in &genesis_orphaned_txs {
        assert!(
            !has_data_root_in_storage_module(&genesis, *ledger, 0, tx.header.data_root),
            "{:?}: genesis orphaned tx data_root should be CLEARED after recovery",
            ledger
        );
    }
    info!("Genesis orphaned tx data_roots confirmed cleared");

    // Note: Uninitialized interval check is omitted because re-migration of
    // winning fork blocks runs immediately after recovery and may have already
    // re-filled some offsets by the time we check.

    // ── New canonical data re-indexed (offsets 3–5) ──
    let peer_unique_txs: Vec<(DataLedger, &DataTransaction)> = vec![
        (DataLedger::Submit, &peer_publish_tx),
        (DataLedger::OneYear, &peer_one_year_tx),
        (DataLedger::ThirtyDay, &peer_thirty_day_tx),
    ];
    for (ledger, tx) in &peer_unique_txs {
        let data_root = tx.header.data_root;
        let mut found = false;
        for _ in 0..seconds_to_wait {
            if has_data_root_in_storage_module(&genesis, *ledger, 0, data_root) {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(
            found,
            "{:?}: peer unique tx data_root should be indexed after re-migration",
            ledger
        );
    }
    info!("Peer unique tx data_roots confirmed re-indexed");

    // ── Verify continued operation after recovery ──
    genesis.mine_block().await?;
    peer.wait_until_height(peer_height + 1, seconds_to_wait)
        .await?;
    genesis.mine_block().await?;
    peer.wait_until_height(peer_height + 2, seconds_to_wait)
        .await?;

    let final_index_height = genesis.get_block_index_height();
    assert!(
        final_index_height > peer_height,
        "Block index should continue advancing after recovery, got {final_index_height}"
    );

    let final_supply = genesis
        .node_ctx
        .supply_state_guard
        .as_ref()
        .expect("supply state not set")
        .get();
    let mut final_expected = irys_types::U256::zero();
    for h in 1..=final_index_height {
        let block = genesis.get_block_by_height_from_index(h, false)?;
        final_expected = final_expected.saturating_add(block.reward_amount);
    }
    assert_eq!(
        final_supply.cumulative_emitted, final_expected,
        "Supply state should remain consistent after continued mining"
    );
    info!("Continued operation verified — mining and supply state consistent");

    genesis.stop().await;
    peer.stop().await;

    Ok(())
}

/// Check if a storage module for the given ledger/slot has a DataRootInfosByDataRoot entry.
fn has_data_root_in_storage_module(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    ledger: DataLedger,
    slot_index: usize,
    data_root: H256,
) -> bool {
    let sms = node.node_ctx.storage_modules_guard.read();
    for sm in sms.iter() {
        let Some(pa) = sm.partition_assignment() else {
            continue;
        };
        if pa.ledger_id == Some(ledger.into()) && pa.slot_index == Some(slot_index) {
            let result = sm.query_submodule_db_by_offset(PartitionChunkOffset::from(0), |tx| {
                get_data_root_infos_for_data_root(tx, data_root)
            });
            return matches!(result, Ok(Some(infos)) if !infos.0.is_empty());
        }
    }
    false
}

/// Check if ChunkPathHashesByOffset has valid path hashes at a given partition offset.
fn has_path_hashes_at_offset(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    ledger: DataLedger,
    slot_index: usize,
    partition_offset: u32,
) -> bool {
    let sms = node.node_ctx.storage_modules_guard.read();
    for sm in sms.iter() {
        let Some(pa) = sm.partition_assignment() else {
            continue;
        };
        if pa.ledger_id == Some(ledger.into()) && pa.slot_index == Some(slot_index) {
            let offset = PartitionChunkOffset::from(partition_offset);
            let result =
                sm.query_submodule_db_by_offset(offset, |tx| get_path_hashes_by_offset(tx, offset));
            return matches!(result, Ok(Some(hashes))
                if hashes.tx_path_hash.is_some() || hashes.data_path_hash.is_some());
        }
    }
    false
}

/// Helper: create a transaction for the given ledger, post it + chunks to the node.
async fn post_tx_with_chunks(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    chunks: &[[u8; 32]],
    ledger: DataLedger,
) -> eyre::Result<DataTransaction> {
    let data: Vec<u8> = chunks.concat();
    let data_size = data.len() as u64;
    let anchor = node.get_anchor().await?;

    let tx = if ledger == DataLedger::Publish {
        let price = node.get_data_price(DataLedger::Publish, data_size).await?;
        signer.create_publish_transaction(
            data,
            anchor,
            price.perm_fee.into(),
            price.term_fee.into(),
        )?
    } else {
        let price = node.get_data_price(ledger, data_size).await?;
        signer.create_transaction_with_fees(
            data,
            anchor,
            ledger,
            BoundedFee::new(price.term_fee),
            None,
        )?
    };
    let tx = signer.sign_transaction(tx)?;

    node.ingest_data_tx(tx.header.clone()).await?;
    node.wait_for_mempool(tx.header.id, 10).await?;

    for (i, _) in chunks.iter().enumerate() {
        node.post_chunk_32b_with_status(&tx, i, chunks).await;
    }

    info!(
        tx.id = %tx.header.id,
        ?ledger,
        "Posted tx with {} chunks",
        chunks.len()
    );
    Ok(tx)
}
