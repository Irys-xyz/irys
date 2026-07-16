use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    BoundedFee, DataLedger, NodeConfig, U256, UnixTimestamp, hardfork_config::Cascade,
};
use reth::rpc::types::TransactionTrait as _;

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
/// 4. At E_b + 4: the tail slot 1 STILL does not expire — it is the partially
///    written frontier slot (15/20 chunks), and only fully-written slots may
///    expire: chunk offsets are cumulative, so expiring the frontier slot would
///    strand every tx later appended into its remainder (non-promotable forever,
///    never settled).
/// 5. Epoch E_c: 5 more chunks fill slot 1 exactly (total 20). A full
///    `epoch_length` after that write, at E_c + 4, slot 1 finally expires.
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

    // --- Check 2 @ E_b + 4: the tail slot 1 is the partially written frontier
    //     slot (15/20 chunks), so it must NOT expire even a full window after
    //     its last write — future appends land in its remainder, and expiring
    //     it would strand them (non-promotable forever, never settled). ---
    let tail_expiry = epoch_b + expiry_blocks;
    while ctx.get_canonical_chain_height().await < tail_expiry {
        ctx.mine_block().await?;
    }
    let (_, _, _, s1_exp_late, _, first_unexpired_late) =
        thirty_day_slots(&ctx, tail_expiry).await?;
    assert!(
        !s1_exp_late,
        "the partially written frontier slot 1 must NOT expire at \
         E_b+{expiry_blocks}={tail_expiry} — only fully-written slots may expire"
    );
    assert_eq!(
        first_unexpired_late, 1,
        "first-unexpired ThirtyDay slot should stay 1 while the frontier slot \
         remains partially written (got {first_unexpired_late})"
    );

    // --- Batch 3 @ E_c: 2 + 3 chunks fill slot 1 exactly (chunks 15-19,
    //     total 20). Because slot 1 never expired, the touch refreshes its
    //     expiry clock to E_c. ---
    for (marker, bytes) in [(101_u8, 64_usize), (102, 96)] {
        let mut tx_data = vec![9_u8; bytes];
        tx_data[0] = marker;
        let price = ctx
            .get_data_price(DataLedger::ThirtyDay, bytes as u64)
            .await?;
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
    let h3 = ctx.mine_block().await?.height;
    let epoch_c = next_epoch_boundary(h3);
    while ctx.get_canonical_chain_height().await < epoch_c {
        ctx.mine_block().await?;
    }
    let (_, _, s1_la_c, s1_exp_c, _, _) = thirty_day_slots(&ctx, epoch_c).await?;
    assert!(
        !s1_exp_c,
        "slot 1 must still be alive at its fill epoch E_c"
    );
    assert_eq!(
        s1_la_c, epoch_c,
        "surviving as the frontier slot lets the fill refresh slot 1's clock to \
         E_c={epoch_c}"
    );

    // --- Check 3 @ E_c + 4: now fully written, slot 1 expires a full window
    //     after its last write. ---
    let tail_expiry = epoch_c + expiry_blocks;
    while ctx.get_canonical_chain_height().await < tail_expiry {
        ctx.mine_block().await?;
    }
    let (_, _, _, s1_exp_final, _, first_unexpired_final) =
        thirty_day_slots(&ctx, tail_expiry).await?;
    assert!(
        s1_exp_final,
        "once fully written, tail slot 1 must expire epoch_length after its \
         last write (E_c+{expiry_blocks}={tail_expiry})"
    );
    assert!(
        first_unexpired_final > 1,
        "first-unexpired ThirtyDay slot should advance past 1 once the filled \
         tail expires (got {first_unexpired_final})"
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

    // === Activate Cascade mid-chain via stop -> set activation = tip+1 -> restart ===
    // Anchor activation to the chain tip's timestamp, NOT wall-clock: tip + 1s is
    // strictly after every pre-restart block (`cascade_at` activates on inclusive >=),
    // so historical blocks stay pre-activation and only newly-mined blocks are
    // cascade-active. Chain-derived, so it's immune to a backward CLOCK_REALTIME lurch
    // that could make `now()` land at/before the tip (see block_producer::current_timestamp).
    let tip_height = node.get_canonical_chain_height().await;
    let activation_timestamp = node
        .get_block_by_height(tip_height)
        .await?
        .timestamp_secs()
        .as_secs()
        + 1;
    let mut stopped = node.stop().await;
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
    let parent_snap = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        tree.get_epoch_snapshot(&parent_block.block_hash)
            .expect("parent epoch snapshot")
    };
    let mut fee_set = {
        // cascade_active=true: this scenario runs post-activation (cascade
        // activated at timestamp 0), matching the producer/validator gate.
        parent_snap
            .get_expiring_partition_info(e, DataLedger::ThirtyDay, e_total, true)
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

    // Third leg: the inclusive non-promotability set (get_all_expired_...) must
    // contain every slot that actually recycled at E, and may exceed it only by
    // slots already is_expired from a prior epoch. Pins all three expiry sets on one
    // block — the property the fully-written-gate fix guarantees by feeding all three
    // the same write-frontier inputs. Uses the snapshot wrapper (chunks_per_slot is
    // supplied internally).
    let blocked: std::collections::BTreeSet<usize> = parent_snap
        .get_all_expired_term_slot_indexes(DataLedger::ThirtyDay, e, true, e_total)
        .into_iter()
        .collect();
    let recycled: std::collections::BTreeSet<usize> = actual_set.iter().copied().collect();
    assert!(
        recycled.is_subset(&blocked),
        "every recycled slot must be in the non-promotability set: recycled={recycled:?} blocked={blocked:?}"
    );
    let parent_slots = parent_snap.ledgers.get_slots(DataLedger::ThirtyDay);
    for extra in blocked.difference(&recycled) {
        assert!(
            parent_slots[*extra].is_expired,
            "blocked slot {extra} not in the recycle set must be a previously-expired slot"
        );
    }

    ctx.stop().await;
    Ok(())
}

/// Closes the refund side of the fee/recycle alignment with the same rigor as
/// the distribution side: a Submit slot RESCUED at its expiry epoch must emit
/// neither a `TermFeeReward` nor a `PermFeeRefund` that round, and the refund
/// must instead land when the slot ACTUALLY recycles later (deferred, not lost,
/// and never doubled).
///
/// Scenario (nbe=2, submit_ledger_epoch_length=2 -> window=4, C=10):
/// 1. Epoch L: post one UNPROMOTED publish-data tx (6 chunks; chunks never
///    uploaded) -> lands in Submit slot 0, allocates empty slots so slot 0 is
///    non-last. slot 0 last_height=L; it holds an unpromoted tx (refund-eligible).
/// 2. Idle the full window so slot 0 ages to its expiry epoch E = L + 4.
/// 3. Epoch E: post another (unpromoted) tx into slot 0 -> touch rescues it.
///    Assert the epoch block at E has ZERO TermFeeReward and ZERO PermFeeRefund:
///    the rescued slot is neither settled nor refunded.
/// 4. Epoch F: post a tx that fills slot 0 exactly (10/10 chunks). Only
///    fully-written slots may recycle — a partially written frontier slot stays
///    live so later appends never land in an expired slot.
/// 5. Epoch F + 4: slot 0 (last write F) finally recycles -> the deferred
///    PermFeeRefund for its unpromoted txs lands here.
#[test_log::test(tokio::test)]
async fn heavy_cascade_rescued_slot_defers_reward_and_refund() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let submit_ledger_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let window = submit_ledger_epoch_length * num_blocks_in_epoch; // 4

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
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

    // Counts (TermFeeReward, PermFeeRefund) shadow txs in the block at `height`.
    async fn expiry_shadow_counts(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        height: u64,
    ) -> eyre::Result<(usize, usize)> {
        let block = ctx.get_block_by_height(height).await?;
        let evm = ctx.wait_for_evm_block(block.evm_block_hash, 30).await?;
        let (mut rewards, mut refunds) = (0_usize, 0_usize);
        for tx in evm.body.transactions {
            let mut input = tx.input().as_ref();
            if let Ok(shadow) = ShadowTransaction::decode(&mut input)
                && let Some(packet) = shadow.as_v1()
            {
                match packet {
                    TransactionPacket::TermFeeReward(_) => rewards += 1,
                    TransactionPacket::PermFeeRefund(_) => refunds += 1,
                    _ => {}
                }
            }
        }
        Ok((rewards, refunds))
    }

    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    // Epoch L: one unpromoted 6-chunk publish tx -> Submit slot 0 (non-last).
    let anchor = ctx.get_anchor().await?;
    let tx1 = ctx
        .post_data_tx(anchor, vec![1_u8; 6 * chunk_size as usize], &signer)
        .await;
    ctx.wait_for_mempool(tx1.header.id, 30).await?;
    let l = next_epoch_boundary(ctx.mine_block().await?.height);
    while ctx.get_canonical_chain_height().await < l {
        ctx.mine_block().await?;
    }

    // Idle through the window so slot 0 ages to its expiry epoch E = L + window.
    let e = l + window;
    while ctx.get_canonical_chain_height().await < e - 1 {
        ctx.mine_block().await?;
    }

    // Resume in epoch E: a second (unpromoted) tx into slot 0 -> rescued.
    let anchor = ctx.get_anchor().await?;
    let tx2 = ctx
        .post_data_tx(anchor, vec![2_u8; chunk_size as usize], &signer)
        .await;
    ctx.wait_for_mempool(tx2.header.id, 30).await?;
    while ctx.get_canonical_chain_height().await < e {
        ctx.mine_block().await?;
    }

    // Phase A: the rescued slot must NOT be settled or refunded this epoch.
    let (rewards_at_e, refunds_at_e) = expiry_shadow_counts(&ctx, e).await?;
    assert_eq!(
        rewards_at_e, 0,
        "rescued Submit slot must not emit a TermFeeReward at its (skipped) expiry epoch E={e}"
    );
    assert_eq!(
        refunds_at_e, 0,
        "rescued Submit slot must not emit a PermFeeRefund at its (skipped) expiry epoch E={e}"
    );

    // Fill slot 0 exactly (chunks 7-9; total 10/10) in epoch F: only a
    // fully-written slot may recycle, and the fill refreshes its clock to F.
    let anchor = ctx.get_anchor().await?;
    let tx3 = ctx
        .post_data_tx(anchor, vec![3_u8; 3 * chunk_size as usize], &signer)
        .await;
    ctx.wait_for_mempool(tx3.header.id, 30).await?;
    let f = next_epoch_boundary(ctx.mine_block().await?.height);
    while ctx.get_canonical_chain_height().await < f {
        ctx.mine_block().await?;
    }

    // The fill must have landed: slot 0 survived the rescue window and its
    // expiry clock now counts from F — otherwise the recycle assertion below
    // would fail obliquely.
    {
        let block = ctx.get_block_by_height(f).await?;
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist");
        let slot0 = &snapshot.ledgers.get_slots(DataLedger::Submit)[0];
        assert!(!slot0.is_expired, "slot 0 must still be alive at F={f}");
        assert_eq!(
            slot0.last_height, f,
            "the fill must refresh slot 0's expiry clock to F={f}"
        );
    }

    // Phase B: when the slot ACTUALLY recycles (F + window), the deferred refund
    // for its unpromoted txs lands — proving the refund was deferred, not lost.
    let recycle = f + window;
    while ctx.get_canonical_chain_height().await < recycle {
        ctx.mine_block().await?;
    }
    let (_, refunds_at_recycle) = expiry_shadow_counts(&ctx, recycle).await?;
    assert!(
        refunds_at_recycle >= 1,
        "deferred PermFeeRefund for the unpromoted txs must land when slot 0 \
         actually recycles at F+{window}={recycle} (got {refunds_at_recycle})"
    );

    ctx.stop().await;
    Ok(())
}

