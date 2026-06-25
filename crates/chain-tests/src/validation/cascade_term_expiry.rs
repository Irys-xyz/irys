use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    BoundedFee, DataLedger, NodeConfig, U256, UnixTimestamp, hardfork_config::Cascade,
};

/// Verify that OneYear and ThirtyDay term ledgers expire at different rates
/// based on their distinct epoch_length values from the Cascade hardfork config.
///
/// With one_year_epoch_length=8 and thirty_day_epoch_length=2 (both using 2-block epochs):
/// - ThirtyDay data expires after 2×2 = 4 blocks
/// - OneYear data expires after 8×2 = 16 blocks
///
/// At the mid-point boundary, ThirtyDay slots should be expiring while OneYear slots
/// remain active. At the late boundary, OneYear slots should also expire.
#[test_log::test(tokio::test)]
async fn heavy_cascade_term_ledger_expiry_respects_distinct_epoch_lengths() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch; // used as mining target
    let one_year_epoch_length = 8_u64; // expires after 8×2 = 16 blocks
    let thirty_day_epoch_length = 2_u64; // expires after 2×2 = 4 blocks
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length,
            thirty_day_epoch_length,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    // Pre-configure 5 storage submodules so there are enough partitions for
    // all 4 data ledgers (Publish, Submit, OneYear, ThirtyDay). The default
    // of 3 isn't enough — the last slot in a ledger never expires, so we
    // need partitions assigned to non-last slots for expiry to produce
    // ExpiringPartitionInfo entries.
    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let ctx = test_node.start_and_wait_for_packing("test", 30).await;

    // Mine past the Cascade activation height
    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    let data = vec![9_u8; 96]; // 3 chunks of 32 bytes
    let data_size = data.len() as u64;

    // Post 4 OneYear txs (12 chunks → 2 slots: slot 0 full, slot 1 partial)
    // Use unique data per tx to avoid duplicate tx ids
    for i in 0_u8..4 {
        let mut tx_data = data.clone();
        tx_data[0] = i;
        let price = ctx.get_data_price(DataLedger::OneYear, data_size).await?;
        let tx = signer.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::OneYear,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }

    // Post 4 ThirtyDay txs (12 chunks → 2 slots: slot 0 full, slot 1 partial)
    for i in 0_u8..4 {
        let mut tx_data = data.clone();
        tx_data[0] = 100 + i;
        let price = ctx.get_data_price(DataLedger::ThirtyDay, data_size).await?;
        let tx = signer.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }

    // Mine a block to include all the term txs
    let inclusion_block = ctx.mine_block().await?;
    let inclusion_height = inclusion_block.height;

    // Track treasury balance — it should increase when term txs are included
    // (95% of each term_fee goes to treasury) and must never go negative.
    let treasury_after_inclusion = inclusion_block.treasury;
    tracing::info!(
        "Treasury after inclusion block {}: {}",
        inclusion_height,
        treasury_after_inclusion
    );
    assert!(
        treasury_after_inclusion > U256::zero(),
        "Treasury should be positive after term tx inclusion (got {})",
        treasury_after_inclusion
    );

    let next_epoch_boundary = |h: u64| -> u64 {
        if h.is_multiple_of(num_blocks_in_epoch) {
            h
        } else {
            h + (num_blocks_in_epoch - (h % num_blocks_in_epoch))
        }
    };

    let thirty_day_expiry_blocks = thirty_day_epoch_length * num_blocks_in_epoch;
    let one_year_expiry_blocks = one_year_epoch_length * num_blocks_in_epoch;

    // Mine to the epoch boundary where ThirtyDay expires but OneYear does not
    let mid_boundary = next_epoch_boundary(inclusion_height + thirty_day_expiry_blocks);
    while ctx.get_canonical_chain_height().await < mid_boundary {
        ctx.mine_block().await?;
    }

    // At mid_boundary, the epoch snapshot has already processed ThirtyDay expiry.
    // Use expired_partition_infos (recorded at snapshot creation) and
    // get_first_unexpired_slot_index to verify the differential expiry.
    let mid_block = ctx.get_block_by_height(mid_boundary).await?;
    let (mid_expired, mid_td_first, mid_oy_first) = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&mid_block.block_hash)
            .expect("epoch snapshot should exist at mid boundary");
        let expired = snapshot.expired_partition_infos.clone().unwrap_or_default();
        (
            expired,
            snapshot.get_first_unexpired_slot_index(DataLedger::ThirtyDay),
            snapshot.get_first_unexpired_slot_index(DataLedger::OneYear),
        )
    };

    // Verify treasury never went negative through the mid-boundary mining.
    // Check every block from inclusion to mid_boundary.
    for h in (inclusion_height + 1)..=mid_boundary {
        let block = ctx.get_block_by_height(h).await?;
        assert!(
            block.treasury >= U256::zero(),
            "Treasury must never go negative (height={}, treasury={})",
            h,
            block.treasury
        );
    }
    let mid_treasury = mid_block.treasury;
    tracing::info!(
        "Treasury at mid-boundary {}: {}",
        mid_boundary,
        mid_treasury
    );

    // ThirtyDay partitions should have been expired at this boundary
    assert!(
        mid_expired
            .iter()
            .any(|p| p.ledger_id == DataLedger::ThirtyDay),
        "expected ThirtyDay partitions expired at height {}, expired: {:?}",
        mid_boundary,
        mid_expired
    );
    // OneYear should NOT have expired yet
    assert!(
        !mid_expired
            .iter()
            .any(|p| p.ledger_id == DataLedger::OneYear),
        "OneYear should not expire yet at height {}",
        mid_boundary
    );
    // ThirtyDay's first unexpired slot should have advanced past OneYear's
    assert!(
        mid_td_first > mid_oy_first,
        "ThirtyDay should advance first unexpired slot before OneYear (td={}, oy={})",
        mid_td_first,
        mid_oy_first
    );

    // Mine to the epoch boundary where OneYear also expires
    let late_boundary = next_epoch_boundary(inclusion_height + one_year_expiry_blocks);
    while ctx.get_canonical_chain_height().await < late_boundary {
        ctx.mine_block().await?;
    }

    let late_block = ctx.get_block_by_height(late_boundary).await?;
    let (late_expired, late_td_first, late_oy_first) = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&late_block.block_hash)
            .expect("epoch snapshot should exist at late boundary");
        let expired = snapshot.expired_partition_infos.clone().unwrap_or_default();
        (
            expired,
            snapshot.get_first_unexpired_slot_index(DataLedger::ThirtyDay),
            snapshot.get_first_unexpired_slot_index(DataLedger::OneYear),
        )
    };

    // Verify treasury never went negative through the late-boundary mining.
    for h in (mid_boundary + 1)..=late_boundary {
        let block = ctx.get_block_by_height(h).await?;
        assert!(
            block.treasury >= U256::zero(),
            "Treasury must never go negative (height={}, treasury={})",
            h,
            block.treasury
        );
    }
    let late_treasury = late_block.treasury;
    tracing::info!(
        "Treasury at late-boundary {}: {}",
        late_boundary,
        late_treasury
    );

    // OneYear partitions should now have expired
    assert!(
        late_expired
            .iter()
            .any(|p| p.ledger_id == DataLedger::OneYear),
        "expected OneYear partitions expired at height {}, expired: {:?}",
        late_boundary,
        late_expired
    );
    assert!(
        late_oy_first > mid_oy_first,
        "OneYear first unexpired slot should advance (before={}, after={})",
        mid_oy_first,
        late_oy_first
    );
    assert!(
        late_td_first >= mid_td_first,
        "ThirtyDay should remain at least as expired as before (before={}, after={})",
        mid_td_first,
        late_td_first
    );

    ctx.stop().await;
    Ok(())
}

