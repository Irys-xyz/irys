// NC-0042 §4b heavy chain-test: the honest producer must not promote a tx whose
// Submit-ledger storage has expired — even when that tx is otherwise promotable
// in the very block its slot expires.
//
// Background:
//   Two independent pipelines run when producing an epoch block:
//     A) tx_selector::get_publish_txs_and_proofs — picks promotable txs.
//     B) block_producer::calculate_expired_ledger_fees — schedules
//        `user_perm_fee_refunds` for unpromoted Submit-ledger txs whose slot
//        is expiring at this epoch.
//   Before NC-0042 §4b, if the same tx X was promotable AND its Submit slot was
//   expiring at this block, both pipelines selected X and `ShadowTxGenerator::new`
//   panicked the producer with `BlockProductionError::Irrecoverable`
//   (the defence-in-depth guard at `shadow_tx_generator.rs:258`).
//   The fix drops any publish candidate that is in the *same* expired-Submit-tx
//   set the refund pipeline acts on (`ledger_expiry::expired_submit_tx_ids`), so
//   "is this tx refunded?" and "may this tx be promoted?" cannot diverge.
//
// What this test proves end-to-end:
//   1. The producer does NOT panic when forced into the scenario.
//   2. It produces the expiry-epoch block successfully.
//   3. No tx is BOTH perm-fee-refunded AND promoted in that block (the core
//      NC-0042 invariant: refunded ⇒ not promoted).
//   4. At least one tx really was both promotable and expiring (so the filter
//      actually fired — the test is not vacuous).
//   5. A peer node validates and accepts the block.
//
// Design notes (read before changing):
//
// - Submit-ledger expiry is a per-SLOT property, not per-tx cycle math. A slot
//   expires at `last_height + blocks_per_cycle`, EXCEPT the last/newest slot is
//   never expired. The genesis slot (slot 0) therefore only expires once a
//   second slot exists. So we must force a 2nd Submit slot: with a tiny partition
//   (`num_chunks_in_partition = 10`) we post >10 single-chunk txs, which trips
//   capacity-based slot allocation at the next epoch. This mirrors the proven
//   expiry setup in `ledger_expiry_with_unstake.rs`.
//
// - `number_of_ingress_proofs_total = 1` (testing default) so a single proof from
//   the genesis signer makes a tx promotable — turning this into a single-node
//   test. Lowering it doesn't change the invariant under test (A2 keys on the
//   expired-partition set, independent of how many proofs accumulated).
//
// - To force the timing collision (promotable AND expiring in the SAME block) we
//   drive the txs through the real Submit flow, then withhold the ingress proofs
//   until one block before the expiry epoch and inject them directly into the
//   local IngressProofs DB. Generating proofs directly (rather than via the
//   chunk-ingress service) makes the timing deterministic.
//
// - The earlier `promote_after_submit_expiry.rs` attempt hand-inserted
//   `MigratedBlockHashes` rows; that collides with the real chain's migration
//   writes at the same height and crashes BlockTreeService. DO NOT do that —
//   drive the txs through the real flow.

use crate::utils::IrysNodeTest;
use irys_database::{db::IrysDatabaseExt as _, store_external_ingress_proof_checked};
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{DataLedger, NodeConfig, ingress::generate_ingress_proof};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeSet;
use tracing::info;

