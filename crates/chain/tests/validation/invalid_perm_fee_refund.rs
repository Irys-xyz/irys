// These tests create malicious block producers that include invalid PermFeeRefund shadow transactions.
// They verify that blocks are rejected when they contain inappropriate refunds.
use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_tree_service::BlockTreeServiceMessage, shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_primitives::Address;
use irys_types::{
    CommitmentTransaction, DataLedger, DataTransactionHeader, H256List, IrysBlockHeader,
    NodeConfig, SystemTransactionLedger, H256, U256,
};
use std::collections::BTreeMap;
use std::sync::Arc;

// Helper function to send a block directly to the block tree service for validation
async fn send_block_to_block_tree(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
    commitment_txs: Vec<CommitmentTransaction>,
    skip_vdf_validation: bool,
) -> eyre::Result<()> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    node_ctx
        .service_senders
        .block_tree
        .send(BlockTreeServiceMessage::BlockPreValidated {
            block,
            commitment_txs: Arc::new(commitment_txs),
            skip_vdf_validation,
            response: response_tx,
        })?;

    response_rx.await??;
    Ok(())
}

// This test verifies that blocks are rejected when they contain a PermFeeRefund
// for a transaction that was successfully promoted (and thus shouldn't get a refund).
#[test_log::test(actix_web::test)]
pub async fn heavy_block_perm_fee_refund_for_promoted_tx_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub data_tx: DataTransactionHeader,
        pub invalid_refund: (H256, U256, Address),
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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            // Include the data transaction in submit ledger
            let data_ledger = SystemTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_ids: H256List(vec![self.data_tx.id]),
            };

            // Create an invalid refund - refunding a promoted transaction
            let user_perm_fee_refunds = vec![self.invalid_refund];

            Ok((
                vec![data_ledger],
                vec![],
                vec![self.data_tx.clone()],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: BTreeMap::new(),
                    user_perm_fee_refunds,
                },
            ))
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

    // Mine initial blocks
    genesis_node.mine_block().await?;
    genesis_node.mine_block().await?;

    // Create a data transaction that appears promoted
    let data_tx = DataTransactionHeader {
        id: H256::random(),
        version: 1,
        anchor: H256::zero(),
        signer: test_signer.address(),
        data_root: H256::random(),
        data_size: 1024,
        header_size: 0,
        term_fee: U256::from(1000),
        perm_fee: Some(U256::from(2000)),
        ledger_id: DataLedger::Submit as u32,
        bundle_format: Some(0),
        chain_id: 1,
        promoted_height: Some(2), // Mark as promoted!
        signature: Default::default(),
    };

    // Create an invalid refund for this promoted transaction
    let invalid_refund = (
        data_tx.id,
        data_tx.perm_fee.unwrap(), // Try to refund the perm_fee
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
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "Block with refund for promoted transaction should be rejected"
    );

    genesis_node.stop().await;

    Ok(())
}

// This test verifies that blocks are rejected when they contain a PermFeeRefund
// for a transaction that doesn't exist in the ledger.
#[test_log::test(actix_web::test)]
pub async fn heavy_block_perm_fee_refund_for_nonexistent_tx_gets_rejected() -> eyre::Result<()> {
    struct PhantomRefundStrategy {
        pub prod: ProductionStrategy,
        pub invalid_refund: (H256, U256, Address),
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for PhantomRefundStrategy {
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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            let user_perm_fee_refunds = vec![self.invalid_refund];

            Ok((
                vec![],
                vec![],
                vec![], // No actual transactions!
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: BTreeMap::new(),
                    user_perm_fee_refunds, // But we have a refund!
                },
            ))
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

    // Mine initial blocks
    genesis_node.mine_block().await?;
    genesis_node.mine_block().await?;

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
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "Block with refund for non-existent transaction should be rejected"
    );

    genesis_node.stop().await;

    Ok(())
}
