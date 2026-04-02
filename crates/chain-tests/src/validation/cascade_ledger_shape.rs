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
