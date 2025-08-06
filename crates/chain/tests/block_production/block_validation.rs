use crate::utils::IrysNodeTest;
use eyre::Result;
use irys_types::NodeConfig;
use std::time::{SystemTime, UNIX_EPOCH};

/// This test ensures that if we attempt to submit a block with a timestamp
/// too far in the future, the node rejects it.
#[actix_web::test]
async fn heavy_test_future_block_rejection() -> Result<()> {
    // 0. Create an evil block producer
    use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome};
    use crate::validation::send_block_to_block_tree;
    use irys_actors::{
        async_trait, reth_ethereum_primitives, BlockProdStrategy, BlockProducerInner,
        ProductionStrategy,
    };
    use irys_domain::EmaSnapshot;
    use irys_types::{
        block_production::SolutionContext, storage_pricing::Amount, AdjustmentStats,
        CommitmentTransaction, DataTransactionHeader, IrysBlockHeader, SystemTransactionLedger,
        TxIngressProof,
    };
    use reth::{core::primitives::SealedBlock, payload::EthBuiltPayload};
    use std::sync::Arc;

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_timestamp: u128,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        fn block_reward(
            &self,
            prev_block_header: &IrysBlockHeader,
            _current_timestamp: u128,
        ) -> eyre::Result<Amount<irys_types::storage_pricing::phantoms::Irys>> {
            self.prod
                .block_reward(prev_block_header, self.invalid_timestamp)
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            commitment_txs_to_bill: &[CommitmentTransaction],
            submit_txs: &[DataTransactionHeader],
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            _timestamp_ms: u128,
        ) -> eyre::Result<EthBuiltPayload> {
            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    commitment_txs_to_bill,
                    submit_txs,
                    reward_amount,
                    self.invalid_timestamp,
                )
                .await
        }

        async fn produce_block_without_broadcasting(
            &self,
            solution: SolutionContext,
            prev_block_header: &IrysBlockHeader,
            submit_txs: Vec<DataTransactionHeader>,
            publish_txs: (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
            system_transaction_ledger: Vec<SystemTransactionLedger>,
            _current_timestamp: u128,
            block_reward: Amount<irys_types::storage_pricing::phantoms::Irys>,
            eth_built_payload: &SealedBlock<reth_ethereum_primitives::Block>,
            prev_block_ema_snapshot: &EmaSnapshot,
        ) -> eyre::Result<Option<(Arc<IrysBlockHeader>, Option<AdjustmentStats>)>> {
            self.prod
                .produce_block_without_broadcasting(
                    solution,
                    prev_block_header,
                    submit_txs,
                    publish_txs,
                    system_transaction_ledger,
                    self.invalid_timestamp,
                    block_reward,
                    eth_built_payload,
                    prev_block_ema_snapshot,
                )
                .await
        }
    }

    // 1. Start a node with default config
    let genesis_config = NodeConfig::testing();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    genesis_node.gossip_disable();

    // create a timestamp too far in the future
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let future_timestamp = now_ms
        + genesis_config
            .consensus_config()
            .max_future_timestamp_drift_millis
        + 10_000; // too far into the future

    // Create block with evil strategy and invalid timestamp
    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        invalid_timestamp: future_timestamp,
    };

    let (block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    // Shut down the node
    genesis_node.stop().await;
    Ok(())
}
