use super::send_block_and_read_state;
use crate::utils::{
    assert_validation_error, gossip_data_tx_to_node, solution_context, BlockValidationOutcome,
    IrysNodeTest,
};
use irys_actors::{
    async_trait,
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_validation::{PreValidationError, ValidationError},
    shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_database::tables::IngressProofs as IngressProofsTable;
use irys_database::walk_all;
use irys_domain::{BlockTreeReadGuard, ChainState};
use irys_types::storage_pricing::{
    calculate_perm_fee_from_config, calculate_term_fee_from_config, Amount,
};
use irys_types::IngressProofsList;
use irys_types::{
    Config, DataLedger, DataTransactionHeader, IrysBlockHeader, NodeConfig, OracleConfig,
    UnixTimestamp, U256,
};
use reth_db::Database as _;
use rust_decimal_macros::dec;
use std::sync::Arc;

// This test ensures that during full block validation, data transaction pricing validates the perm fee
#[test_log::test(tokio::test)]
async fn heavy_block_insufficient_perm_fee_gets_rejected() -> eyre::Result<()> {
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
            _block_timestamp: irys_types::UnixTimestampMs,
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
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
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
        price_info.term_fee.into(),
        Some(insufficient_perm_fee.into()), // Insufficient perm_fee!
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
                config: Config::new_with_random_peer_id(evil_config),
                db: genesis_block_prod.db.clone(),
                block_discovery: genesis_block_prod.block_discovery.clone(),
                mining_broadcaster: genesis_block_prod.mining_broadcaster.clone(),
                service_senders: genesis_block_prod.service_senders.clone(),
                reward_curve: genesis_block_prod.reward_curve.clone(),
                vdf_steps_guard: genesis_block_prod.vdf_steps_guard.clone(),
                block_tree_guard: genesis_block_prod.block_tree_guard.clone(),
                mempool_guard: genesis_block_prod.mempool_guard.clone(),
                price_oracle: genesis_block_prod.price_oracle.clone(),
                reth_payload_builder: genesis_block_prod.reth_payload_builder.clone(),
                reth_provider: genesis_block_prod.reth_provider.clone(),
                consensus_engine_handle: genesis_block_prod.consensus_engine_handle.clone(),
                block_index: genesis_block_prod.block_index.clone(),
                reth_node_adapter: genesis_block_prod.reth_node_adapter.clone(),
                chunk_ingress_state: genesis_block_prod.chunk_ingress_state.clone(),
            }),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    gossip_data_tx_to_node(&genesis_node, &malicious_tx.header).await?;
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with insufficient perm_fee should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

// This test ensures that during full block validation, data transaction pricing validates the term fee
#[test_log::test(actix_web::test)]
async fn heavy_block_insufficient_term_fee_gets_rejected() -> eyre::Result<()> {
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
            _block_timestamp: irys_types::UnixTimestampMs,
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
                epoch_snapshot: irys_domain::dummy_epoch_snapshot(),
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
        wrong_term_fee.into(),            // Insufficient term fee
        Some(price_info.perm_fee.into()), // Correct perm fee for parent EMA
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
                config: Config::new_with_random_peer_id(evil_config),
                db: genesis_block_prod.db.clone(),
                block_discovery: genesis_block_prod.block_discovery.clone(),
                mining_broadcaster: genesis_block_prod.mining_broadcaster.clone(),
                service_senders: genesis_block_prod.service_senders.clone(),
                reward_curve: genesis_block_prod.reward_curve.clone(),
                vdf_steps_guard: genesis_block_prod.vdf_steps_guard.clone(),
                block_tree_guard: genesis_block_prod.block_tree_guard.clone(),
                mempool_guard: genesis_block_prod.mempool_guard.clone(),
                price_oracle: genesis_block_prod.price_oracle.clone(),
                reth_payload_builder: genesis_block_prod.reth_payload_builder.clone(),
                reth_provider: genesis_block_prod.reth_provider.clone(),
                consensus_engine_handle: genesis_block_prod.consensus_engine_handle.clone(),
                block_index: genesis_block_prod.block_index.clone(),
                reth_node_adapter: genesis_block_prod.reth_node_adapter.clone(),
                chunk_ingress_state: genesis_block_prod.chunk_ingress_state.clone(),
            }),
        },
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Validate the block directly via block tree service
    gossip_data_tx_to_node(&genesis_node, &malicious_tx.header).await?;
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&block), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with insufficient term_fee should be rejected",
    );

    genesis_node.stop().await;

    Ok(())
}

