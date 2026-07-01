use crate::utils::{AddTxError, IrysNodeTest};
use irys_actors::mempool_service::TxIngressError;
use irys_testing_utils::initialize_tracing;
use irys_types::{CommitmentTransaction, DataLedger, NodeConfig, irys::IrysSigner};

/// Commitments must be validated against `commitment_anchor_expiry_depth` (the
/// longer commitment window), while data txs stay gated by the shorter
/// `tx_anchor_expiry_depth`. `NodeConfig::testing()` sets tx depth = 20 and
/// commitment depth = 100, so an anchor ~26 blocks deep is too old for a data
/// tx but still valid for a commitment.
#[test_log::test(tokio::test)]
async fn heavy_commitment_accepts_old_anchor_data_tx_rejects() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor, then mine past the tx window (20) but stay
    // within the commitment window (100).
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(25).await?;

    // Commitment (stake) anchored at the old block: accepted.
    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx)
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    // Data tx anchored at the same old block: rejected (too old for the
    // shorter tx window).
    let price_info = node.get_data_price(DataLedger::Publish, 32).await?;
    let data_tx = signer.create_publish_transaction(
        vec![7_u8; 32],
        old_anchor,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = signer.sign_transaction(data_tx)?;

    let result = node.ingest_data_tx(data_tx.header).await;
    match result {
        Err(AddTxError::TxIngress(err)) => {
            assert!(
                matches!(err, TxIngressError::InvalidAnchor(_)),
                "expected InvalidAnchor, got {err:?}"
            );
        }
        other => panic!("expected data tx to be rejected with InvalidAnchor, got {other:?}"),
    }

    node.stop().await;
    Ok(())
}

/// Mempool pruning (not just ingress) must also use the commitment window:
/// once a commitment is accepted with an old-but-in-window anchor, later
/// block confirmations must not prune it until its anchor age exceeds
/// `commitment_anchor_expiry_depth`, even though the same anchor age would
/// already exceed the (shorter) `tx_anchor_expiry_depth` used for data txs.
/// `NodeConfig::testing()`: tx depth = 20, commitment depth = 100,
/// block_migration_depth = 6 -> effective data expiry = 20+6+5 = 31,
/// effective commitment expiry = 100+6+5 = 111. An anchor age of 40 is past
/// the former but well within the latter.
#[test_log::test(tokio::test)]
async fn heavy_commitment_survives_pruning_past_data_tx_window() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    let old_anchor = node.mine_block().await?.block_hash;

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    // Mine past the data-tx effective expiry (31) but stay within the
    // commitment effective expiry (111). Each mined block confirms the
    // previous one, which drives a `prune_pending_txs` cycle.
    node.mine_blocks(40).await?;

    node.wait_for_mempool_commitment_txs(vec![stake_tx.id()], 10)
        .await
        .expect("commitment must survive pruning within its longer window");

    node.stop().await;
    Ok(())
}
