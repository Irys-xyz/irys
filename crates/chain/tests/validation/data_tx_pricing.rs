use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_tree_service::BlockTreeServiceMessage, shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_types::storage_pricing::Amount;
use irys_types::{
    CommitmentTransaction, Config, DataLedger, DataTransactionHeader, DataTransactionLedger,
    H256List, IrysBlockHeader, NodeConfig, SystemTransactionLedger, H256, U256,
};
use std::sync::Arc;

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
            skip_vdf_validation: false,
        })?;

    Ok(response_rx.await??)
}

// This test creates a malicious block producer that includes a data transaction with insufficient perm_fee.
// The assertion will fail (block will be discarded) because data transactions must have perm_fee >= expected amount.
#[test_log::test(actix_web::test)]
async fn slow_heavy_block_insufficient_perm_fee_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub malicious_tx: DataTransactionHeader,
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
            // Return malicious tx in Submit ledger (would normally be waiting for proofs)
            Ok((
                vec![],
                vec![],
                vec![self.malicious_tx.clone()], // Submit ledger tx
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                }, // No Publish ledger txs
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                }, // No expired ledger fees
            ))
        }
    }

    // Configure a test network
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 256;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    // Create a data transaction with insufficient perm_fee
    let data = vec![42_u8; 1024]; // 1KB of data
    let data_size = data.len() as u64;

    // Get the expected price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await?;

    // Create transaction with INSUFFICIENT perm_fee (50% of expected)
    let insufficient_perm_fee = price_info.perm_fee / U256::from(2);
    let malicious_tx = test_signer.create_transaction_with_fees(
        data,
        genesis_node.get_anchor().await?,
        DataLedger::Publish,
        price_info.term_fee,
        Some(insufficient_perm_fee), // Insufficient perm_fee!
    )?;
    let malicious_tx = test_signer.sign_transaction(malicious_tx)?;

    let genesis_block_prod = &genesis_node.node_ctx.block_producer_inner;

    let mut evil_config = genesis_node.node_ctx.config.node_config.clone();
    evil_config
        .consensus
        .get_mut()
        .immediate_tx_inclusion_reward_percent = Amount::new(U256::from(0));

    // Create a block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        malicious_tx: malicious_tx.header.clone(),
        prod: ProductionStrategy {
            inner: Arc::new(BlockProducerInner {
                config: Config::new(evil_config),
                db: genesis_block_prod.db.clone(),
                block_discovery: genesis_block_prod.block_discovery.clone(),
                mining_broadcaster: genesis_block_prod.mining_broadcaster.clone(),
                service_senders: genesis_block_prod.service_senders.clone(),
                reward_curve: genesis_block_prod.reward_curve.clone(),
                vdf_steps_guard: genesis_block_prod.vdf_steps_guard.clone(),
                block_tree_guard: genesis_block_prod.block_tree_guard.clone(),
                price_oracle: genesis_block_prod.price_oracle.clone(),
                reth_payload_builder: genesis_block_prod.reth_payload_builder.clone(),
                reth_provider: genesis_block_prod.reth_provider.clone(),
                shadow_tx_store: genesis_block_prod.shadow_tx_store.clone(),
                reth_service: genesis_block_prod.reth_service.clone(),
                beacon_engine_handle: genesis_block_prod.beacon_engine_handle.clone(),
                block_index: genesis_block_prod.block_index.clone(),
            }),
        },
    };

    // This is the line that doesn't work
    let (mut block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually set the data ledgers with our malicious tx
    let mut irys_block = (*block).clone();
    irys_block.data_ledgers = vec![
        // Publish ledger (empty)
        DataTransactionLedger {
            ledger_id: DataLedger::Publish as u32,
            tx_root: H256::zero(),
            tx_ids: H256List(vec![]),
            total_chunks: 0,
            expires: None,
            proofs: None,
            required_proof_count: Some(1),
        },
        // Submit ledger with our malicious tx
        DataTransactionLedger {
            ledger_id: DataLedger::Submit as u32,
            tx_root: H256::zero(),
            tx_ids: H256List(vec![malicious_tx.header.id]),
            total_chunks: 0,
            expires: None,
            proofs: None,
            required_proof_count: None,
        },
    ];
    test_signer.sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    // This should still be rejected because the perm_fee is insufficient
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}
