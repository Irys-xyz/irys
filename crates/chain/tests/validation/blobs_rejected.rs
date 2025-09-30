use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use eyre::OptionExt as _;
use irys_actors::BlockProdStrategy;
use irys_actors::ProductionStrategy;
use irys_chain::IrysNodeCtx;
use irys_types::{IrysBlockHeader, NodeConfig};
use reth::core::primitives::SealedBlock;
use reth::primitives::Block;
use alloy_eips::eip7685::{Requests, RequestsOrHash};
use reth::rpc::types::engine::ExecutionPayload;
use reth::api::Block as _;
use reth::rpc::api::EngineApiClient as _;

// Helper function to send a block directly to the block tree service for validation
async fn send_block_to_block_tree(
    node_ctx: &IrysNodeCtx,
    block: Arc<IrysBlockHeader>,
    skip_vdf_validation: bool,
) -> eyre::Result<()> {
    use irys_actors::block_tree_service::BlockTreeServiceMessage;

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    node_ctx
        .service_senders
        .block_tree
        .send(BlockTreeServiceMessage::BlockPreValidated {
            block,
            commitment_txs: Arc::new(vec![]),
            skip_vdf_validation,
            response: response_tx,
        })?;

    response_rx.await??;
    Ok(())
}

// Produces a valid block, then returns its header and evm payload (sealed block).
async fn produce_block(
    genesis_node: &IrysNodeTest<IrysNodeCtx>,
) -> eyre::Result<(Arc<IrysBlockHeader>, reth::payload::EthBuiltPayload)> {
    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (block, _adjustment_stats, eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("no block produced"))?;

    Ok((block, eth_payload))
}

// Mutates the sealed block's header in-place via unseal/modify/seal, returns the new sealed block.
fn mutate_header<F>(sealed: &SealedBlock<Block>, mutator: F) -> SealedBlock<Block>
where
    F: FnOnce(&mut Block),
{
    let mut block = sealed.clone().unseal();
    mutator(&mut block);
    block.seal_slow()
}

// Injects a sealed block into the local reth engine via engine API so that
// ExecutionPayloadCache can retrieve it by hash during validation.
async fn inject_payload_into_reth(
    node_ctx: &IrysNodeCtx,
    sealed: SealedBlock<Block>,
    parent_beacon_block_root: irys_types::H256,
) -> eyre::Result<()> {
    let exec_data = <
        <irys_reth_node_bridge::irys_reth::IrysEthereumNode as reth::api::NodeTypes>::Payload as
            reth::api::PayloadTypes
    >::block_to_payload(sealed);

    let payload = exec_data.payload;
    let sidecar = exec_data.sidecar;
    let ExecutionPayload::V3(payload_v3) = payload else {
        eyre::bail!("expected v3 payload");
    };
    let versioned_hashes = sidecar
        .versioned_hashes()
        .ok_or_eyre("version hashes must be present")?
        .clone();

    let engine_api_client = node_ctx.reth_node_adapter.inner.engine_http_client();
    let _ = engine_api_client
        .new_payload_v4(
            payload_v3,
            versioned_hashes,
            parent_beacon_block_root.into(),
            RequestsOrHash::Requests(Requests::new(vec![])),
        )
        .await?;

    Ok(())
}

#[test_log::test(actix_web::test)]
async fn evm_payload_with_requests_hash_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (mut irys_block, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set requests_hash in the EVM header
    let mutated = mutate_header(eth_payload.block(), |blk| {
        // set any non-empty hash
        blk.header.requests_hash = Some(reth::revm::primitives::B256::with_last_byte(0x11));
    });

    // Register mutated payload in local reth so validation can fetch it
    inject_payload_into_reth(&genesis_node.node_ctx, mutated.clone(), irys_block.previous_block_hash)
        .await?;

    // Update irys block header with new evm block hash and resign
    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    signer.sign_block_header(&mut header)?;
    irys_block = Arc::new(header);

    // Send block for validation
    send_block_to_block_tree(&genesis_node.node_ctx, irys_block.clone(), false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &irys_block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;
    Ok(())
}

#[test_log::test(actix_web::test)]
async fn evm_payload_with_blob_gas_used_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (mut irys_block, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set blob_gas_used in the EVM header to non-zero
    let mutated = mutate_header(eth_payload.block(), |blk| {
        blk.header.blob_gas_used = Some(1);
        blk.header.excess_blob_gas = blk.header.excess_blob_gas.or(Some(0));
    });

    // Register mutated payload in local reth so validation can fetch it
    inject_payload_into_reth(&genesis_node.node_ctx, mutated.clone(), irys_block.previous_block_hash)
        .await?;

    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    signer.sign_block_header(&mut header)?;
    irys_block = Arc::new(header);

    send_block_to_block_tree(&genesis_node.node_ctx, irys_block.clone(), false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &irys_block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;
    Ok(())
}

#[test_log::test(actix_web::test)]
async fn evm_payload_with_excess_blob_gas_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (mut irys_block, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set excess_blob_gas in the EVM header to non-zero
    let mutated = mutate_header(eth_payload.block(), |blk| {
        blk.header.excess_blob_gas = Some(1);
        blk.header.blob_gas_used = blk.header.blob_gas_used.or(Some(0));
    });

    // Register mutated payload in local reth so validation can fetch it
    inject_payload_into_reth(&genesis_node.node_ctx, mutated.clone(), irys_block.previous_block_hash)
        .await?;

    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    signer.sign_block_header(&mut header)?;
    irys_block = Arc::new(header);

    send_block_to_block_tree(&genesis_node.node_ctx, irys_block.clone(), false).await?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &irys_block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    genesis_node.stop().await;
    Ok(())
}
