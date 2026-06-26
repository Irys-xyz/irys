// NC-0042 §4b/§4c end-to-end coverage with the Cascade hardfork ACTIVE.
//
// The shared §4b/§4c tests (`publish_after_submit_expiry_filtered.rs` and
// `promote_after_submit_expiry.rs`) both run with Cascade OFF (pinned in
// `submit_expiry_two_node_setup`). Post-Cascade the Submit-ledger expiry
// semantics change — written-slot tracking and last-write anchoring (see
// `cascade_term_expiry.rs`) — and the §4b filter set becomes a strict SUPERSET
// of the refund set (a slot rescued by a late write at its expiry epoch is
// excluded from refunds but still governs promotability). This test pins the
// §4b/§4c rule end-to-end on that post-Cascade path.
//
// What this test proves end-to-end, with Cascade active from genesis:
//   §4b (honest producer): the honest producer mines past the Submit slot's
//     expiry without panicking, and does NOT promote the expired tx. The slot's
//     unpromoted txs are perm-fee-refunded by Pipeline B — so the expired tx is
//     refunded but not promoted (no double-pay).
//   §4c (validator): a hand-built `EvilPublishStrategy` block that promotes the
//     already-expired Submit tx (with a valid ingress proof, so it fails ONLY on
//     the expiry rule) is REJECTED by a validating node with
//     `ShadowTransactionInvalid` carrying the NC-0042 marker.
//
// Directional, NOT exact-equality: under Cascade the §4b filter set is a
// superset of the refund set, so this test deliberately does NOT assert
// `refunded == expired`. It asserts the invariants that actually matter:
//   - the expired tx is non-promotable (the per-candidate verdict says expired),
//   - a block promoting it is rejected (§4c),
//   - it is not BOTH promoted AND refunded in the honest expiry block (§4b /
//     no double-pay).
//
// Setup (mirrors the post-Cascade Submit-expiry aging in `cascade_term_expiry`'s
// `heavy_cascade_rescued_slot_defers_reward_and_refund`, combined with the
// overflow-to-force-a-2nd-slot trick from the pre-Cascade §4b/§4c tests):
//   - num_blocks_in_epoch = 2, submit_ledger_epoch_length = 2 -> Submit slot
//     expiry window = 4 blocks.
//   - tiny partitions (num_chunks_in_partition = 10, chunk_size = 32) + posting
//     >10 single-chunk txs overflows partition 0, forcing a 2nd Submit slot so
//     genesis slot 0 is non-last and can actually expire.
//   - Cascade active from genesis (activation_timestamp = 0).
//   - 5 storage submodules so all 4 Cascade ledgers (Publish/Submit/OneYear/
//     ThirtyDay) plus the overflow 2nd Submit slot have partitions.
//
// We drive the txs through the real Submit flow (no DB seeding of
// MigratedBlockHashes — that collides with the chain's real migration writes and
// crashes BlockTreeService, per the notes in the pre-Cascade tests).

use crate::utils::{IrysNodeTest, assert_validation_error, solution_context};
use crate::validation::send_block_and_read_state;
use irys_actors::block_validation::ValidationError;
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, ProductionStrategy, async_trait,
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs,
};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_domain::BlockTreeReadGuard;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, DataTransactionHeader, H256, IngressProofsList, IrysBlockHeader, NodeConfig,
    UnixTimestamp, UnixTimestampMs, hardfork_config::Cascade,
};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeSet;

