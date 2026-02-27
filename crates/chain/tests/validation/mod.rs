mod blobs_rejected;
mod data_tx_pricing;
mod invalid_perm_fee_refund;
mod mempool_gossip_shape;
mod poa_cases;
mod unpledge_partition;
mod unstake_edge_cases;

use std::collections::HashMap;
use std::sync::Arc;

use crate::utils::{
    assert_validation_error, gossip_commitment_to_node, read_block_from_state, solution_context,
    BlockValidationOutcome, IrysNodeTest,
};
use irys_actors::block_validation::ValidationError;
use irys_actors::validation_service::ValidationServiceMessage;
use irys_actors::{
    async_trait,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_tree_service::BlockTreeServiceMessage,
    block_validation::PreValidationError,
    shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_types::SystemLedger;
use irys_types::{
    BlockTransactions, CommitmentTransaction, DataTransactionHeader, DataTransactionHeaderV1,
    H256List, IrysBlockHeader, IrysTransactionCommon as _, NodeConfig, SystemTransactionLedger,
    H256,
};

// Helper function to send a block directly to the block tree service for validation
pub async fn send_block_to_block_tree(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
    transactions: BlockTransactions,
    skip_vdf_validation: bool,
) -> eyre::Result<()> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    node_ctx
        .service_senders
        .block_tree
        .send(BlockTreeServiceMessage::BlockPreValidated {
            block,
            transactions,
            skip_vdf_validation,
            response: response_tx,
        })?;

    response_rx.await??;
    Ok(())
}

fn send_block_to_block_validation(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
) -> Result<(), PreValidationError> {
    let transactions = BlockTransactions {
        commitment_txs: vec![],
        data_txs: HashMap::new(),
        ..Default::default()
    };

    node_ctx
        .service_senders
        .validation_service
        .send(ValidationServiceMessage::ValidateBlock {
            block,
            transactions,
            skip_vdf_validation: false,
        })
        .unwrap();
    Ok(())
}

// This test creates a malicious block producer that includes a stake commitment with invalid value.
// The assertion will fail (block will be discarded) because stake commitments must have exact stake_value
// from the consensus config.
#[test_log::test(tokio::test)]
async fn heavy_block_invalid_stake_value_gets_rejected() -> eyre::Result<()> {
    use irys_types::CommitmentTypeV1;
    use irys_types::U256;

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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let invalid_stake = self.invalid_stake.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_stake.clone()],
                commitment_txs_to_bill: vec![invalid_stake],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
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
    genesis_node.mine_block().await?;

    // Create a pledge commitment with invalid value
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let mut invalid_pledge = CommitmentTransaction::new(consensus_config);
    invalid_pledge.set_commitment_type(CommitmentTypeV1::Stake);
    invalid_pledge.set_anchor(genesis_node.get_anchor().await?);
    invalid_pledge.set_signer(genesis_config.signer().address());
    invalid_pledge.set_fee(consensus_config.mempool.commitment_fee);
    invalid_pledge.set_value(U256::from(1_000_000)); // Invalid!

    // Sign the commitment
    genesis_config
        .signer()
        .sign_commitment(&mut invalid_pledge)?;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_stake: invalid_pledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    // Note: We do NOT gossip the invalid commitment to mempool because mempool validation
    // would reject it. We're testing block validation, not mempool validation.
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions {
            commitment_txs: vec![invalid_pledge],
            data_txs: HashMap::new(),
            ..Default::default()
        },
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::CommitmentValueInvalid { .. }),
        "block with invalid stake value should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes a pledge commitment with invalid value.