/// Pre-Cascade replay-identity guard for the Submit-ledger expiry-fee path.
///
/// The write-window exclusion in `get_expiring_partition_info` mirrors the
/// `last_height` touch, which is Cascade-gated. But the Submit fee path runs
/// unconditionally (Submit is always live), so the exclusion MUST be gated on
/// the block's Cascade status too. Otherwise, pre-activation, a slot that
/// actually recycles (no touch to rescue it) would be silently dropped from the
/// settled set — diverging from the original master binary and breaking
/// bit-identical replay of pre-Cascade history.
///
/// This is the pre-activation twin of
/// `heavy_cascade_expiry_fee_model_matches_actual_recycle`: there the touch
/// RESCUES the late-filled slot, so it is excluded from BOTH sets and they match
/// at "without slot 1". Here Cascade is inactive, the slot is NOT rescued, so it
/// must appear in BOTH the fee set and the actual recycle set.
///
/// Scenario (nbe=2, submit_ledger_epoch_length=2 -> window=4, C=10 chunks), with
/// NO Cascade configured:
/// 1. Epoch L: write 12 Submit chunks -> slot 0 full, slot 1 partial (frontier);
///    extra empty slots so slot 1 is non-last. Slot 1 allocated at L.
/// 2. Idle the full window so slot 1 ages to its expiry epoch E = L + 4.
/// 3. Epoch E: write again -> lands in slot 1. Pre-Cascade there is NO touch, so
///    slot 1 still expires at E (its `last_height` stays L).
///
/// At E the fee-predicted set (parent, `cascade_active=false`) must equal the
/// actual recycle set AND contain the recycled slot. The buggy unconditional
/// exclusion (`cascade_active=true`) would instead DROP that slot — demonstrated
/// explicitly so a regression in the gate fails here.
#[test_log::test(tokio::test)]
async fn heavy_pre_cascade_submit_expiry_fee_model_matches_actual_recycle() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let submit_ledger_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let window = submit_ledger_epoch_length * num_blocks_in_epoch; // 4

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // Cascade intentionally NOT configured: this exercises pre-activation
        // behavior, where the last_height touch is gated off.
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

    // Post one unpromoted publish-data tx (chunks never uploaded) -> grows the
    // Submit ledger's `total_chunks`. `chunks` controls how many chunks it spans.
    async fn post_submit(
        ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        signer: &irys_types::irys::IrysSigner,
        chunk_size: u64,
        tag: u8,
        chunks: usize,
    ) -> eyre::Result<()> {
        let mut data = vec![7_u8; chunks * chunk_size as usize];
        data[0] = tag;
        let anchor = ctx.get_anchor().await?;
        let tx = ctx.post_data_tx(anchor, data, signer).await;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
        Ok(())
    }

    // Get past genesis epoch processing into a steady state.
    while ctx.get_canonical_chain_height().await <= num_blocks_in_epoch {
        ctx.mine_block().await?;
    }

    // Epoch L: 4 txs × 3 chunks = 12 chunks -> slot 0 full, slot 1 partial frontier.
    for i in 0_u8..4 {
        post_submit(&ctx, &signer, chunk_size, i, 3).await?;
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

    // Resume exactly in epoch E: write into the (still-frontier) slot 1. No touch
    // pre-Cascade, so slot 1 still expires here.
    post_submit(&ctx, &signer, chunk_size, 200, 3).await?;
    while ctx.get_canonical_chain_height().await < e {
        ctx.mine_block().await?;
    }

    // Submit's cumulative total_chunks at the epoch block E (the value the
    // producer/validator feed to the fee calc).
    let epoch_block = ctx.get_block_by_height(e).await?;
    let e_total = epoch_block
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == DataLedger::Submit as u32)
        .map(|dl| dl.total_chunks)
        .unwrap_or(0);
    let parent_block = ctx.get_block_by_height(e - 1).await?;

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
            .filter(|p| p.ledger_id == DataLedger::Submit)
            .map(|p| p.slot_index)
            .collect::<Vec<_>>()
    };
    actual_set.sort_unstable();
    actual_set.dedup();

    // The late-filled headroom slot MUST recycle pre-Cascade (no touch rescue).
    // If this is empty the setup failed to age a non-last slot — fail loudly
    // rather than let the equality assert pass vacuously.
    assert!(
        !actual_set.is_empty(),
        "expected a Submit slot to actually recycle at E={e} pre-Cascade \
         (got empty recycle set; setup failed to age a non-last slot)"
    );

    // Fee-predicted set with the CORRECT pre-activation gate (cascade_active=false):
    // no window exclusion, so it must equal what actually recycles.
    let mut fee_set = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snap = tree
            .get_epoch_snapshot(&parent_block.block_hash)
            .expect("parent epoch snapshot");
        snap.get_expiring_partition_info(e, DataLedger::Submit, e_total, false)
            .into_iter()
            .map(|p| p.slot_index)
            .collect::<Vec<_>>()
    };
    fee_set.sort_unstable();
    fee_set.dedup();

    assert_eq!(
        fee_set, actual_set,
        "pre-Cascade: fee-distribution set (cascade_active=false) must equal the \
         actual recycle set at E={e}; dropping a recycled slot diverges from the \
         original master binary and breaks pre-Cascade replay"
    );

    // Demonstrate the divergence the gate prevents: with the exclusion
    // erroneously applied pre-activation (cascade_active=true), the late-filled
    // slot is dropped from the settled set even though it recycles.
    let mut fee_set_buggy = {
        let tree = ctx.node_ctx.block_tree_guard.read();
        let snap = tree
            .get_epoch_snapshot(&parent_block.block_hash)
            .expect("parent epoch snapshot");
        snap.get_expiring_partition_info(e, DataLedger::Submit, e_total, true)
            .into_iter()
            .map(|p| p.slot_index)
            .collect::<Vec<_>>()
    };
    fee_set_buggy.sort_unstable();
    fee_set_buggy.dedup();

    assert_ne!(
        fee_set_buggy, actual_set,
        "sanity: the unconditional exclusion (cascade_active=true) must DROP the \
         recycled slot here — this is exactly the divergence the Cascade gate \
         prevents pre-activation"
    );

    ctx.stop().await;
    Ok(())
}

