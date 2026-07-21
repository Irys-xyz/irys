//! A Submit slot whose SOLE partition replica is unpledged before expiry (and
//! never backfilled — no spare capacity) reaches its expiry epoch with an empty
//! `partitions` vec. Slot-expiry marking and the non-promotability set are
//! slot-keyed, so the slot still recycles and its txs stay blocked — and fee
//! settlement must be slot-keyed too: the unpromoted tx's perm fee is refunded
//! even though there is no miner to distribute term fees to (those stay in the
//! treasury). A partition-keyed settlement walk never sees the minerless slot,
//! leaving its users charged, non-promotable, and unsettled forever.
//!
//! End-to-end proof of reachability: the honest unpledge flow (no last-replica
//! guard) + capacity starvation produce the state on a real node, and the
//! expiry epoch block must carry the refund.

use crate::utils::{IrysNodeTest, gossip_commitment_to_node};
use alloy_genesis::GenesisAccount;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{CommitmentTransaction, DataLedger, NodeConfig, U256, irys::IrysSigner};
use reth::rpc::types::TransactionTrait as _;
use tracing::info;

#[test_log::test(tokio::test)]
async fn heavy_unpledged_minerless_slot_still_refunds_at_expiry() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const NUM_CHUNKS_IN_PARTITION: u64 = 4;
    const DATA_SIZE: usize = (NUM_CHUNKS_IN_PARTITION * CHUNK_SIZE) as usize; // fills slot 0 exactly
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000; // 10 IRYS
    const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 3;
    const BLOCKS_PER_EPOCH: u64 = 3;
    // Pre-Cascade (no cascade config): slot 0 is allocation-anchored, so it
    // expires at exactly SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH. The
    // minerless-settlement path under test is Cascade-independent (the
    // post-Cascade gate variant is pinned at unit level by
    // `empty_partition_expired_slot_still_refunds_unpromoted_txs`).
    const EXPIRY_HEIGHT: u64 = SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH; // 9

    let mut config = NodeConfig::testing();
    {
        let c = config.consensus.get_mut();
        c.block_migration_depth = 1;
        c.chunk_size = CHUNK_SIZE;
        c.num_chunks_in_partition = NUM_CHUNKS_IN_PARTITION;
        c.epoch.submit_ledger_epoch_length = SUBMIT_LEDGER_EPOCH_LENGTH;
        c.epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    }

    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    let user_address = user_signer.address();
    let initial_balance = U256::from(INITIAL_BALANCE);
    config.consensus.extend_genesis_accounts(vec![(
        user_address,
        GenesisAccount {
            balance: initial_balance.into(),
            ..Default::default()
        },
    )]);

    // 3 submodules (the genesis minimum): Publish slot 0 + Submit slot 0 take
    // two of the pledged partitions at genesis; the growth epoch's newly
    // allocated Submit slot takes the third BEFORE the unpledge applies. From
    // then on the capacity pool is empty, so nothing can backfill the unpledged
    // slot — the "no spare capacity" leg of the fixture (asserted below via the
    // slot staying minerless).
    let test_node = IrysNodeTest::new_genesis(config.clone());
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 3)?;
    let node = test_node
        .start_and_wait_for_packing("unpledged_slot", 30)
        .await;
    let anchor = node.get_block_by_height(0).await?.block_hash;

    // --- 1. One unpromoted tx (no chunks uploaded) exactly fills Submit slot 0.
    //        Filling it crosses the capacity-growth threshold so the next epoch
    //        allocates more Submit slots: slot 0 becomes non-last (expirable). ---
    let tx = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    let perm_fee = tx.header.perm_fee.expect("data tx must carry a perm_fee");
    node.wait_for_mempool(tx.header.id, 20).await?;
    let inclusion = node.mine_block().await?;
    assert_eq!(
        inclusion.ledger_total_chunks(DataLedger::Submit),
        NUM_CHUNKS_IN_PARTITION,
        "the tx must exactly fill Submit slot 0"
    );

    // Reach the first epoch so slot 0's write is processed and growth slots are
    // allocated (slot 0 becomes non-last).
    node.mine_until_next_epoch().await?;
    let slot0_partition = {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        assert!(
            slots.len() >= 2,
            "growth must allocate more Submit slots so slot 0 is non-last (got {})",
            slots.len()
        );
        *slots[0]
            .partitions
            .first()
            .expect("Submit slot 0 must hold its genesis-assigned partition")
    };

    // --- 2. Unpledge slot 0's sole replica via the honest flow (accepted: there
    //        is no last-replica guard), applied at the next epoch boundary. ---
    let genesis_signer = config.signer();
    let unpledge_anchor = node.get_anchor().await?;
    let mut unpledge = CommitmentTransaction::new_unpledge(
        &node.node_ctx.config.consensus,
        unpledge_anchor,
        node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_signer.address(),
        slot0_partition,
    )
    .await;
    genesis_signer.sign_commitment(&mut unpledge)?;
    gossip_commitment_to_node(&node, &unpledge).await?;
    node.wait_for_mempool_commitment_txs(vec![unpledge.id()], 20)
        .await?;

    let (_, unpledge_epoch) = node.mine_until_next_epoch().await?;
    assert!(
        unpledge_epoch < EXPIRY_HEIGHT,
        "the unpledge must apply before slot 0 expires (applied at {unpledge_epoch}, expiry {EXPIRY_HEIGHT})"
    );
    {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slot0 = &snapshot.ledgers.get_slots(DataLedger::Submit)[0];
        assert!(
            slot0.partitions.is_empty(),
            "slot 0 must be minerless after the unpledge epoch (and must stay so: \
             the capacity pool is empty, nothing can backfill it)"
        );
        assert!(
            !slot0.is_expired,
            "slot 0 must not have expired yet at the unpledge epoch {unpledge_epoch}"
        );
    }

    // --- 3. Mine to the expiry epoch: the minerless slot recycles and its
    //        unpromoted tx is refunded — the settlement the partition-keyed walk
    //        used to strand. ---
    while node.get_canonical_chain_height().await < EXPIRY_HEIGHT {
        node.mine_block().await?;
    }
    {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slot0 = &snapshot.ledgers.get_slots(DataLedger::Submit)[0];
        assert!(
            slot0.partitions.is_empty(),
            "slot 0 must still be minerless at its expiry epoch"
        );
        assert!(
            slot0.is_expired,
            "the minerless slot 0 must recycle at its expiry epoch {EXPIRY_HEIGHT}"
        );
    }

    let expiry_block = node.get_block_by_height(EXPIRY_HEIGHT).await?;
    let evm_block = node
        .wait_for_evm_block(expiry_block.evm_block_hash, 20)
        .await?;
    let (mut refund_total, mut reward_count) = (U256::from(0), 0_usize);
    for evm_tx in evm_block.body.transactions {
        if evm_tx.input().len() < 4 {
            continue;
        }
        let Ok(shadow_tx) = ShadowTransaction::decode(&mut evm_tx.input().as_ref()) else {
            continue;
        };
        match shadow_tx.as_v1() {
            Some(TransactionPacket::PermFeeRefund(refund))
                if refund.target == user_address.to_alloy_address() =>
            {
                refund_total =
                    refund_total.saturating_add(U256::from_le_bytes(refund.amount.to_le_bytes()));
            }
            Some(TransactionPacket::TermFeeReward(_)) => reward_count += 1,
            _ => {}
        }
    }
    assert_eq!(
        refund_total, perm_fee,
        "the unpromoted tx in the minerless expired slot must be refunded its full perm_fee"
    );
    assert_eq!(
        reward_count, 0,
        "no miner stored the expired slot, so no TermFeeReward may be emitted \
         (its term fees stay in the treasury)"
    );

    // Balance: the user paid total_cost at inclusion and gets exactly the
    // perm_fee back at expiry.
    let user_final = U256::from_be_bytes(
        node.get_balance(user_address, expiry_block.evm_block_hash)
            .await
            .to_be_bytes(),
    );
    let user_expected = initial_balance
        .saturating_sub(tx.header.total_cost().into())
        .saturating_add(perm_fee.into());
    assert_eq!(
        user_final, user_expected,
        "user balance must reflect exactly one full perm_fee refund"
    );

    // --- 4. Liveness: the chain keeps producing epoch blocks past the
    //        minerless-slot expiry. ---
    node.mine_until_next_epoch().await?;

    info!("minerless expired slot settled: refund={refund_total}, no term-fee rewards");
    node.stop().await;
    Ok(())
}
