// This test verifies that a block containing a transaction in both the Submit and Publish
// ledgers (same-block promotion) passes full validation on a peer node. Our node software
// no longer produces such blocks (ingress proofs require submit confirmation first), but
// the validation rules still permit them. We use a custom BlockProdStrategy to manually
// assemble a valid same-block-promotion block and verify it's accepted.

use crate::utils::{BlockValidationOutcome, IrysNodeTest, solution_context};
use crate::validation::send_block_and_read_state;
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, ProductionStrategy, async_trait,
    block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs,
};
use irys_domain::BlockTreeReadGuard;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, DataTransactionHeader, IngressProofsList, IrysBlockHeader, NodeConfig,
    UnixTimestampMs,
};

#[test_log::test(tokio::test)]
async fn heavy_same_block_promotion_accepted_by_peer() -> eyre::Result<()> {
    struct SameBlockPromotionStrategy {
        pub prod: ProductionStrategy,
        pub data_tx: DataTransactionHeader,
        pub proofs: IngressProofsList,
        pub block_tree_guard: BlockTreeReadGuard,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for SameBlockPromotionStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &IrysBlockHeader,
            _block_timestamp: UnixTimestampMs,
        ) -> Result<
            irys_actors::block_producer::MempoolTxsBundle,
            irys_actors::tx_selector::TxSelectorError,
        > {
            // Include the tx in both submit and publish ledgers — same-block promotion.
            // Our node no longer produces this pattern (ingress proofs require submit
            // confirmation), but validation must still accept it.
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![],
                commitment_txs_to_bill: vec![],
                submit_txs: vec![self.data_tx.clone()],
                one_year_txs: vec![],
                thirty_day_txs: vec![],
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

    // 1. Configure genesis node
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // 2. Spawn peer node for independent validation
    let peer_config = genesis_node.testing_peer_with_signer(&test_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // 3. Mine a block so we have a valid anchor beyond genesis
    genesis_node.mine_block().await?;

    // 4. Create a properly-priced publish data transaction via the price endpoint
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes
    let tx = genesis_node
        .post_publish_data_tx(&test_signer, data.clone())
        .await?;

    // 5. Manually generate an ingress proof signed by the genesis signer (who is staked).
    //    We can't rely on auto-generation because the tx hasn't been confirmed in submit
    //    yet — that's the whole point of same-block promotion.
    let genesis_signer = genesis_config.signer();
    let chunk_size = genesis_config.consensus_config().chunk_size as usize;
    let chunks: Vec<Vec<u8>> = data.chunks(chunk_size).map(Vec::from).collect();
    let anchor = genesis_node.get_anchor().await?;
    let manual_proof = generate_ingress_proof(
        &genesis_signer,
        tx.header.data_root,
        chunks.iter().map(|c| Ok(c.as_slice())),
        genesis_config.consensus_config().chain_id,
        anchor,
    )?;
    let proofs_list = IngressProofsList(vec![manual_proof]);

    // 6. Build the same-block promotion strategy
    let strategy = SameBlockPromotionStrategy {
        data_tx: tx.header.clone(),
        proofs: proofs_list,
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: genesis_node.node_ctx.block_tree_guard.clone(),
    };

    // 7. Produce the block without gossip
    let (block, _adjustment_stats, _eth_payload) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Verify the block structure: tx appears in both submit and publish ledgers
    let header = block.header();
    assert!(
        header.data_ledgers[DataLedger::Submit]
            .tx_ids
            .contains(&tx.header.id),
        "Block submit ledger should contain the transaction"
    );
    assert!(
        header.data_ledgers[DataLedger::Publish]
            .tx_ids
            .contains(&tx.header.id),
        "Block publish ledger should contain the transaction"
    );

    // 8. Send block to peer node for full validation — this is the core assertion.
    //    The peer has never seen this tx before; it validates entirely from the block.
    let peer_outcome =
        send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert!(
        matches!(peer_outcome, BlockValidationOutcome::StoredOnNode(_)),
        "Peer should accept block with same-block promotion, got: {:?}",
        peer_outcome
    );

    // 9. Also validate on genesis node for completeness
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert!(
        matches!(genesis_outcome, BlockValidationOutcome::StoredOnNode(_)),
        "Genesis should accept block with same-block promotion, got: {:?}",
        genesis_outcome
    );

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