// The assertion will fail (block will be discarded) because pledge commitments must have value
// calculated using calculate_pledge_value_at_count().
#[test_log::test(tokio::test)]
async fn heavy_block_invalid_pledge_value_gets_rejected() -> eyre::Result<()> {
    use irys_types::CommitmentTypeV1;
    use irys_types::U256;

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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let invalid_pledge = self.invalid_pledge.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_pledge.clone()],
                commitment_txs_to_bill: vec![invalid_pledge],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
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
    genesis_node.mine_block().await?;

    // Create a pledge commitment with invalid value
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let pledge_count = 0;
    let mut invalid_pledge = CommitmentTransaction::new(consensus_config);
    invalid_pledge.set_commitment_type(CommitmentTypeV1::Pledge {
        pledge_count_before_executing: pledge_count,
    });
    invalid_pledge.set_anchor(genesis_node.get_anchor().await?);
    invalid_pledge.set_signer(genesis_config.signer().address());
    invalid_pledge.set_fee(consensus_config.mempool.commitment_fee);
    invalid_pledge.set_value(U256::from(1_000_000)); // Invalid! Should use calculate_pledge_value_at_count

    // Sign the commitment
    genesis_config
        .signer()
        .sign_commitment(&mut invalid_pledge)?;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_pledge: invalid_pledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    // Note: We do NOT gossip the invalid commitment to mempool because mempool validation
    // would reject it. We're testing block validation, not mempool validation.
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions {
            commitment_txs: vec![invalid_pledge],
            data_txs: HashMap::new(),
            ..Default::default()
        },
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::CommitmentValueInvalid { .. }),
        "block with invalid pledge value should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes commitments in wrong order.
