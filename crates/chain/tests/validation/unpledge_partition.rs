use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use crate::validation::send_block_to_block_tree;
use eyre::WrapErr as _;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    mempool_service::MempoolServiceMessage, shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_primitives::CommitmentType;
use irys_types::{CommitmentTransaction, NodeConfig, U256};
use tokio::sync::oneshot;
use tracing::debug;

async fn gossip_commitment_to_node(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    commitment: &CommitmentTransaction,
) -> eyre::Result<()> {
    let (resp_tx, resp_rx) = oneshot::channel();
    node.node_ctx
        .service_senders
        .mempool
        .send(MempoolServiceMessage::IngestCommitmentTx(
            commitment.clone(),
            resp_tx,
        ))?;

    match resp_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            debug!(
                tx_id = ?commitment.id,
                ?err,
                "Commitment gossip rejected by mempool"
            );
        }
        Err(recv_err) => {
            debug!(
                tx_id = ?commitment.id,
                ?recv_err,
                "Commitment gossip channel dropped"
            );
        }
    }
    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_block_unpledge_partition_not_owned_gets_rejected() -> eyre::Result<()> {
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

    let invalid_unpledge = CommitmentTransaction::new_unpledge(
        consensus_config,
        anchor,
        evil_node.node_ctx.mempool_pledge_provider.as_ref(),
        evil_addr,
        stolen_partition_hash,
    )
    .await;
    let invalid_unpledge = evil_signer.sign_commitment(invalid_unpledge)?;

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
        gossip_commitment_to_node(node, &invalid_unpledge).await?;
    }

    send_block_to_block_tree(
        &genesis_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge.clone()],
        false,
    )
    .await?;
    let genesis_outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        genesis_outcome,
        BlockValidationOutcome::Discarded,
        "genesis node should discard block with unpledge referencing unowned partition"
    );

    send_block_to_block_tree(
        &victim_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge.clone()],
        false,
    )
    .await?;
    let victim_outcome = read_block_from_state(&victim_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        victim_outcome,
        BlockValidationOutcome::Discarded,
        "victim peer should also discard the malicious block"
    );

    send_block_to_block_tree(
        &evil_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge],
        false,
    )
    .await?;
    let evil_outcome = read_block_from_state(&evil_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        evil_outcome,
        BlockValidationOutcome::Discarded,
        "malicious peer must not accept its own invalid block"
    );

    evil_node.stop().await;
    victim_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
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
        tx.commitment_type = CommitmentType::Unpledge {
            pledge_count_before_executing: count,
            partition_hash: assignment.partition_hash.into(),
        };
        tx.value = CommitmentTransaction::calculate_pledge_value_at_count(
            consensus,
            count
                .checked_sub(1)
                .ok_or_else(|| eyre::eyre!("pledge count must be greater than zero"))?,
        );

        let signed = genesis_signer.sign_commitment(tx)?;
        unpledge_txs.push(signed);
    }

    for tx in &unpledge_txs {
        gossip_commitment_to_node(&genesis_node, tx).await?;
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

    send_block_to_block_tree(
        &genesis_node.node_ctx,
        Arc::clone(&block),
        unpledge_txs,
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "block containing invalid unpledge count should be rejected"
    );

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
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
    unpledge_tx.value = unpledge_tx.value.saturating_add(U256::from(1_u64));

    let invalid_unpledge = genesis_signer.sign_commitment(unpledge_tx)?;

    gossip_commitment_to_node(&genesis_node, &invalid_unpledge).await?;

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

    send_block_to_block_tree(
        &genesis_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge],
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "block containing invalid unpledge refund amount should be rejected"
    );

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
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
    let legitimate_unpledge = CommitmentTransaction::new_unpledge(
        consensus,
        anchor,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_addr,
        assignments[0].partition_hash,
    )
    .await;
    let legitimate_unpledge = genesis_signer.sign_commitment(legitimate_unpledge)?;

    gossip_commitment_to_node(&genesis_node, &legitimate_unpledge).await?;
    let inclusion_block = genesis_node.mine_block().await?;
    assert_ne!(
        inclusion_block.height % num_blocks_in_epoch as u64,
        0,
        "Inclusion block must not be an epoch block"
    );

    // Craft an extra unpledge that never appeared on-chain
    let extra_unpledge = CommitmentTransaction::new_unpledge(
        consensus,
        genesis_node.get_anchor().await?,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_addr,
        assignments[1].partition_hash,
    )
    .await;
    let extra_unpledge = genesis_signer.sign_commitment(extra_unpledge)?;
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
        block.height % num_blocks_in_epoch as u64,
        0,
        "Malicious block must be at epoch boundary"
    );

    send_block_to_block_tree(
        &genesis_node.node_ctx,
        Arc::clone(&block),
        commitments,
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "epoch block with extra unpledge should be rejected"
    );

    genesis_node.stop().await;

    Ok(())
}
