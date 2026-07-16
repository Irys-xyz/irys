// NC-0042 Gap C (activation-straddle double-pay guard): a Submit slot that was
// recycled + perm-fee-REFUNDED while only PARTIALLY written *pre-Cascade* must
// stay in the inclusive non-promotability set after Cascade activates — otherwise
// its already-refunded txs could be promoted again post-activation (refund +
// permanent storage = double-pay).
//
// This is enforced by the `is_expired` short-circuit in
// `TermLedger::get_all_expired_slot_indexes` (data_ledger.rs): an already-recycled
// slot is included forever, independent of the post-Cascade fully-written gate.
// Without that override a partial slot (write frontier still inside it) drops out
// of the inclusive set the instant Cascade activates, because
// `cascade_expiry_gate` requires `slot_index < fully_written_slots` and a partial
// slot is not fully written.
//
// The one mid-chain E2E (`slow_heavy_cascade_midchain_submit_expiry_refunds_across_activation`)
// fills all Submit slots, so the fully-written gate always passes and the override
// never bites. This test constructs the state where it DOES bite: a partial,
// non-last Submit slot 0 that expires + refunds pre-Cascade, then Cascade
// activates. It proves both consequences of the override end-to-end:
//   1. an evil block promoting the already-refunded tx is REJECTED post-activation
//      with `ShadowTransactionInvalid` (NC-0042) — no double-pay;
//   2. the chain stays LIVE past the expiry epoch — the expired set stays a
//      contiguous prefix {0, 1}, so `expired_submit_range`'s fail-loud bail
//      never trips.
//
// Fixture (partial slot 0, so the fully-written gate would exclude it):
//   - num_chunks_in_partition = 4; a single 3-chunk (96B) Submit tx never fills a
//     partition, so the write frontier stays inside slot 0 (slot 0 is PARTIAL).
//   - the 3-chunk tx crosses the capacity-growth threshold, so the epoch allocates
//     +2 slots — slot 0 becomes NON-LAST (and therefore expiry-eligible) while
//     still being partial. This is the exact combination the override guards.
//   - the tx is never promoted (no chunks uploaded), so its slot expiry refunds
//     its full perm_fee pre-Cascade.

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
    DataLedger, DataTransactionHeader, IngressProofsList, IrysBlockHeader, NodeConfig, U256,
    UnixTimestamp, UnixTimestampMs, hardfork_config::Cascade,
};
use reth::rpc::types::TransactionTrait as _;