// The assertion will fail (block will be discarded) because stake commitments must come before pledge commitments.
#[test_log::test(tokio::test)]
async fn heavy_block_wrong_commitment_order_gets_rejected() -> eyre::Result<()> {
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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let commitments = self.commitments.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: commitments.clone(),
                commitment_txs_to_bill: commitments,
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
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
    genesis_node.mine_block().await?;

    // Create a stake commitment
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let miner_address = genesis_config.signer().address();
    let mut stake =
        CommitmentTransaction::new_stake(consensus_config, genesis_node.get_anchor().await?);
    stake.set_signer(miner_address);
    stake.set_fee(consensus_config.mempool.commitment_fee * 2); // Higher fee
    genesis_config.signer().sign_commitment(&mut stake)?;

    // Create a pledge commitment
    let _pledge_count = 0;
    let mut pledge = CommitmentTransaction::new_pledge(
        consensus_config,
        genesis_node.get_anchor().await?,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        miner_address,
    )
    .await;
    genesis_config.signer().sign_commitment(&mut pledge)?;

    // Create block with commitments in WRONG order (pledge before stake)
    let block_prod_strategy = EvilBlockProdStrategy {
        commitments: vec![pledge.clone(), stake.clone()], // Wrong order!
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (mut block, _adjustment_stats, _transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually set the commitment IDs in wrong order in the block
    let mut irys_block = (*block).clone();
    irys_block.system_ledgers = vec![SystemTransactionLedger {
        ledger_id: SystemLedger::Commitment as u32,
        tx_ids: H256List(vec![pledge.id(), stake.id()]), // Wrong order!
    }];
    genesis_config.signer().sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Gossip both commitments to the node's mempool
    gossip_commitment_to_node(&genesis_node, &pledge).await?;
    gossip_commitment_to_node(&genesis_node, &stake).await?;

    // Send block directly to block tree service for validation
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions {
            commitment_txs: vec![pledge, stake],
            data_txs: HashMap::new(),
            ..Default::default()
        },
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::CommitmentWrongOrder { .. }),
        "block with wrong commitment order should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

// This test validates fee-based ordering for two Unstake commitments.
// Creates 2 peers with assignments, unpledges all their partitions, mines to next
// epoch to clear pledges. Each peer then creates an unstake with different fees.
// The evil block swaps the order (low fee before high fee), violating the
// canonical fee-descending ordering.
#[test_log::test(tokio::test)]
async fn heavy_block_unstake_wrong_order_gets_rejected() -> eyre::Result<()> {
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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let commitments = self.commitments.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: commitments.clone(),
                commitment_txs_to_bill: commitments,
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
        }
    }

    // Setup network with short epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create and fund 2 peer signers BEFORE starting genesis node
    let test_signer = genesis_config.new_random_signer();
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer, &peer1_signer, &peer2_signer]);

    // Start genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create 2 peers with assignments (both auto-stake and auto-pledge)
    let peer1_node = genesis_node
        .testing_peer_with_assignments(&peer1_signer)
        .await?;
    let peer2_node = genesis_node
        .testing_peer_with_assignments(&peer2_signer)
        .await?;

    let consensus_config = &peer1_node.node_ctx.config.consensus;

    // Unpledge all partitions for both peers
    for (peer_node, peer_signer) in [(&peer1_node, &peer1_signer), (&peer2_node, &peer2_signer)] {
        let partitions: Vec<_> = {
            let sms = peer_node.node_ctx.storage_modules_guard.read();
            sms.iter()
                .filter_map(|sm| sm.partition_assignment())
                .collect()
        };
        for assignment in &partitions {
            let mut unpledge = CommitmentTransaction::new_unpledge(
                consensus_config,
                peer_node.get_anchor().await?,
                peer_node.node_ctx.mempool_pledge_provider.as_ref(),
                peer_signer.address(),
                assignment.partition_hash,
            )
            .await;
            peer_signer.sign_commitment(&mut unpledge)?;
            genesis_node.post_commitment_tx(&unpledge).await?;
        }
    }

    // Mine a block to include unpledges
    genesis_node.mine_block().await?;

    // Mine to next epoch to process unpledge refunds and clear pledges
    genesis_node.mine_until_next_epoch().await?;

    // Wait for epoch processing
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let base_fee = consensus_config.mempool.commitment_fee;

    // Peer 1: Create unstake with HIGH fee (should come FIRST)
    let mut unstake_high_fee =
        CommitmentTransaction::new_unstake(consensus_config, peer1_node.get_anchor().await?);
    unstake_high_fee.set_signer(peer1_signer.address());
    unstake_high_fee.set_fee(base_fee * 2); // HIGH fee
    peer1_signer.sign_commitment(&mut unstake_high_fee)?;

    // Peer 2: Create unstake with LOW fee (should come SECOND)
    let mut unstake_low_fee =
        CommitmentTransaction::new_unstake(consensus_config, peer2_node.get_anchor().await?);
    unstake_low_fee.set_signer(peer2_signer.address());
    unstake_low_fee.set_fee(base_fee); // LOW fee
    peer2_signer.sign_commitment(&mut unstake_low_fee)?;

    // Create evil block with WRONG order: [low fee, high fee]
    // Canonical order should be: [high fee, low fee]
    let block_prod_strategy = EvilBlockProdStrategy {
        commitments: vec![unstake_low_fee.clone(), unstake_high_fee.clone()], // WRONG!
        prod: ProductionStrategy {
            inner: peer1_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (mut block, _adjustment_stats, _block_txs, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&peer1_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually override commitment order in block
    let mut irys_block = (*block).clone();
    irys_block.system_ledgers = vec![SystemTransactionLedger {
        ledger_id: SystemLedger::Commitment as u32,
        tx_ids: H256List(vec![unstake_low_fee.id(), unstake_high_fee.id()]), // WRONG order!
    }];
    peer1_signer.sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Gossip both commitments to genesis node
    gossip_commitment_to_node(&genesis_node, &unstake_low_fee).await?;
    gossip_commitment_to_node(&genesis_node, &unstake_high_fee).await?;

    // Validate the malicious block on genesis node
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions {
            commitment_txs: vec![unstake_low_fee, unstake_high_fee],
            data_txs: Default::default(),
            ..Default::default()
        },
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::CommitmentWrongOrder { .. }),
        "block with unstakes in wrong fee order should be rejected",
    );

    peer1_node.stop().await;
    peer2_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes wrong commitments in an epoch block.
// The assertion will fail (block will be discarded) because epoch blocks must contain exactly
// the commitments from the parent's snapshot.
#[test_log::test(tokio::test)]
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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![self.wrong_commitment.clone()],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
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

    // Create a different commitment that's NOT in the snapshot
    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let mut wrong_commitment =
        CommitmentTransaction::new_stake(consensus_config, genesis_node.get_anchor().await?);
    wrong_commitment.set_signer(test_signer.address());
    test_signer.sign_commitment(&mut wrong_commitment)?;
    genesis_node.mine_block().await?;

    // Now mine block 2 (epoch block) with wrong commitment
    let block_prod_strategy = EvilBlockProdStrategy {
        wrong_commitment: wrong_commitment.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adj_stats, _transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Ensure this is an epoch block
    assert_eq!(
        block.height % num_blocks_in_epoch as u64,
        0,
        "Block must be an epoch block"
    );

    // Send block directly to block tree service for validation
    let err = send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions {
            commitment_txs: vec![wrong_commitment],
            data_txs: HashMap::new(),
            ..Default::default()
        },
        false,
    )
    .await
    .expect_err("block with wrong commitment should be rejected");

    let err = err.downcast::<PreValidationError>()?;
    assert!(matches!(
        err,
        PreValidationError::InvalidEpochSnapshot { .. }
    ));

    genesis_node.stop().await;

    Ok(())
}