// Happy path: adjust EMA interval, mine enough blocks so pricing EMA differs from genesis,
// submit a valid data tx priced via API, and expect the block to be fully validated.
#[test_log::test(actix_web::test)]
async fn heavy_block_valid_data_tx_after_ema_change_gets_accepted() -> eyre::Result<()> {
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
    let (header, _payload, txs) = genesis_node.mine_block_with_payload().await?;

    let body = irys_types::BlockBody {
        block_hash: header.block_hash,
        data_transactions: txs.all_data_txs().cloned().collect(),
        commitment_transactions: txs.all_system_txs().cloned().collect(),
    };
    let block = Arc::new(irys_types::SealedBlock::new(Arc::clone(&header), body).unwrap());

    // Send for validation and expect the block to be stored (accepted)
    let outcome = send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert!(matches!(
        outcome,
        BlockValidationOutcome::StoredOnNode(ChainState::Onchain)
    ));

    genesis_node.stop().await;
    Ok(())
}

// Ensure that we don't validate the perm fee on a tx that gets promoted on a
// different block than it was initially included.
//
// The test deliberately proomotes the tx in a future EMA interval, meaning that
// future price validations will always be invalid
#[test_log::test(tokio::test)]
async fn heavy_block_promoted_tx_with_ema_price_change_gets_accepted() -> eyre::Result<()> {
    // Configure network with short EMA interval and ever-increasing mock oracle
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Set EMA interval to 2 blocks for quick price changes
    let price_adjustment_interval = 2_u64;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = price_adjustment_interval;

    // Configure mock oracle with ever-increasing prices
    genesis_config.oracles = vec![OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).expect("valid token amount"),
        incremental_change: Amount::token(dec!(0.01)).expect("valid token amount"),
        smoothing_interval: 1000, // Ensures continuous increase for 1000 blocks
        poll_interval_ms: 100,
        initial_direction_up: false,
    }];

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_two_ema_intervals(2).await?;
    let block = genesis_node
        .ema_mine_until_next_in_last_quarter(price_adjustment_interval)
        .await?;

    // Create a data transaction with pricing at current height (height 6)
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes
    let data_size = data.len() as u64;

    let price_before_the_interval = genesis_node.get_ema_snapshot(&block.block_hash).unwrap();

    // Calculate expected fees using hardfork params from config to match validation behavior
    let number_of_ingress_proofs_total = genesis_node
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        data_size,
        &genesis_node.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        price_before_the_interval.ema_for_public_pricing(),
    )?;

    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size,
        &genesis_node.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        price_before_the_interval.ema_for_public_pricing(),
        expected_term_fee,
    )?;

    let tx = test_signer.create_transaction_with_fees(
        data,
        genesis_node.get_anchor().await?,
        DataLedger::Publish,
        expected_term_fee.into(),
        Some(expected_perm_fee.amount.into()),
    )?;
    let tx = test_signer.sign_transaction(tx)?;

    // Post ONLY the transaction header (no chunks yet)
    genesis_node.post_data_tx_raw(&tx.header).await;

    let submit_block = genesis_node.mine_block().await?;
    let submit_ledger = &submit_block.data_ledgers[DataLedger::Submit];
    assert!(
        submit_ledger.tx_ids.0.contains(&tx.header.id),
        "Transaction should be in submit ledger"
    );

    // Capture EMA snapshot at the time of submit (height 7)
    let submit_ema_snapshot = genesis_node
        .get_ema_snapshot(&submit_block.block_hash)
        .expect("EMA snapshot for submit block must exist");
    let submit_ema_price = submit_ema_snapshot.ema_for_public_pricing();

    // Mine blocks to forward EMA
    genesis_node.mine_blocks(3).await?;

    // NOW upload the chunks after the block was mined
    genesis_node.upload_chunks(&tx).await?;
    genesis_node
        .wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;
    let promote_block = genesis_node.mine_block().await?;

    let publish_ledger = &promote_block.data_ledgers[DataLedger::Publish];
    assert!(
        publish_ledger.tx_ids.0.contains(&tx.header.id),
        "Transaction should be in publish ledger after promotion"
    );

    // Capture EMA snapshot at the time of promotion
    let promote_ema_snapshot = genesis_node
        .get_ema_snapshot(&promote_block.block_hash)
        .expect("EMA snapshot for promote block must exist");
    let promote_ema_price = promote_ema_snapshot.ema_for_public_pricing();

    // Verify that EMA has increased between submit and promotion
    assert!(
        promote_ema_price < submit_ema_price,
        "EMA price should have decreased from submit (height {}, price {:?}) to promotion (height {}, price {:?})",
        submit_block.height,
        submit_ema_price,
        promote_block.height,
        promote_ema_price
    );

    genesis_node.stop().await;
    Ok(())
}

