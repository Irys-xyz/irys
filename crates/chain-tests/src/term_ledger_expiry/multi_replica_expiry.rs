//! Multi-replica (`num_partitions_per_slot = 2`) Submit-slot expiry settlement,
//! produced-block coverage: ONE expired slot held by TWO DISTINCT miners must
//! split the slot's term-fee treasury between them and refund each unpromoted
//! tx exactly ONCE — never once per replica.
//!
//! Every other expiry E2E runs with 1 partition per slot (production profiles
//! use 10), so the per-slot multi-miner settlement path — the slot's miner
//! list carrying more than one address into `aggregate_balance_deltas` — was
//! exercised only by in-memory unit tests
//! (`test_aggregate_balance_deltas_with_pooled_miners`,
//! `test_aggregate_miner_fees_handles_duplicates`). No produced-block test
//! decoded the on-chain shadow txs for a two-owner slot.
//!
//! Getting two DISTINCT owners into one slot is deterministic here (no RNG
//! dependence on the owner): `process_slot_needs` filters a slot's candidate
//! capacity partitions by the miners already assigned to it, so the genesis
//! miner can fill only ONE replica of each slot no matter how many spare
//! partitions it has. The second replica stays empty until the second miner's
//! pledged capacity exists, at which point it is the only eligible candidate.

use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    DataLedger, IrysAddress, NodeConfig, U256, fee_distribution::TermFeeCharges, irys::IrysSigner,
};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeMap;
use tracing::info;

