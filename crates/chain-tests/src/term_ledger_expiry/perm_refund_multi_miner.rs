//! NC-0042 R4 produced-block coverage: per-miner expiry-reward AMOUNT attribution
//! across TWO DISTINCT miners settled in ONE boundary block.
//!
//! ## The gap this closes
//!
//! When two DISTINCT miners each own a different expired Submit slot that settle
//! in the same boundary block, each miner's `TermFeeReward` must be exactly its
//! own slot's treasury slice — never cross-credited to the other miner. Before
//! this test that invariant was verified ONLY by in-memory unit tests
//! (`aggregate_attributes_each_slots_fee_to_its_own_miner` and
//! `collect_attributes_each_tx_to_its_start_offset_slot_miners` in
//! `crates/actors/src/block_producer/ledger_expiry.rs`). No PRODUCED-block test
//! decoded the on-chain reward shadow-txs and checked each of two distinct miners
//! gets exactly its slice — the existing produced-block refund test
//! (`term_ledger_expiry/perm_refund.rs`) is single-miner (genesis mines every
//! partition). This test exercises the full produce → encode → decode path with
//! two distinct miners owning two distinct expired Submit slots.
//!
//! ## Why the assignment is OBSERVED, not hardcoded
//!
//! Submit-slot → miner assignment runs through `process_slot_needs`
//! (`epoch_snapshot.rs`): each newly-allocated slot draws a capacity partition
//! (hence its owning miner) from a `SimpleRNG` seeded on the epoch block's
//! `last_epoch_hash` — a live block hash (VDF/solution/timestamp dependent) that
//! a test cannot fix. With `num_partitions_per_slot = 1` the per-slot
//! no-duplicate-miner rule never constrains assignment ACROSS slots, so which
//! specific miner owns slot k is genuinely non-deterministic per run. Therefore
//! this test does NOT hardcode "miner A owns slot k"; it READS the actual owner of
//! each recycled Submit slot from the expiry block's epoch snapshot
//! (`expired_partition_infos`, which pairs each recycled partition_hash with its
//! slot_index) and asserts each observed owner's on-chain reward equals exactly the
//! treasury slices of the txs in the slots that owner holds.
//!
//! ## How ≥2 distinct miners is made reliable (not flaky)
//!
//! Two ingredients keep the two-miner property exercised on essentially every run:
//! 1. The genesis miner deterministically owns Submit slot 0 (its pre-existing
//!    bootstrap partition), which the first data tx fills.
//! 2. Cascade is activated from genesis so the Submit-ledger last-write "touch"
//!    runs: every slot WRITTEN in the fill epoch (slot 0 included) has its
//!    `last_height` refreshed to that epoch, so ALL the data slots — genesis's slot
//!    0 and the heavily-pledged peer's later slots — expire TOGETHER in one
//!    boundary block. Without the touch, slot 0 keeps its genesis allocation height
//!    and recycles at an earlier boundary than the later-written slots, splitting
//!    the settlement across blocks.
//! The peer is given many pledges so it wins the non-zero slots. The test still
//! FAILS LOUD (does not pass vacuously) if the random assignment ever collapses
//! every recycled slot onto a single miner.
//!
//! ## Residual gap
//!
//! A bit-exact deterministic "miner A owns slot 0, miner B owns slot 1" mapping is
//! impossible without making the consensus RNG seed injectable in test builds (a
//! consensus-path / system-contract change, deliberately not done here). This test
//! instead proves the on-chain AMOUNTS are correctly partitioned among whatever
//! distinct miners the protocol assigned — which is exactly the cross-crediting
//! property under test.

use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    DataLedger, IrysAddress, NodeConfig, U256, UnixTimestamp, fee_distribution::TermFeeCharges,
    hardfork_config::Cascade, irys::IrysSigner,
};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeMap;
use tracing::info;