#[test_log::test(tokio::test)]
async fn heavy_producer_drops_publish_candidate_whose_submit_storage_expired() -> eyre::Result<()> {
    // --- 1. Config: small cycle + tiny partition so the genesis Submit slot
    //         expires (a 2nd slot gets allocated once we overflow the partition).
    // num_blocks_in_epoch = 5, submit_ledger_epoch_length = 2 → blocks_per_cycle = 10.
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
        // testing() already defaults to 1, but make the assumption explicit.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
    }

    let user_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&user_signer]);

    // --- 2. Start genesis + peer ---
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_config = genesis_node.testing_peer_with_signer(&user_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // --- 3. Post enough single-chunk data txs to overflow one partition,
    //         forcing a 2nd Submit slot (so the genesis slot 0 is non-last and
    //         can expire). Each tx carries a perm_fee (create_publish_transaction),
    //         so every unpromoted one is perm-fee-refundable at expiry. ---
    let num_txs = (num_chunks_in_partition + 2) as usize; // 12 chunks > 10 capacity
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

    // --- 4. Advance to one block before the genesis slot's expiry epoch.
    //         Slot 0 was allocated at genesis (height 0); it expires at
    //         blocks_per_cycle, by which point the capacity overflow has caused a
    //         2nd slot to be allocated (so slot 0 is non-last and does expire). ---
    let expiry_height = blocks_per_cycle; // = 10
    assert_eq!(
        expiry_height % num_blocks_in_epoch as u64,
        0,
        "expiry must coincide with an epoch boundary so the refund pipeline runs"
    );
    while genesis_node.get_canonical_chain_height().await < expiry_height - 1 {
        genesis_node.mine_block().await?;
    }
    let height_before = genesis_node.get_canonical_chain_height().await;
    assert_eq!(
        height_before,
        expiry_height - 1,
        "must be exactly one block before the expiry epoch boundary"
    );

    // --- 5. Inject an ingress proof for EVERY data tx (from the staked genesis
    //         signer) so each becomes a publish candidate. We do NOT mine in
    //         between — the very next block is the expiry epoch block. ---
    let genesis_signer = genesis_config.signer();
    let chain_id = genesis_config.consensus_config().chain_id;
    let proof_anchor = genesis_node.get_anchor().await?;
    for tx in &txs {
        let chunks: Vec<Vec<u8>> = vec![tx.data.clone().unwrap_or_default().into()];
        let proof = generate_ingress_proof(
            &genesis_signer,
            tx.header.data_root,
            chunks.iter().map(|c| Ok(c.as_slice())),
            chain_id,
            proof_anchor,
        )?;
        genesis_node.node_ctx.db.update_eyre(|rw_tx| {
            store_external_ingress_proof_checked(rw_tx, &proof, genesis_signer.address())?;
            Ok(())
        })?;
    }

    // --- 6. Produce the expiry epoch block with the honest producer ---
    // Without the §4b fix, the slot-0 txs are simultaneously selected for
    // promotion (Pipeline A) and perm-fee-refunded (Pipeline B), and
    // `ShadowTxGenerator::new` panics the producer with `Irrecoverable`.
    let expiry_block = genesis_node.mine_block().await?;
    assert_eq!(
        expiry_block.height, expiry_height,
        "mined block must be the expiry epoch block"
    );

    // --- 7. Core invariant: no tx is both promoted and perm-fee-refunded ---
    let publish_ids: BTreeSet<_> = expiry_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Publish)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();

    // Decode the EVM payload to find which txs were perm-fee-refunded.
    let evm_block = genesis_node
        .wait_for_evm_block(expiry_block.evm_block_hash, seconds_to_wait)
        .await?;
    let user_addr = user_signer.address().to_alloy_address();
    let refund_count = evm_block
        .body
        .transactions
        .into_iter()
        .filter(|tx| tx.input().len() >= 4)
        .filter_map(|tx| {
            let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref()).ok()?;
            let packet = shadow_tx.as_v1()?;
            match packet {
                TransactionPacket::PermFeeRefund(refund) if refund.target == user_addr => Some(()),
                _ => None,
            }
        })
        .count();

    info!(
        refund_count,
        publish_count = publish_ids.len(),
        "expiry block produced without panic"
    );

    // The filter must actually have fired. We injected a valid ingress proof for
    // EVERY tx, so every tx is a promotable publish candidate; the only reason any
    // is absent from the Publish ledger is the §4b expiry drop. Refund_count > 0
    // proves expired candidates existed and were diverted to a refund rather than
    // promoted (verified out-of-band: the producer logs N "non-promotable" drops).
    assert!(
        refund_count > 0,
        "at least one expired tx must be perm-fee-refunded (else the test is vacuous)"
    );

    // The NC-0042 invariant: a refunded tx must never also be promoted. We assert
    // it via the expired set the producer used — none of those txs may appear in
    // the Publish ledger.
    // Use the SAME parent epoch snapshot the producer used (parent of the expiry
    // block), which has all the slots allocated by this epoch — not the data
    // block's snapshot, which only had the genesis slot.
    let expiry_parent_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&expiry_block.previous_block_hash)
        .expect("epoch snapshot for the expiry block's parent");
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &expiry_parent_snapshot,
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
        "the genesis Submit slot must have expired at this epoch"
    );
    for expired_tx_id in &expired_set {
        assert!(
            !publish_ids.contains(expired_tx_id),
            "expired Submit tx {expired_tx_id} must NOT be promoted (NC-0042 §4b)"
        );
    }

    // No-divergence property: every tx the producer dropped from promotion is
    // exactly a tx the refund pipeline acted on. All txs are from `user_signer`,
    // so the count of `PermFeeRefund`s to that address must equal the expired set.
    assert_eq!(
        refund_count,
        expired_set.len(),
        "A2's dropped (expired) set must match Pipeline B's refund set exactly — \
         the two must not diverge (NC-0042 §4b)"
    );

    // --- 8. Peer must accept the block (it is a valid canonical block) ---
    peer_node
        .wait_until_height(expiry_height, seconds_to_wait)
        .await?;
    let peer_block = peer_node.get_block_by_height(expiry_height).await?;
    assert_eq!(
        peer_block.block_hash, expiry_block.block_hash,
        "peer must converge on the same block at the expiry height"
    );

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