#[test_log::test(tokio::test)]
async fn heavy_multi_replica_slot_expiry_splits_reward_and_refunds_once() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const NUM_CHUNKS_IN_PARTITION: u64 = 4;
    const DATA_SIZE: usize = (NUM_CHUNKS_IN_PARTITION * CHUNK_SIZE) as usize; // fills slot 0
    const INITIAL_BALANCE: u128 = 30_000_000_000_000_000_000_000; // covers stake (20k) + pledges
    const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 5;
    const BLOCKS_PER_EPOCH: u64 = 3;
    // Pre-Cascade: Submit slot 0 (allocated at genesis) is allocation-anchored
    // and expires at exactly SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH,
    // regardless of when it was written. Multi-replica settlement is
    // Cascade-independent. The window leaves room for the second miner's
    // commitment activation + slot backfill (a few epochs) plus the inclusion,
    // all strictly before expiry.
    const EXPIRY_HEIGHT: u64 = SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH; // 15

    let mut config = NodeConfig::testing();
    {
        let c = config.consensus.get_mut();
        c.block_migration_depth = 1;
        c.chunk_size = CHUNK_SIZE;
        c.num_chunks_in_partition = NUM_CHUNKS_IN_PARTITION;
        c.epoch.submit_ledger_epoch_length = SUBMIT_LEDGER_EPOCH_LENGTH;
        c.epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
        // The property under test: every slot holds TWO replicas.
        c.num_partitions_per_slot = 2;
        c.num_partitions_per_term_ledger_slot = 2;
    }

    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    let user_address = user_signer.address();
    // The second miner only posts commitments (stake + pledges); it never runs
    // a node or mines — settlement needs its partitions assigned, not packed.
    let miner2_signer = IrysSigner::random_signer(&config.consensus_config());
    let miner2_address = miner2_signer.address();
    config.consensus.extend_genesis_accounts(vec![
        (
            user_address,
            GenesisAccount {
                balance: U256::from(INITIAL_BALANCE).into(),
                ..Default::default()
            },
        ),
        (
            miner2_address,
            GenesisAccount {
                balance: U256::from(INITIAL_BALANCE).into(),
                ..Default::default()
            },
        ),
    ]);

    let genesis_address = config.miner_address();
    let test_node = IrysNodeTest::new_genesis(config.clone());
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 3)?;
    let node = test_node
        .start_and_wait_for_packing("multi_replica", 30)
        .await;

    // --- 1. Second miner stakes and pledges two partitions; the next epoch
    //        assigns them to the empty second replicas (Submit slot 0 and
    //        Publish slot 0 — the genesis miner is filtered out of both). ---
    let stake = node
        .post_stake_commitment_with_signer(&miner2_signer)
        .await?;
    let pledge_a = node
        .post_pledge_commitment_with_signer(&miner2_signer)
        .await;
    let pledge_b = node
        .post_pledge_commitment_with_signer(&miner2_signer)
        .await;
    node.wait_for_mempool_commitment_txs(vec![stake.id(), pledge_a.id(), pledge_b.id()], 20)
        .await?;
    // Observed (not hardcoded) assignment: Submit slot 0 must hold two replicas
    // owned by the two distinct miners. Commitment activation and the capacity
    // -> slot backfill land across epoch boundaries, so allow a few epochs —
    // but they must all complete BEFORE the slot's expiry epoch (asserted via
    // `inclusion.height < EXPIRY_HEIGHT` below, which this loop precedes).
    let mut slot0_owners: Vec<IrysAddress> = Vec::new();
    for _ in 0..3 {
        node.mine_until_next_epoch().await?;
        let snapshot = node.get_canonical_epoch_snapshot();
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        slot0_owners = slots[0]
            .partitions
            .iter()
            .map(|hash| {
                snapshot
                    .partition_assignments
                    .get_assignment(*hash)
                    .expect("assigned partition must have an assignment entry")
                    .miner_address
            })
            .collect();
        if slot0_owners.len() == 2 {
            break;
        }
    }
    assert_eq!(
        slot0_owners.len(),
        2,
        "Submit slot 0 must hold both replicas (got owners {slot0_owners:?})"
    );
    assert!(
        slot0_owners.contains(&genesis_address) && slot0_owners.contains(&miner2_address),
        "the two replicas must belong to the two distinct miners (got {slot0_owners:?})"
    );

    // --- 2. One unpromoted tx (no chunks uploaded) exactly fills slot 0. ---
    let anchor = node.get_anchor().await?;
    let tx = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    let perm_fee = tx.header.perm_fee.expect("data tx must carry a perm_fee");
    node.wait_for_mempool(tx.header.id, 20).await?;
    let inclusion = node.mine_block().await?;
    assert!(
        inclusion.height < EXPIRY_HEIGHT,
        "the tx must be included before slot 0 expires"
    );
    assert_eq!(
        inclusion.ledger_total_chunks(DataLedger::Submit),
        NUM_CHUNKS_IN_PARTITION,
        "the tx must exactly fill Submit slot 0"
    );

    // --- 3. Mine to the expiry epoch and decode the settlement shadow txs. ---
    while node.get_canonical_chain_height().await < EXPIRY_HEIGHT {
        node.mine_block().await?;
    }
    {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        assert!(
            slots.len() >= 2,
            "slot 0 must be non-last at expiry (got {} slots)",
            slots.len()
        );
        assert!(
            slots[0].is_expired,
            "Submit slot 0 must recycle at its expiry epoch {EXPIRY_HEIGHT}"
        );
    }

    let expiry_block = node.get_block_by_height(EXPIRY_HEIGHT).await?;
    let evm_block = node
        .wait_for_evm_block(expiry_block.evm_block_hash, 20)
        .await?;
    let mut refund_count = 0_usize;
    let mut refund_total = U256::from(0);
    let mut rewards: BTreeMap<IrysAddress, U256> = BTreeMap::new();
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
                refund_count += 1;
                refund_total =
                    refund_total.saturating_add(U256::from_le_bytes(refund.amount.to_le_bytes()));
            }
            Some(TransactionPacket::TermFeeReward(reward)) => {
                let target = IrysAddress::from(reward.target);
                let amount = U256::from_le_bytes(reward.amount.to_le_bytes());
                assert!(
                    rewards.insert(target, amount).is_none(),
                    "each miner must receive at most one TermFeeReward per settlement block"
                );
            }
            _ => {}
        }
    }

    // Refund: exactly once (per tx, NOT per replica), for the full perm_fee.
    assert_eq!(
        refund_count, 1,
        "the unpromoted tx must be refunded exactly once, not once per replica"
    );
    assert_eq!(
        refund_total, perm_fee,
        "the single refund must be the tx's full perm_fee"
    );

    // Reward: the slot's term-fee treasury is SPLIT between the two owners —
    // both credited, amounts summing exactly to the treasury, neither getting
    // the whole pot. (Split shape: floor(treasury/2) each, remainder to the
    // first miner in deterministic order.)
    let treasury =
        TermFeeCharges::new(tx.header.term_fee, &config.consensus_config())?.term_fee_treasury;
    let base_share = treasury
        .checked_div(U256::from(2))
        .expect("division by two");
    assert_eq!(
        rewards.keys().copied().collect::<Vec<_>>(),
        {
            let mut owners = vec![genesis_address, miner2_address];
            owners.sort();
            owners
        },
        "both distinct owners of the expired slot must be rewarded (got {rewards:?})"
    );
    let sum = rewards
        .values()
        .fold(U256::from(0), |acc, v| acc.saturating_add(*v));
    assert_eq!(
        sum, treasury,
        "the two shares must sum exactly to the slot's term-fee treasury"
    );
    for (miner, amount) in &rewards {
        assert!(
            *amount >= base_share && *amount < treasury,
            "miner {miner} must get a genuine share (got {amount}, treasury {treasury})"
        );
    }

    info!(
        "multi-replica slot settled: refund={refund_total}, rewards={rewards:?}, treasury={treasury}"
    );
    node.stop().await;
    Ok(())
}
