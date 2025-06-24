use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use alloy_core::primitives::FixedBytes;
use irys_actors::{
    async_trait, reth_ethereum_primitives, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::{
    storage_pricing::Amount, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader,
    NodeConfig,
};
use reth::{payload::EthBuiltPayload, primitives::SealedBlock};
use rust_decimal_macros::dec;

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
    peer_node.gossip_eth_block(&eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_reth_hash_gets_rejected() -> eyre::Result<()> {
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
    let block_prod_strategy = ProductionStrategy {
        inner: peer_node.node_ctx.block_producer_inner.clone(),
    };

    peer_node.node_ctx.sync_state.set_is_syncing(true);
    let (block, eth_payload) = block_prod_strategy
        .fully_produce_new_block(solution_context(&peer_node.node_ctx).await?)
        .await?
        .unwrap();

    let mut irys_block = block.as_ref().clone();
    let invalid_evm_blockhash = FixedBytes::new([111; 32]);
    irys_block.evm_block_hash = invalid_evm_blockhash;
    peer_signer.sign_block_header(&mut irys_block)?;
    let irys_block = Arc::new(irys_block);
    peer_node.node_ctx.sync_state.set_is_syncing(false);

    peer_node.gossip_block(&irys_block)?;
    let evm_block = eth_payload.block().clone_block();
    let eth_block = SealedBlock::new_unchecked(evm_block, invalid_evm_blockhash);
    peer_node.gossip_eth_block(&eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_block_system_txs_misalignment_block_rejected() -> eyre::Result<()> {
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
    peer_node.gossip_eth_block(&eth_block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
