use crate::utils::{solution_context, IrysNodeTest};
use eyre::Result;
use irys_actors::block_discovery::BlockTransactions;
use irys_actors::block_validation::{prevalidate_block, PreValidationError};
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_chain::IrysNodeCtx;
use irys_database::SystemLedger;
use irys_domain::{EmaSnapshot, EpochSnapshot};
use irys_types::{
    CommitmentTransaction, DataLedger, DataTransactionHeader, IrysBlockHeader, NodeConfig,
    UnixTimestampMs, H256, U256,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Test context that sets up a genesis node, produces a block, and gathers
/// the parent snapshots needed for prevalidation.
struct PrevalidationTestContext {
    node: IrysNodeTest<IrysNodeCtx>,
    config: NodeConfig,
    block: Arc<IrysBlockHeader>,
    parent_block: IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_ema_snapshot: Arc<EmaSnapshot>,
}

impl PrevalidationTestContext {
    async fn new() -> Result<Self> {
        let config = NodeConfig::testing();
        let node = IrysNodeTest::new_genesis(config.clone()).start().await;

        // Produce a block
        let prod = ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        };
        let (block, _, _, _) = prod
            .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
            .await?
            .unwrap();

        // Get parent info and snapshots
        let parent_block = node
            .get_block_by_height(block.height - 1)
            .await
            .expect("parent block");
        let parent_hash = parent_block.block_hash;
        let (parent_epoch_snapshot, parent_ema_snapshot) = {
            let read = node.node_ctx.block_tree_guard.read();
            (
                read.get_epoch_snapshot(&parent_hash)
                    .expect("epoch snapshot"),
                read.get_ema_snapshot(&parent_hash).expect("ema snapshot"),
            )
        };

        Ok(Self {
            node,
            config,
            block,
            parent_block,
            parent_epoch_snapshot,
            parent_ema_snapshot,
        })
    }

    async fn prevalidate(
        &self,
        block: IrysBlockHeader,
        txs: &BlockTransactions,
    ) -> Result<(), PreValidationError> {
        prevalidate_block(
            block,
            self.parent_block.clone(),
            self.parent_epoch_snapshot.clone(),
            self.node.node_ctx.config.clone(),
            self.node.node_ctx.reward_curve.clone(),
            &self.parent_ema_snapshot,
            txs,
        )
        .await
    }

    async fn stop(self) {
        self.node.stop().await;
    }
}

fn mock_data_txs(count: usize) -> Vec<DataTransactionHeader> {
    (0..count)
        .map(|i| {
            let mut tx = DataTransactionHeader::default();
            tx.id = H256::from_low_u64_be(i as u64);
            tx
        })
        .collect()
}

fn mock_commitment_txs(count: usize) -> Vec<CommitmentTransaction> {
    (0..count)
        .map(|i| {
            let mut tx = CommitmentTransaction::default();
            tx.id = H256::from_low_u64_be(i as u64);
            tx
        })
        .collect()
}

// =============================================================================
// Tests
// =============================================================================

