mod data_tx_pricing;
mod invalid_perm_fee_refund;

use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    block_tree_service::BlockTreeServiceMessage, shadow_tx_generator::PublishLedgerWithTxs,
    BlockProdStrategy, BlockProducerInner, ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_database::SystemLedger;
use irys_types::{
    CommitmentTransaction, DataTransactionHeader, H256List, IrysBlockHeader, NodeConfig,
    SystemTransactionLedger, H256,
};

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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.invalid_stake.id]),
                }],
                vec![self.invalid_stake.clone()],
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
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
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        vec![invalid_pledge],
        false,
    )
    .await?;

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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.invalid_pledge.id]),
                }],
                vec![self.invalid_pledge.clone()],
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
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
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        vec![invalid_pledge],
        false,
    )
    .await?;

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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.commitments[0].id, self.commitments[1].id]),
                }],
                self.commitments.clone(),
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
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
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
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
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        vec![pledge, stake],
        false,
    )
    .await?;

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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![self.wrong_commitment.id]),
                }],
                vec![self.wrong_commitment.clone()],
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
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
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block.clone(),
        vec![wrong_commitment],
        false,
    )
    .await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}

// This test ensures that blocks with incorrect `last_epoch_hash` are rejected during validation.
// Firstly verify rejection of malformed/incorrect last_epoch_hash
// Secondly verify the first-after-epoch rule
#[test_log::test(actix_web::test)]
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

    let (mut block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Tamper with last_epoch_hash to make it invalid
    let mut irys_block = (*block).clone();
    irys_block.last_epoch_hash = irys_block.previous_block_hash;
    test_signer.sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Send the malformed block for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

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

    let (block_after_epoch, _adjustment_stats2, _eth_payload2) = block_prod_strategy
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
    test_signer.sign_block_header(&mut tampered)?;
    let block_after_epoch = Arc::new(tampered);

    // Step 4: Send and expect rejection
    send_block_to_block_tree(
        &genesis_node.node_ctx,
        block_after_epoch.clone(),
        vec![],
        false,
    )
    .await?;

    let outcome =
        read_block_from_state(&genesis_node.node_ctx, &block_after_epoch.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

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
#[test_log::test(actix_web::test)]
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
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            // Create publish ledger with duplicate proofs from the same signer for one transaction
            // This tests that each transaction must have unique signers
            let proofs = IngressProofsList(
                self.duplicate_proofs
                    .iter()
                    .map(|p| p.proof.clone())
                    .collect(),
            );

            Ok((
                vec![],
                vec![],
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![self.data_tx.clone()],
                    proofs: Some(proofs),
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
            ))
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

    // Generate data root
    let leaves =
        irys_types::generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
    let root = irys_types::generate_data_root(leaves)?;
    let data_root = H256(root.id);

    // Create data transaction header
    let data_tx = DataTransactionHeader {
        id: H256::random(),
        version: 1,
        anchor: H256::zero(),
        signer: test_signer.address(),
        data_root,
        data_size: data_bytes.len() as u64,
        header_size: 0,
        term_fee: U256::from(1000),
        perm_fee: Some(U256::from(1000)), // Increased to cover 2 ingress proofs + base storage
        ledger_id: 0,
        bundle_format: Some(0),
        chain_id: 1,
        promoted_height: Some(1),
        signature: Default::default(),
    };

    // Generate two ingress proofs from the SAME signer (duplicate!)
    let chain_id = 1_u64;
    let proof1 = generate_ingress_proof(
        &test_signer,
        data_root,
        chunks.clone().into_iter().map(Ok),
        chain_id,
    )?;

    // IMPORTANT: Create a second proof with the same signer but make it slightly different
    // so it's not an identical proof (which might be filtered out elsewhere)
    // We'll use the same data but the proof generation creates a different proof hash
    let proof2 = generate_ingress_proof(
        &test_signer,
        data_root,
        chunks.into_iter().map(Ok),
        chain_id,
    )?;

    // Verify both proofs have the same data_root and can recover the same signer
    assert_eq!(proof1.data_root, data_root);
    assert_eq!(proof2.data_root, data_root);
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
        use irys_database::tables::{
            CompactCachedIngressProof, CompactTxHeader, IngressProofs, IrysTxHeaders,
        };

        // Store the data transaction
        tx.put::<IrysTxHeaders>(data_tx.id, CompactTxHeader(data_tx.clone()))?;

        // Store the ingress proofs (with duplicates from same address)
        for cached_proof in &duplicate_proofs {
            tx.put::<IngressProofs>(data_root, CompactCachedIngressProof(cached_proof.clone()))?;
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

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
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
    let result = block_discovery.handle_block(block.clone(), false).await;

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
            PublishLedgerWithTxs,
            LedgerExpiryBalanceDelta,
        )> {
            Ok((
                vec![SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: H256List(vec![]),
                }],
                vec![],
                vec![],
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                LedgerExpiryBalanceDelta {
                    miner_balance_increment: std::collections::BTreeMap::new(),
                    user_perm_fee_refunds: Vec::new(),
                },
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

    // Post a valid stake commitment to be included in the epoch
    let pledge_tx = genesis_node.post_pledge_commitment(None).await?;
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

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}
