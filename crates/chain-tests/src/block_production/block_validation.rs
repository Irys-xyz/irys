use crate::utils::{IrysNodeTest, solution_context};
use eyre::Result;
use irys_actors::block_tree_service::BlockTreeServiceMessage;
use irys_actors::block_validation::{PreValidationError, prevalidate_block};
use irys_actors::test_helpers::build_test_service_senders;
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_chain::IrysNodeCtx;
use irys_domain::{EmaSnapshot, EpochSnapshot, HardforkConfigExt as _};
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

    let cascade_active = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let epoch_snapshot = tree.canonical_epoch_snapshot();
        config_override
            .consensus
            .hardforks
            .is_cascade_active_for_epoch(&epoch_snapshot)
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        transactions,
        cascade_active,
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
/// peer would succeed in re-publishing.  The fix is to inspect the
/// metadata's `promoted_height` and reject when set to a migrated height
/// ≤ parent_height.
///
/// **Test driver:** force the in-window walk to be empty by overriding
/// `tx_anchor_expiry_depth = 0`, then construct a Publish-ledger block whose
/// tx already has both `included_height` and `promoted_height` set in
/// `IrysDataTxMetadata` (with matching `MigratedBlockHashes` rows).  Expect
/// `Err(PublishTxAlreadyIncluded { tx_id, block_hash })` matching the
/// pre-populated promoted block hash.
#[tokio::test]
async fn test_prevalidation_rejects_doubly_published_tx_via_fallback() -> Result<()> {
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::{
        insert_tx_header, set_data_tx_included_height, set_data_tx_promoted_height,
    };
    use irys_types::H256List;

    let ctx = PrevalidationTestContext::new().await?;

    // `PrevalidationTestContext` lands `ctx.block` at height 1 (parent =
    // genesis at height 0), so the bad block we'll build below has
    // `parent_height = 0`.  Both `included_height` and `promoted_height`
    // must be ≤ 0 for the fallback to consider the tx "already canonical";
    // we collapse both onto the genesis row and pin the assertion to the
    // genesis block hash already populated in `MigratedBlockHashes[0]`.
    let genesis_block_hash = ctx
        .node
        .get_block_by_height(0)
        .await
        .expect("genesis block")
        .block_hash;

    // A Publish-ledger tx that will impersonate "already canonically
    // promoted" via the pre-populated metadata below.
    let mut tx = DataTransactionHeader::new(&ctx.config.consensus_config());
    tx.data_root = H256::from_low_u64_be(0x517A_1EBA_DD15);
    tx.data_size = 0;
    tx.term_fee = BoundedFee::from_u64(1_000_000_000_000_000_000);
    tx.perm_fee = Some(BoundedFee::from_u64(1_000_000_000_000_000_000));
    tx.ledger_id = DataLedger::Publish as u32;
    let tx = tx
        .sign(&ctx.config.signer())
        .expect("Failed to sign test transaction");

    // Pre-populate canonical metadata: tx was Submit-included AND
    // Publish-promoted at genesis (same-block promotion).  Both heights are
    // ≤ parent_height = 0; both share `MigratedBlockHashes[0]` which the
    // node already populated with `genesis_block_hash`.  This is the
    // minimum-viable canonical-storage state that exercises the fallback's
    // `promoted_height` rejection without touching adjacent canonical
    // rows.
    let prior_submit_height = 0_u64;
    let prior_publish_height = 0_u64;
    ctx.node.node_ctx.db.update_eyre(|db_tx| {
        // `tx_header_by_txid_canonical` reads `IrysDataTxHeaders` first, so
        // metadata-only writes would surface as "tx unknown" — also
        // populate the header table.
        insert_tx_header(db_tx, &tx)?;
        set_data_tx_included_height(db_tx, &tx.id, prior_submit_height)?;
        set_data_tx_promoted_height(db_tx, &tx.id, prior_publish_height)?;
        Ok(())
    })?;

    // Build a block whose Publish ledger re-includes `tx`.  Mirror the
    // pattern from `test_prevalidation_rejects_submit_targeted_tx`: keep
    // ctx.block as the structural baseline and swap the data ledgers.
    let mut body = ctx.block.to_block_body();
    body.data_transactions = vec![tx.clone()];

    let mut header = (**ctx.block.header()).clone();
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

    // `tx_anchor_expiry_depth = 0` collapses the in-window walk to zero
    // blocks, forcing every Publish-ledger tx to take the canonical-DB
    // fallback path that this regression exists to cover.
    let mut consensus = ctx.config.consensus_config();
    consensus.mempool.tx_anchor_expiry_depth = 0;
    let mut node_config = ctx.config.clone();
    node_config.consensus = ConsensusOptions::Custom(consensus);
    let config_override = Config::new_with_random_peer_id(node_config);

    let cascade_active = {
        let tree = ctx.node.node_ctx.block_tree_guard.read();
        let epoch_snapshot = tree.canonical_epoch_snapshot();
        config_override
            .consensus
            .hardforks
            .is_cascade_active_for_epoch(&epoch_snapshot)
    };

    let result = irys_actors::block_validation::data_txs_are_valid(
        &config_override,
        &service_senders,
        bad_block.header(),
        &ctx.node.node_ctx.db,
        &ctx.node.node_ctx.block_tree_guard,
        bad_block.transactions(),
        cascade_active,
    )
    .await;

    match result {
        Err(PreValidationError::PublishTxAlreadyIncluded { tx_id, block_hash }) => {
            assert_eq!(tx_id, tx.id, "rejection must name the doubly-published tx");
            assert_eq!(
                block_hash, genesis_block_hash,
                "rejection must surface the canonical promoted-block hash from MigratedBlockHashes (= genesis hash in this fixture)"
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