// This test ensures that blocks with incorrect `last_epoch_hash` are rejected during validation.
// Firstly verify rejection of malformed/incorrect last_epoch_hash
// Secondly verify the first-after-epoch rule
#[test_log::test(tokio::test)]
async fn block_with_invalid_last_epoch_hash_gets_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Mine an initial block so we can tamper with the next block
    genesis_node.mine_block().await?;

    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (mut block, _adjustment_stats, transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();
    // Tamper with last_epoch_hash to make it invalid
    let mut irys_block = (*block).clone();
    irys_block.last_epoch_hash = H256::random(); // Use random hash to ensure it's invalid
    genesis_config.signer().sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Send the malformed block for validation via BlockDiscovery (includes prevalidation)
    let block_discovery = BlockDiscoveryFacadeImpl::new(
        genesis_node
            .node_ctx
            .service_senders
            .block_discovery
            .clone(),
    );
    let result = block_discovery
        .handle_block(block.clone(), transactions, false)
        .await;
    assert!(
        matches!(
            result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::LastEpochHashMismatch { .. }
            ))
        ),
        "block with invalid last_epoch_hash should fail prevalidation, got: {:?}",
        result
    );

    // Additionally verify the first-after-epoch rule (height % num_blocks_in_epoch == 1)
    // Step 1: Mine up to the next epoch boundary (height % N == 0)
    let current_height = genesis_node.get_canonical_chain_height().await;
    let num_blocks_in_epoch_u64: u64 = num_blocks_in_epoch.try_into()?;
    let blocks_until_boundary = (num_blocks_in_epoch_u64
        - (current_height % num_blocks_in_epoch_u64))
        % num_blocks_in_epoch_u64;
    if blocks_until_boundary > 0 {
        genesis_node
            .mine_blocks(blocks_until_boundary as usize)
            .await?;
    }

    // Step 2: Produce the first block after the epoch boundary
    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (block_after_epoch, _adjustment_stats2, transactions, _eth_payload2) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // ensure we're testing the intended height
    assert_eq!(
        block_after_epoch.height % num_blocks_in_epoch_u64,
        1,
        "Must be first block after an epoch boundary"
    );

    // Step 3: Tamper with last_epoch_hash to be the previous block's last_epoch_hash
    // (invalid for first-after-epoch; should be previous block's block_hash)
    let prev = genesis_node
        .get_block_by_hash(&block_after_epoch.previous_block_hash)
        .expect("prev header");

    let mut tampered = (*block_after_epoch).clone();
    tampered.last_epoch_hash = prev.last_epoch_hash;
    genesis_config.signer().sign_block_header(&mut tampered)?;
    let block_after_epoch = Arc::new(tampered);

    // Step 4: Send and expect prevalidation rejection via BlockDiscovery
    let block_discovery = BlockDiscoveryFacadeImpl::new(
        genesis_node
            .node_ctx
            .service_senders
            .block_discovery
            .clone(),
    );
    let result = block_discovery
        .handle_block(block_after_epoch.clone(), transactions, false)
        .await;
    assert!(
        matches!(
            result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::LastEpochHashMismatch { .. }
            ))
        ),
        "first block after epoch with invalid last_epoch_hash should fail prevalidation, got: {:?}",
        result
    );

    // Positive case: mine a valid first-after-epoch block and expect it to be stored
    let valid_block_after_epoch = genesis_node.mine_block().await?;

    // ensure we're still testing the intended height
    assert_eq!(
        valid_block_after_epoch.height % num_blocks_in_epoch_u64,
        1,
        "Must be first block after an epoch boundary"
    );

    let outcome =
        read_block_from_state(&genesis_node.node_ctx, &valid_block_after_epoch.block_hash).await;
    assert!(matches!(outcome, BlockValidationOutcome::StoredOnNode(_)));

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that includes duplicate ingress proof signers for the same data root.
// The assertion will fail (block will be discarded) because each address can only provide one proof per data root.
#[test_log::test(tokio::test)]
async fn heavy_block_duplicate_ingress_proof_signers_gets_rejected() -> eyre::Result<()> {
    use irys_actors::block_discovery::{BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl};
    use irys_types::{
        ingress::{generate_ingress_proof, CachedIngressProof},
        IngressProofsList, U256,
    };
    use reth_db::{transaction::DbTxMut as _, Database as _};

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub data_tx: DataTransactionHeader,
        pub duplicate_proofs: Vec<CachedIngressProof>,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            // Create publish ledger with duplicate proofs from the same signer for one transaction
            // This tests that each transaction must have unique signers
            let proofs = IngressProofsList(
                self.duplicate_proofs
                    .iter()
                    .map(|p| p.proof.clone())
                    .collect(),
            );

            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![self.data_tx.clone()],
                    proofs: Some(proofs),
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
        }
    }

    // Configure a test network
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 256;
    // Set to expect 2 proofs per transaction so we can test duplicate signers
    genesis_config
        .consensus
        .get_mut()
        .hardforks
        .frontier
        .number_of_ingress_proofs_total = 2;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create test data
    let chunk_size = 256_usize;
    let data_bytes = vec![0_u8; chunk_size * 2];
    let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();
    let anchor = genesis_node.get_anchor().await?;

    // Generate data root
    let leaves =
        irys_types::generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
    let root = irys_types::generate_data_root(leaves)?;
    let data_root = H256(root.id);

    // Create data transaction header and sign it
    let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id: H256::zero(),
            anchor,
            signer: test_signer.address(),
            data_root,
            data_size: data_bytes.len() as u64,
            header_size: 0,
            term_fee: U256::from(1000).into(),
            perm_fee: Some(U256::from(1000).into()), // Increased to cover 2 ingress proofs + base storage
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id: 1,
            signature: Default::default(),
        },
        metadata: irys_types::DataTransactionMetadata::with_promoted_height(1),
    })
    .sign(&test_signer)?;

    // Generate two ingress proofs from the SAME signer (duplicate!)
    let chain_id = 1_u64;
    let proof1 = generate_ingress_proof(
        &test_signer,
        data_root,
        chunks.clone().into_iter().map(Ok),
        chain_id,
        anchor,
    )?;

    // IMPORTANT: Create a second proof with the same signer but make it slightly different
    // so it's not an identical proof (which might be filtered out elsewhere)
    // We'll use the same data but the proof generation creates a different proof hash
    let proof2 = generate_ingress_proof(
        &test_signer,
        data_root,
        chunks.into_iter().map(Ok),
        chain_id,
        anchor,
    )?;

    // Verify both proofs have the same data_root and can recover the same signer
    assert_eq!(proof1.data_root(), data_root);
    assert_eq!(proof2.data_root(), data_root);
    assert_eq!(proof1.recover_signer()?, test_signer.address());
    assert_eq!(proof2.recover_signer()?, test_signer.address());

    // Create cached proofs with the same address (duplicate signers!)
    let duplicate_proofs = vec![
        CachedIngressProof {
            address: test_signer.address(),
            proof: proof1,
        },
        CachedIngressProof {
            address: test_signer.address(), // Same address!
            proof: proof2,
        },
    ];

    // First, add the data transaction and ingress proofs to the database so discovery can find them
    genesis_node.node_ctx.db.update(|tx| {
        use irys_database::tables::{CompactTxHeader, IrysDataTxHeaders};

        // Store the data transaction
        tx.put::<IrysDataTxHeaders>(data_tx.id, CompactTxHeader(data_tx.clone()))?;
        irys_database::cache_data_root(tx, &data_tx, None)?;

        // Store the ingress proofs (with duplicates from same address)
        for cached_proof in &duplicate_proofs {
            irys_database::store_external_ingress_proof_checked(
                tx,
                &cached_proof.proof,
                cached_proof.address,
            )?;
        }

        Ok::<_, eyre::Report>(())
    })??;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        data_tx: data_tx.clone(),
        duplicate_proofs,
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block to discovery service for prevalidation
    let block_discovery = BlockDiscoveryFacadeImpl::new(
        genesis_node
            .node_ctx
            .service_senders
            .block_discovery
            .clone(),
    );

    // This should fail during prevalidation due to duplicate signers
    let result = block_discovery
        .handle_block(block.clone(), transactions, false)
        .await;

    // Assert that the block was rejected due to duplicate ingress proof signers
    assert!(
        result.is_err(),
        "Expected block to be rejected but it succeeded"
    );
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("DuplicateIngressProofSigner"),
        "Expected DuplicateIngressProofSigner error, got: {}",
        err_msg
    );

    genesis_node.stop().await;

    Ok(())
}

