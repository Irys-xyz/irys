// NC-0042 §4b/§4c: the mid-epoch fully-written-boundary-crossing promotability
// window, pinned end-to-end for producer/validator determinism plus the
// next-epoch rescue.
//
// The rule lives (only) as a comment in `block_validation.rs`: the inclusive
// all-expired Submit slot set is recomputed EVERY block from the *parent's*
// advancing Submit total, while a slot's `last_height` is refreshed only by the
// next epoch `touch`. So a stale slot (aged `last_height`) whose fully-written
// boundary is crossed by mid-epoch appends re-enters the blocked set the instant
// the parent total passes it — and stays there until the next epoch `touch`
// bumps its `last_height` (the rescue). A tx appended into such a slot is held
// non-promotable for up to ~one epoch, then rescued (NOT refunded). If the
// producer and the validator ever disagreed inside this window (one keying off
// the tip total, the other the parent total), it would be a consensus fork; this
// test pins that they agree, from the SAME parent state.
//
// Fixture (Cascade active from genesis; num_blocks_in_epoch=5,
// submit_ledger_epoch_length=2 -> Submit slot lifetime = 10 blocks; slot size
// C = 10 chunks, chunk_size = 32 so one 32-byte tx == one Submit chunk):
//   1. Write 15 single-chunk txs -> Submit slot 0 fully written (chunks [0,10))
//      and slot 1 the partial frontier (chunks [10,15)); allocation adds slot 2
//      as headroom so slot 1 is non-last. At the first epoch touch (E_w) both
//      slots 0 and 1 get `last_height = E_w`.
//   2. Age WITHOUT Submit writes. Slot 0 (fully written) recycles a full slot
//      lifetime after E_w; slot 1 (partial frontier) survives — only fully
//      written slots may expire — and keeps its stale `last_height = E_w`.
//   3. MID-EPOCH, append 5 more single-chunk txs pushing the frontier past slot
//      1's end (Submit total -> 20), so slot 1 is now fully written THIS epoch.
//      A chosen tx `T` lands in slot 1; inject T's ingress proof so it is a
//      promotion candidate.
// Because slot 0 is already expired (a contiguous prefix), slot 1 now joins the
// inclusive all-expired set: `expired_submit_range` extends to slot 1's end and
// T maps inside it -> T is non-promotable, even though slot 1's `last_height` is
// not yet refreshed.
//
// Assertions (all from the SAME parent — the load-bearing consensus checks):
//   - Producer: the honest producer mines the window block and does NOT promote
//     T (T absent from the block's Publish ledger).
//   - Validator: an "evil" block built on the SAME parent that DOES promote T is
//     rejected by the peer (and genesis) with `ShadowTransactionInvalid`
//     carrying the NC-0042 marker.
// Then across the epoch boundary:
//   - Rescue: at the epoch block the `touch` refreshes slot 1's `last_height`
//     and the write-window exclusion drops it from the refund set, so NO
//     perm-fee refund is emitted for T's slot, and T becomes promotable
//     afterwards (eventually promoted).
//
// We drive everything through the real Submit flow (no MigratedBlockHashes
// seeding — that collides with the chain's real migration writes and crashes
// BlockTreeService, per the notes in the pre-Cascade §4b/§4c tests).

use crate::utils::{IrysNodeTest, assert_validation_error, solution_context};
use crate::validation::{
    EvilPublishStrategy, is_nc_0042_expiry_rejection, next_epoch_boundary,
    send_block_and_read_state,
};
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::{db::IrysDatabaseExt as _, store_external_ingress_proof_checked};
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, DataTransaction, H256, IngressProofsList, NodeConfig, UnixTimestamp,
    hardfork_config::Cascade,
};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeSet;

