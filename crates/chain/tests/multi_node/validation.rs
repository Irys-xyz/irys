use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, reth_ethereum_primitives, sha, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::{
    storage_pricing::Amount, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader,
    NodeConfig, H256,
};
use reth::payload::EthBuiltPayload;

// This test creates a malicious block producer that squares the reward amount instead of using the correct value.
// The assertion will fail (block will be discarded) because the block rewards between irys block and reth
// block must match.
#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_evm_block_reward_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait(?Send)]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            commitment_txs_to_bill: &[CommitmentTransaction],
            submit_txs: &[IrysTransactionHeader],
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: u128,
        ) -> eyre::Result<EthBuiltPayload> {
            let invalid_reward_amount = Amount::new(reward_amount.amount.pow(2_u64.into()));

            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    commitment_txs_to_bill,
                    submit_txs,
                    // NOTE: Point of error - trying to give yourself extra funds in the evm state
                    invalid_reward_amount,
                    timestamp_ms,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.node_ctx.sync_state.set_is_syncing(true);
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    peer_node.node_ctx.sync_state.set_is_syncing(false);

    peer_node.gossip_block(&block)?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test produces a valid block but then tampers with the evm_block_hash field in the Irys block header,
// setting it to a valid reth block hash, but not the one that was intended to be used.
// The block will be discarded because the system will detect that the reth block hash does not match the one that's been provided.
// (note: the fail in question happens because each evm block hash contains "parent beacon block" hash as part of the seed)
#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_reth_hash_gets_rejected() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;
    // set block migration depth so epoch blocks go to index correctly
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    // produce an invalid block
    let block_prod_strategy = ProductionStrategy {
        inner: peer_node.node_ctx.block_producer_inner.clone(),
    };

    peer_node.node_ctx.sync_state.set_is_syncing(true);
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

    let mut irys_block = block.as_ref().clone();
    irys_block.evm_block_hash = eth_payload_other.block().header().hash_slow();
    peer_signer.sign_block_header(&mut irys_block)?;
    let irys_block = Arc::new(irys_block);
    peer_node.node_ctx.sync_state.set_is_syncing(false);

    peer_node.gossip_block(&irys_block)?;
    peer_node.gossip_eth_block(eth_payload.block())?;
    peer_node.gossip_eth_block(eth_payload_other.block())?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test adds an extra transaction to the EVM block that isn't included in the Irys block's transaction list.
// The assertion will fail (block will be discarded) because during validation, the system will detect
// that the EVM block contains transactions not accounted for in the Irys block, breaking the 1:1 mapping requirement.
#[test_log::test(actix_web::test)]
async fn heavy_block_shadow_txs_misalignment_block_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub extra_tx: IrysTransactionHeader,
    }

    #[async_trait::async_trait(?Send)]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            commitment_txs_to_bill: &[CommitmentTransaction],
            submit_txs: &[IrysTransactionHeader],
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: u128,
        ) -> eyre::Result<EthBuiltPayload> {
            let mut submit_txs = submit_txs.to_vec();
            submit_txs.push(self.extra_tx.clone());

            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    commitment_txs_to_bill,
                    &submit_txs,
                    reward_amount,
                    timestamp_ms,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;
    let extra_tx = peer_node
        .create_submit_data_tx(&peer_signer, "Hello, world!".as_bytes().to_vec())
        .await?;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        extra_tx: extra_tx.header,
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.node_ctx.sync_state.set_is_syncing(true);
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    peer_node.node_ctx.sync_state.set_is_syncing(false);

    peer_node.gossip_block(&block)?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test reverses the order of transactions when creating the EVM block compared to their order in the Irys block.
// The assertion will fail (block will be discarded) because transaction ordering must be preserved between
// the Irys and EVM blocks to ensure deterministic state transitions and proper validation.
#[test_log::test(actix_web::test)]
async fn heavy_block_shadow_txs_different_order_of_txs() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait(?Send)]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            commitment_txs_to_bill: &[CommitmentTransaction],
            submit_txs: &[IrysTransactionHeader],
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            timestamp_ms: u128,
        ) -> eyre::Result<EthBuiltPayload> {
            let mut submit_txs = submit_txs.to_vec();
            // NOTE: We reverse the order of txs, this means
            // that during validation the irys block txs will not match the
            // reth block txs
            assert_eq!(submit_txs.len(), 2);
            submit_txs.reverse();

            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    commitment_txs_to_bill,
                    &submit_txs,
                    reward_amount,
                    timestamp_ms,
                )
                .await
        }
    }

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    // speeds up POA
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;
    let _extra_tx_a = peer_node
        .create_submit_data_tx(&peer_signer, "Hello, world!".as_bytes().to_vec())
        .await?;
    let _extra_tx_b = peer_node
        .create_submit_data_tx(&peer_signer, "Hello, Irys!".as_bytes().to_vec())
        .await?;

    // produce an invalid block
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    peer_node.node_ctx.sync_state.set_is_syncing(true);
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();
    peer_node.node_ctx.sync_state.set_is_syncing(false);

    peer_node.gossip_block(&block)?;
    let eth_block = eth_payload.block();
    peer_node.gossip_eth_block(eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

// This test produces a block with an invalid PoA chunk.
// A new block will not be built on the invalid block.
#[test_log::test(actix_web::test)]
async fn heavy_block_prod_will_not_build_on_invalid_blocks() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait(?Send)]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        fn create_poa_data(
            &self,
            solution: &irys_types::block_production::SolutionContext,
            ledger_id: Option<u32>,
        ) -> eyre::Result<(irys_types::PoaData, H256)> {
            // Create an invalid PoA chunk that doesn't match the actual solution
            let invalid_chunk = vec![0xFF; 256 * 1024]; // Fill with invalid data
            let poa_chunk = irys_types::Base64(invalid_chunk);
            // hash is valid so that prevalidation succeeds
            let poa_chunk_hash = H256(sha::sha256(&poa_chunk.0));

            let poa = irys_types::PoaData {
                tx_path: solution.tx_path.clone().map(irys_types::Base64),
                data_path: solution.data_path.clone().map(irys_types::Base64),
                chunk: Some(poa_chunk),
                recall_chunk_index: solution.recall_chunk_index,
                ledger_id,
                partition_chunk_offset: solution.chunk_offset,
                partition_hash: solution.partition_hash,
            };
            Ok((poa, poa_chunk_hash))
        }
    }

    // Configure test network
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32; // Speed up PoA

    // Create peer signer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start genesis node (node 1)
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Start peer node (node 2) that will produce evil block
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    // Create evil block production strategy
    let evil_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: peer_node.node_ctx.block_producer_inner.clone(),
        },
    };

    // Set syncing to prevent automatic gossip
    peer_node.node_ctx.sync_state.set_is_syncing(true);

    // Produce block with invalid PoA
    let (evil_block, _eth_payload) = evil_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually broadcast the evil block to genesis node
    let initial_height = genesis_node.get_height().await;
    peer_node.gossip_block(&evil_block)?;

    // Mine a block on genesis node to ensure it doesn't build on the invalid block
    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };
    let (_new_block, _) = block_prod_strategy
        .fully_produce_new_block(solution_context(&genesis_node.node_ctx).await?)
        .await
        .unwrap()
        .unwrap();

    // Verify genesis node built on the last valid block, not the evil one
    let new_height = genesis_node.get_height().await;
    assert_eq!(new_height, initial_height + 1);
    assert_eq!(
        new_height, evil_block.height,
        "we ensure we have created a fork"
    );

    // Get the new block and verify its parent is not the evil block
    let new_block = genesis_node.get_block_by_height(new_height).await?;
    assert_ne!(
        new_block.previous_block_hash, evil_block.block_hash,
        "expect the new block parent to NOT be the evil parent block"
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