/// This test ensures that if we attempt to submit a block with a timestamp
/// too far in the future, the node rejects it during block prevalidation.
#[tokio::test]
async fn heavy_test_future_block_rejection() -> Result<()> {
    use irys_actors::{
        async_trait, reth_ethereum_primitives, BlockProdStrategy, BlockProducerInner,
    };
    use irys_types::{block_production::SolutionContext, storage_pricing::Amount, AdjustmentStats};
    use reth::{core::primitives::SealedBlock, payload::EthBuiltPayload};

    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_timestamp: UnixTimestampMs,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        fn block_reward(
            &self,
            prev_block_header: &IrysBlockHeader,
        ) -> eyre::Result<Amount<irys_types::storage_pricing::phantoms::Irys>> {
            self.prod.block_reward(prev_block_header)
        }

        async fn create_evm_block(
            &self,
            prev_block_header: &IrysBlockHeader,
            perv_evm_block: &reth_ethereum_primitives::Block,
            mempool: &irys_actors::block_producer::MempoolTxsBundle,
            reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
            _timestamp_ms: UnixTimestampMs,
            solution_hash: H256,
        ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError>
        {
            self.prod
                .create_evm_block(
                    prev_block_header,
                    perv_evm_block,
                    mempool,
                    reward_amount,
                    self.invalid_timestamp,
                    solution_hash,
                )
                .await
        }

        async fn produce_block_without_broadcasting(
            &self,
            solution: &SolutionContext,
            prev_block_header: &IrysBlockHeader,
            mempool_bundle: irys_actors::block_producer::MempoolTxsBundle,
            _current_timestamp: UnixTimestampMs,
            block_reward: Amount<irys_types::storage_pricing::phantoms::Irys>,
            eth_built_payload: &SealedBlock<reth_ethereum_primitives::Block>,
            prev_block_ema_snapshot: &EmaSnapshot,
            treasury: U256,
        ) -> eyre::Result<
            Option<(
                Arc<IrysBlockHeader>,
                Option<AdjustmentStats>,
                BlockTransactions,
            )>,
        > {
            self.prod
                .produce_block_without_broadcasting(
                    solution,
                    prev_block_header,
                    mempool_bundle,
                    self.invalid_timestamp,
                    block_reward,
                    eth_built_payload,
                    prev_block_ema_snapshot,
                    treasury,
                )
                .await
        }
    }

    // Setup node
    let genesis_config = NodeConfig::testing();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    genesis_node.gossip_disable();

    // Create timestamp too far in the future
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let future_timestamp = now_ms
        + genesis_config
            .consensus_config()
            .max_future_timestamp_drift_millis
        + 10_000;

    let block_prod_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        invalid_timestamp: UnixTimestampMs::from_millis(future_timestamp),
    };

    let (block, _, _, _) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Get parent info and snapshots
    let parent_block_header = genesis_node
        .get_block_by_height(block.height - 1)
        .await
        .expect("parent block header");
    let parent_hash = parent_block_header.block_hash;
    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let read = genesis_node.node_ctx.block_tree_guard.read();
        (
            read.get_epoch_snapshot(&parent_hash)
                .expect("parent epoch snapshot"),
            read.get_ema_snapshot(&parent_hash)
                .expect("parent ema snapshot"),
        )
    };

    // Verify prevalidation fails with TimestampTooFarInFuture
    let result = prevalidate_block(
        (*block).clone(),
        parent_block_header,
        parent_epoch_snapshot,
        genesis_node.node_ctx.config.clone(),
        genesis_node.node_ctx.reward_curve.clone(),
        &parent_ema_snapshot,
        &Default::default(),
    )
    .await;

    assert!(
        matches!(
            result,
            Err(PreValidationError::TimestampTooFarInFuture { .. })
        ),
        "expected TimestampTooFarInFuture, got {:?}",
        result
    );

    genesis_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_tampered_vdf_seeds() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Tamper the VDF seeds (make them parent-inconsistent)
    let mut tampered = (*ctx.block).clone();
    let mut seed_bytes = tampered.vdf_limiter_info.seed.0;
    seed_bytes[0] ^= 0xFF;
    tampered.vdf_limiter_info.seed.0 = seed_bytes;

    let result = ctx.prevalidate(tampered, &Default::default()).await;

    let err_msg = result
        .expect_err("pre-validation should fail for tampered VDF seeds")
        .to_string();
    assert!(
        err_msg.contains("Seed data is invalid"),
        "error message should indicate the seed mismatch: {err_msg}"
    );

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_too_many_data_txs() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    let max = ctx.config.consensus_config().mempool.max_data_txs_per_block as usize;
    let mut txs = BlockTransactions::default();
    txs.data_txs
        .insert(DataLedger::Submit, mock_data_txs(max + 1));

    let result = ctx.prevalidate((*ctx.block).clone(), &txs).await;

    match result {
        Err(PreValidationError::TooManyDataTxs { max: m, got }) => {
            assert_eq!(m, max as u64);
            assert_eq!(got, max + 1);
        }
        other => panic!("expected TooManyDataTxs, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_too_many_commitment_txs() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Check if block has a commitment ledger (needed for this test)
    let has_commitment_ledger = ctx
        .block
        .system_ledgers
        .iter()
        .any(|l| l.ledger_id == SystemLedger::Commitment);

    if !has_commitment_ledger {
        // Skip if block doesn't have commitment ledger (not at epoch boundary)
        ctx.stop().await;
        return Ok(());
    }

    let max = ctx
        .config
        .consensus_config()
        .mempool
        .max_commitment_txs_per_block as usize;
    let txs = BlockTransactions {
        commitment_txs: mock_commitment_txs(max + 1),
        ..Default::default()
    };

    let result = ctx.prevalidate((*ctx.block).clone(), &txs).await;

    match result {
        Err(PreValidationError::TooManyCommitmentTxs { max: m, got }) => {
            assert_eq!(m, max as u64);
            assert_eq!(got, max + 1);
        }
        other => panic!("expected TooManyCommitmentTxs, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_mismatched_tx_ids() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    let submit_ledger = ctx
        .block
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Submit)
        .expect("submit ledger");

    if submit_ledger.tx_ids.0.is_empty() {
        // Skip if no submit txs in block
        ctx.stop().await;
        return Ok(());
    }

    let expected_id = submit_ledger.tx_ids.0[0];
    let actual_id = H256::from_low_u64_be(999);

    let mut tx = DataTransactionHeader::default();
    tx.id = actual_id;

    let mut txs = BlockTransactions::default();
    txs.data_txs.insert(DataLedger::Submit, vec![tx]);

    let result = ctx.prevalidate((*ctx.block).clone(), &txs).await;

    match result {
        Err(PreValidationError::TransactionIdMismatch { expected, actual }) => {
            assert_eq!(expected, expected_id);
            assert_eq!(actual, actual_id);
        }
        other => panic!("expected TransactionIdMismatch, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_missing_transactions() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    let submit_ledger = ctx
        .block
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Submit)
        .expect("submit ledger");

    if submit_ledger.tx_ids.0.is_empty() {
        // Skip if no submit txs in block
        ctx.stop().await;
        return Ok(());
    }

    let expected_ids = submit_ledger.tx_ids.0.clone();

    let mut txs = BlockTransactions::default();
    txs.data_txs.insert(DataLedger::Submit, vec![]);

    let result = ctx.prevalidate((*ctx.block).clone(), &txs).await;

    match result {
        Err(PreValidationError::MissingTransactions(missing)) => {
            assert_eq!(missing, expected_ids);
        }
        other => panic!("expected MissingTransactions, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_prevalidation_rejects_unexpected_commitments() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Remove any commitment ledger from block
    let mut block = (*ctx.block).clone();
    block
        .system_ledgers
        .retain(|l| l.ledger_id != SystemLedger::Commitment);

    let txs = BlockTransactions {
        commitment_txs: vec![CommitmentTransaction::default()],
        ..Default::default()
    };

    let result = ctx.prevalidate(block, &txs).await;

    assert!(
        matches!(
            result,
            Err(PreValidationError::UnexpectedCommitmentTransactions)
        ),
        "expected UnexpectedCommitmentTransactions, got {:?}",
        result
    );

    ctx.stop().await;
    Ok(())
}