#[test_log::test(tokio::test)]
async fn slow_heavy_promote_after_activation_straddle_rejected() -> eyre::Result<()> {
    /// Forces an already-expired+refunded Submit tx into the Publish ledger with a
    /// valid ingress proof, so the block fails validation ONLY on the §4c expiry
    /// rule. No refund is scheduled in the bundle, so the in-constructor guard does
    /// not fire during production — the block is built and must be caught at
    /// validation time.
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

    // --- 1. Config: Cascade OFF (activated mid-chain below), small partitions so
    //         a single sub-partition tx leaves slot 0 partial. ---
    let num_blocks_in_epoch = 2_u64;
    let submit_ledger_epoch_length = 2_u64;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 4_u64;
    // 3 chunks < 4-chunk partition -> the write frontier stays inside slot 0
    // (slot 0 is PARTIAL) but still crosses the capacity-growth threshold so the
    // epoch allocates a 2nd/3rd slot and slot 0 becomes non-last.
    let data_size = 96_usize;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // One ingress proof from the (staked) genesis signer makes a tx promotable.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        // Cascade intentionally NOT configured yet — activated mid-chain below.
    });

    let user = config.new_random_signer();
    let user_addr = user.address();
    config.fund_genesis_accounts(vec![&user]);

    let genesis_config = config.clone();
    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 8)?;
    let node = test_node.start_and_wait_for_packing("straddle", 30).await;

    // --- 2. Pre-Cascade: include ONE unpromoted 3-chunk Submit tx (no chunks
    //         uploaded -> never promoted -> refundable). Its 3 chunks stay inside
    //         slot 0 (partial), and the capacity-growth threshold allocates extra
    //         slots so slot 0 is non-last and can expire. ---
    let anchor = node.get_block_by_height(0).await?.block_hash;
    let tx = node
        .post_data_tx(anchor, vec![7_u8; data_size], &user)
        .await;
    let tx_perm_fee = tx.header.perm_fee.expect("data tx must carry a perm_fee");
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.mine_block().await?;

    // --- 3. Mine (still pre-Cascade) until slot 0 expires and `tx` is perm-fee
    //         refunded. Detected data-driven by scanning epoch blocks for a
    //         PermFeeRefund to the user. A refund proves slot 0 became non-last and
    //         aged out normally pre-Cascade. ---
    let mut refund_amount = U256::from(0);
    let mut refund_height = 0_u64;
    const MAX_EPOCHS: usize = 12;
    'mine: for _ in 0..MAX_EPOCHS {
        let (_, height) = node.mine_until_next_epoch().await?;
        let block = node.get_block_by_height(height).await?;
        let evm_block = node.wait_for_evm_block(block.evm_block_hash, 20).await?;
        for evm_tx in evm_block.body.transactions {
            if evm_tx.input().len() < 4 {
                continue;
            }
            let Ok(shadow_tx) = ShadowTransaction::decode(&mut evm_tx.input().as_ref()) else {
                continue;
            };
            let Some(TransactionPacket::PermFeeRefund(refund)) = shadow_tx.as_v1() else {
                continue;
            };
            if refund.target == user_addr.to_alloy_address() {
                refund_amount = U256::from_le_bytes(refund.amount.to_le_bytes());
                refund_height = height;
                break 'mine;
            }
        }
    }
    assert!(
        refund_height > 0,
        "slot 0 must have expired + refunded pre-Cascade (no PermFeeRefund seen) — the \
         fixture failed to produce the partial-pre-Cascade-refund state the override guards"
    );
    assert_eq!(
        refund_amount, tx_perm_fee,
        "an unpromoted tx whose partial Submit slot expired pre-Cascade must be refunded \
         its full perm_fee"
    );
    // Bury the pre-Cascade expiry epoch below the restart durability floor. The
    // restart drops the unmigrated chain tip, so the epoch block that set slot 0's
    // is_expired must be a durable (migrated) block before we stop — otherwise the
    // restart discards it and slot 0's pre-Cascade expiry is lost, re-mined instead
    // as a cascade-active epoch (where the partial slot never expires). Still
    // pre-Cascade (cascade=None), so the expiry stays allocation-anchored.
    let durable_target = refund_height + 8;
    while node.get_canonical_chain_height().await < durable_target {
        node.mine_block().await?;
    }
    let pre_activation_height = node.get_canonical_chain_height().await;

    // --- 4. Activate Cascade mid-chain: stop -> set activation = tip+1 -> restart.
    //         Anchored to the tip timestamp (not wall-clock) so historical blocks
    //         stay pre-activation and only newly-mined blocks are cascade-active
    //         (deterministic; immune to a backward CLOCK_REALTIME lurch). ---
    let tip_block = node.get_block_by_height(pre_activation_height).await?;
    let activation_timestamp = tip_block.timestamp_secs().as_secs() + 1;
    let mut stopped = node.stop().await;
    stopped.cfg.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
        one_year_epoch_length,
        thirty_day_epoch_length,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
    let node = stopped.start().await;

    // Mine a couple cascade-active epochs so `activate_cascade` runs (OneYear /
    // ThirtyDay ledgers come into existence) and the evil block's parent is
    // cascade-active.
    for _ in 0..(2 * num_blocks_in_epoch) {
        node.mine_block().await?;
    }

    // --- 5. NON-VACUITY: confirm slot 0 is genuinely PARTIAL and NON-LAST
    //         post-activation, and that the fully-written gate WOULD exclude it —
    //         so the `is_expired` override is exactly what keeps it in the
    //         inclusive non-promotability set. ---
    let evil_parent_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let evil_height = evil_parent_block.height + 1;
    let parent_submit_total = evil_parent_block.ledger_total_chunks(DataLedger::Submit);
    let tip_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&evil_parent_block.block_hash)
        .expect("epoch snapshot for the current tip");

    let (submit_slot_count, slot0_is_expired, slot0_has_been_written) = {
        let slots = tip_snapshot.ledgers.get_slots(DataLedger::Submit);
        (slots.len(), slots[0].is_expired, slots[0].has_been_written)
    };
    assert!(
        submit_slot_count >= 2,
        "slot 0 must be NON-LAST post-activation (got {submit_slot_count} Submit slots) — else \
         the never-expire-the-last-slot rule, not the override, keeps the tx non-promotable"
    );
    // Partial <=> fully_written_slot_count(parent_total, chunks_per_slot) == 0
    //         <=> parent_total < num_chunks_in_partition.
    assert!(
        parent_submit_total < num_chunks_in_partition,
        "slot 0 must be PARTIAL post-activation (write frontier inside it): Submit total \
         {parent_submit_total} must be < chunks_per_slot {num_chunks_in_partition}; otherwise \
         the fully-written gate passes and the override is not what keeps the tx non-promotable"
    );
    assert!(
        slot0_is_expired,
        "slot 0 must carry is_expired from the pre-Cascade refund (the override keys on it)"
    );
    // The fully-written gate (has_been_written && slot_index < fully_written) is
    // FALSE for slot 0 (fully_written == 0), so the inclusive set can only contain
    // slot 0 via the is_expired override.
    let inclusive_expired = tip_snapshot.get_all_expired_term_slot_indexes(
        DataLedger::Submit,
        evil_height,
        true, // cascade active
        parent_submit_total,
    );
    assert!(
        inclusive_expired.contains(&0),
        "the is_expired override must keep partial slot 0 in the inclusive non-promotability \
         set post-activation (has_been_written={slot0_has_been_written}, fully_written=0, so the \
         Cascade gate alone would exclude it); got {inclusive_expired:?}"
    );

    // Confirm the per-candidate (§4c) verdict marks our tx expired at the evil
    // block's height under the Cascade gate — the exact predicate validation uses.
    let per_candidate = irys_actors::block_producer::ledger_expiry::is_submit_storage_expired(
        tx.header.id,
        evil_height,
        &tip_snapshot,
        &evil_parent_block,
        &node.node_ctx.config,
        true, // cascade active
        &node.node_ctx.block_producer_inner.block_index,
        &node.node_ctx.block_tree_guard,
        &node.node_ctx.mempool_guard,
        &node.node_ctx.db,
    )
    .await?;
    assert!(
        per_candidate,
        "the already-refunded tx must be non-promotable post-activation (per-candidate §4c \
         verdict) — this is the double-pay the override prevents"
    );
    drop(tip_snapshot);

    // --- 6. §4c: build an evil block at tip+1 that promotes the already-refunded
    //         tx, with a valid ingress proof so it fails ONLY on the expiry rule. ---
    let genesis_signer = genesis_config.signer();
    let proof_anchor = node.get_anchor().await?;
    let target_data = tx.data.clone().expect(
        "posted tx payload must be retained to build the ingress proof (fixture invariant)",
    );
    let chunks: Vec<Vec<u8>> = vec![target_data.into()];
    let proof = generate_ingress_proof(
        &genesis_signer,
        tx.header.data_root,
        chunks.iter().map(|c| Ok(c.as_slice())),
        genesis_config.consensus_config().chain_id,
        proof_anchor,
    )?;

    let evil_strategy = EvilPublishStrategy {
        expired_tx: tx.header.clone(),
        proofs: IngressProofsList(vec![proof]),
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: node.node_ctx.block_tree_guard.clone(),
    };
    let (block, _adj, _payload) = evil_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .expect("evil block should be produced (the guard fires at validation, not production)");

    assert_eq!(block.header().height, evil_height, "evil block at tip+1");
    assert!(
        block.header().data_ledgers[DataLedger::Publish]
            .tx_ids
            .contains(&tx.header.id),
        "evil block must promote the already-refunded tx"
    );

    // The §4c rejection message carries the "NC-0042" marker — match on it so we
    // reject for the right reason, not an unrelated shadow-tx error.
    let is_nc_0042_expiry_rejection = |e: &ValidationError| match e {
        ValidationError::ShadowTransactionInvalid(msg) => msg.contains("NC-0042"),
        _ => false,
    };

    let outcome = send_block_and_read_state(&node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        is_nc_0042_expiry_rejection,
        "a block promoting a pre-Cascade-refunded straddle tx must be rejected post-activation \
         (NC-0042 §4c — the is_expired override keeps partial slot 0 non-promotable)",
    );
    node.assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;

    // --- 7. Liveness: honest epoch blocks keep being produced past the expiry
    //         epoch. The override keeps the expired set a contiguous prefix {0, 1}, so
    //         `expired_submit_range`'s fail-loud bail never trips (which would halt
    //         the chain). ---
    let before = node.get_canonical_chain_height().await;
    node.mine_until_next_epoch().await?;
    node.mine_until_next_epoch().await?;
    let after = node.get_canonical_chain_height().await;
    assert!(
        after > before,
        "chain must progress past the activation-straddle expiry (before={before}, after={after})"
    );

    node.stop().await;
    Ok(())
}
