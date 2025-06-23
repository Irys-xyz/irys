use crate::utils::{
    read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest,
};
use irys_actors::current_timestamp;
use irys_types::{storage_pricing::Amount, NodeConfig};
use reth::core::primitives::SealedBlock;
use rust_decimal_macros::dec;

#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_block_reward_gets_rejected() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;
    let solution = solution_context(&peer_node.node_ctx).await?;

    // produce an invalid block
    let block = {
        let prev_block_header = peer_node
            .node_ctx
            .block_producer_inner
            .parent_irys_block()?;
        let prev_evm_block = peer_node
            .node_ctx
            .block_producer_inner
            .get_evm_block(&prev_block_header)
            .await?;
        let current_timestamp = current_timestamp(&prev_block_header).await;

        let (system_tx_ledger, commitment_txs_to_bill, submit_txs) = peer_node
            .node_ctx
            .block_producer_inner
            .get_mempool_txs(&prev_block_header)
            .await?;
        let block_reward = peer_node
            .node_ctx
            .block_producer_inner
            .block_reward(&prev_block_header, current_timestamp)?;
        let eth_built_payload = peer_node
            .node_ctx
            .block_producer_inner
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &commitment_txs_to_bill,
                &submit_txs,
                // NOTE: gifting too much money to oneself
                Amount::token(dec!(99999)).unwrap(),
                current_timestamp,
            )
            .await?;
        let evm_block = eth_built_payload.block();

        peer_node
            .node_ctx
            .block_producer_inner
            .produce_block(
                solution,
                &prev_block_header,
                submit_txs,
                system_tx_ledger,
                current_timestamp,
                block_reward,
                evm_block,
            )
            .await?
    }
    .unwrap();

    peer_node.gossip_block(&block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_block_tampered_storage_txs_gets_rejected() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;
    let solution = solution_context(&peer_node.node_ctx).await?;

    let extra_tx = peer_node
        .create_submit_data_tx(&peer_signer, "Hello, world!".as_bytes().to_vec())
        .await?;

    // produce a block with tampered storage txs (one too many)
    let block = {
        let prev_block_header = peer_node
            .node_ctx
            .block_producer_inner
            .parent_irys_block()?;
        let prev_evm_block = peer_node
            .node_ctx
            .block_producer_inner
            .get_evm_block(&prev_block_header)
            .await?;
        let current_timestamp = current_timestamp(&prev_block_header).await;

        let (system_tx_ledger, commitment_txs_to_bill, submit_txs) = peer_node
            .node_ctx
            .block_producer_inner
            .get_mempool_txs(&prev_block_header)
            .await?;
        let block_reward = peer_node
            .node_ctx
            .block_producer_inner
            .block_reward(&prev_block_header, current_timestamp)?;

        // Create tampered submit_txs with one extra (duplicate the first one if available)
        let mut tampered_submit_txs = submit_txs.clone();
        tampered_submit_txs.push(extra_tx.header.clone());

        // Create EVM block with tampered storage transactions
        let eth_built_payload = peer_node
            .node_ctx
            .block_producer_inner
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &commitment_txs_to_bill,
                &tampered_submit_txs, // Contains one extra storage tx
                block_reward,
                current_timestamp,
            )
            .await?;
        let evm_block = eth_built_payload.block();

        // Produce block using original submit_txs but tampered EVM payload
        peer_node
            .node_ctx
            .block_producer_inner
            .produce_block(
                solution,
                &prev_block_header,
                submit_txs, // Original submit_txs (not tampered)
                system_tx_ledger,
                current_timestamp,
                block_reward,
                evm_block, // Tampered EVM payload
            )
            .await?
    }
    .unwrap();

    peer_node.gossip_block(&block)?;

    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_evm_block_invalid_hash() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;
    let solution = solution_context(&peer_node.node_ctx).await?;

    // produce a block with tampered storage txs (one too many)
    let block = {
        let prev_block_header = peer_node
            .node_ctx
            .block_producer_inner
            .parent_irys_block()?;
        let prev_evm_block = peer_node
            .node_ctx
            .block_producer_inner
            .get_evm_block(&prev_block_header)
            .await?;
        let current_timestamp = current_timestamp(&prev_block_header).await;

        let (system_tx_ledger, commitment_txs_to_bill, submit_txs) = peer_node
            .node_ctx
            .block_producer_inner
            .get_mempool_txs(&prev_block_header)
            .await?;
        let block_reward = peer_node
            .node_ctx
            .block_producer_inner
            .block_reward(&prev_block_header, current_timestamp)?;

        // Create EVM block with tampered storage transactions
        let eth_built_payload = peer_node
            .node_ctx
            .block_producer_inner
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &commitment_txs_to_bill,
                &submit_txs,
                block_reward,
                current_timestamp,
            )
            .await?;

        // NOTE: We produce invalid ethereum block hash here, it will be rejected by reth on the peer
        let evm_block = eth_built_payload.block().clone_block();
        let block = SealedBlock::new_unchecked(evm_block, [111; 32].into());

        // Produce block using original submit_txs but tampered EVM payload
        peer_node
            .node_ctx
            .block_producer_inner
            .produce_block(
                solution,
                &prev_block_header,
                submit_txs,
                system_tx_ledger,
                current_timestamp,
                block_reward,
                &block, // Tampered EVM payload
            )
            .await?
    }
    .unwrap();

    // NOTE: we assert the peer node (where the block was produced) because the reth payload never gets gossiped
    let outcome = read_block_from_state(&peer_node.node_ctx, &block.block_hash).await;
    assert_eq!(outcome, BlockValidationOutcome::Discarded);

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
