use std::sync::Arc;

use crate::utils::{assert_validation_error, solution_context, IrysNodeTest};
use crate::validation::send_block_and_read_state;
use eyre::WrapErr as _;
use irys_actors::block_validation::ValidationError;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::SendTraced as _;
use irys_types::{CommitmentTransaction, NodeConfig, PledgeDataProvider as _};
use tokio::sync::oneshot;
use tracing::debug;

async fn gossip_commitment_to_node(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    commitment: &CommitmentTransaction,
) -> eyre::Result<()> {
    let (resp_tx, resp_rx) = oneshot::channel();
    node.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::IngestCommitmentTxFromGossip(commitment.clone(), resp_tx),
    )?;

    match resp_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            debug!(
                tx.id = ?commitment.id(),
                tx.err = ?err,
                "Commitment gossip rejected by mempool"
            );
        }
        Err(recv_err) => {
            debug!(
                tx.id = ?commitment.id(),
                tx.err = ?recv_err,
                "Commitment gossip channel dropped"
            );
        }
    }
    Ok(())
}

/// Test scenario where a malicious block producer attempts to include an unstake commitment
/// for an account that still has active pledges.
/// Expected behavior: Block validation must reject the block because unstake commitments
/// are invalid when the account has pledge_count > 0.
#[test_log::test(tokio::test)]
async fn heavy_block_unstake_with_active_pledges_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_unstake: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &irys_types::IrysBlockHeader,
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let invalid_unstake = self.invalid_unstake.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_unstake.clone()],
                commitment_txs_to_bill: vec![invalid_unstake],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
            })
        }
    }

    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = genesis_node
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await
        .wrap_err("failed to bootstrap peer with assignments")?;

    let peer_addr = peer_signer.address();

    // Verify peer has active pledges
    let peer_assignments = genesis_node.get_partition_assignments(peer_addr);
    eyre::ensure!(
        !peer_assignments.is_empty(),
        "peer must have at least one partition assignment (active pledge)"
    );

    let pledge_count = peer_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(peer_addr)
        .await;
    eyre::ensure!(
        pledge_count > 0,
        "peer must have pledge_count > 0 for this test"
    );

    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    // Create an unstake commitment for a peer that still has active pledges (invalid!)
    let mut invalid_unstake = CommitmentTransaction::new_unstake(consensus_config, anchor);
    invalid_unstake.set_signer(peer_addr);
    peer_signer.sign_commitment(&mut invalid_unstake)?;

    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_unstake: invalid_unstake.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    // Gossip the commitment to all nodes
    for node in [&genesis_node, &peer_node] {
        gossip_commitment_to_node(node, &invalid_unstake).await?;
    }

    // Send block to genesis node for validation
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        genesis_outcome,
        |e| {
            matches!(
                e,
                ValidationError::CommitmentSnapshotRejected {
                    status: irys_domain::CommitmentSnapshotStatus::HasActivePledges,
                    ..
                }
            )
        },
        "Expected CommitmentSnapshotRejected with HasActivePledges status",
    );

    // Send block to peer node for validation
    let peer_outcome =
        send_block_and_read_state(&peer_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        peer_outcome,
        |e| {
            matches!(
                e,
                ValidationError::CommitmentSnapshotRejected {
                    status: irys_domain::CommitmentSnapshotStatus::HasActivePledges,
                    ..
                }
            )
        },
        "Expected CommitmentSnapshotRejected with HasActivePledges status",
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

/// Test scenario where a malicious block producer attempts to include an unstake commitment
/// for an account that was never staked (never had a stake commitment).
/// Expected behavior: Block validation must reject the block because unstake commitments
/// are invalid when the account has no stake in the epoch snapshot.
#[test_log::test(tokio::test)]
async fn heavy3_block_unstake_never_staked_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_unstake: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &irys_types::IrysBlockHeader,
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let invalid_unstake = self.invalid_unstake.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_unstake.clone()],
                commitment_txs_to_bill: vec![invalid_unstake],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
            })
        }
    }

    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a user that will never stake
    let never_staked_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&never_staked_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let never_staked_addr = never_staked_signer.address();

    // Verify the user has balance but no stake
    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance = genesis_node
        .get_balance(never_staked_addr, head_block.evm_block_hash)
        .await;
    eyre::ensure!(
        balance > irys_types::U256::from(0_u64),
        "user must have balance (was funded in genesis)"
    );

    // Verify user is NOT in the epoch snapshot stake commitments
    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    eyre::ensure!(
        !epoch_snapshot
            .commitment_state
            .stake_commitments
            .contains_key(&never_staked_addr),
        "user must not be staked (never submitted stake commitment)"
    );

    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    // Create an unstake commitment for a user that was never staked (invalid!)
    let mut invalid_unstake = CommitmentTransaction::new_unstake(consensus_config, anchor);
    invalid_unstake.set_signer(never_staked_addr);
    never_staked_signer.sign_commitment(&mut invalid_unstake)?;

    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_unstake: invalid_unstake.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    // Gossip the commitment to the node
    gossip_commitment_to_node(&genesis_node, &invalid_unstake).await?;

    // Send block to genesis node for validation
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        outcome,
        |e| {
            matches!(
                e,
                ValidationError::CommitmentSnapshotRejected {
                    status: irys_domain::CommitmentSnapshotStatus::Unstaked,
                    ..
                }
            )
        },
        "Expected CommitmentSnapshotRejected with Unstaked status",
    );

    genesis_node.stop().await;

    Ok(())
}
