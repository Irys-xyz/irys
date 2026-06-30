// NC-0042 §4c validator consensus rule: a block that promotes a tx whose
// Submit-ledger storage has already expired must be rejected with
// `ShadowTransactionInvalid` — even when the expiry happened in an *earlier*
// block (the cross-block silent double-pay).
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

use crate::utils::{assert_validation_error, solution_context};
use crate::validation::{send_block_and_read_state, submit_expiry_two_node_setup};
use irys_actors::block_validation::ValidationError;
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, ProductionStrategy, async_trait,
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs,
};
use irys_domain::BlockTreeReadGuard;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, DataTransactionHeader, IngressProofsList, IrysBlockHeader, UnixTimestampMs,
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

    // --- 1 + 2. Shared config + two-node startup + overflow data posting ---
    let setup = submit_expiry_two_node_setup(1).await?;
    let genesis_config = setup.genesis_config;
    let genesis_node = setup.genesis_node;
    let peer_node = setup.peer_node;
    let txs = setup.txs;
    let blocks_per_cycle = setup.blocks_per_cycle;
    let seconds_to_wait = setup.seconds_to_wait;

    // --- 3. Mine honestly to the expiry epoch. Pipeline B refunds the slot-0 txs
    //         here and the honest producer drops them from promotion (§4b). ---
    let expiry_height = blocks_per_cycle; // = 10
    let height_before_loop = genesis_node.get_canonical_chain_height().await;
    while genesis_node.get_canonical_chain_height().await < expiry_height {
        genesis_node.mine_block().await?;
    }
    // If the §4b producer filter were absent, the producer would panic at the
    // expiry epoch block and the chain would stall — the loop would never
    // terminate. Assert that height advanced so a stall fails fast instead.
    assert!(
        genesis_node.get_canonical_chain_height().await > height_before_loop,
        "chain must have advanced past the expiry epoch (§4b filter must not have stalled the producer)"
    );

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
    // The block-under-test's own Cascade status — exactly what production passes
    // to `expired_submit_range`; compute once and reuse for both oracle helpers.
    let evil_cascade_active = genesis_node
        .node_ctx
        .config
        .consensus
        .hardforks
        .is_cascade_active_at(evil_parent_block.timestamp_secs());
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &tip_snapshot,
        &evil_parent_block,
        evil_height,
        &genesis_node.node_ctx.config,
        evil_cascade_active,
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

    // Differential check: the per-candidate predicate the producer filter and
    // validator check use (`is_submit_storage_expired`) must agree EXACTLY with the
    // `expired_submit_tx_ids` walk oracle for every posted tx. Both are now exact
    // offset comparisons — the refund walk's `same_block` over-inclusion (which this
    // single-block layout hits) was closed alongside the per-candidate filter
    // (NC-0042 §4b / R2), so the oracle no longer over-includes whole blocks. We
    // still drive the evil block off the per-candidate verdict, which is what §4c
    // enforces.
    let mut per_candidate_expired = Vec::new();
    for tx in &txs {
        let per_candidate = irys_actors::block_producer::ledger_expiry::is_submit_storage_expired(
            tx.header.id,
            evil_height,
            &tip_snapshot,
            &evil_parent_block,
            &genesis_node.node_ctx.config,
            evil_cascade_active,
            &genesis_node.node_ctx.block_producer_inner.block_index,
            &genesis_node.node_ctx.block_tree_guard,
            &genesis_node.node_ctx.mempool_guard,
            &genesis_node.node_ctx.db,
        )
        .await?;
        assert_eq!(
            per_candidate,
            expired_set.contains(&tx.header.id),
            "is_submit_storage_expired must agree exactly with the expired_submit_tx_ids \
             walk oracle for tx {} (both are exact offset comparisons — NC-0042 §4b/R2)",
            tx.header.id,
        );
        if per_candidate {
            per_candidate_expired.push(tx);
        }
    }

    let target = per_candidate_expired
        .first()
        .copied()
        .expect("at least one posted tx must be expired by the per-candidate (§4c) verdict");
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

    // The §4c rejection message includes "See NC-0042." — match on that to ensure
    // the block is rejected for the right reason and not an unrelated shadow-tx error.
    let is_nc_0042_expiry_rejection = |e: &ValidationError| match e {
        ValidationError::ShadowTransactionInvalid(msg) => msg.contains("NC-0042"),
        _ => false,
    };

    // --- 7. Both nodes must reject it with ShadowTransactionInvalid (§4c) ---
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        genesis_outcome,
        is_nc_0042_expiry_rejection,
        "Genesis must reject a block promoting an already-expired Submit tx (NC-0042 §4c)",
    );

    // Peer must have the parent chain up to expiry_height before we send it the E+1 block;
    // otherwise it rejects for ParentBlockMissing rather than ShadowTransactionInvalid.
    peer_node
        .wait_until_height(expiry_height, seconds_to_wait)
        .await?;

    let peer_outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        peer_outcome,
        is_nc_0042_expiry_rejection,
        "Peer must reject a block promoting an already-expired Submit tx (NC-0042 §4c)",
    );

    // Neither node should have committed the evil block to the EVM layer.
    genesis_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;
    peer_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
