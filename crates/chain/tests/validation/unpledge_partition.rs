use std::sync::Arc;

use crate::utils::{
    assert_validation_error, gossip_commitment_to_node, solution_context, IrysNodeTest,
};
use crate::validation::{send_block_and_read_state, send_block_to_block_tree};
use eyre::WrapErr as _;
use irys_actors::block_validation::ValidationError;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::CommitmentTypeV2;
use irys_types::{CommitmentTransaction, NodeConfig, U256};

#[test_log::test(tokio::test)]
async fn heavy4_block_unpledge_partition_not_owned_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_unpledge: CommitmentTransaction,
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
            let invalid_unpledge = self.invalid_unpledge.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_unpledge.clone()],
                commitment_txs_to_bill: vec![invalid_unpledge],
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

    let victim_signer = genesis_config.new_random_signer();
    let evil_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&victim_signer, &evil_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let victim_config = genesis_node.testing_peer_with_signer(&victim_signer);
    let victim_node = genesis_node
        .testing_peer_with_assignments_and_name(victim_config, "VICTIM")
        .await
        .wrap_err("failed to bootstrap victim peer with assignments")?;

    let evil_config = genesis_node.testing_peer_with_signer(&evil_signer);
    let evil_node = genesis_node
        .testing_peer_with_assignments_and_name(evil_config, "EVIL")
        .await
        .wrap_err("failed to bootstrap malicious peer with assignments")?;

    let victim_addr = victim_signer.address();
    let evil_addr = evil_signer.address();

    let victim_assignments = genesis_node.get_partition_assignments(victim_addr);
    eyre::ensure!(
        !victim_assignments.is_empty(),
        "victim peer must have at least one partition assignment"
    );

    let evil_assignments = genesis_node.get_partition_assignments(evil_addr);
    eyre::ensure!(
        !evil_assignments.is_empty(),
        "malicious peer must have at least one partition assignment"
    );

    let stolen_partition_hash = victim_assignments[0].partition_hash;

    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    let mut invalid_unpledge = CommitmentTransaction::new_unpledge(
        consensus_config,
        anchor,
        evil_node.node_ctx.mempool_pledge_provider.as_ref(),
        evil_addr,
        stolen_partition_hash,
    )
    .await;
    evil_signer.sign_commitment(&mut invalid_unpledge)?;

    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_unpledge: invalid_unpledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    for node in [&genesis_node, &victim_node, &evil_node] {
        let _ = gossip_commitment_to_node(node, &invalid_unpledge).await;
    }

    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        genesis_outcome,
        |e| matches!(e, ValidationError::UnpledgePartitionNotOwned { .. }),
        "genesis node should discard block with unpledge referencing unowned partition",
    );

    let victim_outcome =
        send_block_and_read_state(&victim_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        victim_outcome,
        |e| matches!(e, ValidationError::UnpledgePartitionNotOwned { .. }),
        "victim peer should also discard the malicious block",
    );

    let evil_outcome =
        send_block_and_read_state(&evil_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        evil_outcome,
        |e| matches!(e, ValidationError::UnpledgePartitionNotOwned { .. }),
        "malicious peer must not accept its own invalid block",
    );

    evil_node.stop().await;
    victim_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_block_unpledge_invalid_count_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub commitments: Vec<CommitmentTransaction>,
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
            let commitment_txs = self.commitments.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: commitment_txs.clone(),
                commitment_txs_to_bill: commitment_txs,
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

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_signer = genesis_node.cfg.signer();
    let genesis_addr = genesis_signer.address();

    let assignments = genesis_node.get_partition_assignments(genesis_addr);
    eyre::ensure!(
        assignments.len() >= 3,
        "genesis node must expose at least three partitions for invalid count test"
    );

    let consensus = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;
    let target_counts = [3_u64, 2, 4];

    let mut unpledge_txs = Vec::with_capacity(target_counts.len());
    for (idx, assignment) in assignments.iter().take(target_counts.len()).enumerate() {
        let mut tx = CommitmentTransaction::new_unpledge(
            consensus,
            anchor,
            genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
            genesis_addr,
            assignment.partition_hash,
        )
        .await;

        let count = target_counts[idx];
        tx.set_commitment_type(CommitmentTypeV2::Unpledge {
            pledge_count_before_executing: count,
            partition_hash: assignment.partition_hash,
        });
        tx.set_value(CommitmentTransaction::calculate_pledge_value_at_count(
            consensus,
            count
                .checked_sub(1)
                .ok_or_else(|| eyre::eyre!("pledge count must be greater than zero"))?,
        ));

        genesis_signer.sign_commitment(&mut tx)?;
        unpledge_txs.push(tx);
    }

    for tx in &unpledge_txs {
        let _ = gossip_commitment_to_node(&genesis_node, tx).await;
    }

    let block_prod_strategy = EvilBlockProdStrategy {
        commitments: unpledge_txs.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        outcome,
        |e| {
            matches!(
                e,
                ValidationError::CommitmentSnapshotRejected {
                    status: irys_domain::CommitmentSnapshotStatus::InvalidPledgeCount,
                    ..
                }
            )
        },
        "block containing invalid unpledge count should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_block_unpledge_invalid_value_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub commitment: CommitmentTransaction,
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
            let commitment = self.commitment.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![commitment.clone()],
                commitment_txs_to_bill: vec![commitment],
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

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_signer = genesis_node.cfg.signer();
    let genesis_addr = genesis_signer.address();
    let assignments = genesis_node.get_partition_assignments(genesis_addr);
    eyre::ensure!(
        !assignments.is_empty(),
        "genesis node must expose at least one partition for invalid value test"
    );

    let consensus = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    let mut unpledge_tx = CommitmentTransaction::new_unpledge(
        consensus,
        anchor,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_addr,
        assignments[0].partition_hash,
    )
    .await;

    // Corrupt the refund amount
    unpledge_tx.set_value(unpledge_tx.value().saturating_add(U256::from(1_u64)));

    genesis_signer.sign_commitment(&mut unpledge_tx)?;
    let invalid_unpledge = unpledge_tx;

    let block_prod_strategy = EvilBlockProdStrategy {
        commitment: invalid_unpledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    // The block is rejected because the commitment transaction has an invalid unpledge value.
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::CommitmentValueInvalid { .. }),
        "block containing invalid unpledge refund amount should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_epoch_block_with_extra_unpledge_gets_rejected() -> eyre::Result<()> {
    struct EvilEpochStrategy {
        pub prod: ProductionStrategy,
        pub commitments: Vec<CommitmentTransaction>,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilEpochStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &irys_types::IrysBlockHeader,
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let mut commitments = self.commitments.clone();
            commitments.sort(); // mimic canonical ordering
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: commitments.clone(),
                commitment_txs_to_bill: commitments,
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

    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_signer = genesis_node.cfg.signer();
    let genesis_addr = genesis_signer.address();
    let assignments = genesis_node.get_partition_assignments(genesis_addr);
    eyre::ensure!(
        assignments.len() >= 2,
        "genesis node must expose at least two partitions for epoch extra unpledge test"
    );

    let consensus = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    // Legitimate unpledge that will be included before the epoch boundary
    let mut legitimate_unpledge = CommitmentTransaction::new_unpledge(
        consensus,
        anchor,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_addr,
        assignments[0].partition_hash,
    )
    .await;
    genesis_signer.sign_commitment(&mut legitimate_unpledge)?;

    gossip_commitment_to_node(&genesis_node, &legitimate_unpledge).await?;
    let inclusion_block = genesis_node.mine_block().await?;
    assert_ne!(
        inclusion_block.height % num_blocks_in_epoch as u64,
        0,
        "Inclusion block must not be an epoch block"
    );

    // Craft an extra unpledge that never appeared on-chain
    let mut extra_unpledge = CommitmentTransaction::new_unpledge(
        consensus,
        genesis_node.get_anchor().await?,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_addr,
        assignments[1].partition_hash,
    )
    .await;
    genesis_signer.sign_commitment(&mut extra_unpledge)?;
    gossip_commitment_to_node(&genesis_node, &extra_unpledge).await?;

    let commitments = vec![legitimate_unpledge.clone(), extra_unpledge.clone()];
    let block_prod_strategy = EvilEpochStrategy {
        commitments: commitments.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    assert_eq!(
        block.header().height % num_blocks_in_epoch as u64,
        0,
        "Malicious block must be at epoch boundary"
    );

    let err = send_block_to_block_tree(&genesis_node.node_ctx, Arc::clone(&block), false)
        .await
        .expect_err("epoch block with extra unpledge should be rejected");

    let err = err
        .downcast::<irys_actors::block_validation::PreValidationError>()
        .expect("should be PreValidationError");
    assert!(
        matches!(
            err,
            irys_actors::block_validation::PreValidationError::InvalidEpochSnapshot { .. }
        ),
        "epoch block with extra unpledge should be rejected during pre-validation, got: {:?}",
        err
    );

    genesis_node.stop().await;

    Ok(())
}
