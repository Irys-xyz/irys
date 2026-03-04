// These tests create malicious block producers that include invalid PermFeeRefund shadow transactions.
// They verify that blocks are rejected when they contain inappropriate refunds.
use crate::utils::{assert_validation_error, solution_context, IrysNodeTest};
use crate::validation::send_block_and_read_state;
use irys_actors::block_validation::ValidationError;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::IrysAddress;
use irys_types::{DataLedger, DataTransactionHeader, IrysBlockHeader, NodeConfig, H256, U256};
use std::collections::BTreeMap;

// This test verifies that blocks are rejected when they contain a PermFeeRefund
// for a transaction that was successfully promoted (and thus shouldn't get a refund).
#[test_log::test(tokio::test)]
pub async fn heavy_block_perm_fee_refund_for_promoted_tx_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub data_tx: DataTransactionHeader,
        pub invalid_refund: (H256, U256, IrysAddress),
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
            // Include the data transaction in submit ledger
            // Create an invalid refund - refunding a promoted transaction
            let user_perm_fee_refunds = vec![self.invalid_refund];

            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![self.data_tx.clone()],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    reward_balance_increment: BTreeMap::new(),
                    user_perm_fee_refunds,
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
            })
        }
    }

    // Configure a test network with accelerated epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 2;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Spawn a peer node that will receive blocks via gossip
    let peer_config = genesis_node.testing_peer_with_signer(&test_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // Create a properly signed data transaction that appears promoted
    use irys_types::IrysTransactionCommon as _;
    let mut data_tx = DataTransactionHeader::new(&genesis_config.consensus_config());
    data_tx.data_root = H256::random();
    data_tx.data_size = 1024;
    data_tx.term_fee = U256::from(1000).into();
    data_tx.perm_fee = Some(U256::from(2000).into());
    data_tx.ledger_id = DataLedger::Submit as u32;

    // Sign the transaction
    data_tx = data_tx
        .sign(&test_signer)
        .expect("Failed to sign transaction");

    // Wrap in V1 with promoted metadata
    let DataTransactionHeader::V1(mut tx_with_meta) = data_tx;
    tx_with_meta.metadata = irys_types::DataTransactionMetadata::with_promoted_height(2);
    let data_tx = DataTransactionHeader::V1(tx_with_meta);

    // Create an invalid refund for this promoted transaction
    let invalid_refund = (
        data_tx.id,
        data_tx.perm_fee.unwrap().get(), // Try to refund the perm_fee
        data_tx.signer,
    );

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        data_tx: data_tx.clone(),
        invalid_refund,
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block to the Genesis node for validation
    let outcome = send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Genesis should reject block with refund for promoted transaction",
    );

    // Send block to the PEER node for validation
    let outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Peer should reject block with refund for promoted transaction",
    );

    // Verify the block was NOT submitted to the PEER's reth due to shadow validation failure
    // The new shadow validation sequence should prevent submission of blocks with invalid shadow transactions
    peer_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;

    // Clean up both nodes
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test verifies that blocks are rejected when they contain a PermFeeRefund
// for a transaction that doesn't exist in the ledger.
#[test_log::test(tokio::test)]
pub async fn heavy_block_perm_fee_refund_for_nonexistent_tx_gets_rejected() -> eyre::Result<()> {
    struct PhantomRefundStrategy {
        pub prod: ProductionStrategy,
        pub invalid_refund: (H256, U256, IrysAddress),
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for PhantomRefundStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
            _block_timestamp: irys_types::UnixTimestampMs,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let user_perm_fee_refunds = vec![self.invalid_refund];

            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta {
                    reward_balance_increment: BTreeMap::new(),
                    user_perm_fee_refunds, // But we have a refund!
                },
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
            })
        }
    }

    // Configure a test network with accelerated epochs
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 2;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Spawn a peer node that will receive blocks via gossip
    let peer_config = genesis_node.testing_peer_with_signer(&test_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // Create a phantom refund for a transaction that doesn't exist
    let phantom_tx_id = H256::random();
    let invalid_refund = (
        phantom_tx_id,
        U256::from(3000), // Arbitrary refund amount
        test_signer.address(),
    );

    let block_prod_strategy = PhantomRefundStrategy {
        invalid_refund,
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block to the Genesis node for validation
    let outcome = send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Genesis should reject block with refund for nonexistent transaction",
    );

    // Send block to the PEER node for validation
    let outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "Peer should reject block with refund for nonexistent transaction",
    );

    // Verify the block was NOT submitted to the PEER's reth due to shadow validation failure
    // The new shadow validation sequence should prevent submission of blocks with invalid shadow transactions
    peer_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;

    // Clean up both nodes
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
