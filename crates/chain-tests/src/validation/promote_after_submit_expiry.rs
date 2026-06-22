// NC-0042 §4c validator consensus rule: a block that promotes a tx whose
// Submit-ledger storage has already expired must be rejected with
// `ShadowTransactionInvalid` — even when the expiry happened in an *earlier*
// block (the cross-block silent double-pay, devnet 39960/39962).
//
// Scenario (real flow, no DB seeding):
//   1. Drive enough data txs through the Submit ledger to force a 2nd slot, so
//      the genesis slot genuinely expires (per-slot expiry; the last slot is
//      never expired — see `publish_after_submit_expiry_filtered.rs`).
//   2. Mine honestly to the expiry epoch. The honest producer drops those txs
//      from promotion (§4b) and perm-fee-refunds them (Pipeline B).
//   3. AFTER expiry, an `EvilPublishStrategy` produces a block that promotes one
//      of the already-expired+refunded txs anyway (the malicious/buggy peer).
//   4. Both the genesis node and a peer must REJECT that block with
//      `ShadowTransactionInvalid`. Before §4c this block was accepted by every
//      validator (no check covered the cross-block case) → permanent double-pay.
//
// The earlier version of this test hand-inserted `MigratedBlockHashes` rows;
// that collides with the real chain's migration writes and crashes
// BlockTreeService. We drive the tx through the real Submit flow instead.

use crate::utils::{IrysNodeTest, assert_validation_error, solution_context};
use crate::validation::send_block_and_read_state;
use irys_actors::block_validation::ValidationError;
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, ProductionStrategy, async_trait,
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs,
};
use irys_domain::BlockTreeReadGuard;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, DataTransactionHeader, IngressProofsList, IrysBlockHeader, NodeConfig,
    UnixTimestampMs,
};