/// Cascade-active last-write expiry for the PERMANENT (Publish) ledger.
///
/// `touch_active_ledger_slots` iterates `Ledgers::active_ledgers()`, which
/// includes Publish — so when `publish_ledger_epoch_length` makes perm slots
/// expirable, the last-write touch applies to them too. The term-ledger tests
/// above never exercise this (Publish data only grows via promotion), so this
/// test pins the perm path end-to-end.
///
/// Scenario (nbe=2, publish_ledger_epoch_length=4 -> window=8, perm slot C=4):
/// 1. Promote a full slot's worth of data (4 chunks) -> fills perm slot 0; the
///    epoch allocates a headroom slot 1 (empty), stamped `last_height = a1`.
/// 2. Idle an epoch (no Publish data) so the headroom slot 1 ages unchanged.
/// 3. Promote another 4 chunks -> fills slot 1 at a LATER epoch a2; the touch
///    advances slot 1's `last_height` from its allocation epoch a1 to a2 (and
///    allocates slot 2, so slot 1 is now non-last and expiry-eligible).
/// 4. At a1 + window the slot WOULD expire if anchored to its allocation epoch;
///    with last-write anchoring it survives because its last write was a2 > a1.
///
/// On `master` (touch gated off / absent) slot 1 keeps `last_height = a1` and
/// both the step-3 advance and the step-4 retention assertions fail.
#[test_log::test(tokio::test)]
async fn heavy_cascade_publish_last_write_expiry() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let activation_height = num_blocks_in_epoch;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let publish_ledger_epoch_length = 4_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 4_u64; // perm slot capacity = 4 chunks
    let window = publish_ledger_epoch_length * num_blocks_in_epoch; // 8
    let full_slot_bytes = (num_chunks_in_partition * chunk_size) as usize; // fills one slot

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.epoch.publish_ledger_epoch_length = Some(publish_ledger_epoch_length);
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.num_chunks_in_recall_range = 1;
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
    let node = test_node.start_and_wait_for_packing("test", 30).await;

    // Post and fully promote one publish-data tx (Submit -> Publish), growing the
    // Publish ledger's `total_chunks` by `bytes`-worth of chunks.
    async fn promote_publish(
        node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
        signer: &irys_types::irys::IrysSigner,
        bytes: usize,
        tag: u8,
    ) -> eyre::Result<()> {
        let mut data = vec![7_u8; bytes];
        data[0] = tag;
        let anchor = node.get_anchor().await?;
        let tx = node.post_data_tx(anchor, data, signer).await;
        node.wait_for_mempool(tx.header.id, 30).await?;
        node.upload_chunks(&tx).await?;
        node.mine_block().await?; // confirm in Submit, trigger ingress-proof generation
        node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 30)
            .await?;
        node.mine_block().await?; // promote Submit -> Publish
        Ok(())
    }

    // Mine past Cascade activation so the perm/term expiry machinery is live.
    while node.get_canonical_chain_height().await <= activation_height {
        node.mine_block().await?;
    }

    // --- Batch 1: fill perm slot 0; epoch allocates headroom slot 1. ---
    promote_publish(&node, &signer, full_slot_bytes, 1).await?;
    node.mine_until_next_epoch().await?;
    let a1 = node.get_canonical_chain_height().await;
    let a1_slot1_last_height = {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slots = snapshot.ledgers.get_slots(DataLedger::Publish);
        assert!(
            slots.len() >= 2,
            "expected >=2 perm slots after batch 1 (got {})",
            slots.len()
        );
        slots[1].last_height
    };

    // --- Idle one epoch with no Publish data: headroom slot 1 must not move. ---
    node.mine_until_next_epoch().await?;

    // --- Batch 2: fill perm slot 1 at a later epoch; touch advances its clock. ---
    promote_publish(&node, &signer, full_slot_bytes, 2).await?;
    node.mine_until_next_epoch().await?;
    let a2 = node.get_canonical_chain_height().await;
    assert!(
        a2 > a1,
        "batch 2 epoch (a2={a2}) must be after batch 1 epoch (a1={a1})"
    );
    let a2_slot1_last_height = {
        let snapshot = node.get_canonical_epoch_snapshot();
        let slots = snapshot.ledgers.get_slots(DataLedger::Publish);
        assert!(
            slots.len() >= 3,
            "expected >=3 perm slots after batch 2 (got {})",
            slots.len()
        );
        slots[1].last_height
    };

    // CORE: the Cascade touch advanced the previously-allocated headroom slot to
    // its actual write epoch. On master it would keep its allocation last_height.
    assert!(
        a2_slot1_last_height > a1_slot1_last_height,
        "Cascade touch must advance perm slot 1's last_height from its allocation \
         epoch ({a1_slot1_last_height}) to its write epoch (got {a2_slot1_last_height})"
    );

    // --- Retention: at a1 + window the slot would expire if anchored to its
    //     allocation epoch; last-write anchoring keeps it alive (last write a2). ---
    let alloc_anchored_expiry = a1 + window;
    assert!(
        a2 < alloc_anchored_expiry,
        "test setup drift: batch 2 epoch (a2={a2}) must precede a1+window ({alloc_anchored_expiry})"
    );
    while node.get_canonical_chain_height().await < alloc_anchored_expiry {
        node.mine_block().await?;
    }
    let snapshot = node.get_canonical_epoch_snapshot();
    let slots = snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        slots.len() >= 3,
        "slot 1 must remain non-last (expiry-eligible) at a1+window"
    );
    assert!(
        !slots[1].is_expired,
        "perm slot 1 must still be ALIVE at a1+window ({alloc_anchored_expiry}): its \
         last write was a2={a2}, so last-write anchoring retains it (allocation-anchored \
         expiry would have recycled it here)"
    );
    drop(snapshot);

    node.stop().await;
    Ok(())
}

