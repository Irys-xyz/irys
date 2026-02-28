use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{hardfork_config::Cascade, BoundedFee, DataLedger, NodeConfig, UnixTimestamp};

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
            Some(BoundedFee::default()),
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
            Some(BoundedFee::default()),
        )?;
        let tx = signer.sign_transaction(tx)?;
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }

    // Mine a block to include all the term txs
    let inclusion_block = ctx.mine_block().await?;
    let inclusion_height = inclusion_block.height;

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
