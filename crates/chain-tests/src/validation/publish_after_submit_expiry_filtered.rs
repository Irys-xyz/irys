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
//   The fix is a per-candidate predicate in `tx_selector::get_publish_txs_and_proofs`
//   that calls `ledger_expiry::is_submit_storage_expired` for each publish candidate.
//   Any candidate whose Submit storage has expired is dropped before it can reach
//   the `ShadowTxGenerator`, so "is this tx refunded?" and "is this tx promoted?"
//   cannot both be true for the same tx.
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

use crate::validation::submit_expiry_two_node_setup;
use irys_database::{db::IrysDatabaseExt as _, store_external_ingress_proof_checked};
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{DataLedger, H256, ingress::generate_ingress_proof};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeSet;
use tracing::info;

#[test_log::test(tokio::test)]
async fn heavy_producer_drops_publish_candidate_whose_submit_storage_expired() -> eyre::Result<()> {
    // --- 1 + 2 + 3. Shared config + two-node startup + overflow data posting ---
    // (See submit_expiry_two_node_setup for the parameter rationale.)
    let setup = submit_expiry_two_node_setup(1).await?;
    let genesis_config = setup.genesis_config;
    let genesis_node = setup.genesis_node;
    let peer_node = setup.peer_node;
    let txs = setup.txs;
    let user_signer = setup.user_signer;
    let blocks_per_cycle = setup.blocks_per_cycle;
    let num_blocks_in_epoch = setup.num_blocks_in_epoch;
    let seconds_to_wait = setup.seconds_to_wait;

    // Verify all posted txs landed in the Submit ledger.
    // The data block was already mined by submit_expiry_two_node_setup.
    let data_block = genesis_node
        .get_block_by_height(genesis_node.get_canonical_chain_height().await)
        .await?;
    let num_txs = txs.len();
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
    // `BalanceIncrement::irys_ref` carries the CL tx_id, enabling identity-based
    // comparison against the expired set rather than a count-only check.
    let evm_block = genesis_node
        .wait_for_evm_block(expiry_block.evm_block_hash, seconds_to_wait)
        .await?;
    let user_addr = user_signer.address().to_alloy_address();
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
                    // irys_ref is the CL tx_id encoded as FixedBytes<32>
                    Some(H256(refund.irys_ref.into()))
                }
                _ => None,
            }
        })
        .collect();

    info!(
        refund_count = refunded_tx_ids.len(),
        publish_count = publish_ids.len(),
        "expiry block produced without panic"
    );

    // The filter must actually have fired. We injected a valid ingress proof for
    // EVERY tx, so every tx is a promotable publish candidate; the only reason any
    // is absent from the Publish ledger is the §4b expiry drop. Refund_count > 0
    // proves expired candidates existed and were diverted to a refund rather than
    // promoted (verified out-of-band: the producer logs N "non-promotable" drops).
    assert!(
        !refunded_tx_ids.is_empty(),
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
    let expiry_parent_block = genesis_node.get_block_by_height(expiry_height - 1).await?;
    assert_eq!(
        expiry_parent_block.block_hash, expiry_block.previous_block_hash,
        "parent header must be the expiry block's parent"
    );
    let expiry_cascade_active = genesis_node
        .node_ctx
        .config
        .consensus
        .hardforks
        .is_cascade_active_at(expiry_parent_block.timestamp_secs());
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &expiry_parent_snapshot,
        &expiry_parent_block,
        expiry_height,
        &genesis_node.node_ctx.config,
        expiry_cascade_active,
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

    // No-divergence (identity): the set of txs the refund pipeline acted on must
    // equal the expired set the producer used — the two must not diverge (NC-0042 §4b).
    assert_eq!(
        refunded_tx_ids, expired_set,
        "refunded tx-id set must equal the expired set exactly — \
         Pipeline A's drop list and Pipeline B's refund list must not diverge (NC-0042 §4b)"
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
