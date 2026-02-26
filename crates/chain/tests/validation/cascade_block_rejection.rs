use std::sync::Arc;

use crate::utils::{solution_context, IrysNodeTest};
use irys_actors::{
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    block_validation::PreValidationError,
    BlockProdStrategy as _, ProductionStrategy,
};
use irys_types::{DataLedger, DataTransactionLedger, H256List, NodeConfig, SealedBlock, H256};

/// Verify that blocks containing OneYear/ThirtyDay ledgers are rejected
/// when the Cascade hardfork is not active on the receiving node.
///
/// Approach:
/// 1. Start a node WITHOUT Cascade configured
/// 2. Produce a valid block (2 ledgers: Publish + Submit)
/// 3. Tamper with the header: inject OneYear + ThirtyDay data_ledgers
/// 4. Re-sign the header so the signature is valid
/// 5. Submit through BlockDiscovery for prevalidation
/// 6. Assert rejection with PreValidationError::InvalidLedgerId
#[test_log::test(tokio::test)]
async fn heavy_cascade_block_rejects_invalid_term_ledger_metadata() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = 32;
    });

    // Ensure no Cascade hardfork
    assert!(genesis_config
        .consensus_config()
        .hardforks
        .cascade
        .is_none());

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", 20)
        .await;
    genesis_node.mine_block().await?;

    // Produce a valid block using the normal production strategy
    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .unwrap();

    // Verify the valid block has exactly 2 ledgers (no Cascade)
    assert_eq!(block.header().data_ledgers.len(), 2);

    // Tamper: clone header and inject OneYear + ThirtyDay ledgers
    let mut header = (**block.header()).clone();
    header.data_ledgers.push(DataTransactionLedger {
        ledger_id: DataLedger::OneYear as u32,
        tx_root: H256::zero(),
        tx_ids: H256List::default(),
        total_chunks: 0,
        expires: Some(365),
        proofs: None,
        required_proof_count: None,
    });
    header.data_ledgers.push(DataTransactionLedger {
        ledger_id: DataLedger::ThirtyDay as u32,
        tx_root: H256::zero(),
        tx_ids: H256List::default(),
        total_chunks: 0,
        expires: Some(30),
        proofs: None,
        required_proof_count: None,
    });

    // Re-sign so the signature is valid for the modified header
    genesis_config.signer().sign_block_header(&mut header)?;
    let mut tampered_body = block.to_block_body();
    tampered_body.block_hash = header.block_hash;
    let tampered_block = Arc::new(SealedBlock::new(header, tampered_body).unwrap());

    // Submit through BlockDiscovery for prevalidation
    let block_discovery = BlockDiscoveryFacadeImpl::new(
        genesis_node
            .node_ctx
            .service_senders
            .block_discovery
            .clone(),
    );
    let result = block_discovery.handle_block(tampered_block, false).await;

    // Should be rejected because Cascade is not active on this node
    assert!(
        matches!(
            result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::InvalidLedgerId { .. }
            ))
        ),
        "block with OneYear/ThirtyDay ledgers should be rejected when Cascade is not active, got: {:?}",
        result
    );

    genesis_node.stop().await;
    Ok(())
}