#[test_log::test(tokio::test)]
async fn slow_heavy_per_miner_expiry_reward_attribution_two_miners() -> eyre::Result<()> {
    // ==================== CONFIG ====================
    const CHUNK_SIZE: u64 = 32;
    const NUM_CHUNKS_IN_PARTITION: u64 = 5; // 1 partition = 1 slot = 160 bytes
    const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 1; // expires after 1 epoch
    // Large enough that all NUM_SLOTS data txs (one alone per block) fit inside a
    // SINGLE epoch, so every data slot is allocated at the same epoch boundary and
    // therefore expires together in one boundary block.
    const BLOCKS_PER_EPOCH: u64 = 8;
    // One tx fills exactly one partition → one Submit slot. Each tx, mined alone,
    // deterministically occupies the next slot by start offset (tx k → slot k). The
    // LAST slot never expires (protocol rule), so to get several EXPIRING slots
    // split across two miners we create a handful of slots and only assert over the
    // ones that actually recycle.
    const PARTITION_BYTES: usize = (CHUNK_SIZE * NUM_CHUNKS_IN_PARTITION) as usize;
    const NUM_SLOTS: usize = 5;
    // A generous peer pledge count makes it overwhelmingly likely the
    // randomly-seeded slot assignment hands at least one of the new slots (1, 2) to
    // the peer rather than the genesis miner: with the genesis miner holding only a
    // small spare-capacity pool, P(every new slot lands on genesis) shrinks roughly
    // like (genesis_spare / (genesis_spare + N))^(new slots). The peer is never
    // started as a node, so its pledges add no packing cost here — they only
    // populate the genesis node's epoch commitment state.
    const PEER_PLEDGE_COUNT: usize = 16;
    // Pledges queue behind the stake until it is mined and must not overflow the
    // mempool's pending-pledge buffer; drain by mining after each batch.
    const PLEDGE_BATCH: usize = 4;
    // The peer must afford one stake (20_000 IRYS) plus PEER_PLEDGE_COUNT pledges
    // (~950 IRYS each). The data-posting user only needs enough for a few data
    // txs. Fund both generously.
    const USER_BALANCE: u128 = 1_000_000_000_000_000_000_000; // 1_000 IRYS
    const PEER_BALANCE: u128 = 1_000_000_000_000_000_000_000_000; // 1_000_000 IRYS

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = NUM_CHUNKS_IN_PARTITION;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = SUBMIT_LEDGER_EPOCH_LENGTH;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    // Pin to 1 so each expired slot maps to exactly one miner — required for the
    // per-miner attribution assertions below (one partition = one owner per slot).
    config
        .consensus
        .get_mut()
        .num_partitions_per_term_ledger_slot = 1;
    // Activate Cascade from genesis so the Submit-ledger last-write "touch" runs:
    // every slot WRITTEN in the data epoch (including the genesis-owned slot 0) gets
    // its `last_height` refreshed to that epoch, so all the data slots expire
    // TOGETHER in one boundary block. Without the touch, slot 0 keeps its genesis
    // allocation height (0) and expires at an earlier boundary than the
    // later-written slots, splitting the settlement across multiple blocks.
    config.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(0),
        one_year_epoch_length: 1000,
        thirty_day_epoch_length: 1000,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });

    let consensus_config = config.consensus_config();

    // The data poster (a regular user) and the peer miner's signer. The peer's
    // signer must be funded at genesis so its stake + pledge commitments are
    // affordable.
    let user_signer = IrysSigner::random_signer(&consensus_config);
    let peer_signer = IrysSigner::random_signer(&consensus_config);
    config.consensus.extend_genesis_accounts(vec![
        (
            user_signer.address(),
            GenesisAccount {
                balance: U256::from(USER_BALANCE).into(),
                ..Default::default()
            },
        ),
        (
            peer_signer.address(),
            GenesisAccount {
                balance: U256::from(PEER_BALANCE).into(),
                ..Default::default()
            },
        ),
    ]);

    // ==================== START GENESIS + PEER ====================
    // Cascade adds OneYear/ThirtyDay ledgers; pre-configure extra storage submodules
    // so there are enough partitions for all four data ledgers.
    let test_node = IrysNodeTest::new_genesis(config.clone());
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let node = test_node
        .start_and_wait_for_packing("perm_refund_two_miners", 30)
        .await;
    let genesis_address = node.node_ctx.config.node_config.miner_address();
    let peer_address = peer_signer.address();
    assert_ne!(
        genesis_address, peer_address,
        "genesis and peer must be distinct miners"
    );

    // Stake + pledge the peer (commitments posted to the genesis node; the genesis
    // node is the only mining node — the peer just needs partition assignments so
    // the epoch can hand it Submit slots). Many pledges make peer-ownership of a
    // new slot overwhelmingly likely. Stake first and mine it in, then post pledges
    // in batches mining between them so the mempool's pending-pledge buffer never
    // overflows (a flood of un-mined pledges returns 503 MempoolFull).
    node.post_stake_commitment_with_signer(&peer_signer).await?;
    node.mine_block().await?;
    let mut posted = 0;
    while posted < PEER_PLEDGE_COUNT {
        let batch = PLEDGE_BATCH.min(PEER_PLEDGE_COUNT - posted);
        for _ in 0..batch {
            let _pledge = node.post_pledge_commitment_with_signer(&peer_signer).await;
        }
        posted += batch;
        node.mine_block().await?;
    }

    // Mine an epoch so the peer's pledges become assigned capacity partitions,
    // available for the upcoming Submit-slot allocation.
    node.mine_until_next_epoch().await?;
    let peer_assignments = node.get_partition_assignments(peer_address);
    assert!(
        !peer_assignments.is_empty(),
        "peer should have capacity partition assignments after its pledges are processed"
    );
    info!(
        "peer has {} partition assignments after pledge epoch",
        peer_assignments.len()
    );

    // ==================== FILL SLOTS ====================
    // Post one partition-sized tx per block, each mined alone, so each tx
    // deterministically occupies the next Submit slot (start offset technique from
    // perm_refund.rs). Track each tx's term_fee → its slot index.
    info!(
        "Posting {} partition-filling txs (one per block)",
        NUM_SLOTS
    );
    let mut slot_term_fees: Vec<U256> = Vec::with_capacity(NUM_SLOTS);
    for slot in 0..NUM_SLOTS {
        let anchor = node.get_anchor().await?;
        let data = vec![(slot as u8).wrapping_add(1); PARTITION_BYTES];
        let tx = node.post_data_tx(anchor, data, &user_signer).await;
        node.wait_for_mempool(tx.header.id, 10).await?;
        let block = node.mine_block().await?;
        let submit = block
            .get_data_ledger_tx_ids()
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        assert!(
            submit.len() == 1 && submit[0] == tx.header.id,
            "tx for slot {} must be alone in its block (got {:?})",
            slot,
            submit
        );
        slot_term_fees.push(tx.header.term_fee.into());
        info!(
            "slot {}: tx={}, term_fee={}",
            slot, tx.header.id, slot_term_fees[slot]
        );
    }

    // ==================== MINE TO EXPIRY + COLLECT REWARDS ====================
    // The Submit slots holding this data all share one last-write height (the
    // Cascade touch refreshed them in the fill epoch), so they expire TOGETHER in a
    // single boundary block. Mine epoch-by-epoch, scanning each newly-mined range
    // for the first block whose EVM body carries TermFeeReward shadow-txs (the
    // expiry settlement), and stop as soon as it is found — pinning to that single
    // boundary and avoiding overshoot.
    let mut scanned_height = node.get_canonical_chain_height().await;
    let mut expiry_block_height = None;
    let mut on_chain_rewards: BTreeMap<IrysAddress, U256> = BTreeMap::new();
    // submit_ledger_epoch_length epochs to expiry, plus a couple of epochs of
    // headroom for boundary alignment.
    let max_epochs = SUBMIT_LEDGER_EPOCH_LENGTH + 3;
    'outer: for _ in 0..max_epochs {
        node.mine_until_next_epoch().await?;
        let h = node.get_canonical_chain_height().await;
        for height in (scanned_height + 1)..=h {
            let block = node.get_block_by_height(height).await?;
            let evm = node.wait_for_evm_block(block.evm_block_hash, 30).await?;
            let mut block_rewards: BTreeMap<IrysAddress, U256> = BTreeMap::new();
            for tx in &evm.body.transactions {
                let mut input = tx.input().as_ref();
                let Ok(shadow) = ShadowTransaction::decode(&mut input) else {
                    continue;
                };
                if let Some(TransactionPacket::TermFeeReward(reward)) = shadow.as_v1() {
                    let target_addr = IrysAddress::from(reward.target);
                    let amount = U256::from_le_bytes(reward.amount.to_le_bytes());
                    let entry = block_rewards.entry(target_addr).or_insert(U256::zero());
                    *entry = entry.saturating_add(amount);
                }
            }
            if !block_rewards.is_empty() {
                expiry_block_height = Some(height);
                on_chain_rewards = block_rewards;
                break 'outer;
            }
        }
        scanned_height = h;
    }
    let expiry_block_height =
        expiry_block_height.expect("an expiry boundary block emitting TermFeeReward must exist");
    info!(
        "expiry boundary block at height {} with rewards {:?}",
        expiry_block_height, on_chain_rewards
    );

    // ==================== RESOLVE EXPIRED SLOT OWNERS ====================
    // The set of slots that actually recycled this boundary is recorded on the
    // EXPIRY block's epoch snapshot as `expired_partition_infos` — each entry pairs
    // a recycled partition_hash with the slot_index it left. The owner is resolved
    // from that same partition's assignment (the recycled partition keeps its
    // miner_address). The slot_index equals the start-offset slot of the data tx
    // mined into it (tx k → slot k), so `slot_term_fees[slot]` is that slot's fee.
    let expiry_block = node.get_block_by_height(expiry_block_height).await?;
    let mut expired_slot_owners: BTreeMap<usize, IrysAddress> = BTreeMap::new();
    {
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&expiry_block.block_hash)
            .expect("expiry-block epoch snapshot must exist");
        let expired = snapshot.expired_partition_infos.clone().unwrap_or_default();
        for info_entry in expired.iter().filter(|p| p.ledger_id == DataLedger::Submit) {
            let slot_index = info_entry.slot_index;
            // Only the slots holding one of our partition-filling data txs.
            if slot_index >= NUM_SLOTS {
                continue;
            }
            let owner = snapshot
                .get_data_partition_assignment(info_entry.partition_hash)
                .expect("recycled partition must retain its assignment")
                .miner_address;
            expired_slot_owners.insert(slot_index, owner);
            info!("expired slot {} owned by miner {}", slot_index, owner);
        }
    }
    assert!(
        !expired_slot_owners.is_empty(),
        "no Submit slots holding our data recycled at the boundary — test setup \
         failed to age expiring (non-last) slots"
    );

    // FAIL LOUD if the random assignment collapsed every expired slot onto one
    // miner — the cross-crediting property is only exercised with ≥2 distinct
    // owners. The heavily-pledged peer makes this collapse astronomically unlikely.
    let distinct_owners: std::collections::BTreeSet<_> =
        expired_slot_owners.values().copied().collect();
    assert!(
        distinct_owners.len() >= 2,
        "expired Submit slots collapsed onto a single miner ({:?}); the two-miner \
         cross-crediting property was not exercised. Expired slot owners: {:?}",
        distinct_owners,
        expired_slot_owners
    );

    // ==================== EXPECTED PER-MINER REWARD ====================
    // Each single-miner slot's whole term_fee_treasury goes to that slot's owner
    // (distribution_on_expiry over a one-element miner list). Sum per owner over the
    // slots that owner holds among the expired set.
    let mut expected_rewards: BTreeMap<IrysAddress, U256> = BTreeMap::new();
    for (&slot_index, &owner) in &expired_slot_owners {
        let treasury = TermFeeCharges::new(slot_term_fees[slot_index].into(), &consensus_config)?
            .distribution_on_expiry(&[owner])?[0];
        let entry = expected_rewards.entry(owner).or_insert(U256::zero());
        *entry = entry.saturating_add(treasury);
    }

    // ==================== ASSERT: NO CROSS-CREDITING ====================
    // Each owner's on-chain TermFeeReward total must equal exactly the sum of the
    // treasury slices of the slots IT owns — neither miner receives any part of
    // the other's slot.
    for (owner, expected) in &expected_rewards {
        let got = on_chain_rewards.get(owner).copied().unwrap_or(U256::zero());
        assert_eq!(
            got, *expected,
            "miner {} on-chain expiry reward ({}) must equal exactly its own slots' \
             treasury slices ({}) — no cross-crediting from the other miner's slot",
            owner, got, expected
        );
    }

    // And no recipient outside the expected owner set received an expiry reward in
    // this block (catches a stray credit to a third party / the wrong miner).
    for (recipient, amount) in &on_chain_rewards {
        if *amount == U256::zero() {
            continue;
        }
        assert!(
            expected_rewards.contains_key(recipient),
            "unexpected TermFeeReward recipient {} (amount {}) in the expiry block — \
             only the expired slots' owners should be credited",
            recipient,
            amount
        );
    }

    info!(
        "two-miner expiry reward attribution verified: {:?}",
        expected_rewards
    );

    node.stop().await;
    Ok(())
}
