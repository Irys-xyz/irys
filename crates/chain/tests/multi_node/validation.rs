use crate::utils::{
    assert_validation_error, read_block_from_state, solution_context, IrysNodeTest,
};
use irys_actors::{
    async_trait,
    block_discovery::{AnchorItemType, BlockDiscoveryError, BlockDiscoveryFacade as _},
    block_validation::ValidationError,
    reth_ethereum_primitives,
    shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, MempoolServiceMessage, MempoolTxs, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::tables::IrysBlockHeaders;
use irys_types::{
    ingress::generate_ingress_proof, storage_pricing::Amount, CommitmentTransaction,
    DataTransactionHeader, IngressProofsList, IrysBlockHeader, NodeConfig, SendTraced as _,
    UnixTimestampMs, H256, U256,
};
use reth::payload::EthBuiltPayload;
use reth_db::transaction::DbTxMut as _;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

fn insert_block_header_for_gossip_test(
    node: &IrysNodeTest<IrysNodeCtx>,
    block_hash: H256,
    header: IrysBlockHeader,
) -> eyre::Result<()> {
    // Since this block would be rejected by the validation of the producer itself, we need to
    // manually insert it into the db for gossiping block bodies
    node.node_ctx.db.update_eyre(move |tx| {
        tx.put::<IrysBlockHeaders>(block_hash, header.into())?;
        Ok(())
    })
}

// This test creates a malicious block producer that squares the reward amount instead of using the correct value.
// The assertion will fail (block will be discarded) because the block rewards between irys block and reth
// block must match.
#[test_log::test(tokio::test)]
async fn heavy_block_invalid_evm_block_reward_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            mempool: &irys_actors::block_producer::MempoolTxsBundle,
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: UnixTimestampMs,
            solution_hash: H256,
        ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError>
        {
            let invalid_reward_amount = Amount::new(reward_amount.amount.pow(2_u64.into()));
            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    mempool,
                    // NOTE: Point of error - trying to give yourself extra funds in the evm state
                    invalid_reward_amount,
                    timestamp_ms,
                    solution_hash,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.gossip_disable();
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    insert_block_header_for_gossip_test(
        &peer_node,
        block.header().block_hash,
        (**block.header()).clone(),
    )?;
    peer_node.gossip_enable();

    peer_node.gossip_block_to_peers(block.header())?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block_to_peers(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.header().block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with invalid EVM block reward should be rejected",
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test produces a valid block but then tampers with the evm_block_hash field in the Irys block header,
// setting it to a valid reth block hash, but not the one that was intended to be used.
// The block will be discarded because the system will detect that the reth block hash does not match the one that's been provided.
// (note: the fail in question happens because each evm block hash contains "parent beacon block" hash as part of the seed)
#[test_log::test(tokio::test)]
async fn slow_heavy_block_invalid_reth_hash_gets_rejected() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;
    // set block migration depth so epoch blocks go to index correctly
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;

    // produce an invalid block
    let block_prod_strategy = ProductionStrategy {
        inner: peer_node.node_ctx.block_producer_inner.clone(),
    };

    peer_node.gossip_disable();
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    let (_block, eth_payload_other) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    assert_ne!(
        eth_payload.block().header().hash_slow(),
        eth_payload_other.block().header().hash_slow(),
        "eth payloads must have different hashes"
    );

    let mut irys_block = (**block.header()).clone();
    irys_block.evm_block_hash = eth_payload_other.block().header().hash_slow();
    peer_signer.sign_block_header(&mut irys_block)?;
    // Re-signing actually changes the block hash, so we need to manually insert the header to the db
    //  for this test to work, because fetching the block body from the peer now requires that
    //  peer to actually have this block header in its database/mempool/cache
    insert_block_header_for_gossip_test(&peer_node, irys_block.block_hash, irys_block.clone())?;
    let irys_block = Arc::new(irys_block);
    peer_node.gossip_enable();

    peer_node.gossip_block_to_peers(&irys_block)?;
    peer_node.gossip_eth_block_to_peers(eth_payload.block())?;
    peer_node.gossip_eth_block_to_peers(eth_payload_other.block())?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &irys_block.block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with invalid reth hash should be rejected",
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test adds an extra transaction to the EVM block that isn't included in the Irys block's transaction list.
// The assertion will fail (block will be discarded) because during validation, the system will detect
// that the EVM block contains transactions not accounted for in the Irys block, breaking the 1:1 mapping requirement.
#[test_log::test(tokio::test)]
async fn heavy_block_shadow_txs_misalignment_block_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub extra_tx: DataTransactionHeader,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            mempool: &irys_actors::block_producer::MempoolTxsBundle,
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: UnixTimestampMs,
            solution_hash: H256,
        ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError>
        {
            let mut tampered_mempool = mempool.clone();
            tampered_mempool.submit_txs.push(self.extra_tx.clone());
            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    &tampered_mempool,
                    reward_amount,
                    timestamp_ms,
                    solution_hash,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;
    let extra_tx = peer_node
        .post_publish_data_tx(&peer_signer, "Hello, world!".as_bytes().to_vec())
        .await?;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        extra_tx: extra_tx.header,
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.gossip_disable();
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();

    insert_block_header_for_gossip_test(
        &peer_node,
        block.header().block_hash,
        (**block.header()).clone(),
    )?;

    peer_node.gossip_enable();

    peer_node.gossip_block_to_peers(block.header())?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block_to_peers(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.header().block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with misaligned shadow transactions should be rejected",
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test reverses the order of transactions when creating the EVM block compared to their order in the Irys block.
// The assertion will fail (block will be discarded) because transaction ordering must be preserved between
// the Irys and EVM blocks to ensure deterministic state transitions and proper validation.
#[test_log::test(tokio::test)]
async fn heavy_block_shadow_txs_different_order_of_txs() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            mempool: &irys_actors::block_producer::MempoolTxsBundle,
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: UnixTimestampMs,
            solution_hash: H256,
        ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError>
        {
            // NOTE: We reverse the order of txs, this means
            // that during validation the irys block txs will not match the
            // reth block txs
            let mut tampered_mempool = mempool.clone();
            assert_eq!(tampered_mempool.submit_txs.len(), 2);
            tampered_mempool.submit_txs.reverse();
            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    &tampered_mempool,
                    reward_amount,
                    timestamp_ms,
                    solution_hash,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;
    let _extra_tx_a = peer_node
        .post_publish_data_tx(&peer_signer, "Hello, world!".as_bytes().to_vec())
        .await?;
    let _extra_tx_b = peer_node
        .post_publish_data_tx(&peer_signer, "Hello, Irys!".as_bytes().to_vec())
        .await?;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.gossip_disable();
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    insert_block_header_for_gossip_test(
        &peer_node,
        block.header().block_hash,
        (**block.header()).clone(),
    )?;
    peer_node.gossip_enable();

    peer_node.gossip_block_to_peers(block.header())?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block_to_peers(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.header().block_hash).await;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with reordered shadow transactions should be rejected",
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[actix_web::test]
async fn heavy_ensure_block_validation_double_checks_anchors() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    irys_testing_utils::initialize_tracing();

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub txs: Mutex<MempoolTxs>,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn fetch_best_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<MempoolTxs> {
            Ok(self.txs.lock().unwrap().clone())
        }
    }
    let seconds_to_wait = 30;

    let mut config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.epoch.num_blocks_in_epoch = 3;
            consensus.block_migration_depth = 1;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.mempool.ingress_proof_anchor_expiry_depth = 5;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();
    let peer_signer = config.new_random_signer();

    // Start the genesis node and wait for packing
    config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    let genesis_hash = genesis_node.get_block_by_height(0).await?.block_hash;

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let data_tx = genesis_signer.create_publish_transaction(
        data.clone(),
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = genesis_signer.sign_transaction(data_tx)?;

    let data_tx_old = genesis_signer.create_publish_transaction(
        data.clone(),
        genesis_hash,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let old_data_tx = genesis_signer.sign_transaction(data_tx_old)?;

    let mut commitment_tx_old = CommitmentTransaction::new_pledge(
        &config.consensus_config(),
        genesis_hash,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
        genesis_signer.address(),
    )
    .await;

    genesis_signer.sign_commitment(&mut commitment_tx_old)?;

    // now we generate an ingress proof anchored to the genesis block
    // this should be too old, and should be rejected by the API.
    let too_old_ingress_proof = generate_ingress_proof(
        &genesis_signer,
        data_tx.header.data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        genesis_hash,
    )?;

    // submit old tx via gossip

    let (tx, rx) = tokio::sync::oneshot::channel();
    // ingest as gossip
    genesis_node
        .node_ctx
        .service_senders
        .mempool
        .send_traced(MempoolServiceMessage::IngestDataTxFromGossip(
            old_data_tx.header.clone(),
            tx,
        ))
        .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
    // Ignore possible ingestion errors in tests
    rx.await??;

    let (tx, rx) = tokio::sync::oneshot::channel();
    // ingest as gossip
    genesis_node
        .node_ctx
        .service_senders
        .mempool
        .send_traced(MempoolServiceMessage::IngestCommitmentTxFromGossip(
            commitment_tx_old.clone(),
            tx,
        ))
        .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
    // Ignore possible ingestion errors in tests
    rx.await??;

    // submit the correctly anchored data tx
    genesis_node.ingest_data_tx(data_tx.header.clone()).await?;

    genesis_node
        .wait_for_mempool(data_tx.header.id, seconds_to_wait)
        .await?;

    // ensure we are past the 5-block anchor depth for ingress proofs
    // the txs we injected above never get included in these blocks
    // as get best mempool txs is too smart for that ;)
    genesis_node
        .mine_blocks(
            6_usize.saturating_sub(genesis_node.get_canonical_chain_height().await as usize),
        )
        .await?;

    peer_node.wait_until_height(6, seconds_to_wait).await?;

    // the tx should still be in the mempool
    genesis_node
        .wait_for_mempool(old_data_tx.header.id, seconds_to_wait)
        .await?;

    // 1.) try producing a block with an incorrectly anchored data tx
    let txs = Mutex::new(MempoolTxs {
        commitment_tx: vec![],
        submit_tx: vec![old_data_tx.header],
        publish_tx: PublishLedgerWithTxs::default(),
    });

    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        txs,
    };

    genesis_node.gossip_disable();
    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_candidate(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    genesis_node.gossip_enable();
    let preval_res = block_prod_strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    assert!(matches!(
        preval_res,
        Err(BlockDiscoveryError::InvalidAnchor {
            item_type: AnchorItemType::DataTransaction { tx_id: _ },
            anchor: _
        })
    ));

    // 2.) commitment tx with incorrectly anchored commitment tx
    {
        let mut lck = block_prod_strategy.txs.lock().unwrap();
        *lck = MempoolTxs {
            commitment_tx: vec![commitment_tx_old.clone()],
            submit_tx: vec![],
            publish_tx: PublishLedgerWithTxs::default(),
        };
    };

    genesis_node.gossip_disable();
    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_candidate(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    genesis_node.gossip_enable();
    let preval_res = block_prod_strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    assert!(matches!(
        preval_res,
        Err(BlockDiscoveryError::InvalidAnchor {
            item_type: AnchorItemType::SystemTransaction { tx_id: _ },
            anchor: _
        })
    ));

    // 3.) publish tx with incorrectly anchored ingress proof (publish txs don't get their anchor re-validated)
    {
        let mut lck = block_prod_strategy.txs.lock().unwrap();
        *lck = MempoolTxs {
            commitment_tx: vec![],
            submit_tx: vec![],
            publish_tx: PublishLedgerWithTxs {
                txs: vec![data_tx.header.clone()],
                proofs: Some(IngressProofsList(vec![too_old_ingress_proof])),
            },
        }
    };

    genesis_node.gossip_disable();
    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_candidate(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    genesis_node.gossip_enable();
    let preval_res = block_prod_strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    assert!(matches!(
        preval_res,
        Err(BlockDiscoveryError::InvalidAnchor {
            item_type: AnchorItemType::IngressProof {
                promotion_target_id: _,
                id: _
            },
            anchor: _
        })
    ));

    // 4.) edge case: ingress proof anchored at min_tx_anchor_height - 1
    // this regression tests for a bug where an anchor at exactly (current_height - tx_anchor_expiry_depth)
    // fails validation because the logic used to collect the set of valid anchor block hashes is exclusive instead of inclusive.

    // Setup:
    // - current_height: obtained from genesis_node.get_canonical_chain_height()
    // - When validating a block at height (current_height + 1):
    //   - min_tx_anchor_height = (current_height + 1) - tx_anchor_expiry
    //   - min_ingress_proof_anchor_height = (current_height + 1) - ingress_anchor_expiry
    //   - valid tx anchors from block tree: current_height down to min_tx_anchor_height (inclusive)
    //   - bt_finished_height = min_tx_anchor_height - 1
    //   - valid ingress anchors from block index: for height in [min_ingress_proof_anchor_height, bt_finished_height)
    //   - MISSING: bt_finished_height itself <- this is the bug
    //   - edge_case_height = current_height - tx_anchor_expiry (which equals min_tx_anchor_height)
    //
    // An ingress proof anchored at edge_case_height used to fail validation due to exclusive range logic.
    // After the fix, the range is inclusive, so edge_case_height must be valid.

    let current_height = genesis_node.get_canonical_chain_height().await;

    let tx_anchor_expiry = config.consensus_config().mempool.tx_anchor_expiry_depth as u64;
    let ingress_anchor_expiry = config
        .consensus_config()
        .mempool
        .ingress_proof_anchor_expiry_depth as u64;

    // Edge-case anchor height that used to be excluded; after the inclusive fix it must be valid.
    let edge_case_height = current_height - tx_anchor_expiry;
    let edge_case_block = genesis_node.get_block_by_height(edge_case_height).await?;

    info!(
        "expected valid ingress anchors: heights {}, {}, {}, {} - edge case {}",
        current_height,
        current_height - 1,
        current_height - 2,
        (current_height + 1) - ingress_anchor_expiry,
        edge_case_height
    );

    // new txs
    let fresh_submit_tx = genesis_signer.create_publish_transaction(
        vec![99; 96],
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let fresh_submit_tx = genesis_signer.sign_transaction(fresh_submit_tx)?;

    let fresh_publish_tx = genesis_signer.create_publish_transaction(
        vec![88; 96],
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let fresh_publish_tx = genesis_signer.sign_transaction(fresh_publish_tx)?;

    // create an ingress proof anchored at the edge case height
    let edge_case_ingress_proof = generate_ingress_proof(
        &genesis_signer,
        fresh_publish_tx.header.data_root,
        [[88; 32], [88; 32], [88; 32]].iter().copied().map(Ok),
        config.consensus_config().chain_id,
        edge_case_block.block_hash,
    )?;

    info!(
        "created ingress proof ID {} with anchor at height {} (anchor block hash: {})",
        edge_case_ingress_proof.id(),
        edge_case_height,
        edge_case_block.block_hash
    );

    {
        let mut lck = block_prod_strategy.txs.lock().unwrap();
        // set MempoolTxs
        *lck = MempoolTxs {
            commitment_tx: vec![],
            submit_tx: vec![fresh_submit_tx.header.clone()], // Include submit to trigger first loop
            publish_tx: PublishLedgerWithTxs {
                txs: vec![fresh_publish_tx.header.clone()],
                proofs: Some(IngressProofsList(vec![edge_case_ingress_proof])),
            },
        };
    }

    genesis_node.gossip_disable();
    let (block, _, _) = block_prod_strategy
        .fully_produce_new_block_candidate(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    let preval_res = block_prod_strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    debug!("validation result: {:?}", preval_res);

    // the bug would manifest as an InvalidAnchor error here
    if matches!(
        preval_res,
        Err(BlockDiscoveryError::InvalidAnchor {
            item_type: AnchorItemType::IngressProof {
                promotion_target_id: _,
                id: _
            },
            anchor: _
        })
    ) {
        info!("bug detected! this is now a regression test, panicking");
        eyre::bail!("REGRESSION: An ingress proof with a valid anchor is now causing block production to fail. edge case height: {}, block production result: {:?}", &edge_case_height, &preval_res );
    } else if preval_res.is_ok() {
        info!("block validation succeeded with edge case anchor");
    } else {
        warn!("got a different error than expected: {:?}", preval_res);
        eyre::bail!(
            "unexpected error, expected BlockDiscoveryError::InvalidAnchor, got {:?}",
            &preval_res
        );
    }

    genesis_node.stop().await;

    Ok(())
}
