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

/// Verify last-write expiry for a transaction that SPANS a slot boundary, and
/// that the two slots it occupies expire independently and correctly — i.e. the
/// spanning tx is never *under*-retained ("half a tx expiring early").
///
/// A tx's data is a contiguous chunk range; when it crosses the slot boundary
/// `C` it occupies the head slot N (where it starts) and the tail slot N+1. The
/// touch refreshes `last_height` on every slot the epoch's chunk window covers,
/// so at write time BOTH halves get the same `last_height`. Because `total_chunks`
/// is monotonic, the head slot then *freezes* at the write epoch (it can never
/// receive more data), while the tail keeps filling in later epochs.
///
/// Scenario (num_blocks_in_epoch=2, thirty_day_epoch_length=2 -> expiry after
/// 4 blocks; slot size C = 10 chunks):
/// 1. Epoch E_a: write 4 txs × 3 chunks = 12 chunks. They fill slot 0 (chunks
///    0-9) and spill into slot 1 (chunks 10-11). The 4th tx (chunks 9,10,11)
///    SPANS the slot 0/1 boundary: it starts in slot 0 (head) and ends in slot 1
///    (tail). Allocation also adds extra slots so neither slot 0 nor slot 1 is
///    the protected last slot. Both slot 0 and slot 1 get `last_height = E_a`.
/// 2. Epoch E_b (> E_a): write 1 more tx × 3 chunks into slot 1 (chunks 12-14).
///    Only slot 1 is touched -> `last_height = E_b`. Slot 0 (head) stays at E_a,
///    proving the head's retention is NOT coupled to later tail writes.
/// 3. At E_a + 4: the head slot 0 expires — exactly `epoch_length` after the
///    spanning tx was written (full retention honored, not premature) — while the
///    tail slot 1 is still ALIVE (its last write was E_b). The spanning tx's two
///    slots expire at different times; the head governs the tx's readable life.
/// 4. At E_b + 4: the tail slot 1 also expires.
///
/// Without the last-write fix slot 0 would keep its genesis `last_height` and be
/// expired well before E_a + 4 — so this test fails on `master`.
#[test_log::test(tokio::test)]
async fn heavy_cascade_spanning_tx_last_write_expiry() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64; // expires 2×2 = 4 blocks after last write
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64; // slot size C = 10 chunks
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

    // Reads (slot0_last_height, slot0_expired, slot1_last_height, slot1_expired,
    // slot_count, first_unexpired) for the ThirtyDay ledger at `height`.
    async fn thirty_day_slots(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        height: u64,
    ) -> eyre::Result<(u64, bool, u64, bool, usize, usize)> {
        let block = ctx.get_block_by_height(height).await?;
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        Ok((
            slots[0].last_height,
            slots[0].is_expired,
            slots[1].last_height,
            slots[1].is_expired,
            slots.len(),
            snapshot.get_first_unexpired_slot_index(DataLedger::ThirtyDay),
        ))
    }

    // Mine past the Cascade activation height.
    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    // --- Batch 1 @ E_a: 4 txs × 3 chunks = 12 chunks -> slot 0 (0-9) + slot 1
    //     (10-11). The 4th tx (chunks 9,10,11) spans the slot 0/1 boundary. ---
    for i in 0_u8..4 {
        let mut tx_data = vec![9_u8; 96]; // 96 bytes / 32 = 3 chunks
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
    while ctx.get_canonical_chain_height().await < epoch_a {
        ctx.mine_block().await?;
    }

    let (s0_la_a, s0_exp_a, s1_la_a, _s1_exp_a, slot_count, _) =
        thirty_day_slots(&ctx, epoch_a).await?;
    assert!(
        slot_count >= 3,
        "expected slots 0 and 1 to both be non-last (slot_count={})",
        slot_count
    );
    // The spanning tx bumps BOTH halves to the write epoch.
    assert_eq!(
        s0_la_a, epoch_a,
        "head slot 0 last_height should be the write epoch E_a={}",
        epoch_a
    );
    assert_eq!(
        s1_la_a, epoch_a,
        "tail slot 1 last_height should be the write epoch E_a={} (spanning tx \
         touched both halves)",
        epoch_a
    );
    assert!(!s0_exp_a, "head slot 0 must not be expired at E_a");

    // --- Batch 2 @ E_b (> E_a): 1 tx × 3 chunks into slot 1 (chunks 12-14) ---
    while ctx.get_canonical_chain_height().await <= epoch_a {
        ctx.mine_block().await?;
    }
    {
        let mut tx_data = vec![9_u8; 96];
        tx_data[0] = 100;
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
    let h2 = ctx.mine_block().await?.height;
    let epoch_b = next_epoch_boundary(h2);
    assert!(
        epoch_b > epoch_a,
        "E_b ({epoch_b}) must be after E_a ({epoch_a})"
    );
    while ctx.get_canonical_chain_height().await < epoch_b {
        ctx.mine_block().await?;
    }

    let (s0_la_b, _, s1_la_b, _, _, _) = thirty_day_slots(&ctx, epoch_b).await?;
    // Head slot froze at E_a; the later tail write must NOT extend the head.
    assert_eq!(
        s0_la_b, epoch_a,
        "head slot 0 must stay frozen at E_a={} after a later tail write (got {})",
        epoch_a, s0_la_b
    );
    assert_eq!(
        s1_la_b, epoch_b,
        "tail slot 1 should advance to its last-write epoch E_b={}",
        epoch_b
    );

    // --- Check 1 @ E_a + 4: head expires (full retention from its write), tail
    //     survives. The spanning tx's two slots expire at DIFFERENT times. ---
    let head_expiry = epoch_a + expiry_blocks;
    while ctx.get_canonical_chain_height().await < head_expiry {
        ctx.mine_block().await?;
    }
    let (_, s0_exp_mid, _, s1_exp_mid, _, first_unexpired_mid) =
        thirty_day_slots(&ctx, head_expiry).await?;
    assert!(
        s0_exp_mid,
        "head slot 0 must expire exactly epoch_length after the spanning tx's \
         write (E_a+{expiry_blocks}={head_expiry})"
    );
    assert!(
        !s1_exp_mid,
        "tail slot 1 must still be ALIVE at E_a+{expiry_blocks} — its last write \
         was E_b={epoch_b} (no half-tx premature loss; tail lingers)"
    );
    assert_eq!(
        first_unexpired_mid, 1,
        "first-unexpired ThirtyDay slot should be 1 (head expired, tail alive)"
    );

    // --- Check 2 @ E_b + 4: the tail slot 1 also expires. ---
    let tail_expiry = epoch_b + expiry_blocks;
    while ctx.get_canonical_chain_height().await < tail_expiry {
        ctx.mine_block().await?;
    }
    let (_, _, _, s1_exp_late, _, first_unexpired_late) =
        thirty_day_slots(&ctx, tail_expiry).await?;
    assert!(
        s1_exp_late,
        "tail slot 1 must expire epoch_length after its last write \
         (E_b+{expiry_blocks}={tail_expiry})"
    );
    assert!(
        first_unexpired_late > 1,
        "first-unexpired ThirtyDay slot should advance past 1 once the tail \
         expires (got {first_unexpired_late})"
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

/// Reproduction: the term-fee model in `block_producer::ledger_expiry.rs` must
/// distribute fees for EXACTLY the slots that actually recycle.
///
/// The fee calc predicts the expiring set from the PARENT epoch snapshot
/// (`get_expiring_partition_info`, pre-this-epoch-touch), while the real
/// recycle set is `expired_partition_infos` on the NEW epoch snapshot
/// (post-touch). They can diverge for a slot that is written in the very epoch
/// it would otherwise expire: the touch rescues it from recycling, but the
/// parent-based fee calc still settles it -> its fees would be distributed now
/// AND again when it actually recycles later (double distribution).
///
/// Scenario (nbe=2, thirty_day_epoch_length=2 -> window=4 blocks, C=10 chunks):
/// 1. Epoch L: write 12 chunks -> slot 0 full, slot 1 partial (the frontier),
///    extra empty slots allocated so slot 1 is non-last. Both get last_height=L.
/// 2. Idle for the full window (no ThirtyDay data) so slot 1 stays at L and
///    becomes due to expire at E = L + window.
/// 3. Epoch E = L + 4: write again -> lands in slot 1 (still the frontier),
///    touching it to E so it is RESCUED from recycling.
///
/// At E the fee-predicted set (parent) must equal the actual recycle set (new).
/// Pre-fix this FAILS: slot 1 is in the fee set but not the recycle set.
#[test_log::test(tokio::test)]
async fn heavy_cascade_expiry_fee_model_matches_actual_recycle() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let window = thirty_day_epoch_length * num_blocks_in_epoch; // 4

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

    async fn post_thirty_day(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        signer: &irys_types::irys::IrysSigner,
        tag: u8,
        bytes: usize,
    ) -> eyre::Result<()> {
        let mut data = vec![9_u8; bytes];
        data[0] = tag;
        let price = ctx
            .get_data_price(DataLedger::ThirtyDay, bytes as u64)
            .await?;
        let tx = signer.create_transaction_with_fees(
            data,
            ctx.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
        Ok(())
    }

    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    // Epoch L: 4 txs × 3 chunks = 12 chunks -> slot 0 full, slot 1 partial frontier.
    for i in 0_u8..4 {
        post_thirty_day(&ctx, &signer, i, 96).await?;
    }
    let l = next_epoch_boundary(ctx.mine_block().await?.height);
    while ctx.get_canonical_chain_height().await < l {
        ctx.mine_block().await?;
    }

    // Idle through the window so slot 1 ages to its expiry epoch E = L + window.
    let e = l + window;
    while ctx.get_canonical_chain_height().await < e - 1 {
        ctx.mine_block().await?;
    }

    // Resume exactly in epoch E: write into the (still-frontier) slot 1.
    post_thirty_day(&ctx, &signer, 200, 96).await?;
    while ctx.get_canonical_chain_height().await < e {
        ctx.mine_block().await?;
    }

    // ThirtyDay's cumulative total_chunks at the epoch block E (the value the
    // producer/validator feed to the fee calc).
    let epoch_block = ctx.get_block_by_height(e).await?;
    let e_total = epoch_block
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == DataLedger::ThirtyDay as u32)
        .map(|dl| dl.total_chunks)
        .unwrap_or(0);

    // Fee-predicted expiring set: the PARENT epoch snapshot (as the producer
    // sees it) asked which ThirtyDay slots will actually recycle at height E.
    let parent_block = ctx.get_block_by_height(e - 1).await?;
    let mut fee_set = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snap = tree
            .get_epoch_snapshot(&parent_block.block_hash)
            .expect("parent epoch snapshot");
        snap.get_expiring_partition_info(e, DataLedger::ThirtyDay, e_total)
            .into_iter()
            .map(|p| p.slot_index)
            .collect::<Vec<_>>()
    };
    fee_set.sort_unstable();
    fee_set.dedup();

    // Actual recycle set recorded by the NEW epoch snapshot at E.
    let mut actual_set = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snap = tree
            .get_epoch_snapshot(&epoch_block.block_hash)
            .expect("epoch snapshot at E");
        snap.expired_partition_infos
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.ledger_id == DataLedger::ThirtyDay)
            .map(|p| p.slot_index)
            .collect::<Vec<_>>()
    };
    actual_set.sort_unstable();
    actual_set.dedup();

    assert_eq!(
        fee_set, actual_set,
        "fee-distribution expiring set (parent, get_expiring_partition_info) must \
         equal the actual recycle set (expired_partition_infos) at epoch E={e}; \
         a mismatch means fees are distributed for a slot that did not recycle"
    );

    ctx.stop().await;
    Ok(())
}
