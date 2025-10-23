use crate::utils::{read_block_from_state, BlockValidationOutcome, IrysNodeTest};
use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope as EthereumTxEnvelope};
use alloy_eips::Encodable2718 as _;
use alloy_network::TxSignerSync as _;
use alloy_primitives::{TxKind, U256};
use alloy_signer_local::LocalSigner;
use irys_reth::pd_tx::{
    build_pd_access_list, detect_and_decode_pd_header, prepend_pd_header_v1_to_calldata,
    sum_pd_chunks_in_access_list, PdHeaderV1, PdKey,
};
use irys_types::{Address, NodeConfig};

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
        .mempool
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
            .mempool
            .max_pd_chunks_per_block = peer_max_accepted_chunks_per_block;
        peer_node.start_with_name("peer").await
    };

    // Build a PD transaction with 80 chunks (exceeds peer's limit of 10, but within genesis limit of 100)
    let chain_id = genesis_node.node_ctx.config.consensus.chain_id;

    // Build access list with 80 chunks (4 keys * 20 chunks each)
    let storage_keys = (0..4).map(|_i| PdKey {
        slot_index_be: [0xff; 26],
        offset: 0,
        chunk_count: 20,
    });
    let access_list = build_pd_access_list(storage_keys);
    let chunks = sum_pd_chunks_in_access_list(&access_list);
    assert!(
        chunks > genesis_max_accepted_chunks_per_block,
        "expect to have more chunks in access list than the genesis node would accept"
    );
    assert!(
        chunks <= peer_max_accepted_chunks_per_block,
        "expect chunks to fit in the limits of the peer node"
    );

    // Build transaction calldata with PD header
    let header = PdHeaderV1 {
        max_priority_fee_per_chunk: U256::from(1_000_000_000_u64),
        max_base_fee_per_chunk: U256::from(10_000_000_000_u64),
    };
    let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);

    // Create and sign EIP-1559 transaction manually using LocalSigner
    let local_signer = LocalSigner::from(pd_tx_signer.signer.clone());
    let mut tx = TxEip1559 {
        access_list,
        chain_id,
        gas_limit: 100_000,
        input: calldata,
        max_fee_per_gas: 1_000_000_000, // basefee=0 => effective gas price 0
        max_priority_fee_per_gas: 0,
        nonce: 0,
        to: TxKind::Call(Address::random()),
        value: U256::ZERO,
    };
    let _decoded = detect_and_decode_pd_header(&tx.input)
        .expect("pd header parse error")
        .unwrap();
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
    let (block, block_eth_payload) = peer_node.mine_block_without_gossip().await?;
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
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        outcome,
        BlockValidationOutcome::Discarded,
        "Peer node should have rejected the block containing 80 PD chunks (exceeds limit of 10)"
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
