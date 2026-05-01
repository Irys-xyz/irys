use crate::utils::IrysNodeTest;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::CachedDataRoot;
use irys_database::tables::CachedDataRoots;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{DataLedger, NodeConfig, SendTraced as _, UnixTimestamp};
use reth_db::transaction::DbTxMut as _;
use tracing::info;

/// Regression test for: stale txid in CachedDataRoot.txid_set blocks block production.
///
/// Reproduces the scenario where a data tx enters the mempool, its data_root is cached
/// with an ingress proof, but the tx is later pruned from the mempool without being
/// included in a block. The CachedDataRoot.txid_set retains the stale txid, and the
/// IngressProof persists (local proof exemption). When block production runs
/// select_best_txs → get_publish_txs_and_proofs, it reads the stale txid and fails
/// with "Missing transactions" because the txid exists in neither the mempool nor the
/// IrysDataTxHeaders DB table.
#[cfg(debug_assertions)]
#[tokio::test]
async fn stale_txid_in_cached_data_root_blocks_block_production() -> eyre::Result<()> {
    let seconds_to_wait = 30;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.block_migration_depth = 1;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();

    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Mine blocks to establish a working chain
    genesis_node.mine_blocks(3).await?;

    // Create a data tx — we will NOT submit it to the mempool.
    // This simulates a tx that was pruned (anchor expired) after entering the mempool
    // but before being included in any block.
    let chunks: Vec<[u8; 32]> = vec![[10; 32], [20; 32], [30; 32]];
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.iter()).copied().collect();

    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await?;

    let data_tx = genesis_signer.create_publish_transaction(
        data,
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = genesis_signer.sign_transaction(data_tx)?;

    let stale_txid = data_tx.header.id;
    let data_root = data_tx.header.data_root;

    info!(
        tx.id = %stale_txid,
        tx.data_root = %data_root,
        "Injecting stale CachedDataRoot + IngressProof (tx NOT in mempool or DB)"
    );

    // Inject the stale state directly into the database.
    // This is exactly the state left behind when:
    //   1. A tx enters the mempool → CachedDataRoot is created with txid in txid_set
    //   2. Chunks are uploaded → IngressProof is generated and stored
    //   3. The tx is pruned from mempool (anchor expired) — but CachedDataRoot and
    //      IngressProof are NOT cleaned up because the local proof exemption in
    //      prune_data_root_cache() skips entries with a locally-generated proof
    genesis_node.node_ctx.db.update_eyre(|db_tx| {
        // Write CachedDataRoot with the stale txid
        let cached = CachedDataRoot {
            data_size: data_tx.header.data_size,
            data_size_confirmed: true,
            txid_set: vec![stale_txid],
            block_set: vec![],
            expiry_height: Some(1000), // far future so prune_data_root_cache won't delete it
            cached_at: UnixTimestamp::from_secs(0),
        };
        db_tx.put::<CachedDataRoots>(data_root, cached)?;
        Ok(())
    })?;

    // Generate and store an ingress proof for this data_root.
    // The proof must exist in IngressProofs for get_publish_txs_and_proofs to find it.
    let anchor = genesis_node.get_anchor().await?;
    let ingress_proof = generate_ingress_proof(
        &genesis_signer,
        data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        anchor,
    )?;

    genesis_node.node_ctx.db.update_eyre(|db_tx| {
        irys_database::store_ingress_proof_checked(db_tx, &ingress_proof, &genesis_signer)
    })?;

    // Verify the stale state is set up correctly
    genesis_node.node_ctx.db.view_eyre(|db_tx| {
        let cdr = irys_database::cached_data_root_by_data_root(db_tx, data_root)?
            .expect("CachedDataRoot should exist");
        assert!(
            cdr.txid_set.contains(&stale_txid),
            "CachedDataRoot.txid_set should contain the stale txid"
        );

        let proofs = irys_database::ingress_proofs_by_data_root(db_tx, data_root)?;
        assert!(
            !proofs.is_empty(),
            "IngressProofs should contain a proof for this data_root"
        );

        Ok(())
    })?;

    // Now trigger the bug: call select_best_txs via get_best_mempool_tx.
    // In debug builds, the debug_assert! in get_data_tx_in_parallel_inner will panic.
    // In release builds, it would log a warning and skip the stale txid.
    let parent = genesis_node
        .get_block_by_height(genesis_node.get_canonical_chain_height().await)
        .await?;

    // Spawn in a separate task so the panic is caught as a JoinError.
    // We clone all Arc-wrapped values; references to them are valid within the async move block.
    let db = genesis_node.node_ctx.db.clone();
    let block_tree = genesis_node.node_ctx.block_tree_guard.clone();
    let reth_adapter = genesis_node.node_ctx.reth_node_adapter.clone();
    let config = genesis_node.node_ctx.config.clone();
    let mempool_guard = genesis_node.node_ctx.mempool_guard.clone();
    let chunk_ingress_state = genesis_node.node_ctx.chunk_ingress_state.clone();
    let parent_hash = parent.block_hash;
    let parent_timestamp = {
        let tree = block_tree.read();
        tree.get_block(&parent_hash)
            .expect("parent block should exist")
            .timestamp_secs()
    };

    let result = tokio::task::spawn(async move {
        let ctx = irys_actors::tx_selector::TxSelectionContext {
            block_tree: &block_tree,
            db: &db,
            reth_adapter: &reth_adapter,
            config: &config,
            mempool_state: mempool_guard.atomic_state(),
            chunk_ingress_state: &chunk_ingress_state,
        };
        irys_actors::tx_selector::select_best_txs(
            parent_hash,
            irys_types::UnixTimestamp::from_secs(parent_timestamp.as_secs() + 1),
            &ctx,
        )
        .await
    })
    .await;

    // The spawned task should panic due to debug_assert
    assert!(
        result.is_err(),
        "Expected debug_assert panic from stale txid in CachedDataRoot, but got: {:?}",
        result.ok()
    );
    info!("Bug confirmed: stale txid caused debug_assert panic as expected");

    genesis_node.stop().await;

    Ok(())
}

