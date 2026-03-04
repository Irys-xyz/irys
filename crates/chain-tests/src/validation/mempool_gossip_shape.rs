use crate::utils::IrysNodeTest;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_types::{CommitmentTransaction, NodeConfig, SendTraced as _};
use tokio::sync::oneshot;

// Validate that gossip ingress rejects commitments with wrong value and marks them invalid.
#[test_log::test(tokio::test)]
async fn heavy_gossip_rejects_commitment_with_wrong_value_and_blacklists() -> eyre::Result<()> {
    // Setup a single node
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Build a Stake commitment with an incorrect value
    let consensus = &genesis_node.cfg.consensus_config();
    let anchor = genesis_node.get_anchor().await?;
    let mut tx = CommitmentTransaction::new_stake(consensus, anchor);
    tx.set_value(consensus.stake_value.amount.saturating_add(1.into())); // wrong value
    signer.sign_commitment(&mut tx)?;

    // Gossip it and expect an error
    let (resp_tx, resp_rx) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(tx.clone(), resp_tx),
    )?;

    // Should error due to InvalidStakeValue
    let res = resp_rx.await.expect("mempool responded");
    assert!(res.is_err(), "expected gossip to reject invalid value");

    // It should not be present in the mempool
    let (exists_tx, exists_rx) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::CommitmentTxExists(tx.id(), exists_tx),
    )?;
    let exists = exists_rx
        .await
        .expect("mempool responded")
        .unwrap()
        .is_known_and_valid();
    assert!(!exists, "invalid-value tx must not be stored in mempool");

    // Re-gossip same tx; precheck should now skip due to recent_invalid_tx marking
    let (resp_tx2, resp_rx2) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(tx.clone(), resp_tx2),
    )?;
    let res2 = resp_rx2.await.expect("mempool responded");
    assert!(res2.is_err(), "expected gossip to skip already invalid tx");

    // Teardown
    genesis_node.stop().await;
    Ok(())
}

// Validate that gossip ingress rejects commitments with insufficient fee and marks them invalid.
#[test_log::test(tokio::test)]
async fn heavy_gossip_rejects_commitment_with_low_fee_and_blacklists() -> eyre::Result<()> {
    // Setup a single node
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Build a Stake commitment with a too-low fee
    let consensus = &genesis_node.cfg.consensus_config();
    let anchor = genesis_node.get_anchor().await?;
    let mut tx = CommitmentTransaction::new_stake(consensus, anchor);
    tx.set_fee(consensus.mempool.commitment_fee.saturating_sub(1)); // insufficient fee
    signer.sign_commitment(&mut tx)?;

    // Gossip it and expect an error
    let (resp_tx, resp_rx) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(tx.clone(), resp_tx),
    )?;
    let res = resp_rx.await.expect("mempool responded");
    assert!(res.is_err(), "expected gossip to reject low-fee commitment");

    // It should not be present in the mempool
    let (exists_tx, exists_rx) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::CommitmentTxExists(tx.id(), exists_tx),
    )?;
    let exists = exists_rx
        .await
        .expect("mempool responded")
        .unwrap()
        .is_known_and_valid();
    assert!(!exists, "low-fee tx must not be stored in mempool");

    // Re-gossip same tx; should be skipped now
    let (resp_tx2, resp_rx2) = oneshot::channel();
    genesis_node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(tx.clone(), resp_tx2),
    )?;
    let res2 = resp_rx2.await.expect("mempool responded");
    assert!(res2.is_err(), "expected gossip to skip already invalid tx");

    // Teardown
    genesis_node.stop().await;
    Ok(())
}
