use std::sync::Arc;

use super::send_block_and_read_state;
use crate::utils::{assert_validation_error, solution_context, IrysNodeTest};
use alloy_consensus::{EthereumTxEnvelope, SignableTransaction as _, TxEip4844};
use alloy_eips::eip4895::{Withdrawal, Withdrawals};
use alloy_primitives::Signature as AlloySignature;
use alloy_primitives::{Bytes, B256, U256};
use irys_actors::block_validation::ValidationError;
use irys_actors::BlockProdStrategy as _;
use irys_actors::ProductionStrategy;
use irys_chain::IrysNodeCtx;
use irys_types::{
    BlockBody, BlockTransactions, IrysBlockHeader, NodeConfig, SealedBlock as IrysSealedBlock,
};
use reth::api::Block as _;
use reth::core::primitives::SealedBlock;
use reth_ethereum_primitives::Block;

// Produces a valid block, then returns its header and evm payload (sealed block).
async fn produce_block(
    genesis_node: &IrysNodeTest<IrysNodeCtx>,
) -> eyre::Result<(
    Arc<IrysBlockHeader>,
    BlockTransactions,
    reth::payload::EthBuiltPayload,
)> {
    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (block, _adjustment_stats, eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("no block produced"))?;

    let header = block.header().clone();
    let transactions = block.transactions().as_ref().clone();
    Ok((header, transactions, eth_payload))
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

// Insert a sealed block directly into the execution payload cache so that
// validation uses the cached payload rather than submitting to the engine.
async fn inject_payload_into_cache(node_ctx: &IrysNodeCtx, sealed: SealedBlock<Block>) {
    node_ctx
        .block_pool
        .add_execution_payload_to_cache(sealed)
        .await;
}

#[test_log::test(tokio::test)]
async fn heavy_evm_payload_with_blob_gas_used_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (irys_block, _transactions, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set blob_gas_used in the EVM header to non-zero
    let mutated = mutate_header(eth_payload.block(), |blk| {
        blk.header.blob_gas_used = Some(131072);
        blk.header.excess_blob_gas = blk.header.excess_blob_gas.or(Some(0));
    });

    // Register mutated payload in local cache so validation can fetch it
    inject_payload_into_cache(&genesis_node.node_ctx, mutated.clone()).await;

    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    // Clear tx_ids in ledgers to match the empty body (keep ledger structure intact)
    for ledger in header.data_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    for ledger in header.system_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    signer.sign_block_header(&mut header)?;

    // Create new SealedBlock with mutated header and matching body.block_hash
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![],
        commitment_transactions: vec![],
    };
    let sealed_block = Arc::new(IrysSealedBlock::new(header, body)?);

    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, sealed_block.clone(), true).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with blob_gas_used should be rejected",
    );

    genesis_node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_evm_payload_with_excess_blob_gas_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (irys_block, _transactions, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set excess_blob_gas in the EVM header to non-zero
    let mutated = mutate_header(eth_payload.block(), |blk| {
        blk.header.excess_blob_gas = Some(1);
        blk.header.blob_gas_used = blk.header.blob_gas_used.or(Some(0));
    });

    // Register mutated payload in local cache so validation can fetch it
    inject_payload_into_cache(&genesis_node.node_ctx, mutated.clone()).await;

    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    // Clear tx_ids in ledgers to match the empty body (keep ledger structure intact)
    for ledger in header.data_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    for ledger in header.system_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    signer.sign_block_header(&mut header)?;

    // Create new SealedBlock with mutated header and matching body.block_hash
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![],
        commitment_transactions: vec![],
    };
    let sealed_block = Arc::new(IrysSealedBlock::new(header, body)?);

    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, sealed_block.clone(), true).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with excess_blob_gas should be rejected",
    );

    genesis_node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_evm_payload_with_withdrawals_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (irys_block, _transactions, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: set a non-empty withdrawals list in the EVM body
    let mutated = mutate_header(eth_payload.block(), |blk| {
        let w = Withdrawal {
            index: 0,
            validator_index: 0,
            address: genesis_node
                .node_ctx
                .config
                .node_config
                .reward_address
                .into(),
            amount: 1,
        };
        blk.body.withdrawals = Some(Withdrawals::new(vec![w]));
    });

    // Register mutated payload in local cache so validation can fetch it
    inject_payload_into_cache(&genesis_node.node_ctx, mutated.clone()).await;

    // Update irys block header with new evm block hash and resign
    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    // Clear tx_ids in ledgers to match the empty body (keep ledger structure intact)
    for ledger in header.data_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    for ledger in header.system_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    signer.sign_block_header(&mut header)?;

    // Create new SealedBlock with mutated header and matching body.block_hash
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![],
        commitment_transactions: vec![],
    };
    let sealed_block = Arc::new(IrysSealedBlock::new(header, body)?);

    // Send block for validation
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, sealed_block.clone(), true).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with withdrawals should be rejected",
    );

    genesis_node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_evm_payload_with_versioned_hashes_is_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let (irys_block, _transactions, eth_payload) = produce_block(&genesis_node).await?;

    // Mutate: append an EIP-4844 transaction that carries blob_versioned_hashes (non-empty)
    let mutated = mutate_header(eth_payload.block(), |blk| {
        let tx_eip4844 = TxEip4844 {
            chain_id: genesis_node.node_ctx.config.consensus.chain_id,
            nonce: 0,
            max_fee_per_gas: 1_000_000_000_u128,
            max_priority_fee_per_gas: 0,
            gas_limit: 100_000,
            to: genesis_node
                .node_ctx
                .config
                .node_config
                .reward_address
                .into(),
            value: U256::ZERO,
            input: Bytes::new(),
            access_list: Default::default(),
            blob_versioned_hashes: vec![B256::with_last_byte(0xAB)],
            max_fee_per_blob_gas: 1,
        };
        let sig = AlloySignature::test_signature().with_parity(true);
        let env = EthereumTxEnvelope::<TxEip4844>::Eip4844(tx_eip4844.into_signed(sig));
        blk.body.transactions.push(env);
    });

    // Register mutated payload in local cache so validation can fetch it
    inject_payload_into_cache(&genesis_node.node_ctx, mutated.clone()).await;

    // Update irys block header with new evm block hash and resign
    let mut header = (*irys_block).clone();
    header.evm_block_hash = mutated.hash();
    // Clear tx_ids in ledgers to match the empty body (keep ledger structure intact)
    for ledger in header.data_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    for ledger in header.system_ledgers.iter_mut() {
        ledger.tx_ids.0.clear();
    }
    signer.sign_block_header(&mut header)?;

    // Create new SealedBlock with mutated header and matching body.block_hash
    let body = BlockBody {
        block_hash: header.block_hash,
        data_transactions: vec![],
        commitment_transactions: vec![],
    };
    let sealed_block = Arc::new(IrysSealedBlock::new(header, body)?);

    // Send block for validation
    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, sealed_block.clone(), true).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::ShadowTransactionInvalid(_)),
        "block with versioned_hashes should be rejected",
    );

    genesis_node.stop().await;
    Ok(())
}
