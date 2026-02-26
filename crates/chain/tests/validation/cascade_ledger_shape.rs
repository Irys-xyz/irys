use crate::utils::IrysNodeTest;
use irys_types::{DataLedger, NodeConfig};

/// Verify that block headers have the correct data_ledgers shape before and after
/// Cascade hardfork activation.
/// Pre-Cascade: 2 ledgers (Publish + Submit).
/// Post-Cascade: 4 ledgers (Publish + Submit + OneYear + ThirtyDay) with correct metadata.
#[test_log::test(tokio::test)]
async fn heavy_cascade_block_header_ledger_shape_at_activation_epoch() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;

    let activation_height = 4_u64;

    let num_blocks_in_epoch = 4_u64;
    let config = NodeConfig::testing().with_consensus(|c| {
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.hardforks.cascade = Some(Cascade {
            activation_height,
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    for _ in 0..activation_height {
        ctx.mine_block().await?;
    }

    // Pre-Cascade blocks (1..3): exactly 2 data ledgers
    for h in 1..=3_u64 {
        let block = ctx.get_block_by_height(h).await?;
        let ledgers = &block.data_ledgers;

        assert_eq!(ledgers.len(), 2, "block {} should have 2 data ledgers", h);
        let ledger_ids: Vec<u32> = ledgers.iter().map(|l| l.ledger_id).collect();
        assert_eq!(
            ledger_ids,
            vec![DataLedger::Publish as u32, DataLedger::Submit as u32],
            "block {} pre-cascade ledger ids mismatch",
            h
        );
    }

    // Post-Cascade block (at activation_height): exactly 4 data ledgers
    let block = ctx.get_block_by_height(activation_height).await?;
    let ledgers = &block.data_ledgers;

    assert_eq!(
        ledgers.len(),
        4,
        "block {} should have 4 data ledgers at/after cascade",
        activation_height
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
        "post-cascade ledger ids mismatch"
    );

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
