use crate::utils::{IrysNodeTest, solution_context};
use eyre::Result;
use irys_actors::block_tree_service::BlockTreeServiceMessage;
use irys_actors::block_validation::{PreValidationError, prevalidate_block};
use irys_actors::test_helpers::build_test_service_senders;
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_chain::IrysNodeCtx;
use irys_domain::{EmaSnapshot, EpochSnapshot};
use irys_types::{
    BoundedFee, CommitmentTransaction, Config, ConsensusOptions, DataLedger, DataTransactionHeader,
    H256, IrysBlockHeader, IrysTransactionCommon as _, NodeConfig, SealedBlock, SystemLedger, U256,
    UnixTimestampMs,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Test context that sets up a genesis node, produces a block, and gathers
/// the parent snapshots needed for prevalidation.
struct PrevalidationTestContext {
    node: IrysNodeTest<IrysNodeCtx>,
    config: NodeConfig,
    block: Arc<SealedBlock>,
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
        let (block, _, _) = prod
            .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
            .await?
            .unwrap();

        // Get parent info and snapshots
        let parent_block = node
            .get_block_by_height(block.header().height - 1)
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

    async fn prevalidate(&self, block: &SealedBlock) -> Result<(), PreValidationError> {
        let pool = Arc::new(irys_vdf::build_verification_pool(
            &self.node.node_ctx.config.vdf,
        ));
        prevalidate_block(
            block,
            &self.parent_block,
            self.parent_epoch_snapshot.clone(),
            self.node.node_ctx.config.clone(),
            pool,
            self.node.node_ctx.reward_curve.clone(),
            &self.parent_ema_snapshot,
        )
        .await
    }

    async fn stop(self) {
        self.node.stop().await;
    }
}

fn mock_data_txs(count: usize) -> Vec<DataTransactionHeader> {
    use irys_types::{IrysTransactionCommon as _, NodeConfig};

    let config = NodeConfig::testing();
    let signer = config.signer();
    let consensus = config.consensus_config();

    (0..count)
        .map(|i| {
            let mut tx = DataTransactionHeader::new(&consensus);
            // Make each transaction unique by setting different data_root
            tx.data_root = H256::from_low_u64_be(i as u64);
            tx.sign(&signer).expect("Failed to sign test transaction")
        })
        .collect()
}

fn mock_commitment_txs(count: usize) -> Vec<CommitmentTransaction> {
    use irys_types::{IrysTransactionCommon as _, NodeConfig};

    let config = NodeConfig::testing();
    let signer = config.signer();
    let consensus = config.consensus_config();

    (0..count)
        .map(|i| {
            // Make each transaction unique by using different anchor
            let anchor = H256::from_low_u64_be(i as u64);
            let tx = CommitmentTransaction::new_stake(&consensus, anchor);
            tx.sign(&signer).expect("Failed to sign test transaction")
        })
        .collect()
}

// =============================================================================
// Tests
// =============================================================================

/// This test ensures that if we attempt to submit a block with a timestamp
/// too far in the future, the node rejects it during block prevalidation.
#[tokio::test]
async fn test_future_block_rejection() -> Result<()> {
    use irys_actors::{
        BlockProdStrategy, BlockProducerInner, async_trait, reth_ethereum_primitives,
    };
    use irys_types::{AdjustmentStats, block_production::SolutionContext, storage_pricing::Amount};
    use reth::{core::primitives::SealedBlock as RethSealedBlock, payload::EthBuiltPayload};

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
            eth_built_payload: &RethSealedBlock<reth_ethereum_primitives::Block>,
            prev_block_ema_snapshot: &EmaSnapshot,
            treasury: U256,
        ) -> eyre::Result<Option<(Arc<SealedBlock>, Option<AdjustmentStats>)>> {
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

    let (block, _, _) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Get parent info and snapshots
    let parent_block_header = genesis_node
        .get_block_by_height(block.header().height - 1)
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
    let pool = Arc::new(irys_vdf::build_verification_pool(
        &genesis_node.node_ctx.config.vdf,
    ));
    let result = prevalidate_block(
        &block,
        &parent_block_header,
        parent_epoch_snapshot,
        genesis_node.node_ctx.config.clone(),
        pool,
        genesis_node.node_ctx.reward_curve.clone(),
        &parent_ema_snapshot,
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
async fn test_prevalidation_rejects_tampered_vdf_seeds() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Tamper the VDF seeds (make them parent-inconsistent)
    let mut tampered_header = (**ctx.block.header()).clone();
    let mut seed_bytes = tampered_header.vdf_limiter_info.seed.0;
    seed_bytes[0] ^= 0xFF;
    tampered_header.vdf_limiter_info.seed.0 = seed_bytes;

    // Re-sign the header after tampering
    ctx.config
        .signer()
        .sign_block_header(&mut tampered_header)?;

    // Reconstruct SealedBlock with updated body.block_hash
    let mut tampered_body = ctx.block.to_block_body();
    tampered_body.block_hash = tampered_header.block_hash;
    let tampered_block = Arc::new(SealedBlock::new(tampered_header, tampered_body)?);

    let result = ctx.prevalidate(&tampered_block).await;

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
async fn test_prevalidation_rejects_too_many_data_txs() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    let max = ctx.config.consensus_config().mempool.max_data_txs_per_block as usize;
    // Create excessive transactions
    let excessive_txs = mock_data_txs(max + 1);

    // Construct new body with excessive transactions
    let mut body = ctx.block.to_block_body();
    body.data_transactions = excessive_txs.clone();

    // Update header to match new transactions (so SealedBlock accepts it)
    let mut header = (**ctx.block.header()).clone();
    use irys_types::H256List;
    let tx_ids: H256List = H256List(excessive_txs.iter().map(|tx| tx.id).collect());

    // Update Submit ledger in header
    let ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    ledger.tx_ids = tx_ids;

    ctx.config.signer().sign_block_header(&mut header)?;

    // Update body.block_hash to match the re-signed header
    body.block_hash = header.block_hash;

    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    let result = ctx.prevalidate(&bad_block).await;

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

/// A block whose ledger `tx_root` does not match the folded `(data_root, prefix_hash)`
/// recompute of its included transactions must be rejected with `TxRootMismatch`. This is
/// the consensus enforcement that authenticates each tx's `prefix_hash` via the
/// block-signature-sealed `tx_root`.
#[tokio::test]
async fn test_prevalidation_rejects_tx_root_mismatch() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Tamper the Submit ledger's tx_root so it no longer matches the recompute of the
    // ledger's transactions (genesis-produced block has an empty Submit ledger, which
    // folds to H256::zero()), then re-sign so the block is otherwise well-formed.
    let mut header = (**ctx.block.header()).clone();
    let bogus = H256::from_low_u64_be(0xDEAD_BEEF);
    let ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    assert_ne!(
        ledger.tx_root, bogus,
        "test value must differ from real root"
    );
    ledger.tx_root = bogus;

    ctx.config.signer().sign_block_header(&mut header)?;
    let mut body = ctx.block.to_block_body();
    body.block_hash = header.block_hash;
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    match ctx.prevalidate(&bad_block).await {
        Err(PreValidationError::TxRootMismatch { ledger_id, .. }) => {
            assert_eq!(ledger_id, DataLedger::Submit as u32);
        }
        other => panic!("expected TxRootMismatch, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

/// A block that includes a `data_size == 0` data tx must be rejected with `ZeroSizeDataTx`.
/// A zero-size tx stores no data and would inject a zero-width leaf into the ledger
/// `tx_root` tree, colliding start offsets with the next tx and breaking PoA owning-tx
/// recovery — so consensus refuses it at the authoritative gate (`prevalidate_block`), not
/// only at mempool ingress. End-to-end companion to the `block_validation` unit tests
/// `first_zero_size_data_tx_flags_zero_size_txs` / `poa_owner_recovery_skips_zero_width_leaves`.
#[tokio::test]
async fn test_prevalidation_rejects_zero_size_data_tx() -> Result<()> {
    use irys_types::{DataTransactionLedger, H256List, IrysTransactionCommon as _};

    let ctx = PrevalidationTestContext::new().await?;

    // A signed, otherwise-valid Submit data tx with data_size == 0 (the honest builder
    // never emits one, but a hand-crafted peer tx is accepted by structural validation).
    let consensus = ctx.config.consensus_config();
    let signer = ctx.config.signer();
    let mut ztx = DataTransactionHeader::new(&consensus);
    ztx.data_size = 0;
    ztx.ledger_id = DataLedger::Submit as u32;
    let ztx = ztx.sign(&signer)?;

    // Splice it into the block's Submit ledger (tx_ids + the matching folded tx_root, so the
    // tx_root check would pass — it's the zero-size guard that must fire) and re-sign.
    let mut header = (**ctx.block.header()).clone();
    let ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    ledger.tx_ids = H256List(vec![ztx.id()]);
    ledger.tx_root = DataTransactionLedger::compute_tx_root(std::slice::from_ref(&ztx));
    ctx.config.signer().sign_block_header(&mut header)?;

    // The sealed block's transactions are derived from the body, so the tx must live there too.
    let mut body = ctx.block.to_block_body();
    body.data_transactions = vec![ztx.clone()];
    body.block_hash = header.block_hash;
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    match ctx.prevalidate(&bad_block).await {
        Err(PreValidationError::ZeroSizeDataTx { ledger_id, txid }) => {
            assert_eq!(ledger_id, DataLedger::Submit as u32);
            assert_eq!(txid, ztx.id());
        }
        other => panic!("expected ZeroSizeDataTx, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

/// A block that includes a data tx whose committed `prefix_size` exceeds `data_size` must be
/// rejected with `PrefixSizeExceedsDataSize`. `prefix_hash` commits to the first `prefix_size`
/// data bytes, so `prefix_size > data_size` is a structurally impossible claim — consensus
/// refuses it at the authoritative gate (`prevalidate_block`), not only at mempool ingress.
/// Companion to the `block_validation` unit test `first_prefix_size_exceeds_data_size_flags_oversized_prefix`.
#[tokio::test]
async fn test_prevalidation_rejects_prefix_size_exceeds_data_size() -> Result<()> {
    use irys_types::{DataTransactionLedger, H256List, IrysTransactionCommon as _};

    let ctx = PrevalidationTestContext::new().await?;

    // A signed, otherwise-valid Submit data tx whose prefix_size (101) exceeds data_size (100).
    // The honest builder never emits one, but a hand-crafted peer tx passes structural validation.
    let consensus = ctx.config.consensus_config();
    let signer = ctx.config.signer();
    let mut ptx = DataTransactionHeader::new(&consensus);
    ptx.data_size = 100;
    ptx.prefix_size = 101;
    ptx.ledger_id = DataLedger::Submit as u32;
    let ptx = ptx.sign(&signer)?;

    // Splice it into the block's Submit ledger (tx_ids + the matching folded tx_root, so the
    // tx_root check would pass — it's the prefix_size guard that must fire) and re-sign.
    let mut header = (**ctx.block.header()).clone();
    let ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    ledger.tx_ids = H256List(vec![ptx.id()]);
    ledger.tx_root = DataTransactionLedger::compute_tx_root(std::slice::from_ref(&ptx));
    ctx.config.signer().sign_block_header(&mut header)?;

    // The sealed block's transactions are derived from the body, so the tx must live there too.
    let mut body = ctx.block.to_block_body();
    body.data_transactions = vec![ptx.clone()];
    body.block_hash = header.block_hash;
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    match ctx.prevalidate(&bad_block).await {
        Err(PreValidationError::PrefixSizeExceedsDataSize {
            ledger_id,
            txid,
            prefix_size,
            data_size,
        }) => {
            assert_eq!(ledger_id, DataLedger::Submit as u32);
            assert_eq!(txid, ptx.id());
            assert_eq!(prefix_size, 101);
            assert_eq!(data_size, 100);
        }
        other => panic!("expected PrefixSizeExceedsDataSize, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

/// A block that includes a data tx carrying a foreign `chain_id` must be rejected with
/// `DataTxChainIdMismatch`. `chain_id` is a signed field, so a tx signed for another chain
/// must not be admitted into a block on this one — consensus refuses it at the authoritative
/// gate (`prevalidate_block`), not only at mempool ingress.
#[tokio::test]
async fn test_prevalidation_rejects_data_tx_chain_id_mismatch() -> Result<()> {
    use irys_types::{DataTransactionLedger, H256List, IrysTransactionCommon as _};

    let ctx = PrevalidationTestContext::new().await?;

    // A signed Submit data tx whose chain_id is foreign (node chain_id + 1). sign() signs over
    // the tx's own chain_id, so the signature is valid yet the chain_id is wrong.
    let consensus = ctx.config.consensus_config();
    let signer = ctx.config.signer();
    let mut ftx = DataTransactionHeader::new(&consensus);
    ftx.data_size = 100;
    ftx.ledger_id = DataLedger::Submit as u32;
    ftx.chain_id = consensus.chain_id + 1;
    let ftx = ftx.sign(&signer)?;

    // Splice it into the block's Submit ledger (tx_ids + matching folded tx_root) and re-sign.
    let mut header = (**ctx.block.header()).clone();
    let ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    ledger.tx_ids = H256List(vec![ftx.id()]);
    ledger.tx_root = DataTransactionLedger::compute_tx_root(std::slice::from_ref(&ftx));
    ctx.config.signer().sign_block_header(&mut header)?;

    let mut body = ctx.block.to_block_body();
    body.data_transactions = vec![ftx.clone()];
    body.block_hash = header.block_hash;
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    match ctx.prevalidate(&bad_block).await {
        Err(PreValidationError::DataTxChainIdMismatch {
            ledger_id,
            txid,
            expected,
            actual,
        }) => {
            assert_eq!(ledger_id, DataLedger::Submit as u32);
            assert_eq!(txid, ftx.id());
            assert_eq!(expected, consensus.chain_id);
            assert_eq!(actual, consensus.chain_id + 1);
        }
        other => panic!("expected DataTxChainIdMismatch, got {:?}", other),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn test_prevalidation_rejects_submit_targeted_tx() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Create a tx that incorrectly targets Submit (otherwise valid + well-funded)
    let mut bad_tx = DataTransactionHeader::new(&ctx.config.consensus_config());
    bad_tx.data_root = H256::from_low_u64_be(42);
    bad_tx.data_size = 0;
    bad_tx.term_fee = BoundedFee::from_u64(1_000_000_000_000_000_000);
    bad_tx.perm_fee = Some(BoundedFee::from_u64(1_000_000_000_000_000_000));
    bad_tx.ledger_id = DataLedger::Submit as u32;
    bad_tx = bad_tx
        .sign(&ctx.config.signer())
        .expect("Failed to sign test transaction");

    // Build a block that is otherwise valid, but includes the Submit-targeted tx
    let mut body = ctx.block.to_block_body();
    body.data_transactions = vec![bad_tx.clone()];

    let mut header = (**ctx.block.header()).clone();
    use irys_types::H256List;

    let publish_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .expect("Publish ledger should exist");
    publish_ledger.tx_ids = H256List(Vec::new());

    let submit_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    submit_ledger.tx_ids = H256List(vec![bad_tx.id]);

    ctx.config.signer().sign_block_header(&mut header)?;
    body.block_hash = header.block_hash;

    let bad_block = Arc::new(SealedBlock::new(header, body)?);
    {
        let mut tree = ctx.node.node_ctx.block_tree_guard.write();
        let parent_hash = bad_block.header().previous_block_hash;
        let commitment_snapshot = tree
            .get_commitment_snapshot(&parent_hash)
            .expect("parent commitment snapshot");
        let epoch_snapshot = tree
            .get_epoch_snapshot(&parent_hash)
            .expect("parent epoch snapshot");
        let ema_snapshot = tree
            .get_ema_snapshot(&parent_hash)
            .expect("parent ema snapshot");
        tree.add_block(
            &bad_block,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
        )?;
    }

    let (service_senders, mut service_receivers) = build_test_service_senders();
    let block_tree_guard = ctx.node.node_ctx.block_tree_guard.clone();
    tokio::spawn(async move {
        while let Some(msg) = service_receivers.block_tree.recv().await {
            let BlockTreeServiceMessage::GetBlockTreeReadGuard { response } = msg.inner else {
                continue;
            };
            let _ = response.send(block_tree_guard.clone());
        }
    });

    let transactions = bad_block.transactions();

    // Use a config override to limit anchor expiry depth for this test.
    let mut consensus = ctx.config.consensus_config();
    consensus.mempool.tx_anchor_expiry_depth = 0;
    let mut node_config = ctx.config.clone();
    node_config.consensus = ConsensusOptions::Custom(consensus);
    let config_override = Config::new_with_random_peer_id(node_config);

    // Caller fetches parent epoch + EMA snapshots once and passes them in;
    // `data_txs_are_valid` derives `cascade_active` from the epoch snapshot.
    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let parent_hash = bad_block.header().previous_block_hash;
        (
            tree.get_epoch_snapshot(&parent_hash)
                .expect("parent epoch snapshot"),
            tree.get_ema_snapshot(&parent_hash)
                .expect("parent ema snapshot"),
        )
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        transactions,
        parent_epoch_snapshot,
        parent_ema_snapshot,
    )
    .await;

    match result {
        Err(PreValidationError::InvalidLedgerIdForTx {
            tx_id,
            expected,
            actual,
        }) => {
            assert_eq!(tx_id, bad_tx.id);
            assert_eq!(expected, DataLedger::Publish as u32);
            assert_eq!(actual, DataLedger::Submit as u32);
        }
        other => panic!(
            "expected InvalidLedgerIdForTx for Submit-targeted tx, got {:?}",
            other
        ),
    }

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
async fn test_prevalidation_rejects_too_many_commitment_txs() -> Result<()> {
    let ctx = PrevalidationTestContext::new().await?;

    // Check if block has a commitment ledger (needed for this test)
    let has_commitment_ledger = ctx
        .block
        .header()
        .system_ledgers
        .iter()
        .any(|l| l.ledger_id == SystemLedger::Commitment as u32);

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

    let excessive_txs = mock_commitment_txs(max + 1);

    let mut body = ctx.block.to_block_body();
    body.commitment_transactions = excessive_txs.clone();

    let mut header = (**ctx.block.header()).clone();
    use irys_types::H256List;
    let tx_ids: H256List = H256List(
        excessive_txs
            .iter()
            .map(irys_types::CommitmentTransaction::id)
            .collect(),
    );

    let ledger = header
        .system_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == SystemLedger::Commitment as u32)
        .expect("Commitment ledger should exist");
    ledger.tx_ids = tx_ids;

    // Re-sign the header after modification
    ctx.config.signer().sign_block_header(&mut header)?;

    // Update body.block_hash to match the re-signed header
    body.block_hash = header.block_hash;

    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    let result = ctx.prevalidate(&bad_block).await;

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

/// Regression for the double-publish bypass via the canonical-DB fallback at
/// `data_txs_are_valid`'s `Searching { ledger_current: Publish }` arm.
///
/// **Exploit shape:** a tx has already been canonically Submit-confirmed and
/// Publish-promoted, but the prior Publish block sits outside
/// `anchor_expiry_depth` from the new block's parent.  The in-window walk's
/// `Found { historical: Publish, current: Publish }` arm — which would emit
/// `PublishTxAlreadyIncluded` — never fires because the prior Publish is
/// past the scan window.  Without a `promoted_height` check in the fallback,
/// the canonical-DB lookup would accept the tx (warns + continues) and the
/// peer would succeed in re-publishing.  The fix is to reject when the
/// canonical lookup content-verifies a prior promotion below the walk
/// window.
///
/// **Test driver:** mine to height 3, then validate a sibling candidate at
/// height 3 (parent = the real block 2) under an override of
/// `tx_anchor_expiry_depth = 0` and `block_tree_depth = 1`.  The by-hash
/// walk (depth `max(0, 1) = 1`) then covers exactly the real, tx-free block
/// 2 on the candidate's own branch, so the tx stays `Searching` and the
/// canonical-DB fallback owns the heights below the window.  The canonical
/// prior is a fabricated finalized block at height 1 — stored as a real
/// header carrying the tx in both Submit and Publish, and pointed at by
/// `MigratedBlockHashes[1]` — so both canonical lookups content-verify
/// against it.  Expect `Err(PublishTxAlreadyIncluded { tx_id, block_hash })`
/// naming the fabricated promoted block.
#[tokio::test]
async fn test_prevalidation_rejects_doubly_published_tx_via_fallback() -> Result<()> {
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::tables::MigratedBlockHashes;
    use irys_database::{
        insert_block_header, insert_tx_header, set_data_tx_included_height,
        set_data_tx_promoted_height,
    };
    use irys_types::{BlockBody, H256List};
    use reth_db::transaction::DbTxMut as _;

    let ctx = PrevalidationTestContext::new().await?;

    // Extend the chain so a content-verified canonical prior can sit BELOW
    // the by-hash walk window.  `ctx.block` was produced without broadcast
    // and never lands on-chain, so the tip is still genesis; mine three real
    // blocks to put the tip at height 3.  With the walk-depth override below
    // (`max(tx_anchor_expiry_depth = 0, block_tree_depth = 1) = 1`) the walk
    // for a height-3 candidate covers exactly the real, tx-free block 2,
    // leaving height 1 to the canonical-DB fallback.
    ctx.node.mine_blocks(3).await?;
    let real_tip = ctx.node.get_block_by_height(3).await?;

    // A Publish-ledger tx that will impersonate "already canonically
    // promoted" via the fabricated finalized prior seeded below.
    let mut tx = DataTransactionHeader::new(&ctx.config.consensus_config());
    tx.data_root = H256::from_low_u64_be(0x517A_1EBA_DD15);
    tx.data_size = 0;
    tx.term_fee = BoundedFee::from_u64(1_000_000_000_000_000_000);
    tx.perm_fee = Some(BoundedFee::from_u64(1_000_000_000_000_000_000));
    tx.ledger_id = DataLedger::Publish as u32;
    let tx = tx
        .sign(&ctx.config.signer())
        .expect("Failed to sign test transaction");

    // Fabricated finalized prior at height 1, NOT on the candidate's
    // ancestry: a real header carrying the tx in BOTH Submit and Publish
    // (same-block promotion), pointed at by `MigratedBlockHashes[1]`.
    // `canonical_submit_height` / `canonical_promoted_height` load the block
    // MBH names at the hinted height and content-verify ledger membership,
    // so the header must genuinely carry the tx — a bare metadata hint would
    // read as a stranded row and resolve to None.  (At tip height 3 with
    // testing `block_migration_depth = 6` no post-genesis block has
    // migrated, so the MBH[1] write collides with nothing.)  Below the walk
    // window this MBH-keyed, content-verified row IS the canonical answer —
    // exactly the regime the fallback is designed for.
    let prior_height = 1_u64;
    let mut prior_block = IrysBlockHeader::new_mock_header();
    prior_block.height = prior_height;
    prior_block.block_hash = H256::random();
    prior_block.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx.id]);
    prior_block.data_ledgers[DataLedger::Publish].tx_ids = H256List(vec![tx.id]);
    let promoted_block_hash = prior_block.block_hash;

    ctx.node.node_ctx.db.update_eyre(|db_tx| {
        insert_tx_header(db_tx, &tx)?;
        set_data_tx_included_height(db_tx, &tx.id, prior_height)?;
        set_data_tx_promoted_height(db_tx, &tx.id, prior_height)?;
        insert_block_header(db_tx, &prior_block)?;
        db_tx.put::<MigratedBlockHashes>(prior_height, promoted_block_hash)?;
        Ok(())
    })?;

    // Build a sibling candidate at height 3 (parent = the real block 2)
    // whose Publish ledger re-includes `tx`: clone the real height-3 header
    // as the structural baseline and swap the data ledgers.
    let mut header = real_tip.clone();
    let publish_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .expect("Publish ledger should exist");
    publish_ledger.tx_ids = H256List(vec![tx.id]);
    let submit_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    submit_ledger.tx_ids = H256List(Vec::new());

    ctx.config.signer().sign_block_header(&mut header)?;
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![tx.clone()],
        commitment_transactions: Vec::new(),
    };
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    {
        let mut tree = ctx.node.node_ctx.block_tree_guard.write();
        let parent_hash = bad_block.header().previous_block_hash;
        let commitment_snapshot = tree
            .get_commitment_snapshot(&parent_hash)
            .expect("parent commitment snapshot");
        let epoch_snapshot = tree
            .get_epoch_snapshot(&parent_hash)
            .expect("parent epoch snapshot");
        let ema_snapshot = tree
            .get_ema_snapshot(&parent_hash)
            .expect("parent ema snapshot");
        tree.add_block(
            &bad_block,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
        )?;
    }

    let (service_senders, mut service_receivers) = build_test_service_senders();
    let block_tree_guard = ctx.node.node_ctx.block_tree_guard.clone();
    tokio::spawn(async move {
        while let Some(msg) = service_receivers.block_tree.recv().await {
            let BlockTreeServiceMessage::GetBlockTreeReadGuard { response } = msg.inner else {
                continue;
            };
            let _ = response.send(block_tree_guard.clone());
        }
    });

    // Walk-depth override: `max(tx_anchor_expiry_depth = 0, block_tree_depth
    // = 1) = 1`, so the by-hash walk for the height-3 candidate covers
    // exactly the real, tx-free block 2 on its own branch — the tx stays
    // `Searching` and the canonical-DB fallback (which owns the heights
    // below the walk window, where the fabricated prior at height 1 lives)
    // is the path this regression exists to cover.
    let mut consensus = ctx.config.consensus_config();
    consensus.mempool.tx_anchor_expiry_depth = 0;
    consensus.block_tree_depth = 1;
    let mut node_config = ctx.config.clone();
    node_config.consensus = ConsensusOptions::Custom(consensus);
    let config_override = Config::new_with_random_peer_id(node_config);

    // Match the new `data_txs_are_valid` signature (parent_epoch_snapshot
    // + parent_ema_snapshot replaces the old `cascade_active: bool` arg —
    // see the sibling call site in this file).
    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let parent_hash = bad_block.header().previous_block_hash;
        (
            tree.get_epoch_snapshot(&parent_hash)
                .expect("parent epoch snapshot"),
            tree.get_ema_snapshot(&parent_hash)
                .expect("parent ema snapshot"),
        )
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        bad_block.transactions(),
        parent_epoch_snapshot,
        parent_ema_snapshot,
    )
    .await;

    match result {
        Err(PreValidationError::PublishTxAlreadyIncluded { tx_id, block_hash }) => {
            assert_eq!(tx_id, tx.id, "rejection must name the doubly-published tx");
            assert_eq!(
                block_hash, promoted_block_hash,
                "rejection must surface the canonical promoted-block hash from MigratedBlockHashes (= the fabricated height-1 prior in this fixture)"
            );
        }
        other => panic!(
            "expected PublishTxAlreadyIncluded via canonical-DB fallback, got {:?}",
            other
        ),
    }

    ctx.stop().await;
    Ok(())
}

/// Symmetric companion to `test_prevalidation_rejects_doubly_published_tx_via_fallback`.
///
/// **Production fault this corresponds to:** a tx had
/// `IrysDataTxMetadata.promoted_height = 24688` stranded from an orphaned
/// local-tip write, while `MigratedBlockHashes[24688] = None` because the
/// writer block was never migrated.  Pre-fix the validator's DB fallback
/// raised `BlockBoundsLookupError` → NodeFault → restart on the MBH
/// mismatch.  Post-fix, `canonical_promoted_height` returns `None` for
/// the unverifiable hint and the fallback arm in `data_txs_are_valid`
/// falls through to warn-and-accept the current Publish as a legitimate
/// first promotion.
///
/// **Harness scope.** The fixture mines to height 3 and validates a sibling
/// candidate at height 3 under a walk-depth override
/// (`max(tx_anchor_expiry_depth = 0, block_tree_depth = 1) = 1`), so the
/// by-hash walk covers exactly the real, tx-free block 2 and the tx stays
/// `Searching`.  The prior Submit is a fabricated finalized block at height
/// 1 (below the walk window, content-verified via `MigratedBlockHashes[1]`)
/// so the fallback's Submit gate passes; the stranded `promoted_height = 3`
/// exceeds the fallback's height cap, so the hint is stripped by the
/// `max_height` check before MBH is even consulted.  The exact "MBH
/// disagreement" strip branch from the production fault is covered directly
/// at the helper boundary by
/// `database::canonical_height_tests::promoted_height_returns_none_when_mbh_disagrees`;
/// both branches surface `canonical_promoted_height = None` to the
/// validator, so the downstream behaviour is shared.
///
/// **What this asserts.** That the validator does NOT regress to any of:
/// `BlockBoundsLookupError` (pre-fix panic-then-restart),
/// `PublishTxAlreadyIncluded` (false rejection of a legitimate first
/// promotion), or `PublishTxMissingPriorSubmit` (the Submit branch of this
/// same fallback arm — control flow must reach the promoted-height check,
/// otherwise the test exercises a different arm than the production
/// fault).  Post-fallback-warn, `data_txs_are_valid` continues into
/// ingress-proof / chunk-availability checks that this minimal fixture
/// intentionally doesn't satisfy — any failure from those downstream
/// phases is unrelated to the regression under test and is tolerated.
#[test_log::test(tokio::test)]
async fn test_prevalidation_accepts_publish_when_promoted_height_hint_stripped() -> Result<()> {
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::tables::MigratedBlockHashes;
    use irys_database::{
        insert_block_header, insert_tx_header, set_data_tx_included_height,
        set_data_tx_promoted_height,
    };
    use irys_types::{BlockBody, H256List};
    use reth_db::transaction::DbTxMut as _;

    let ctx = PrevalidationTestContext::new().await?;

    // Same geometry as the sibling regression test: extend the chain (the
    // context's unbroadcast block never lands on-chain, so mine three real
    // blocks to reach height 3) so a content-verified prior Submit can sit
    // BELOW the by-hash walk window (which, under the override below, covers
    // exactly the real block 2 for a height-3 candidate).
    ctx.node.mine_blocks(3).await?;
    let real_tip = ctx.node.get_block_by_height(3).await?;

    // A Publish-ledger tx whose `IrysDataTxMetadata` will carry a
    // `promoted_height` the canonical helper must strip.
    let mut tx = DataTransactionHeader::new(&ctx.config.consensus_config());
    tx.data_root = H256::from_low_u64_be(0xDEAD_BEEF_C0DE);
    tx.data_size = 0;
    tx.term_fee = BoundedFee::from_u64(1_000_000_000_000_000_000);
    tx.perm_fee = Some(BoundedFee::from_u64(1_000_000_000_000_000_000));
    tx.ledger_id = DataLedger::Publish as u32;
    let tx = tx
        .sign(&ctx.config.signer())
        .expect("Failed to sign test transaction");

    // Canonical prior Submit: a fabricated finalized block at height 1
    // (below the walk window) carrying the tx in its Submit ledger ONLY,
    // pointed at by `MigratedBlockHashes[1]`, so `canonical_submit_height`
    // content-verifies it and the fallback's Submit gate passes.  The
    // stranded `promoted_height = 3` exceeds the fallback's height cap, so
    // the canonical helper strips it via the `max_height` check (before MBH
    // is even consulted — orthogonal to the MBH-None strip but producing
    // the same stripped metadata).
    let prior_submit_height = 1_u64;
    let stranded_promote_height = 3_u64;
    let mut prior_block = IrysBlockHeader::new_mock_header();
    prior_block.height = prior_submit_height;
    prior_block.block_hash = H256::random();
    prior_block.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx.id]);
    let prior_block_hash = prior_block.block_hash;

    ctx.node.node_ctx.db.update_eyre(|db_tx| {
        insert_tx_header(db_tx, &tx)?;
        set_data_tx_included_height(db_tx, &tx.id, prior_submit_height)?;
        set_data_tx_promoted_height(db_tx, &tx.id, stranded_promote_height)?;
        insert_block_header(db_tx, &prior_block)?;
        db_tx.put::<MigratedBlockHashes>(prior_submit_height, prior_block_hash)?;
        Ok(())
    })?;

    // Same block shape as the sibling regression test: re-include the tx on
    // the Publish ledger of a sibling candidate at height 3 (parent = the
    // real block 2).
    let mut header = real_tip.clone();
    let publish_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .expect("Publish ledger should exist");
    publish_ledger.tx_ids = H256List(vec![tx.id]);
    let submit_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    submit_ledger.tx_ids = H256List(Vec::new());

    ctx.config.signer().sign_block_header(&mut header)?;
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![tx.clone()],
        commitment_transactions: Vec::new(),
    };
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    {
        let mut tree = ctx.node.node_ctx.block_tree_guard.write();
        let parent_hash = bad_block.header().previous_block_hash;
        let commitment_snapshot = tree
            .get_commitment_snapshot(&parent_hash)
            .expect("parent commitment snapshot");
        let epoch_snapshot = tree
            .get_epoch_snapshot(&parent_hash)
            .expect("parent epoch snapshot");
        let ema_snapshot = tree
            .get_ema_snapshot(&parent_hash)
            .expect("parent ema snapshot");
        tree.add_block(
            &bad_block,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
        )?;
    }

    let (service_senders, mut service_receivers) = build_test_service_senders();
    let block_tree_guard = ctx.node.node_ctx.block_tree_guard.clone();
    tokio::spawn(async move {
        while let Some(msg) = service_receivers.block_tree.recv().await {
            let BlockTreeServiceMessage::GetBlockTreeReadGuard { response } = msg.inner else {
                continue;
            };
            let _ = response.send(block_tree_guard.clone());
        }
    });

    // Walk-depth override: `max(tx_anchor_expiry_depth = 0, block_tree_depth
    // = 1) = 1`, so the by-hash walk for the height-3 candidate covers
    // exactly the real, tx-free block 2 — the tx stays `Searching` and the
    // validator takes the canonical-DB fallback path this regression
    // exercises.
    let mut consensus = ctx.config.consensus_config();
    consensus.mempool.tx_anchor_expiry_depth = 0;
    consensus.block_tree_depth = 1;
    let mut node_config = ctx.config.clone();
    node_config.consensus = ConsensusOptions::Custom(consensus);
    let config_override = Config::new_with_random_peer_id(node_config);

    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let parent_hash = bad_block.header().previous_block_hash;
        (
            tree.get_epoch_snapshot(&parent_hash)
                .expect("parent epoch snapshot"),
            tree.get_ema_snapshot(&parent_hash)
                .expect("parent ema snapshot"),
        )
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        bad_block.transactions(),
        parent_epoch_snapshot,
        parent_ema_snapshot,
    )
    .await;

    match result {
        Ok(()) => {
            // Stripped metadata + minimal fixture happened to satisfy
            // every downstream check too — the strongest possible
            // outcome.
        }
        Err(PreValidationError::BlockBoundsLookupError(msg)) => {
            panic!(
                "regression: validator surfaced BlockBoundsLookupError \
                 instead of treating the unverifiable promoted_height as \
                 'no canonical promotion' via canonical_promoted_height (msg: {msg})"
            );
        }
        Err(PreValidationError::PublishTxAlreadyIncluded { tx_id, block_hash }) => {
            panic!(
                "regression: validator rejected a legitimate first \
                 promotion as PublishTxAlreadyIncluded (tx={tx_id}, \
                 block={block_hash}) — canonical_promoted_height should \
                 have returned None for the stranded hint before this \
                 arm fired"
            );
        }
        Err(PreValidationError::PublishTxMissingPriorSubmit { tx_id }) => {
            // This is the Submit branch of the same fallback arm under
            // test — not "downstream".  Firing here means
            // `canonical_submit_height` failed to content-verify the
            // fabricated height-1 Submit block seeded via `MBH[1]`, so
            // control flow never reached the promoted-height strip the
            // test is actually exercising.
            panic!(
                "regression: validator rejected as PublishTxMissingPriorSubmit \
                 (tx={tx_id}) — canonical_submit_height must attest the \
                 fabricated height-1 Submit row (MBH[1]) so control flow \
                 reaches the promoted-height check this test exercises"
            );
        }
        Err(downstream) => {
            // Any other failure is downstream of the regression under
            // test (e.g. PublishLedgerProofCountMismatch / ingress-proof
            // checks on this proofs-less fixture).  Documented as
            // tolerated by the "Harness scope" / "What this asserts"
            // sections above.
            tracing::info!(
                tolerated.err = ?downstream,
                "stranded-promotion fall-through reached a downstream \
                 phase unrelated to the regression — assertion target \
                 (no BlockBoundsLookupError / no PublishTxAlreadyIncluded / \
                 no PublishTxMissingPriorSubmit) still satisfied"
            );
        }
    }

    ctx.stop().await;
    Ok(())
}

/// Pins the EXCLUSION side of the `Searching { Publish }` fallback cap in
/// `data_txs_are_valid`.
///
/// The two sibling fixtures above pin the INCLUSION side: a content-verified
/// canonical row strictly below the cap is trusted. Neither would fail if the
/// cap regressed to a looser bound (`parent_height`, or the reorg floor
/// `block.height - block_tree_depth`) — both fixtures seed their fabricated
/// prior far enough below the window that a looser cap still admits it. This
/// test seeds the fabricated prior INSIDE the walk window instead, so a
/// loosened cap would wrongly admit it.
///
/// The by-hash walk owns its window by hash on the candidate's own branch — a
/// content-verified node-local row at an in-window height describes THIS
/// node's chain, not necessarily the candidate's branch. The fallback cap
/// (`block.height - prior_inclusion_walk_depth(config) - 1`) is the only
/// thing keeping such a row out of consideration. This test fails if the cap
/// is ever loosened to `parent_height` or the reorg floor.
///
/// **Test driver:** same scaffolding as
/// `test_prevalidation_rejects_doubly_published_tx_via_fallback` — mine to
/// height 3, validate a sibling candidate at height 3 (parent = the real
/// block 2) under `tx_anchor_expiry_depth = 0` / `block_tree_depth = 1`, so
/// the by-hash walk (depth 1) covers exactly the real block 2 on the
/// candidate's branch. The fabricated canonical prior here sits at height
/// **2** — inside that walk window — carrying the tx in both Submit and
/// Publish (metadata `included_height = promoted_height = 2`), and
/// `MigratedBlockHashes[2]` is repointed at it (overwriting the real block-2
/// row if one exists; at tip height 3 with testing `block_migration_depth =
/// 6` nothing has migrated yet, so nothing is actually clobbered). The real
/// block 2 on the candidate's ancestry does not carry the tx, so the walk
/// still leaves the tx `Searching`.
///
/// The fallback cap is `3 - 1 - 1 = 1`, strictly below height 2, so
/// `canonical_submit_height` rejects the height-2 hint before ever reading
/// `MigratedBlockHashes[2]` and returns `None` → `PublishTxMissingPriorSubmit`.
/// A cap of `parent_height` (2) or the old `reorg_floor` (`3 - 1 = 2`) would
/// instead admit the fabricated row, content-verify it via the (now
/// overwritten) MBH entry, and reject with `PublishTxAlreadyIncluded` naming
/// the fabricated block — a node-history-dependent outcome that forks the
/// network. That panic arm below is this test's falsifiability check.
#[tokio::test]
async fn test_prevalidation_ignores_content_verified_row_inside_walk_window() -> Result<()> {
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::tables::MigratedBlockHashes;
    use irys_database::{
        insert_block_header, insert_tx_header, set_data_tx_included_height,
        set_data_tx_promoted_height,
    };
    use irys_types::{BlockBody, H256List};
    use reth_db::transaction::DbTxMut as _;

    let ctx = PrevalidationTestContext::new().await?;

    // Same geometry as the sibling regression tests: mine three real blocks
    // so the by-hash walk (depth 1 under the override below) covers exactly
    // the real, tx-free block 2 for a height-3 candidate.
    ctx.node.mine_blocks(3).await?;
    let real_tip = ctx.node.get_block_by_height(3).await?;

    // A Publish-ledger tx that will impersonate "already canonically
    // included/promoted" via the fabricated in-window prior seeded below.
    let mut tx = DataTransactionHeader::new(&ctx.config.consensus_config());
    tx.data_root = H256::from_low_u64_be(0x011D_E1DE_5EED);
    tx.data_size = 0;
    tx.term_fee = BoundedFee::from_u64(1_000_000_000_000_000_000);
    tx.perm_fee = Some(BoundedFee::from_u64(1_000_000_000_000_000_000));
    tx.ledger_id = DataLedger::Publish as u32;
    let tx = tx
        .sign(&ctx.config.signer())
        .expect("Failed to sign test transaction");

    // Fabricated canonical prior at height 2 — INSIDE the by-hash walk
    // window, not on the candidate's ancestry: a real header carrying the tx
    // in BOTH Submit and Publish, pointed at by `MigratedBlockHashes[2]`
    // (overwriting the real block-2 row, which is fine at this tip height
    // since nothing has migrated yet). This is a node-local, content-verified
    // row describing this node's chain — not the candidate's branch — and
    // only the fallback cap keeps it from being trusted.
    let seeded_height = 2_u64;
    let mut prior_block = IrysBlockHeader::new_mock_header();
    prior_block.height = seeded_height;
    prior_block.block_hash = H256::random();
    prior_block.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx.id]);
    prior_block.data_ledgers[DataLedger::Publish].tx_ids = H256List(vec![tx.id]);
    let seeded_block_hash = prior_block.block_hash;

    ctx.node.node_ctx.db.update_eyre(|db_tx| {
        insert_tx_header(db_tx, &tx)?;
        set_data_tx_included_height(db_tx, &tx.id, seeded_height)?;
        set_data_tx_promoted_height(db_tx, &tx.id, seeded_height)?;
        insert_block_header(db_tx, &prior_block)?;
        db_tx.put::<MigratedBlockHashes>(seeded_height, seeded_block_hash)?;
        Ok(())
    })?;

    // Build a sibling candidate at height 3 (parent = the real block 2)
    // whose Publish ledger re-includes `tx`: clone the real height-3 header
    // as the structural baseline and swap the data ledgers.
    let mut header = real_tip.clone();
    let publish_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .expect("Publish ledger should exist");
    publish_ledger.tx_ids = H256List(vec![tx.id]);
    let submit_ledger = header
        .data_ledgers
        .iter_mut()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .expect("Submit ledger should exist");
    submit_ledger.tx_ids = H256List(Vec::new());

    ctx.config.signer().sign_block_header(&mut header)?;
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![tx.clone()],
        commitment_transactions: Vec::new(),
    };
    let bad_block = Arc::new(SealedBlock::new(header, body)?);

    {
        let mut tree = ctx.node.node_ctx.block_tree_guard.write();
        let parent_hash = bad_block.header().previous_block_hash;
        let commitment_snapshot = tree
            .get_commitment_snapshot(&parent_hash)
            .expect("parent commitment snapshot");
        let epoch_snapshot = tree
            .get_epoch_snapshot(&parent_hash)
            .expect("parent epoch snapshot");
        let ema_snapshot = tree
            .get_ema_snapshot(&parent_hash)
            .expect("parent ema snapshot");
        tree.add_block(
            &bad_block,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
        )?;
    }

    let (service_senders, mut service_receivers) = build_test_service_senders();
    let block_tree_guard = ctx.node.node_ctx.block_tree_guard.clone();
    tokio::spawn(async move {
        while let Some(msg) = service_receivers.block_tree.recv().await {
            let BlockTreeServiceMessage::GetBlockTreeReadGuard { response } = msg.inner else {
                continue;
            };
            let _ = response.send(block_tree_guard.clone());
        }
    });

    // Walk-depth override: `max(tx_anchor_expiry_depth = 0, block_tree_depth
    // = 1) = 1`, so the by-hash walk for the height-3 candidate covers
    // exactly the real, tx-free block 2 on its own branch — the tx stays
    // `Searching` and the canonical-DB fallback (capped at `3 - 1 - 1 = 1`,
    // strictly below the seeded height 2) is the path under test.
    let mut consensus = ctx.config.consensus_config();
    consensus.mempool.tx_anchor_expiry_depth = 0;
    consensus.block_tree_depth = 1;
    let mut node_config = ctx.config.clone();
    node_config.consensus = ConsensusOptions::Custom(consensus);
    let config_override = Config::new_with_random_peer_id(node_config);

    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let parent_hash = bad_block.header().previous_block_hash;
        (
            tree.get_epoch_snapshot(&parent_hash)
                .expect("parent epoch snapshot"),
            tree.get_ema_snapshot(&parent_hash)
                .expect("parent ema snapshot"),
        )
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        bad_block.transactions(),
        parent_epoch_snapshot,
        parent_ema_snapshot,
    )
    .await;

    match result {
        Err(PreValidationError::PublishTxMissingPriorSubmit { tx_id }) => {
            assert_eq!(tx_id, tx.id, "rejection must name the tx under test");
        }
        Err(PreValidationError::PublishTxAlreadyIncluded { tx_id, block_hash }) => {
            panic!(
                "regression: the fallback trusted a content-verified node-local \
                 row INSIDE the by-hash walk window (tx={tx_id}, block={block_hash}) \
                 — this is the fork vector the fallback cap exists to close: a \
                 row at this height describes only this node's chain, not \
                 necessarily the candidate's branch, so it must never decide \
                 validity"
            );
        }
        other => panic!(
            "expected PublishTxMissingPriorSubmit (fallback cap excludes the \
             in-window seeded height), got {:?}",
            other
        ),
    }

    ctx.stop().await;
    Ok(())
}