/// P1 regression (mid-chain Cascade + indexed term-ledger expiry): settling a
/// ThirtyDay slot that expires AFTER the data has migrated into the block index
/// must not stall the chain when Cascade was activated mid-chain.
///
/// The expiry-fee walk (`block_producer::ledger_expiry::resolve_ledger_offset_to_block`)
/// resolves each expired chunk offset against the finalized block index. On a
/// mid-chain activation the pre-activation blocks carry only Publish+Submit — no
/// ThirtyDay entry — so the index binary search probes a block with no entry for
/// the ledger. The buggy version used `BlockIndex::get_block_index_item`, which
/// errors ("Ledger ThirtyDay not found in block at height N") on that probe; the
/// error bubbles out of BOTH the producer and the validator epoch-block expiry
/// path, so the expiry epoch can neither be produced nor accepted → the chain
/// halts. The fix uses the tolerant `get_block_bounds`, which treats a missing
/// entry as `total_chunks = 0` and keeps searching.
///
/// Why the existing term-expiry tests miss it: they activate Cascade at genesis
/// (`activation_timestamp = 0`), so every block carries all four ledgers and the
/// index never holds a ThirtyDay-less item. This test activates mid-chain and
/// keeps the pre-activation phase the majority of the chain, so the binary
/// search's first probe (`latest_height / 2`) lands on a pre-activation block.
/// `block_migration_depth = 1` guarantees the ThirtyDay data is in the index
/// (not the un-migrated tree tail, whose resolution path is unaffected) by the
/// time it expires.
#[test_log::test(tokio::test)]
async fn slow_heavy_cascade_midchain_thirty_day_expiry_resolves_through_index() -> eyre::Result<()>
{
    let num_blocks_in_epoch = 2_u64;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64; // expires 2×2 = 4 blocks after last write
    let expiry_window = thirty_day_epoch_length * num_blocks_in_epoch; // 4
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // Cascade intentionally NOT configured yet — activated mid-chain below so
        // the pre-activation blocks carry only Publish+Submit (the regression).
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

    // === Pre-activation phase: keep it the MAJORITY of the chain ===
    // The index binary search's first probe is `latest_height / 2`; making the
    // pre-activation span exceed half the chain at expiry guarantees that probe
    // hits a pre-activation (ThirtyDay-less) block — the exact trigger.
    let pre_activation_target = 16_u64;
    while node.get_canonical_chain_height().await < pre_activation_target {
        node.mine_block().await?;
    }

    // === Activate Cascade mid-chain (stop -> set activation = tip+1 -> restart) ===
    // Anchor activation to the chain tip's timestamp, NOT wall-clock: tip + 1s is
    // strictly after every pre-restart block (`cascade_at` activates on inclusive >=),
    // so historical blocks stay pre-activation and only newly-mined blocks are
    // cascade-active. Chain-derived, so it's immune to a backward CLOCK_REALTIME lurch
    // that could make `now()` land at/before the tip (see block_producer::current_timestamp).
    let tip_height = node.get_canonical_chain_height().await;
    let activation_timestamp = node
        .get_block_by_height(tip_height)
        .await?
        .timestamp_secs()
        .as_secs()
        + 1;
    let mut stopped = node.stop().await;
    stopped.cfg.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
        one_year_epoch_length,
        thirty_day_epoch_length,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
    let node = stopped.start().await;

    // Mine a couple of cascade-active epochs so activate_cascade runs and the
    // OneYear/ThirtyDay ledgers come into existence.
    for _ in 0..(2 * num_blocks_in_epoch) {
        node.mine_block().await?;
    }

    // === Post-activation: write 12 chunks of ThirtyDay data ===
    // 4 txs × 3 chunks = 12 chunks → slot 0 (0-9) fills and slot 1 (10-11) starts,
    // so slot 0 is non-last and therefore eligible to expire.
    for i in 0_u8..4 {
        let mut tx_data = vec![9_u8; 96]; // 96 / 32 = 3 chunks
        tx_data[0] = i;
        let price = node.get_data_price(DataLedger::ThirtyDay, 96).await?;
        let tx = signer.create_transaction_with_fees(
            tx_data,
            node.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = signer.sign_transaction(tx)?;
        node.ingest_data_tx(tx.header.clone()).await?;
        node.wait_for_mempool(tx.header.id, 30).await?;
    }
    let write_height = node.mine_block().await?.height;
    let write_epoch = next_epoch_boundary(write_height);
    while node.get_canonical_chain_height().await < write_epoch {
        node.mine_block().await?;
    }

    // Sanity: the ThirtyDay ledger now holds data across ≥2 slots, so slot 0 is
    // non-last and the data is in the migrated index (depth = 1).
    {
        let block = node.get_block_by_height(write_epoch).await?;
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        assert!(
            slots.len() >= 2 && !slots[0].is_expired,
            "ThirtyDay slot 0 must be non-last and not yet expired (slots={}, slot0_expired={})",
            slots.len(),
            slots.first().map(|s| s.is_expired).unwrap_or(false),
        );
    }

    // === Mine through the expiry epoch ===
    // At `write_epoch + expiry_window` the producer settles ThirtyDay slot 0,
    // walking `find_block_range(ThirtyDay)` → `resolve_ledger_offset_to_block`
    // → the index binary search that probes a pre-activation block. Without the
    // fix `mine_block` errors here ("Ledger ThirtyDay not found …") and the chain
    // cannot advance past this epoch.
    let expiry_epoch = write_epoch + expiry_window;
    while node.get_canonical_chain_height().await < expiry_epoch {
        node.mine_block().await?;
    }

    assert!(
        node.get_canonical_chain_height().await >= expiry_epoch,
        "chain must advance through the ThirtyDay expiry epoch without stalling"
    );

    // Settlement actually ran: slot 0 recycled (is_expired) at the expiry epoch.
    {
        let block = node.get_block_by_height(expiry_epoch).await?;
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.block_hash)
            .expect("epoch snapshot should exist");
        let slots = snapshot.ledgers.get_slots(DataLedger::ThirtyDay);
        assert!(
            slots[0].is_expired,
            "ThirtyDay slot 0 must be expired/recycled at the expiry epoch (proves the \
             indexed expiry-fee walk completed)"
        );
    }

    node.stop().await;
    Ok(())
}