/// Verify that a slot's expiry is anchored to the LAST epoch data was written
/// into it, not to when the slot was allocated.
///
/// This exercises the Cascade retention fix: `last_height` is refreshed each
/// epoch a slot receives new canonical data, so a slot that keeps receiving
/// writes across multiple epochs stays alive for `epoch_length` epochs measured
/// from its *last* write.
///
/// Scenario (num_blocks_in_epoch=2, thirty_day_epoch_length=2 -> expiry after
/// 4 blocks):
/// 1. Write 6 chunks of ThirtyDay data -> lands in slot 0; the next epoch
///    boundary E_a allocates extra slots (so slot 0 is no longer the protected
///    last slot) and stamps slot 0's `last_height = E_a`.
/// 2. One epoch later, write 2 more chunks -> also lands in slot 0 (offsets 6,7
///    are still within slot 0's [0, 10) range); the next boundary E_b = E_a + 2
///    re-stamps slot 0's `last_height = E_b`.
/// 3. At boundary E_a + 4 (when allocation-time semantics would have expired
///    slot 0) the slot must still be ACTIVE, because its last write was at E_b.
/// 4. At boundary E_b + 4 the slot finally expires.
///
/// Without the last-write fix slot 0 would retain its genesis/allocation
/// `last_height` and be expired by step 3 — so this test fails on `master`.
#[test_log::test(tokio::test)]
async fn heavy_cascade_slot_expiry_anchored_to_last_write() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64; // expires 2×2 = 4 blocks after last write
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let expiry_blocks = thirty_day_epoch_length * num_blocks_in_epoch; // 4

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length,
            thirty_day_epoch_length,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let ctx = test_node.start_and_wait_for_packing("test", 30).await;

    let next_epoch_boundary = |h: u64| -> u64 {
        if h.is_multiple_of(num_blocks_in_epoch) {
            h
        } else {
            h + (num_blocks_in_epoch - (h % num_blocks_in_epoch))
        }
    };

    // Mine past the Cascade activation height.
    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    // --- Batch 1: 6 chunks (2 txs × 96 bytes) into slot 0 ---
    for i in 0_u8..2 {
        let mut tx_data = vec![9_u8; 96];
        tx_data[0] = i;
        let price = ctx.get_data_price(DataLedger::ThirtyDay, 96).await?;
        let tx = signer.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }
    let h1 = ctx.mine_block().await?.height;
    let epoch_a = next_epoch_boundary(h1);

    // Mine to E_a so the epoch snapshot allocates extra slots (so slot 0 is no
    // longer the protected last slot) and stamps slot 0's last_height = E_a.
    while ctx.get_canonical_chain_height().await < epoch_a {
        ctx.mine_block().await?;
    }

    let block_a = ctx.get_block_by_height(epoch_a).await?;
    let (la_after_a, expired_after_a, slot_count) = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block_a.block_hash)
            .expect("epoch snapshot should exist at E_a");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        (slots[0].last_height, slots[0].is_expired, slots.len())
    };
    assert!(
        slot_count >= 2,
        "expected slot 0 to no longer be the (protected) last slot, slot_count={}",
        slot_count
    );
    assert_eq!(
        la_after_a, epoch_a,
        "slot 0 last_height should be stamped to first-write epoch E_a={}",
        epoch_a
    );
    assert!(!expired_after_a, "slot 0 must not be expired at E_a");

    // --- Batch 2: 2 more chunks (1 tx × 64 bytes), one epoch later ---
    // Ensure we're strictly past E_a so the second write lands in a later epoch.
    while ctx.get_canonical_chain_height().await <= epoch_a {
        ctx.mine_block().await?;
    }
    {
        let mut tx_data = vec![9_u8; 64];
        tx_data[0] = 100;
        let price = ctx.get_data_price(DataLedger::ThirtyDay, 64).await?;
        let tx = signer.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }
    let h2 = ctx.mine_block().await?.height;
    let epoch_b = next_epoch_boundary(h2);
    assert!(
        epoch_b > epoch_a,
        "second write must occur in a later epoch (E_a={}, E_b={})",
        epoch_a,
        epoch_b
    );

    // --- Check 1: at E_a + expiry_blocks, slot 0 must still be ALIVE ---
    // (Allocation-time semantics would have expired it here.)
    let first_write_expiry = epoch_a + expiry_blocks;
    while ctx.get_canonical_chain_height().await < first_write_expiry {
        ctx.mine_block().await?;
    }
    let block_mid = ctx.get_block_by_height(first_write_expiry).await?;
    let (la_mid, expired_mid, first_unexpired_mid) = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block_mid.block_hash)
            .expect("epoch snapshot should exist at E_a+expiry");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        (
            slots[0].last_height,
            slots[0].is_expired,
            snapshot.get_first_unexpired_slot_index(DataLedger::ThirtyDay),
        )
    };
    assert_eq!(
        la_mid, epoch_b,
        "slot 0 last_height should have advanced to last-write epoch E_b={}",
        epoch_b
    );
    assert!(
        !expired_mid,
        "slot 0 must NOT be expired at E_a+{} ({}) — its last write was at E_b={}",
        expiry_blocks, first_write_expiry, epoch_b
    );
    assert_eq!(
        first_unexpired_mid, 0,
        "ThirtyDay first-unexpired slot should still be 0 at E_a+{}",
        expiry_blocks
    );

    // --- Check 2: at E_b + expiry_blocks, slot 0 finally expires ---
    let last_write_expiry = epoch_b + expiry_blocks;
    while ctx.get_canonical_chain_height().await < last_write_expiry {
        ctx.mine_block().await?;
    }
    let block_late = ctx.get_block_by_height(last_write_expiry).await?;
    let (expired_late, first_unexpired_late) = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block_late.block_hash)
            .expect("epoch snapshot should exist at E_b+expiry");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        (
            slots[0].is_expired,
            snapshot.get_first_unexpired_slot_index(DataLedger::ThirtyDay),
        )
    };
    assert!(
        expired_late,
        "slot 0 must be expired at E_b+{} ({})",
        expiry_blocks, last_write_expiry
    );
    assert!(
        first_unexpired_late > 0,
        "ThirtyDay first-unexpired slot should advance past 0 once slot 0 expires (got {})",
        first_unexpired_late
    );

    ctx.stop().await;
    Ok(())
}