#[test_log::test(tokio::test)]
async fn heavy_block_promoting_already_expired_submit_tx_gets_rejected() -> eyre::Result<()> {
    /// Forces an already-expired Submit tx into the Publish ledger, with a valid
    /// ingress proof so the block fails validation *only* on the §4c expiry rule
    /// (not on a missing/invalid proof). No refund is scheduled in the bundle, so
    /// the in-constructor defence-in-depth guard does not fire during production —
    /// the block is built and must be caught at validation time.
    struct EvilPublishStrategy {
        prod: ProductionStrategy,
        expired_tx: DataTransactionHeader,
        proofs: IngressProofsList,
        block_tree_guard: BlockTreeReadGuard,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilPublishStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
            _block_timestamp: UnixTimestampMs,
        ) -> Result<
            irys_actors::block_producer::MempoolTxsBundle,
            irys_actors::tx_selector::TxSelectorError,
        > {
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![],
                one_year_txs: vec![],
                thirty_day_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![self.expired_tx.clone()],
                    proofs: Some(self.proofs.clone()),
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: self.block_tree_guard.read().canonical_epoch_snapshot(),
            })
        }
    }

    // --- 1. Config (mirrors the expiry setup of the §4b producer test) ---
    let num_blocks_in_epoch: usize = 5;
    let submit_ledger_epoch_length: u64 = 2;
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let blocks_per_cycle = num_blocks_in_epoch as u64 * submit_ledger_epoch_length;
    let seconds_to_wait = 40;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    {
        let c = genesis_config.consensus.get_mut();
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.block_migration_depth = 1;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
    }

    let user_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&user_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_config = genesis_node.testing_peer_with_signer(&user_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // --- 2. Post enough single-chunk data txs to overflow one partition (forces a
    //         2nd Submit slot so the genesis slot 0 is non-last and expires). ---
    let num_txs = (num_chunks_in_partition + 2) as usize;
    let anchor = genesis_node.get_anchor().await?;
    let mut txs = Vec::with_capacity(num_txs);
    for i in 0..num_txs {
        let data = vec![100 + i as u8; chunk_size as usize];
        let tx = genesis_node.post_data_tx(anchor, data, &user_signer).await;
        genesis_node
            .wait_for_mempool(tx.header.id, seconds_to_wait)
            .await?;
        txs.push(tx);
    }
    genesis_node.mine_block().await?;

    // --- 3. Mine honestly to the expiry epoch. Pipeline B refunds the slot-0 txs
    //         here and the honest producer drops them from promotion (§4b). ---
    let expiry_height = blocks_per_cycle; // = 10
    while genesis_node.get_canonical_chain_height().await < expiry_height {
        genesis_node.mine_block().await?;
    }

    // --- 4. Pick a target tx and confirm it is genuinely expired at the NEXT
    //         (cross-block) height we'll produce the evil block at. ---
    let evil_height = expiry_height + 1; // E+1: cross-block, not the epoch block
    // The current tip is the parent of the evil block we'll produce at E+1.
    let evil_parent_block = genesis_node.get_block_by_height(expiry_height).await?;
    let tip_hash = evil_parent_block.block_hash;
    // Bind the snapshot in its own scope so the block-tree read guard is dropped
    // before the await below (held-guard-across-await is a clippy deny).
    let tip_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&tip_hash)
        .expect("epoch snapshot for the current tip");
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &tip_snapshot,
        &evil_parent_block,
        evil_height,
        &genesis_node.node_ctx.config,
        genesis_node
            .node_ctx
            .block_producer_inner
            .block_index
            .clone(),
        &genesis_node.node_ctx.block_tree_guard,
        &genesis_node.node_ctx.mempool_guard,
        &genesis_node.node_ctx.db,
    )
    .await?;

    // Differential check (the equivalence the inversion relies on): the
    // per-candidate predicate that the producer filter and validator check now
    // use must agree with the `expired_submit_tx_ids` walk oracle — in both
    // directions — for every posted Submit tx, against real chain state.
    for tx in &txs {
        let per_candidate = irys_actors::block_producer::ledger_expiry::is_submit_storage_expired(
            tx.header.id,
            evil_height,
            &tip_snapshot,
            &evil_parent_block,
            &genesis_node.node_ctx.config,
            &genesis_node.node_ctx.block_producer_inner.block_index,
            &genesis_node.node_ctx.block_tree_guard,
            &genesis_node.node_ctx.mempool_guard,
            &genesis_node.node_ctx.db,
        )
        .await?;
        assert_eq!(
            per_candidate,
            expired_set.contains(&tx.header.id),
            "is_submit_storage_expired must match the expired_submit_tx_ids walk oracle for tx {}",
            tx.header.id,
        );
    }

    let target = txs
        .iter()
        .find(|tx| expired_set.contains(&tx.header.id))
        .expect("at least one posted tx must have expired by the cross-block height");
    let expired_tx = target.header.clone();

    // --- 5. Build a valid ingress proof for the expired tx (genesis signer is
    //         staked) so the evil block fails ONLY on the §4c expiry rule. ---
    let genesis_signer = genesis_config.signer();
    let proof_anchor = genesis_node.get_anchor().await?;
    let chunks: Vec<Vec<u8>> = vec![target.data.clone().unwrap_or_default().into()];
    let proof = generate_ingress_proof(
        &genesis_signer,
        expired_tx.data_root,
        chunks.iter().map(|c| Ok(c.as_slice())),
        genesis_config.consensus_config().chain_id,
        proof_anchor,
    )?;

    // --- 6. Evil producer builds a block at E+1 that promotes the expired tx ---
    let evil_strategy = EvilPublishStrategy {
        expired_tx: expired_tx.clone(),
        proofs: IngressProofsList(vec![proof]),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: genesis_node.node_ctx.block_tree_guard.clone(),
    };
    let (block, _adj, _payload) = evil_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .expect("evil block should be produced (the guard fires at validation, not production)");

    assert_eq!(block.header().height, evil_height, "evil block at E+1");
    assert!(
        block.header().data_ledgers[DataLedger::Publish]
            .tx_ids
            .contains(&expired_tx.id),
        "evil block must promote the expired tx"
    );

    // --- 7. Both nodes must reject it with ShadowTransactionInvalid (§4c) ---
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        genesis_outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Genesis must reject a block promoting an already-expired Submit tx (NC-0042 §4c)",
    );

    let peer_outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        peer_outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Peer must reject a block promoting an already-expired Submit tx (NC-0042 §4c)",
    );

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