/// NC-0042 P1 (clean-cutover, mixed-mode): a Submit tx **included before Cascade
/// activation** must be correctly perm-fee-refunded when its slot **expires after
/// activation**, and the chain must progress past that expiry epoch without a
/// §4c-induced halt. The §4c rejection + the refund/fee algorithm are
/// intentionally NOT Cascade-gated (see the CLEAN-CUTOVER note above the §4c block
/// in `block_validation`), so this exercises them across the pre→post activation
/// boundary — the coverage the all-pre-Cascade (`perm_refund`) and no-expiry
/// midchain (`..._submit_last_height_transition`) tests each leave open.
///
/// A single unpromoted 16-chunk tx (512B / 32B) spans slots 0..3 with a 4-chunk
/// partition, so slot 0 is non-last (expirable) and the tx is attributed to it.
/// We activate Cascade mid-chain BEFORE slot 0's expiry, then mine until the
/// refund shadow-tx appears — the expiry height is found data-driven because the
/// allocation→fill anchoring transition at activation can shift it.
#[test_log::test(tokio::test)]
async fn slow_heavy_cascade_midchain_submit_expiry_refunds_across_activation() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let submit_ledger_epoch_length = 2_u64;
    let one_year_epoch_length = 8_u64;
    let thirty_day_epoch_length = 2_u64;
    let chunk_size = 32_u64;
    // Small partition so one 16-chunk tx spans several slots ⇒ slot 0 is non-last.
    let num_chunks_in_partition = 4_u64;
    let data_size = 512_usize;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        // Cascade intentionally NOT configured yet — activated mid-chain below.
    });

    let user = config.new_random_signer();
    let user_addr = user.address();
    config.fund_genesis_accounts(vec![&user]);

    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 8)?;
    let node = test_node
        .start_and_wait_for_packing("midchain_expiry", 30)
        .await;

    // === Pre-Cascade: include one UNPROMOTED Submit tx (no chunks uploaded → never
    // promoted → refundable). Its 16 chunks span slots 0..3, so slot 0 is non-last
    // and will expire; the tx is attributed to slot 0 by its start offset. ===
    let anchor = node.get_block_by_height(0).await?.block_hash;
    let tx = node
        .post_data_tx(anchor, vec![1_u8; data_size], &user)
        .await;
    let tx_perm_fee = tx.header.perm_fee.expect("data tx must carry a perm_fee");
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.mine_block().await?;
    // One epoch so the slot allocation settles while still pre-Cascade.
    node.mine_until_next_epoch().await?;
    let pre_activation_height = node.get_canonical_chain_height().await;

    // === Activate Cascade mid-chain: stop → set activation = tip+1 → restart. Anchor
    // to the chain tip's timestamp (NOT wall-clock): tip + 1s is strictly after every
    // pre-restart block (`cascade_at` activates on inclusive >=), so historical blocks
    // stay pre-activation and only newly mined blocks are cascade-active — deterministic
    // and immune to a backward CLOCK_REALTIME lurch (see block_producer::current_timestamp). ===
    let tip_height = node.get_canonical_chain_height().await;
    let activation_timestamp = node
        .get_block_by_height(tip_height)
        .await?
        .timestamp_secs()
        .as_secs()
        + 1;
    let mut stopped = node.stop().await;
    stopped.cfg.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
        one_year_epoch_length,
        thirty_day_epoch_length,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
    let node = stopped.start().await;

    // === Mine post-activation until slot 0 expires and `tx` is refunded. The
    // expiry epoch is detected data-driven (scan each epoch block's shadow txs for
    // a PermFeeRefund to the user) rather than hard-coded, since the anchoring
    // transition can move it. ===
    let mut refund_amount = U256::from(0);
    let mut refund_height = 0_u64;
    const MAX_EPOCHS: usize = 16;
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

    // The refund settled AFTER activation (mixed-mode: pre-Cascade inclusion,
    // post-activation expiry) — the §4c/refund path ran cascade-active over a
    // pre-activation tx and did not halt.
    assert!(
        refund_height > pre_activation_height,
        "refund must settle after Cascade activation (refund_height={refund_height}, \
         pre_activation_height={pre_activation_height})"
    );
    assert_eq!(
        refund_amount, tx_perm_fee,
        "pre-Cascade-included unpromoted tx must be refunded its full perm_fee at the \
         post-activation expiry"
    );
    // Chain kept progressing past the expiry epoch (no §4c-induced stall).
    assert!(
        node.get_canonical_chain_height().await >= refund_height,
        "chain must progress past the post-activation expiry epoch"
    );

    node.stop().await;
    Ok(())
}