/// Verifies that after Fix B, mempool pruning cleans stale txids from
/// CachedDataRoot.txid_set so block production succeeds.
///
/// Flow: submit tx via gossip (with genesis anchor that expires quickly) → upload
/// chunks → wait for ingress proof → mine blocks until anchor expires →
/// verify txid_set was cleaned → mine a block successfully.
#[tokio::test]
async fn slow_heavy_stale_txid_in_cached_data_root_does_not_block_after_fix() -> eyre::Result<()> {
    let seconds_to_wait = 30;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.block_migration_depth = 1;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();

    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Mine initial blocks
    genesis_node.mine_blocks(2).await?;

    let chunks: Vec<[u8; 32]> = vec![[10; 32], [20; 32], [30; 32]];
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.iter()).copied().collect();

    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await?;

    // Use block 0 (genesis) as anchor — it will expire quickly.
    // Submit via gossip to bypass anchor maturity checks at ingress.
    let genesis_block = genesis_node.get_block_by_height(0).await?;
    let data_tx = genesis_signer.create_publish_transaction(
        data,
        genesis_block.block_hash,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = genesis_signer.sign_transaction(data_tx)?;

    let txid = data_tx.header.id;
    let data_root = data_tx.header.data_root;

    info!(
        tx.id = %txid,
        tx.data_root = %data_root,
        "Submitting tx via gossip with genesis anchor (will expire quickly)"
    );

    // Ingest via gossip to bypass anchor maturity check
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    genesis_node
        .node_ctx
        .service_senders
        .mempool
        .send_traced(MempoolServiceMessage::IngestDataTxFromGossip(
            data_tx.header.clone(),
            resp_tx,
        ))
        .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
    let _ = resp_rx.await?;

    genesis_node.wait_for_mempool(txid, seconds_to_wait).await?;

    // The tx has a genesis anchor that will expire quickly, so it can never enter the
    // submit ledger (tx_selector rejects expired anchors). Manually generate and store
    // an ingress proof to simulate the pre-pruning state.
    let anchor = genesis_node.get_anchor().await?;
    let ingress_proof = generate_ingress_proof(
        &genesis_signer,
        data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        anchor,
    )?;
    genesis_node.node_ctx.db.update_eyre(|db_tx| {
        irys_database::store_ingress_proof_checked(db_tx, &ingress_proof, &genesis_signer)
    })?;

    // Verify CachedDataRoot.txid_set contains our txid
    genesis_node.node_ctx.db.view_eyre(|db_tx| {
        let cdr = irys_database::cached_data_root_by_data_root(db_tx, data_root)?
            .expect("CachedDataRoot should exist");
        assert!(cdr.txid_set.contains(&txid));
        Ok(())
    })?;

    // Mine enough blocks to expire the anchor.
    // Anchor at height 0, effective_expiry = tx_anchor_expiry_depth(3) + block_migration_depth(1) + 5 = 9
    // Prune when: anchor_height(0) < current_height - 9 → current_height > 9
    // We're at height 2, so mine 9 more blocks to reach height 11.
    genesis_node.mine_blocks(9).await?;

    // Poll until the stale txid is gone from CachedDataRoot.txid_set (or the entry is
    // deleted entirely).  The cleanup arrives via a fire-and-forget channel message to the
    // cache service, so we cannot rely on a fixed sleep — poll with a bounded deadline instead.
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let still_present = genesis_node.node_ctx.db.view_eyre(|db_tx| {
                Ok(
                    irys_database::cached_data_root_by_data_root(db_tx, data_root)?
                        .is_some_and(|cdr| cdr.txid_set.contains(&txid)),
                )
            })?;
            if !still_present {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Timed out waiting for stale txid to be pruned from CachedDataRoot.txid_set"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // Verify the txid was pruned from CachedDataRoot.txid_set by Fix B
    genesis_node.node_ctx.db.view_eyre(|db_tx| {
        match irys_database::cached_data_root_by_data_root(db_tx, data_root)? {
            Some(cdr) => {
                assert!(
                    !cdr.txid_set.contains(&txid),
                    "Fix B should have removed the stale txid from CachedDataRoot.txid_set"
                );
            }
            None => {
                // Entry was fully deleted — also acceptable
            }
        }
        Ok(())
    })?;

    // Mine one more block — should succeed without hitting the debug_assert
    let block = genesis_node.mine_block().await?;

    // The stale txid should NOT appear in any ledger
    for ledger in block.data_ledgers.iter() {
        assert!(
            !ledger.tx_ids.contains(&txid),
            "Stale txid should not appear in any block ledger"
        );
    }

    genesis_node.stop().await;

    Ok(())
}