/// Mid-chain Cascade activation: verify the Submit ledger's `last_height`
/// maintenance transitions correctly *across* the activation epoch.
///
/// The Submit ledger is live before Cascade, with `last_height` frozen at
/// allocation (the touch is gated off). This test pins the transition:
/// 1. **Pre-activation:** writing Submit data does NOT advance slot 0's
///    `last_height` — it stays at its genesis allocation height (0).
/// 2. **Post-activation:** the touch runs, and a write advances `last_height`
///    to the epoch that processes it, so expiry now counts from the last write.
///
/// Activation is performed via stop -> set `cascade.activation_timestamp = now`
/// -> restart (the established mid-chain hardfork pattern, as in the Aurora
/// tests). On replay the historical epoch blocks predate the activation
/// timestamp so they stay pre-activation, while every newly mined epoch block
/// is post-activation — deterministic, no wall-clock race.
///
/// Note: the first post-activation epoch re-anchors any Submit data whose
/// cumulative-chunk delta lands in that epoch's window (the activation-straddle
/// cohort). The touch never *retroactively* rescans slots already fully counted
/// and dormant before activation — those keep their allocation-time
/// `last_height` — but data still being counted around the boundary is picked
/// up. This test therefore asserts the robust endpoints (frozen before, tracks
/// the write after) rather than the exact straddle value.
///
/// A single, large partition keeps all the small Submit writes inside slot 0,
/// which therefore stays the only (last) slot: protected from expiry, so its
/// `last_height` can be observed cleanly across the boundary.
#[test_log::test(tokio::test)]
async fn slow_heavy_cascade_midchain_activation_submit_last_height_transition() -> eyre::Result<()>
{
    let num_blocks_in_epoch = 2_u64;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    // Large partition so the handful of 1-chunk Submit writes all land in slot 0.
    let num_chunks_in_partition = 100_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // Cascade intentionally NOT configured yet — activated mid-chain below.
    });

    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let node = test_node.start_and_wait_for_packing("test", 30).await;

    let next_epoch_boundary = |h: u64| -> u64 {
        if h.is_multiple_of(num_blocks_in_epoch) {
            h
        } else {
            h + (num_blocks_in_epoch - (h % num_blocks_in_epoch))
        }
    };

    // Post one 1-chunk data tx. The Submit ledger is not user-targetable
    // directly; published data lands in Submit (pre-promotion), so a Publish
    // data tx is what grows Submit's `total_chunks`.
    async fn post_submit_chunk(
        node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        signer: &irys_types::irys::IrysSigner,
        chunk_size: u64,
        tag: u8,
    ) -> eyre::Result<()> {
        let mut data = vec![7_u8; chunk_size as usize];
        data[0] = tag;
        let anchor = node.get_anchor().await?;
        let tx = node.post_data_tx(anchor, data, signer).await;
        node.wait_for_mempool(tx.header.id, 30).await?;
        Ok(())
    }

    // Reads (submit_slot0_last_height, submit_slot_count, active_ledger_count)
    // from the epoch snapshot at the canonical tip.
    async fn submit_state(
        node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    ) -> eyre::Result<(u64, usize, usize)> {
        let h = node.get_canonical_chain_height().await;
        let block = node.get_block_by_height(h).await?;
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist at tip");
        let slots = snapshot.ledgers.get_slots(DataLedger::Submit);
        Ok((
            slots[0].last_height,
            slots.len(),
            snapshot.ledgers.active_ledgers().len(),
        ))
    }

    // === Pre-activation: write Submit data across two epochs ===
    // Cascade is inactive, so the touch never runs; slot 0 must keep its genesis
    // allocation last_height (0) despite receiving data.
    for tag in 0_u8..2 {
        post_submit_chunk(&node, &signer, chunk_size, tag).await?;
        let boundary = next_epoch_boundary(node.get_canonical_chain_height().await + 1);
        while node.get_canonical_chain_height().await < boundary {
            node.mine_block().await?;
        }
    }

    let (pre_last_height, pre_slot_count, pre_ledger_count) = submit_state(&node).await?;
    assert_eq!(
        pre_ledger_count, 2,
        "cascade ledgers must be absent pre-activation (Publish + Submit only)"
    );
    assert_eq!(pre_slot_count, 1, "Submit should still be a single slot");
    assert_eq!(
        pre_last_height, 0,
        "pre-activation: touch gated off, slot 0 keeps its genesis last_height"
    );

    // === Activate Cascade mid-chain via stop -> set activation = now -> restart ===
    let mut stopped = node.stop().await;
    let activation_timestamp = UnixTimestamp::now()
        .expect("system time after unix epoch")
        .as_secs();
    stopped.cfg.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
        one_year_epoch_length,
        thirty_day_epoch_length,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
    let node = stopped.start().await;

    // Mine a couple of cascade-active epochs so activate_cascade runs.
    for _ in 0..(2 * num_blocks_in_epoch) {
        node.mine_block().await?;
    }

    let (_, _, mid_ledger_count) = submit_state(&node).await?;
    assert_eq!(
        mid_ledger_count, 4,
        "cascade ledgers (OneYear + ThirtyDay) must be present after activation"
    );

    // === Post-activation write: the touch now advances last_height ===
    post_submit_chunk(&node, &signer, chunk_size, 200).await?;
    let write_height = node.mine_block().await?.height;
    let boundary = next_epoch_boundary(write_height);
    while node.get_canonical_chain_height().await < boundary {
        node.mine_block().await?;
    }

    let (post_last_height, post_slot_count, _) = submit_state(&node).await?;
    assert_eq!(post_slot_count, 1, "Submit should still be a single slot");
    assert!(
        post_last_height > pre_last_height,
        "post-activation: last_height must advance past the frozen pre-activation \
         value (pre={}, post={})",
        pre_last_height,
        post_last_height
    );
    assert_eq!(
        post_last_height, boundary,
        "post-activation write must anchor last_height to the epoch that processes \
         it (write at height {}, processed at epoch {})",
        write_height, boundary
    );

    node.stop().await;
    Ok(())
}
