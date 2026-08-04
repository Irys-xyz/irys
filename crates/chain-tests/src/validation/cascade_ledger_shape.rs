use crate::utils::IrysNodeTest;
use irys_types::{DataLedger, NodeConfig, UnixTimestamp};

/// Verify that block headers have the correct data_ledgers shape when Cascade is active.
/// With activation_timestamp=0 (active from genesis): all blocks have 4 ledgers
/// (Publish + Submit + OneYear + ThirtyDay) with correct metadata.
#[test_log::test(tokio::test)]
async fn heavy_cascade_block_header_ledger_shape_at_activation_epoch() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;

    let num_blocks_in_epoch = 4_u64;
    let config = NodeConfig::testing().with_consensus(|c| {
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // Mine a few blocks
    for _ in 0..num_blocks_in_epoch {
        ctx.mine_block().await?;
    }

    // With activation_timestamp=0, Cascade is active from genesis.
    // All blocks should have 4 data ledgers.
    for h in 1..=num_blocks_in_epoch {
        let block = ctx.get_block_by_height(h).await?;
        let ledgers = &block.data_ledgers;

        assert_eq!(
            ledgers.len(),
            4,
            "block {} should have 4 data ledgers with cascade active from genesis",
            h
        );
        let ledger_ids: Vec<u32> = ledgers.iter().map(|l| l.ledger_id).collect();
        assert_eq!(
            ledger_ids,
            vec![
                DataLedger::Publish as u32,
                DataLedger::Submit as u32,
                DataLedger::OneYear as u32,
                DataLedger::ThirtyDay as u32,
            ],
            "block {} ledger ids mismatch",
            h
        );
    }

    // Verify metadata on the last mined block
    let block = ctx.get_block_by_height(num_blocks_in_epoch).await?;
    let ledgers = &block.data_ledgers;

    // OneYear ledger: no ingress proofs, correct expiry
    let one_year = ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::OneYear as u32)
        .expect("OneYear ledger must exist after Cascade");
    assert!(one_year.proofs.is_none(), "OneYear must not have proofs");
    assert!(
        one_year.required_proof_count.is_none(),
        "OneYear must not have required_proof_count"
    );
    assert_eq!(one_year.expires, Some(365));

    // ThirtyDay ledger: no ingress proofs, correct expiry
    let thirty_day = ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::ThirtyDay as u32)
        .expect("ThirtyDay ledger must exist after Cascade");
    assert!(
        thirty_day.proofs.is_none(),
        "ThirtyDay must not have proofs"
    );
    assert!(
        thirty_day.required_proof_count.is_none(),
        "ThirtyDay must not have required_proof_count"
    );
    assert_eq!(thirty_day.expires, Some(30));

    // Publish ledger: permanent (no expiry)
    let publish = ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .expect("Publish ledger must exist");
    assert!(publish.expires.is_none(), "Publish must not expire");

    ctx.stop().await;
    Ok(())
}

