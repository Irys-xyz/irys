use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use crate::validation::unpledge_partition::gossip_data_tx_to_node;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_tree_service::BlockTreeServiceMessage, shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_domain::ChainState;
use irys_types::storage_pricing::Amount;
use irys_types::{
    CommitmentTransaction, Config, DataLedger, DataTransactionHeader, IrysBlockHeader, NodeConfig,
    U256,
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

// This test ensures that during full block validation, data transaction pricing validates the perm fee
#[test_log::test(tokio::test)]
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
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![self.malicious_tx.clone()],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
        }
    }

    // Configure a test network
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

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

    // Create a block with evil strategy and zero immediate inclusion reward percent
    let genesis_block_prod = &genesis_node.node_ctx.block_producer_inner;
    let mut evil_config = genesis_node.node_ctx.config.node_config.clone();
    evil_config
        .consensus
        .get_mut()
        .immediate_tx_inclusion_reward_percent = Amount::new(U256::from(0));

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
                beacon_engine_handle: genesis_block_prod.beacon_engine_handle.clone(),
                block_index: genesis_block_prod.block_index.clone(),
            }),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    gossip_data_tx_to_node(&genesis_node, &malicious_tx.header).await?;
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test ensures that during full block validation, data transaction pricing validates the term fee
#[test_log::test(actix_web::test)]
async fn slow_heavy_block_insufficient_term_fee_gets_rejected() -> eyre::Result<()> {
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
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![self.malicious_tx.clone()],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
            })
        }
    }

    // Configure a test network
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Ensure we have at least one block mined so parent EMA is present
    genesis_node.mine_block().await?;

    // Prepare data for the malicious tx
    let data = vec![7_u8; 2048]; // 2KB of data
    let data_size = data.len() as u64;

    // Get the expected price from the API (uses parent EMA under the hood)
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await?;

    // Underpay term fee but keep perm fee equal to API value
    let wrong_term_fee = price_info.term_fee / U256::from(2);

    // Build a data transaction with fees derived from the wrong EMA (will be insufficient)
    let malicious_tx = test_signer.create_transaction_with_fees(
        data,
        genesis_node.get_anchor().await?,
        DataLedger::Publish,
        wrong_term_fee,            // Insufficient term fee
        Some(price_info.perm_fee), // Correct perm fee for parent EMA
    )?;
    let malicious_tx = test_signer.sign_transaction(malicious_tx)?;

    // Use an evil strategy to insert the malicious tx with zero immediate inclusion reward percent
    let genesis_block_prod = &genesis_node.node_ctx.block_producer_inner;
    let mut evil_config = genesis_node.node_ctx.config.node_config.clone();
    evil_config
        .consensus
        .get_mut()
        .immediate_tx_inclusion_reward_percent = Amount::new(U256::from(0));

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
                beacon_engine_handle: genesis_block_prod.beacon_engine_handle.clone(),
                block_index: genesis_block_prod.block_index.clone(),
            }),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Validate the block directly via block tree service
    gossip_data_tx_to_node(&genesis_node, &malicious_tx.header).await?;
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// Happy path: adjust EMA interval, mine enough blocks so pricing EMA differs from genesis,
// submit a valid data tx priced via API, and expect the block to be fully validated.
#[test_log::test(actix_web::test)]
async fn slow_heavy_block_valid_data_tx_after_ema_change_gets_accepted() -> eyre::Result<()> {
    // Configure network with small EMA interval so pricing EMA diverges from genesis quickly
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;
    // Make EMA recalc frequent to speed up test
    let price_adjustment_interval = 3_u64;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = price_adjustment_interval;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Mine 2 intervals worth of blocks so pricing EMA switches away from genesis
    for _ in 0..(price_adjustment_interval * 2) {
        genesis_node.mine_block().await?;
    }

    // Verify EMA used for public pricing diverged from genesis price
    let tip_hash = genesis_node.node_ctx.block_tree_guard.read().tip;
    let ema_snapshot = genesis_node
        .get_ema_snapshot(&tip_hash)
        .expect("EMA snapshot for tip must exist");
    let ema_for_pricing = ema_snapshot.ema_for_public_pricing();
    assert_ne!(
        ema_for_pricing, genesis_node.node_ctx.config.consensus.genesis.genesis_price,
        "EMA for pricing should differ from genesis after two intervals"
    );

    // Create and broadcast a valid data transaction using API pricing (uses tip's EMA for next block)
    let data = vec![1_u8; 1024];
    let _tx = genesis_node
        .post_publish_data_tx(&test_signer, data)
        .await?;

    // Produce a block using the standard production strategy which will pick the tx from mempool
    let block = genesis_node.mine_block().await?;
    let block = Arc::new(block);

    // Send for validation and expect the block to be stored (accepted)
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert!(matches!(
        outcome,
        BlockValidationOutcome::StoredOnNode(ChainState::Onchain)
    ));

    genesis_node.stop().await;
    Ok(())
}