// This test creates a malicious block producer that omits expected commitments from an epoch block.
// The assertion will fail (block will be discarded) because epoch blocks must contain all
// commitments from the parent's snapshot.
#[test_log::test(tokio::test)]
async fn heavy_block_epoch_missing_commitments_gets_rejected() -> eyre::Result<()> {
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
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
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

    // Post a valid stake commitment to be included in the epoch
    let pledge_tx = genesis_node.post_pledge_commitment(None).await?;
    genesis_node
        .wait_for_mempool(pledge_tx.id(), seconds_to_wait)
        .await?;

    // Mine block 1 to include the commitment
    genesis_node.mine_block().await?;

    // Now mine block 2 (epoch block) WITHOUT expected commitments
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _transactions, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Ensure this is an epoch block
    assert_eq!(
        block.height % num_blocks_in_epoch as u64,
        0,
        "Block must be an epoch block"
    );
    dbg!(&block);

    let err = send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        BlockTransactions::default(),
        false,
    )
    .await
    .expect_err("block with missing commitments should be rejected");

    let err = err.downcast::<PreValidationError>()?;
    assert!(matches!(
        err,
        PreValidationError::InvalidEpochSnapshot { .. }
    ));

    genesis_node.stop().await;

    Ok(())
}

/// Peer mines a block on top of common state with genesis
/// But peer does not broadcast execution payload (effectively, block is stuck in validation on the genesis)
///
/// Expectation: genesis mines ahead, and the block validation task for the block that's stuck gets cancelled
#[test_log::test(tokio::test)]
async fn heavy_block_validation_discards_a_block_if_its_too_old() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().block_tree_depth = 3;
    genesis_config.consensus.get_mut().block_migration_depth = 1;
    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&test_signer)
        .await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    peer_node.gossip_disable();
    let (block, _payload, _) = peer_node.mine_block_without_gossip().await?;
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash);

    // send directly to validation service, otherwise (if we send to block tree) block producer of genesis
    // node will wait for this block to be validated for quite a while until it starts mining
    send_block_to_block_validation(&genesis_node.node_ctx, block.clone())?;

    genesis_node.mine_blocks_without_gossip(3).await?;
    genesis_node.gossip_enable();
    peer_node.gossip_enable();
    peer_node.gossip_block_to_peers(&block)?;

    // Send block for validation
    let outcome = outcome.await;

    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ValidationCancelled { .. }),
        "block with versioned_hashes should be rejected",
    );
    genesis_node.stop().await;
    peer_node.stop().await;

    Ok(())
}
