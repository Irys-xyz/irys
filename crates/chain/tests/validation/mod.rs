use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, block_tree_service::BlockTreeServiceMessage, BlockProdStrategy,
    BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_database::SystemLedger;
use irys_types::{
    CommitmentTransaction, DataTransactionHeader, H256List, IrysBlockHeader, NodeConfig,
    SystemTransactionLedger, TxIngressProof, H256,
};

// Helper function to send a block directly to the block tree service for validation
async fn send_block_to_block_tree(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
    commitment_txs: Vec<CommitmentTransaction>,
) -> eyre::Result<()> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    node_ctx
        .service_senders
        .block_tree
        .send(BlockTreeServiceMessage::BlockPreValidated {
            block,
            commitment_txs: Arc::new(commitment_txs),
            response: response_tx,
        })?;

    response_rx.await?
}

// This test creates a malicious block producer that includes a stake commitment with invalid value.
// The assertion will fail (block will be discarded) because stake commitments must have exact stake_value
// from the consensus config.
#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_stake_value_gets_rejected() -> eyre::Result<()> {
    use irys_database::SystemLedger;
    use irys_primitives::CommitmentType;
    use irys_types::{H256List, SystemTransactionLedger, U256};

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_stake: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }
        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.invalid_stake.id]),
                }],
                vec![self.invalid_stake.clone()],
                vec![],
                (vec![], vec![]),
            ))
        }
    }

    // Configure a test network with accelerated epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    genesis_node.mine_block().await?;

    // Create a pledge commitment with invalid value
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let mut invalid_pledge = CommitmentTransaction::new(consensus_config);
    invalid_pledge.commitment_type = CommitmentType::Stake;
    invalid_pledge.anchor = H256::zero();
    invalid_pledge.signer = test_signer.address();
    invalid_pledge.fee = consensus_config.mempool.commitment_fee;
    invalid_pledge.value = U256::from(1_000_000); // Invalid!

    // Sign the commitment
    let invalid_pledge = test_signer.sign_commitment(invalid_pledge)?;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_stake: invalid_pledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![invalid_pledge]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes a pledge commitment with invalid value.
// The assertion will fail (block will be discarded) because pledge commitments must have value
// calculated using calculate_pledge_value_at_count().
#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_pledge_value_gets_rejected() -> eyre::Result<()> {
    use irys_database::SystemLedger;
    use irys_primitives::CommitmentType;
    use irys_types::{H256List, SystemTransactionLedger, U256};

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_pledge: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }
        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.invalid_pledge.id]),
                }],
                vec![self.invalid_pledge.clone()],
                vec![],
                (vec![], vec![]),
            ))
        }
    }

    // Configure a test network with accelerated epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    genesis_node.mine_block().await?;

    // Create a pledge commitment with invalid value
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let pledge_count = 0;
    let mut invalid_pledge = CommitmentTransaction::new(consensus_config);
    invalid_pledge.commitment_type = CommitmentType::Pledge {
        pledge_count_before_executing: pledge_count,
    };
    invalid_pledge.anchor = H256::zero();
    invalid_pledge.signer = genesis_config.signer().address();
    invalid_pledge.fee = consensus_config.mempool.commitment_fee;
    invalid_pledge.value = U256::from(1_000_000); // Invalid! Should use calculate_pledge_value_at_count

    // Sign the commitment
    let invalid_pledge = genesis_config.signer().sign_commitment(invalid_pledge)?;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_pledge: invalid_pledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![invalid_pledge]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes commitments in wrong order.
// The assertion will fail (block will be discarded) because stake commitments must come before pledge commitments.
#[test_log::test(actix_web::test)]
async fn heavy_block_wrong_commitment_order_gets_rejected() -> eyre::Result<()> {
    use irys_database::SystemLedger;
    use irys_types::{H256List, SystemTransactionLedger};

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
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.commitments[0].id, self.commitments[1].id]),
                }],
                self.commitments.clone(),
                vec![],
                (vec![], vec![]),
            ))
        }
    }

    // Configure a test network with accelerated epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create a stake commitment
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let mut stake = CommitmentTransaction::new_stake(consensus_config, H256::zero());
    stake.signer = test_signer.address();
    stake.fee = consensus_config.mempool.commitment_fee * 2; // Higher fee
    let stake = test_signer.sign_commitment(stake)?;

    // Create a pledge commitment
    let _pledge_count = 0;
    let pledge = CommitmentTransaction::new_pledge(
        consensus_config,
        H256::zero(),
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        test_signer.address(),
    )
    .await;
    let pledge = test_signer.sign_commitment(pledge)?;

    // Create block with commitments in WRONG order (pledge before stake)
    let block_prod_strategy = EvilBlockProdStrategy {
        commitments: vec![pledge.clone(), stake.clone()], // Wrong order!
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (mut block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually set the commitment IDs in wrong order in the block
    let mut irys_block = (*block).clone();
    irys_block.system_ledgers = vec![SystemTransactionLedger {
        ledger_id: SystemLedger::Commitment as u32,
        tx_ids: H256List(vec![pledge.id, stake.id]), // Wrong order!
    }];
    test_signer.sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![pledge, stake]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes wrong commitments in an epoch block.
// The assertion will fail (block will be discarded) because epoch blocks must contain exactly
// the commitments from the parent's snapshot.
#[test_log::test(actix_web::test)]
async fn heavy_block_epoch_commitment_mismatch_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub wrong_commitment: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.wrong_commitment.id]),
                }],
                vec![self.wrong_commitment.clone()],
                vec![],
                (vec![], vec![]),
            ))
        }
    }

    // Configure a test network with 2 blocks per epoch so we can quickly reach epoch blocks
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 1;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Create a different commitment that's NOT in the snapshot
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let mut wrong_commitment = CommitmentTransaction::new_stake(consensus_config, H256::zero());
    wrong_commitment.signer = test_signer.address();
    let wrong_commitment = test_signer.sign_commitment(wrong_commitment)?;
    genesis_node.mine_block().await?;

    // Now mine block 2 (epoch block) with wrong commitment
    let block_prod_strategy = EvilBlockProdStrategy {
        wrong_commitment: wrong_commitment.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adj_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Ensure this is an epoch block
    assert_eq!(
        block.height % num_blocks_in_epoch as u64,
        0,
        "Block must be an epoch block"
    );

    // Send block directly to block tree service for validation
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        vec![wrong_commitment],
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that omits expected commitments from an epoch block.
// The assertion will fail (block will be discarded) because epoch blocks must contain all
// commitments from the parent's snapshot.
#[test_log::test(actix_web::test)]
async fn heavy_block_epoch_missing_commitments_gets_rejected() -> eyre::Result<()> {
    use irys_database::SystemLedger;
    use irys_types::{H256List, SystemTransactionLedger};

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![]),
                }],
                vec![],
                vec![],
                (vec![], vec![]),
            ))
        }
    }

    // Configure a test network with 2 blocks per epoch
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 1;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Post a valid stake commitment to be included in the epoch
    let pledge_tx = genesis_node.post_pledge_commitment(H256::zero()).await;
    genesis_node
        .wait_for_mempool(pledge_tx.id, seconds_to_wait)
        .await?;

    // Mine block 1 to include the commitment
    genesis_node.mine_block().await?;

    // Now mine block 2 (epoch block) WITHOUT expected commitments
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Ensure this is an epoch block
    assert_eq!(
        block.height % num_blocks_in_epoch as u64,
        0,
        "Block must be an epoch block"
    );
    dbg!(&block);

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}