// Test that ensures we validate the price data fields of a data tx that gets
// promoted in the same block that it was submitted in.
// This is done by crafting a tx with invalid price fields -
// if we skip validation then the block won't be rejected.
#[test_log::test(tokio::test)]
async fn heavy_same_block_promoted_tx_with_ema_price_change_gets_accepted() -> eyre::Result<()> {
    // Configure network with short EMA interval and ever-increasing mock oracle
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Set EMA interval to 2 blocks for quick price changes
    let price_adjustment_interval = 2_u64;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = price_adjustment_interval;

    // Configure mock oracle with ever-increasing prices
    genesis_config.oracles = vec![OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).expect("valid token amount"),
        incremental_change: Amount::token(dec!(0.01)).expect("valid token amount"),
        smoothing_interval: 1000, // Ensures continuous increase for 1000 blocks
        poll_interval_ms: 100,
        initial_direction_up: false,
    }];

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_two_ema_intervals(2).await?;
    let block = genesis_node
        .ema_mine_until_next_in_last_quarter(price_adjustment_interval)
        .await?;

    // Create a data transaction with pricing at current height (height 6)
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes
    let data_size = data.len() as u64;

    let price_before_the_interval = genesis_node.get_ema_snapshot(&block.block_hash).unwrap();

    // Calculate expected fees using hardfork params from config to match validation behavior
    let number_of_ingress_proofs_total = genesis_node
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        data_size,
        &genesis_node.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        price_before_the_interval.ema_for_public_pricing(),
    )?
    .checked_div(U256::from(2))
    .unwrap();

    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size,
        &genesis_node.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        price_before_the_interval.ema_for_public_pricing(),
        expected_term_fee,
    )?;

    let tx = test_signer.create_transaction_with_fees(
        data,
        genesis_node.get_anchor().await?,
        DataLedger::Publish,
        expected_term_fee.into(),
        Some((expected_perm_fee.amount).into()),
    )?;
    let tx = test_signer.sign_transaction(tx)?;

    // Gossip ONLY the transaction header (no chunks yet) via mempool service
    gossip_data_tx_to_node(&genesis_node, &tx.header).await?;

    // Upload chunks via public API and wait for ingress proofs to be generated
    genesis_node.upload_chunks(&tx).await?;
    genesis_node
        .wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;

    // Build an EvilBlockProducer that force-includes the tx in both ledgers
    // Collect proofs for this tx from the DB
    let proofs_list = {
        let ro_tx = genesis_node
            .node_ctx
            .db
            .as_ref()
            .tx()
            .expect("create mdbx read tx");
        let mut proofs = Vec::new();
        for (root, cached) in walk_all::<IngressProofsTable, _>(&ro_tx).expect("walk proofs") {
            if root == tx.header.data_root {
                proofs.push(cached.proof.clone());
            }
        }
        IngressProofsList(proofs)
    };

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub data_tx: DataTransactionHeader,
        pub proofs: IngressProofsList,
        pub block_tree_guard: BlockTreeReadGuard,
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
                submit_txs: vec![self.data_tx.clone()],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![self.data_tx.clone()],
                    proofs: Some(self.proofs.clone()),
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
                unstake_refund_events: vec![],
                epoch_snapshot: self.block_tree_guard.read().canonical_epoch_snapshot(),
            })
        }
    }

    let block_prod_strategy = EvilBlockProdStrategy {
        data_tx: tx.header.clone(),
        proofs: proofs_list,
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: genesis_node.node_ctx.block_tree_guard.clone(),
    };

    let (promote_block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Validate by sending block directly to the block tree
    // Expect the block to be rejected for insufficient perm_fee during prevalidation
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, promote_block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| {
            matches!(
                e,
                ValidationError::PreValidation(PreValidationError::InsufficientPermFee { .. })
            )
        },
        "block with insufficient perm_fee should be rejected",
    );

    genesis_node.stop().await;
    Ok(())
}