/// Mid-chain Cascade activation must leave the term ledgers with slots and
/// partition assignments in the epoch that activates them.
///
/// The activation epoch block's own header carries only Publish + Submit: the
/// producer derives the header ledger set from the parent epoch snapshot, which
/// still predates the activation. Slot allocation skips ledgers absent from the
/// header, so before Delta the term ledgers stayed slotless — no partitions, no
/// miner storing them — for a full epoch, while blocks in that epoch already
/// accepted term data.
///
/// Activation is performed via stop -> set `cascade.activation_timestamp` from
/// the chain tip -> restart, so replayed history stays pre-activation and only
/// newly mined epoch blocks are cascade-active (no wall-clock race).
#[test_log::test(tokio::test)]
async fn heavy_cascade_midchain_activation_seeds_term_ledger_slots() -> eyre::Result<()> {
    use irys_config::submodules::StorageSubmodulesConfig;
    use irys_types::hardfork_config::{Cascade, Delta};

    let num_blocks_in_epoch = 2_u64;
    let config = NodeConfig::testing().with_consensus(|c| {
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        // Delta is active for the whole chain; it only has an effect in an epoch
        // that activates a ledger.
        c.hardforks.delta = Some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(1),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
        });
        // Cascade intentionally NOT configured yet — activated mid-chain below.
    });

    // 5 submodules: 2 partitions for the genesis ledgers, the rest available for
    // the term ledger slots seeded at activation.
    let test = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test.cfg.base_directory.clone(), 5)?;
    let node = test.start_and_wait_for_packing("GENESIS", 20).await;

    // Mine to an epoch boundary while pre-Cascade.
    while node.get_canonical_chain_height().await < num_blocks_in_epoch {
        node.mine_block().await?;
    }
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
        one_year_epoch_length: 365,
        thirty_day_epoch_length: 30,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
    let node = stopped.start().await;

    // Advance epoch by epoch until one lands at or after the activation
    // timestamp: that is the activation epoch block, and we assert on it before
    // any later epoch can paper over the gap. Driven by the mined block's own
    // timestamp rather than an assumed height, so a restart that finishes inside
    // the same wall-clock second cannot mis-identify the boundary.
    let activation_epoch = loop {
        let boundary = super::next_epoch_boundary(
            node.get_canonical_chain_height().await + 1,
            num_blocks_in_epoch,
        );
        while node.get_canonical_chain_height().await < boundary {
            node.mine_block().await?;
        }
        if node
            .get_block_by_height(boundary)
            .await?
            .timestamp_secs()
            .as_secs()
            >= activation_timestamp
        {
            break boundary;
        }
    };

    let epoch_block = node.get_block_by_height(activation_epoch).await?;
    assert_eq!(
        epoch_block.data_ledgers.len(),
        2,
        "the activation epoch block's header carries only the pre-activation \
         ledger set — this is the condition Delta compensates for"
    );

    let snapshot = {
        let tree = node.node_ctx.block_tree_guard.read();
        tree.get_epoch_snapshot(&epoch_block.block_hash)
            .expect("epoch snapshot should exist for the activation epoch block")
    };
    assert_eq!(
        snapshot.ledgers.active_ledgers().len(),
        4,
        "cascade must activate the term ledgers at this epoch boundary"
    );
    for ledger in [DataLedger::OneYear, DataLedger::ThirtyDay] {
        let slots = snapshot.ledgers.get_slots(ledger);
        assert_eq!(
            slots.len(),
            1,
            "{ledger:?} must be seeded with a slot in its activation epoch"
        );
        assert_eq!(
            slots[0].partitions.len() as u64,
            node.node_ctx.config.consensus.num_partitions_per_slot,
            "{ledger:?} slot 0 must have its partitions assigned in the same epoch"
        );
    }

    node.stop().await;
    Ok(())
}

/// Regression test for the 2026-06-11 devnet incident: a positive Cascade
/// activation timestamp at or before the genesis timestamp must yield a
/// genesis header with all four data ledgers, and a node hosting term-ledger
/// partitions must mine past block_migration_depth — the point where the
/// migrated-block pointer reaches genesis — without the chunk orchestrator
/// panicking.
#[test_log::test(tokio::test)]
async fn heavy_cascade_pre_genesis_activation_active_from_genesis() -> eyre::Result<()> {
    use irys_config::submodules::StorageSubmodulesConfig;
    use irys_types::hardfork_config::Cascade;

    let seconds_to_wait = 20;
    let config = NodeConfig::testing().with_consensus(|c| {
        c.hardforks.cascade = Some(Cascade {
            // Non-zero and earlier than any realistic genesis timestamp —
            // the devnet encoding of "active from genesis" that previously
            // produced a two-ledger genesis header.
            activation_timestamp: UnixTimestamp::from_secs(1),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    // Pre-configure 5 storage submodules so there are enough partitions for
    // all 4 data ledgers (Publish, Submit, OneYear, ThirtyDay).
    let test = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test.cfg.base_directory.clone(), 5)?;
    let ctx = test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // The genesis header must carry all four data ledgers.
    let genesis_block = ctx.get_block_by_height(0).await?;
    let ledger_ids: Vec<u32> = genesis_block
        .data_ledgers
        .iter()
        .map(|l| l.ledger_id)
        .collect();
    assert_eq!(
        ledger_ids,
        vec![
            DataLedger::Publish as u32,
            DataLedger::Submit as u32,
            DataLedger::OneYear as u32,
            DataLedger::ThirtyDay as u32,
        ],
        "genesis header must contain all four data ledgers"
    );

    // The node must own term-ledger partition assignments, so its term-ledger
    // chunk orchestrators actually run (regression precondition).
    let miner_address = ctx.node_ctx.config.node_config.miner_address();
    let assignments = ctx.get_partition_assignments(miner_address);
    for ledger in [DataLedger::OneYear, DataLedger::ThirtyDay] {
        assert!(
            assignments
                .iter()
                .any(|pa| pa.ledger_id == Some(ledger as u32)),
            "node must own a {ledger:?} partition assignment"
        );
    }

    // Mine past block_migration_depth so the migrated-block pointer crosses
    // the genesis block — the exact window where the devnet node panicked.
    let depth = ctx.node_ctx.config.consensus.block_migration_depth as u64;
    for h in 1..=depth + 1 {
        ctx.mine_block().await?;
        let block = ctx.get_block_by_height(h).await?;
        assert_eq!(
            block.data_ledgers.len(),
            4,
            "block {h} must carry four data ledgers"
        );
    }

    // Reaching this point without an abort is the core regression assertion.
    ctx.stop().await;
    Ok(())
}