#[test_log::test(tokio::test)]
async fn heavy_cascade_block_promoting_already_expired_submit_tx_gets_rejected() -> eyre::Result<()>
{
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

    // --- 1. Config: Cascade ACTIVE from genesis + Submit-expiry aging params ---
    let num_blocks_in_epoch: u64 = 2;
    let submit_ledger_epoch_length: u64 = 2;
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    // Post-Cascade Submit-slot expiry window, measured from the slot's LAST write
    // (not its allocation) — unlike the pre-Cascade tests where this equals the
    // absolute expiry height.
    let expiry_window = num_blocks_in_epoch * submit_ledger_epoch_length; // = 4
    let one_year_epoch_length: u64 = 8;
    let thirty_day_epoch_length: u64 = 2;
    let seconds_to_wait = 40_usize;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    {
        let c = genesis_config.consensus.get_mut();
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.block_migration_depth = 1;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        // One ingress proof from the genesis signer makes a tx promotable.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        // Cascade active from genesis -> post-Cascade written-slot / last-write
        // Submit-expiry semantics for the whole run.
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length,
            thirty_day_epoch_length,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    }

    let user_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&user_signer]);

    // 5 storage submodules: 4 Cascade ledgers (Publish/Submit/OneYear/ThirtyDay)
    // plus headroom for the overflow 2nd Submit slot need partition assignments.
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_node.cfg.base_directory.clone(), 5)?;
    let genesis_node = genesis_node
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = {
        let peer_config = genesis_node.testing_peer_with_signer(&user_signer);
        IrysNodeTest::new(peer_config).start_with_name("PEER").await
    };

    // --- 2. Post >10 single-chunk txs to overflow partition 0, forcing a 2nd
    //         Submit slot so genesis slot 0 is non-last and can expire. ---
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
    let data_block = genesis_node.mine_block().await?;

    // Sanity: all posted txs landed in the Submit ledger at the data block.
    let submit_ids: BTreeSet<_> = data_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    assert_eq!(
        submit_ids.len(),
        num_txs,
        "all posted txs should land in the Submit ledger at the data block"
    );

    // --- 3. Mine to the next epoch boundary so the capacity overflow allocates a
    //         2nd Submit slot (genesis slot 0 becomes non-last) and the Cascade
    //         last-write touch stamps slot 0's `last_height` at the epoch that
    //         processes its writes. ---
    let next_epoch_boundary = |h: u64| -> u64 {
        if h.is_multiple_of(num_blocks_in_epoch) {
            h
        } else {
            h + (num_blocks_in_epoch - (h % num_blocks_in_epoch))
        }
    };
    let alloc_epoch = next_epoch_boundary(data_block.height + 1);
    while genesis_node.get_canonical_chain_height().await < alloc_epoch {
        genesis_node.mine_block().await?;
    }

    // Read slot 0's last-write height + the slot count from the alloc-epoch
    // snapshot. Under Cascade, Submit slot 0 expires at `last_height + window`
    // (last-write anchored), NOT at the allocation-anchored cycle height.
    let alloc_block = genesis_node.get_block_by_height(alloc_epoch).await?;
    let (slot0_last_height, submit_slot_count) = {
        let tree = genesis_node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&alloc_block.block_hash)
            .expect("epoch snapshot at the allocation epoch");
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        (slots[0].last_height, slots.len())
    };
    assert!(
        submit_slot_count >= 2,
        "capacity overflow must have allocated a 2nd Submit slot so slot 0 is non-last \
         (got {submit_slot_count} slots)"
    );

    // Submit slot 0 expires (post-Cascade) at the first epoch boundary at or after
    // `last_height + window`. Pipeline B refunds slot-0's unpromoted txs at that
    // epoch and the honest producer drops them from promotion (§4b). The loop
    // terminating proves the producer did NOT panic.
    let expiry_height = next_epoch_boundary(slot0_last_height + expiry_window);
    assert_eq!(
        expiry_height % num_blocks_in_epoch,
        0,
        "expiry must coincide with an epoch boundary so the refund pipeline runs"
    );
    let height_before_loop = genesis_node.get_canonical_chain_height().await;
    while genesis_node.get_canonical_chain_height().await < expiry_height {
        genesis_node.mine_block().await?;
    }
    assert!(
        genesis_node.get_canonical_chain_height().await > height_before_loop,
        "chain must advance past the expiry epoch (§4b filter must not stall the producer)"
    );

    // --- 4. §4b directional property: the honest expiry block must NOT promote
    //         any expired tx, and the expired txs must be perm-fee-refunded (so an
    //         expired tx is refunded but not promoted — no double-pay). ---
    let expiry_block = genesis_node.get_block_by_height(expiry_height).await?;
    let publish_ids: BTreeSet<_> = expiry_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Publish)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();

    // The expired Submit tx-id set the producer used at the expiry epoch (derived
    // from the expiry block's PARENT snapshot, post-Cascade gate).
    let expiry_parent_block = genesis_node.get_block_by_height(expiry_height - 1).await?;
    assert_eq!(
        expiry_parent_block.block_hash, expiry_block.previous_block_hash,
        "parent header must be the expiry block's parent"
    );
    let expiry_parent_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&expiry_block.previous_block_hash)
        .expect("epoch snapshot for the expiry block's parent");
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &expiry_parent_snapshot,
        &expiry_parent_block,
        expiry_height,
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
    assert!(
        !expired_set.is_empty(),
        "the genesis Submit slot must have expired at this epoch under Cascade"
    );
    // §4b: none of the expired txs may be promoted at the honest expiry block.
    for expired_tx_id in &expired_set {
        assert!(
            !publish_ids.contains(expired_tx_id),
            "expired Submit tx {expired_tx_id} must NOT be promoted (NC-0042 §4b, Cascade)"
        );
    }

    // No double-pay: at least one expired tx was perm-fee-refunded in the honest
    // expiry block, and that refunded tx is NOT in the Publish ledger. (Directional
    // — under Cascade the refund set may be a strict subset of `expired_set`, so we
    // do not assert exact equality.)
    let user_addr = user_signer.address().to_alloy_address();
    let evm_block = genesis_node
        .wait_for_evm_block(expiry_block.evm_block_hash, seconds_to_wait)
        .await?;
    let refunded_tx_ids: BTreeSet<H256> = evm_block
        .body
        .transactions
        .into_iter()
        .filter(|tx| tx.input().len() >= 4)
        .filter_map(|tx| {
            let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref()).ok()?;
            let packet = shadow_tx.as_v1()?;
            match packet {
                TransactionPacket::PermFeeRefund(refund) if refund.target == user_addr => {
                    Some(H256(refund.irys_ref.into()))
                }
                _ => None,
            }
        })
        .collect();
    assert!(
        !refunded_tx_ids.is_empty(),
        "at least one expired Submit tx must be perm-fee-refunded at the expiry epoch \
         (else the §4b/no-double-pay check is vacuous)"
    );
    for refunded in &refunded_tx_ids {
        assert!(
            expired_set.contains(refunded),
            "every refunded tx must be in the expired set (refund ⊆ expired under Cascade)"
        );
        assert!(
            !publish_ids.contains(refunded),
            "a perm-fee-refunded tx {refunded} must NOT also be promoted (no double-pay, §4b)"
        );
    }

    // --- 5. §4c: build an evil block at E+1 that promotes an already-expired tx
    //         and confirm a validator rejects it with the NC-0042 error. ---
    let evil_height = expiry_height + 1; // E+1: cross-block, not the epoch block
    let evil_parent_block = genesis_node.get_block_by_height(expiry_height).await?;
    let tip_hash = evil_parent_block.block_hash;
    let tip_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&tip_hash)
        .expect("epoch snapshot for the current tip");

    // Pick a target tx that the per-candidate (§4c) predicate marks expired at the
    // evil block's height, under the Cascade gate.
    let mut target: Option<&irys_types::DataTransaction> = None;
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
        if per_candidate {
            target = Some(tx);
            break;
        }
    }
    let target = target.expect(
        "at least one posted tx must be expired by the per-candidate (§4c) verdict under Cascade",
    );
    let expired_tx = target.header.clone();

    // Build a valid ingress proof for the expired tx (genesis signer is staked) so
    // the evil block fails ONLY on the §4c expiry rule.
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

    // The §4c rejection message includes "NC-0042" — match on that to ensure the
    // block is rejected for the right reason, not an unrelated shadow-tx error.
    let is_nc_0042_expiry_rejection = |e: &ValidationError| match e {
        ValidationError::ShadowTransactionInvalid(msg) => msg.contains("NC-0042"),
        _ => false,
    };

    // Genesis must reject it.
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        genesis_outcome,
        is_nc_0042_expiry_rejection,
        "Genesis must reject a Cascade block promoting an already-expired Submit tx (NC-0042 §4c)",
    );

    // Peer must have the parent chain up to expiry_height before validating E+1.
    peer_node
        .wait_until_height(expiry_height, seconds_to_wait)
        .await?;
    let peer_outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        peer_outcome,
        is_nc_0042_expiry_rejection,
        "Peer must reject a Cascade block promoting an already-expired Submit tx (NC-0042 §4c)",
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
