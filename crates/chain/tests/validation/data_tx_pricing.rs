use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, block_tree_service::BlockTreeServiceMessage,
    shadow_tx_generator::PublishLedgerWithTxs, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_chain::IrysNodeCtx;
use irys_types::{
    CommitmentTransaction, DataLedger, DataTransactionHeader, DataTransactionLedger, H256List,
    IrysBlockHeader, NodeConfig, SystemTransactionLedger, H256, U256,
};

// Helper function to send a block directly to the block tree service for validation
async fn send_block_to_block_tree(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
    commitment_txs: Vec<CommitmentTransaction>,
) -> eyre::Result<()> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    node_ctx
        .service_senders
        .block_tree
        .send(BlockTreeServiceMessage::BlockPreValidated {
            block,
            commitment_txs: Arc::new(commitment_txs),
            response: response_tx,
        })?;

    Ok(response_rx.await??)
}

// This test creates a malicious block producer that includes a data transaction with insufficient perm_fee.
// The assertion will fail (block will be discarded) because data transactions must have perm_fee >= expected amount.
#[test_log::test(actix_web::test)]
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
        ) -> eyre::Result<(
            Vec<SystemTransactionLedger>,
            Vec<CommitmentTransaction>,
            Vec<DataTransactionHeader>,
            PublishLedgerWithTxs,
        )> {
            // Return malicious tx in Submit ledger (would normally be waiting for proofs)
            Ok((
                vec![],
                vec![],
                vec![self.malicious_tx.clone()], // Submit ledger tx
                PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                }, // No Publish ledger txs
            ))
        }
    }

    // Configure a test network
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 256;

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
        Some(H256::zero()),
        DataLedger::Publish,
        price_info.term_fee,
        Some(insufficient_perm_fee), // Insufficient perm_fee!
    )?;
    let malicious_tx = test_signer.sign_transaction(malicious_tx)?;

    // Create block with evil strategy
    let block_prod_strategy = EvilBlockProdStrategy {
        malicious_tx: malicious_tx.header.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (mut block, _adjustment_stats, _eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Manually set the data ledgers with our malicious tx
    let mut irys_block = (*block).clone();
    irys_block.data_ledgers = vec![
        // Publish ledger (empty)
        DataTransactionLedger {
            ledger_id: DataLedger::Publish as u32,
            tx_root: H256::zero(),
            tx_ids: H256List(vec![]),
            max_chunk_offset: 0,
            expires: None,
            proofs: None,
        },
        // Submit ledger with our malicious tx
        DataTransactionLedger {
            ledger_id: DataLedger::Submit as u32,
            tx_root: H256::zero(),
            tx_ids: H256List(vec![malicious_tx.header.id]),
            max_chunk_offset: 0,
            expires: None,
            proofs: None,
        },
    ];
    test_signer.sign_block_header(&mut irys_block)?;
    block = Arc::new(irys_block);

    // Send block directly to block tree service for validation
    send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![]).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    // This should still be rejected because the perm_fee is insufficient
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;

    Ok(())
}
