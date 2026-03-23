use crate::utils::{read_block_from_state, BlockValidationOutcome, IrysNodeTest};
use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope as EthereumTxEnvelope};
use alloy_eips::Encodable2718 as _;
use alloy_network::TxSignerSync as _;
use alloy_primitives::{TxKind, U256};
use alloy_signer_local::LocalSigner;
use irys_reth::pd_tx::{build_pd_access_list_with_fees, sum_pd_chunks_in_access_list};
use irys_types::chunk_provider::ChunkConfig;
use irys_types::{range_specifier::PdDataRead, NodeConfig};

#[test_log::test(actix_web::test)]
async fn heavy_test_reth_block_with_pd_too_large_gets_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    let genesis_max_accepted_chunks_per_block = 10;
    let peer_max_accepted_chunks_per_block = 100;
    genesis_config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing")
        .max_pd_chunks_per_block = genesis_max_accepted_chunks_per_block;

    // Create and fund a signer for PD transaction
    let pd_tx_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // Start genesis node (Node A)
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create peer node (Node B) with max_pd_chunks_per_block = 10
    let peer_node = {
        let peer_node = genesis_node
            .testing_peer_with_assignments(&pd_tx_signer)
            .await?;
        let mut peer_node = peer_node.stop().await;
        peer_node
            .cfg
            .consensus
            .get_mut()
            .hardforks
            .sprite
            .as_mut()
            .expect("Sprite hardfork must be configured for testing")
            .max_pd_chunks_per_block = peer_max_accepted_chunks_per_block;
        peer_node.start_with_name("peer").await
    };

    // Build a PD transaction with 80 chunks (exceeds peer's limit of 10, but within genesis limit of 100)
    let chain_id = genesis_node.node_ctx.config.consensus.chain_id;

    // Build access list with 80 chunks (4 data reads * 20 chunks each).
    // chunk_size=32, so 20 chunks = 640 bytes per read.
    let chunk_size = 32_u32;
    let data_reads: Vec<PdDataRead> = (0..4_u32)
        .map(|i| PdDataRead {
            partition_index: u64::MAX,
            start: i * 20, // distinct offsets to avoid duplicate key rejection
            len: 20 * chunk_size,
            byte_off: 0,
        })
        .collect();
    // Build access list with 80 chunks AND fee parameters.
    // Note: Fees must be high enough to meet min_pd_transaction_cost ($0.01 USD).
    // At $1/IRYS price, min_cost_irys = $0.01 * 1e18 = 1e16 wei.
    // With 80 chunks, we need: total_fees >= 1e16, so per-chunk >= 1e16/80 = 1.25e14 wei.
    // Using higher values for safety margin.
    let access_list = build_pd_access_list_with_fees(
        &data_reads,
        U256::from(1_000_000_000_000_000_u64), // 1e15 wei = 0.001 IRYS
        U256::from(1_000_000_000_000_000_u64), // 1e15 wei = 0.001 IRYS
    )?;
    let chunk_config = ChunkConfig::from_consensus(genesis_config.consensus.get_mut());
    let chunks = sum_pd_chunks_in_access_list(&access_list, &chunk_config);
    assert!(
        chunks > genesis_max_accepted_chunks_per_block,
        "expect to have more chunks in access list than the genesis node would accept"
    );
    assert!(
        chunks <= peer_max_accepted_chunks_per_block,
        "expect chunks to fit in the limits of the peer node"
    );

    // Create and sign EIP-1559 transaction manually using LocalSigner
    let local_signer = LocalSigner::from(pd_tx_signer.signer.clone());
    let mut tx = TxEip1559 {
        access_list,
        chain_id,
        gas_limit: 100_000,
        input: alloy_primitives::Bytes::new(),
        max_fee_per_gas: 1_000_000_000, // basefee=0 => effective gas price 0
        max_priority_fee_per_gas: 0,
        nonce: 0,
        to: TxKind::Call(alloy_primitives::Address::random()),
        value: U256::ZERO,
    };
    // Verify PD metadata parses correctly from the access list
    assert!(matches!(
        irys_reth::pd_tx::parse_pd_transaction(&tx.access_list, &chunk_config),
        irys_reth::pd_tx::PdParseResult::ValidPd(_)
    ));
    let signature = local_signer
        .sign_transaction_sync(&mut tx)
        .expect("PD tx must be signable");

    // Inject transaction into peer node
    let tx_envelope = EthereumTxEnvelope::Eip1559(tx.into_signed(signature))
        .encoded_2718()
        .into();
    peer_node
        .node_ctx
        .reth_node_adapter
        .rpc
        .inject_tx(tx_envelope)
        .await
        .expect("PD tx should be accepted by the peer node");

    // Mine block on genesis node containing the PD transaction
    let (block, block_eth_payload, _) = peer_node.mine_block_without_gossip().await?;
    let txs = block_eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|x| x.signature() == &signature);
    assert!(txs, "expect the large pd tx to be included");

    // Gossip block to peer node
    peer_node.gossip_block_to_peers(&block)?;
    peer_node.gossip_eth_block_to_peers(block_eth_payload.block())?;

    // Check that peer node rejected the block
    let event_rx = genesis_node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash, event_rx).await;
    assert!(
        matches!(outcome, BlockValidationOutcome::Discarded(_)),
        "Peer node should have rejected the block containing 80 PD chunks (exceeds limit of 10), got: {:?}",
        outcome
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