#[test_log::test(tokio::test)]
async fn heavy_mid_epoch_boundary_crossing_defers_then_rescues() -> eyre::Result<()> {
    // --- 1. Config: Cascade active from genesis + tight Submit-expiry aging ---
    let num_blocks_in_epoch: u64 = 5;
    let submit_ledger_epoch_length: u64 = 2;
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10; // slot size C = 10 chunks
    let slot_lifetime = submit_ledger_epoch_length * num_blocks_in_epoch; // 10 blocks
    let seconds_to_wait = 40_usize;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    {
        let c = genesis_config.consensus.get_mut();
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // Fast migration so a Submit inclusion resolves promptly (fast path); the
        // un-migrated tip still resolves via the branch-correct by-hash walk.
        c.block_migration_depth = 1;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        // One ingress proof from the genesis signer makes a tx promotable.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        // Cascade active from genesis -> last-write anchoring + written-this-epoch
        // rescue, the semantics this window depends on.
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 8,
            thirty_day_epoch_length: 2,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    }

    let user_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&user_signer]);

    // Enough submodules for the Submit data slots (0,1,2) + the other Cascade
    // ledgers (Publish/OneYear/ThirtyDay) + capacity headroom.
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_node.cfg.base_directory.clone(), 10)?;
    let genesis_node = genesis_node
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = {
        let peer_config = genesis_node.testing_peer_with_signer(&user_signer);
        IrysNodeTest::new(peer_config).start_with_name("PEER").await
    };

    // Reads (slot0_last_height, slot0_expired, slot1_last_height, slot1_expired,
    // slot_count) for the Submit ledger from `height`'s epoch snapshot.
    async fn submit_slots(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        height: u64,
    ) -> eyre::Result<(u64, bool, u64, bool, usize)> {
        let block = ctx.get_block_by_height(height).await?;
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist");
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        Ok((
            slots[0].last_height,
            slots[0].is_expired,
            slots[1].last_height,
            slots[1].is_expired,
            slots.len(),
        ))
    }

    // Posts `n` single-chunk (one Submit chunk each) txs and waits for the mempool.
    async fn post_single_chunk_txs(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        signer: &irys_types::irys::IrysSigner,
        n: usize,
        marker_base: u8,
        chunk_size: usize,
        seconds_to_wait: usize,
    ) -> eyre::Result<Vec<DataTransaction>> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let mut data = vec![7_u8; chunk_size];
            data[0] = marker_base.wrapping_add(i as u8);
            let anchor = ctx.get_anchor().await?;
            let tx = ctx.post_data_tx(anchor, data, signer).await;
            ctx.wait_for_mempool(tx.header.id, seconds_to_wait).await?;
            out.push(tx);
        }
        Ok(out)
    }

    // --- 2. Write slot 0 full (10 chunks) + slot 1 partial (5 chunks) ---
    // Held only to keep the posted txs alive for the run; not referenced again.
    let _initial_txs = post_single_chunk_txs(
        &genesis_node,
        &user_signer,
        (num_chunks_in_partition + num_chunks_in_partition / 2) as usize, // 15
        100,
        chunk_size as usize,
        seconds_to_wait,
    )
    .await?;
    let data_block = genesis_node.mine_block().await?;
    assert_eq!(
        data_block.ledger_total_chunks(DataLedger::Submit),
        15,
        "one 32-byte tx must equal one Submit chunk (15 txs -> 15 chunks: slot 0 full + slot 1 partial)"
    );

    // Mine to the first epoch touch E_w so slots 0 and 1 get `last_height = E_w`
    // and allocation adds slot 2 (so slot 1 is non-last).
    let ew = next_epoch_boundary(data_block.height, num_blocks_in_epoch);
    while genesis_node.get_canonical_chain_height().await < ew {
        genesis_node.mine_block().await?;
    }
    let (s0_lh, s0_exp, s1_lh, s1_exp, slot_count) = submit_slots(&genesis_node, ew).await?;
    assert!(
        slot_count >= 3,
        "capacity allocation must add slot 2 so slot 1 is non-last (got {slot_count} slots)"
    );
    assert_eq!(
        s0_lh, ew,
        "slot 0 last_height should be the write epoch E_w"
    );
    assert_eq!(
        s1_lh, ew,
        "slot 1 last_height should be the write epoch E_w"
    );
    assert!(!s0_exp, "slot 0 must not be expired yet at E_w");
    assert!(!s1_exp, "slot 1 must not be expired yet at E_w");

    // --- 3. Age WITHOUT Submit writes. Slot 0 recycles a full slot lifetime after
    //         E_w; slot 1 (partial frontier) survives with its stale last_height. ---
    let slot0_expiry_epoch = next_epoch_boundary(ew + slot_lifetime, num_blocks_in_epoch);
    while genesis_node.get_canonical_chain_height().await < slot0_expiry_epoch {
        genesis_node.mine_block().await?;
    }
    let (s0_lh2, s0_exp2, s1_lh2, s1_exp2, _) =
        submit_slots(&genesis_node, slot0_expiry_epoch).await?;
    assert_eq!(s0_lh2, ew, "slot 0 last_height stays stale (no rewrite)");
    assert!(
        s0_exp2,
        "the fully-written aged slot 0 must recycle at E_w + slot_lifetime"
    );
    assert_eq!(s1_lh2, ew, "slot 1 last_height stays stale (no rewrite)");
    assert!(
        !s1_exp2,
        "the partially-written frontier slot 1 must NOT expire (only fully-written slots may)"
    );

    // Peer follows the honest chain up to the recycle epoch.
    peer_node
        .wait_until_height(slot0_expiry_epoch, seconds_to_wait)
        .await?;

    // --- 4. MID-EPOCH: append 5 more single-chunk txs pushing the frontier past
    //         slot 1's end (Submit total -> 20). Slot 1 is now fully written THIS
    //         epoch; T (the first appended tx) lands in slot 1. ---
    let mid_txs = post_single_chunk_txs(
        &genesis_node,
        &user_signer,
        (num_chunks_in_partition / 2) as usize, // 5 -> slot 1 filled to [10,20)
        200,
        chunk_size as usize,
        seconds_to_wait,
    )
    .await?;
    let target = mid_txs
        .first()
        .cloned()
        .expect("must have appended at least one mid-epoch tx");
    let append_block = genesis_node.mine_block().await?;
    assert_eq!(
        append_block.ledger_total_chunks(DataLedger::Submit),
        20,
        "mid-epoch append must fill slot 1 exactly (Submit total -> 20 chunks)"
    );
    // The append is a mid-epoch (non-epoch) block, on top of the recycle epoch.
    assert!(
        !append_block.height.is_multiple_of(num_blocks_in_epoch),
        "the append must be a mid-epoch block, not an epoch block"
    );

    // Inject T's ingress proof (from the staked genesis signer) so it is a
    // promotion candidate — the only reason it can be dropped/rejected is expiry.
    let genesis_signer = genesis_config.signer();
    let chain_id = genesis_config.consensus_config().chain_id;
    let proof_anchor = genesis_node.get_anchor().await?;
    let target_chunks: Vec<Vec<u8>> = vec![
        target
            .data
            .clone()
            .expect("posted tx must carry data bytes")
            .into(),
    ];
    let proof = generate_ingress_proof(
        &genesis_signer,
        target.header.data_root,
        target_chunks.iter().map(|c| Ok(c.as_slice())),
        chain_id,
        proof_anchor,
    )?;
    genesis_node.node_ctx.db.update_eyre(|rw_tx| {
        store_external_ingress_proof_checked(rw_tx, &proof, genesis_signer.address())?;
        Ok(())
    })?;

    // --- 5. The window: T must be non-promotable at the next (mid-epoch) block,
    //         computed from the SAME parent (the append block). ---
    let window_height = append_block.height + 1;
    assert!(
        !window_height.is_multiple_of(num_blocks_in_epoch),
        "the window block must be mid-epoch (before the rescuing epoch boundary)"
    );
    let parent_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&append_block.block_hash)
        .expect("epoch snapshot for the append (parent) block");
    let cascade_active_for_block = genesis_node
        .node_ctx
        .config
        .consensus
        .hardforks
        .is_cascade_active_at(append_block.timestamp_secs());

    // Oracle (anti-vacuity): T really is inside the boundary-crossed-but-unrescued
    // window as of the block we are about to produce.
    let t_expired = irys_actors::block_producer::ledger_expiry::is_submit_storage_expired(
        target.header.id,
        window_height,
        &parent_snapshot,
        &append_block,
        &genesis_node.node_ctx.config,
        cascade_active_for_block,
        &genesis_node.node_ctx.block_producer_inner.block_index,
        &genesis_node.node_ctx.block_tree_guard,
        &genesis_node.node_ctx.mempool_guard,
        &genesis_node.node_ctx.db,
    )
    .await?;
    assert!(
        t_expired,
        "T must map into a boundary-crossed-but-unrescued slot at the window block \
         (else the producer-drop / validator-reject checks would be vacuous)"
    );

    // Validator check FIRST (while the tip is still the append block, T's parent):
    // an evil block that DOES promote T, built on the SAME parent, must be
    // rejected with ShadowTransactionInvalid carrying the NC-0042 marker.
    peer_node
        .wait_until_height(append_block.height, seconds_to_wait)
        .await?;
    let evil_strategy = EvilPublishStrategy {
        publish_tx: target.header.clone(),
        proofs: IngressProofsList(vec![proof.clone()]),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: genesis_node.node_ctx.block_tree_guard.clone(),
    };
    let (evil_block, _adj, _payload) = evil_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .expect("evil block should be produced (the guard fires at validation, not production)");
    assert_eq!(
        evil_block.header().height,
        window_height,
        "evil block must be at the window height (same parent as the honest block)"
    );
    assert_eq!(
        evil_block.header().previous_block_hash,
        append_block.block_hash,
        "evil block must build on the same parent state the producer uses"
    );
    assert!(
        evil_block.header().data_ledgers[DataLedger::Publish]
            .tx_ids
            .contains(&target.header.id),
        "evil block must promote T"
    );

    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, evil_block.clone(), false).await?;
    assert_validation_error(
        genesis_outcome,
        is_nc_0042_expiry_rejection,
        "Genesis must reject a block promoting a boundary-crossed-but-unrescued Submit tx (NC-0042)",
    );
    let peer_outcome =
        send_block_and_read_state(&peer_node.node_ctx, evil_block.clone(), false).await?;
    assert_validation_error(
        peer_outcome,
        is_nc_0042_expiry_rejection,
        "Peer must reject a block promoting a boundary-crossed-but-unrescued Submit tx (NC-0042)",
    );
    genesis_node
        .assert_evm_block_absent(evil_block.header().evm_block_hash, 2)
        .await?;

    // Producer check: the honest producer mines the window block on the SAME
    // parent and does NOT promote T.
    let window_block = genesis_node.mine_block().await?;
    assert_eq!(
        window_block.height, window_height,
        "honest window block at the expected height"
    );
    assert_eq!(
        window_block.previous_block_hash, append_block.block_hash,
        "honest window block builds on the same parent as the evil block"
    );
    let window_publish_ids: BTreeSet<H256> = window_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Publish)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    assert!(
        !window_publish_ids.contains(&target.header.id),
        "the honest producer must NOT promote T inside the boundary-crossed-but-unrescued window"
    );

    // --- 6. Rescue: advance across the epoch boundary. The `touch` refreshes slot
    //         1's last_height and the write-window exclusion drops it from refunds,
    //         so NO perm-fee refund is emitted for T's slot at the epoch block. ---
    let rescue_epoch = next_epoch_boundary(window_height, num_blocks_in_epoch);
    while genesis_node.get_canonical_chain_height().await < rescue_epoch {
        genesis_node.mine_block().await?;
    }
    let (_, _, s1_lh_r, s1_exp_r, _) = submit_slots(&genesis_node, rescue_epoch).await?;
    assert_eq!(
        s1_lh_r, rescue_epoch,
        "the epoch touch must refresh slot 1's last_height (rescue)"
    );
    assert!(
        !s1_exp_r,
        "slot 1 must be rescued (not recycled) because it was written this epoch"
    );

    // No perm-fee refund for T's slot at the epoch block (it was rescued, not
    // settled).
    let epoch_block = genesis_node.get_block_by_height(rescue_epoch).await?;
    let user_addr = user_signer.address().to_alloy_address();
    let evm_block = genesis_node
        .wait_for_evm_block(epoch_block.evm_block_hash, seconds_to_wait)
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
    // No perm-fee refund at all for the user at this epoch: slot 0 already recycled
    // (and settled) at its own earlier expiry epoch, and slot 1 is rescued (written
    // this epoch), so nothing recycles here. A non-empty set would mean either T's
    // rescued slot was wrongly refunded or a sibling slot leaked a refund (NC-0042).
    assert!(
        refunded_tx_ids.is_empty(),
        "no perm-fee refund may be emitted for the user at the rescue epoch block \
         (rescued slot 1, slot 0 settled earlier) (NC-0042); got {refunded_tx_ids:?}"
    );

    // --- 7. After the rescue, T becomes promotable (eventually promoted). ---
    let mut promoted = false;
    for _ in 0..num_blocks_in_epoch {
        let block = genesis_node.mine_block().await?;
        let publish_ids: BTreeSet<H256> = block
            .get_data_ledger_tx_ids()
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        if publish_ids.contains(&target.header.id) {
            promoted = true;
            break;
        }
    }
    assert!(
        promoted,
        "a rescued-slot tx must become promotable after the epoch (eventually promoted)"
    );

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
