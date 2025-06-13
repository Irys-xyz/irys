use crate::utils::*;
use alloy_core::primitives::{Bytes, TxKind, B256, U256};
use alloy_eips::{BlockId, Encodable2718 as _};
use alloy_genesis::GenesisAccount;
use alloy_signer_local::LocalSigner;
use assert_matches::assert_matches;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_chain::IrysNodeCtx;
use irys_reth_node_bridge::{
    ext::IrysRethRpcTestContextExt as _, reth_e2e_test_utils::transaction::TransactionTestContext,
    IrysRethNodeAdapter,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, DataLedger, LedgerChunkOffset, NodeConfig, H256,
};
use k256::ecdsa::SigningKey;
use reth::{
    network::{PeerInfo, Peers},
    primitives::{Receipt, Transaction},
    rpc::{
        api::EthApiClient,
        types::{Block, Header, TransactionRequest},
    },
};
use std::time::Duration;
use tokio::{sync::oneshot, time::sleep};

#[actix::test]
async fn heavy_pending_chunks_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testnet();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in chunks.iter() {
        data.extend_from_slice(chunk);
    }

    let tx = signer.create_transaction(data, None)?;
    let tx = signer.sign_transaction(tx)?;

    // First post the chunks
    post_chunk(&app, &tx, 0, &chunks).await;
    post_chunk(&app, &tx, 1, &chunks).await;
    post_chunk(&app, &tx, 2, &chunks).await;

    // Then post the tx
    post_storage_tx(&app, &tx).await;

    // Mine some blocks to trigger chunk migration
    genesis_node.mine_blocks(2).await?;

    // Finally verify the chunks didn't get dropped
    let c1 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(0)).await;
    let c2 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(1)).await;
    let c3 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(2)).await;
    assert_matches!(c1, Some(_));
    assert_matches!(c2, Some(_));
    assert_matches!(c3, Some(_));

    genesis_node.stop().await;

    Ok(())
}

#[actix::test]
async fn heavy_pending_pledges_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testnet();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let _ = genesis_node.start_public_api().await;

    // Create stake and pledge commitments for the signer
    let stake_tx = new_stake_tx(&H256::zero(), &signer);
    let pledge_tx = new_pledge_tx(&H256::zero(), &signer);

    // Post the pledge before the stake
    genesis_node.post_commitment_tx(&pledge_tx).await;
    genesis_node.post_commitment_tx(&stake_tx).await;

    // Mine a block to confirm the commitments
    genesis_node.mine_block().await.unwrap();

    // Validate the SystemLedger in the block that it contains the correct commitments
    let block = genesis_node.get_block_by_height(1).await.unwrap();
    assert_eq!(
        block.system_ledgers[0].tx_ids,
        vec![stake_tx.id, pledge_tx.id]
    );

    genesis_node.stop().await;

    Ok(())
}

#[actix::test]
/// Test mempool persists to disk during shutdown
///
/// FIXME: This test will not be effective until mempool tree/index separation work is complete
///
/// post stake, post pledge, restart node
/// confirm pledge is present in mempool
/// post storage tx, restart node
/// confirm storage tx is present in mempool
async fn mempool_persistence_test() -> eyre::Result<()> {
    // Turn on tracing even before the node starts
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testnet();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let _ = genesis_node.start_public_api().await;

    // Create and post stake commitment for the signer
    let stake_tx = new_stake_tx(&H256::zero(), &signer);
    genesis_node.post_commitment_tx(&stake_tx).await;
    genesis_node.mine_block().await.unwrap();

    let expected_txs = vec![stake_tx.id];
    let result = genesis_node
        .wait_for_mempool_commitment_txs(expected_txs, 20)
        .await;
    assert!(result.is_ok());

    //create and post pledge commitment for the signer
    let pledge_tx = new_pledge_tx(&H256::zero(), &signer);
    genesis_node.post_commitment_tx(&pledge_tx).await;

    // test storage data
    let chunks = [[10; 32], [20; 32], [30; 32]];
    let data: Vec<u8> = chunks.concat();

    // post storage tx
    let storage_tx = genesis_node
        .post_storage_tx_without_gossip(H256::zero(), data, &signer)
        .await;

    let expected_txs = vec![storage_tx.header.clone()];
    let result = genesis_node.wait_for_confirmed_txs(expected_txs, 20).await;
    assert!(result.is_ok());

    // Restart the node
    tracing::info!("Restarting node");
    let restarted_node = genesis_node.stop().await.start().await;

    // confirm the mempool tx have appeared back in the mempool after a restart
    for txid_to_check in vec![storage_tx.header.id] {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let get_tx_msg = MempoolServiceMessage::GetTx(txid_to_check, oneshot_tx);
        if let Err(err) = restarted_node
            .node_ctx
            .service_senders
            .mempool
            .send(get_tx_msg)
        {
            tracing::error!("error sending message to mempool: {:?}", err);
        }
        let tx_from_mempool = oneshot_rx.await.expect("expected result");
        tracing::error!("transaction: {:?}", txid_to_check);
        assert!(tx_from_mempool.is_some());
    }

    // TODO: once mempool does not write directly to db, confirm commitment txs appear back in mempool after restart

    restarted_node.stop().await;

    Ok(())
}

// This test aims to (currently) test how the EVM interacts with forks and reorgs in the context of the mempool deciding which txs it should select
// it does this by:
// 1.) creating a fork with a transfer that would allow an account (recipient2) to afford a storage transaction (& validating this tx is included by the mempool)
// 2.) checking that the mempool function called for the block before this fork would prevent their transaction from being selected
// 3.) re-connecting the peers and ensuring that the correct fork was selected, and the account cannot afford the storage transaction (the funding tx was on the shorter fork)
// This test will probably be expanded in the future - it also includes a set of primitives for managing forks on the EVM/reth side too

#[actix_web::test]
async fn heavy_mempool_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=none;irys_p2p::server=none;irys_actors::mining=error",
    );
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().num_chunks_in_partition = 10;
    genesis_config
        .consensus
        .get_mut()
        .num_chunks_in_recall_range = 2;
    genesis_config.consensus.get_mut().num_partitions_per_slot = 1;
    genesis_config.storage.num_writes_before_sync = 1;
    genesis_config
        .consensus
        .get_mut()
        .entropy_packing_iterations = 1_000;
    genesis_config.consensus.get_mut().chunk_migration_depth = 1;

    // Create signers for the test accounts
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    let rich_account = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient1 = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient2 = IrysSigner::random_signer(&genesis_config.consensus_config());

    let chain_id = genesis_config.consensus_config().chain_id;

    // Fund genesis accounts for EVM transactions
    genesis_config.consensus.extend_genesis_accounts(vec![(
        rich_account.address(),
        GenesisAccount {
            balance: U256::from(1000000000000000000_u128), // 1 ETH
            ..Default::default()
        },
    )]);

    // Fund the peer signers for network participation
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis.start_public_api().await;

    // Setup Reth context for EVM transactions
    let genesis_reth_context = genesis.node_ctx.reth_node_adapter.clone();

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis.testnet_peer_with_signer(&peer1_signer);
    let peer2_config = genesis.testnet_peer_with_signer(&peer2_signer);

    // Start the peers
    let peer1 = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;
    peer1.start_public_api().await;

    let peer2 = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;
    peer2.start_public_api().await;

    // Setup Reth contexts for peers
    let peer1_reth_context = peer1.node_ctx.reth_node_adapter.clone();
    let peer2_reth_context = peer2.node_ctx.reth_node_adapter.clone();

    // ensure recipients have 0 balance
    let recipient1_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient1.address(), None)?;
    let recipient2_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient2.address(), None)?;
    assert_eq!(recipient1_init_balance, U256::ZERO);
    assert_eq!(recipient2_init_balance, U256::ZERO);

    // need to stake & pledge peers before they can mine
    let post_wait_stake_commitment =
        async |peer: &IrysNodeTest<IrysNodeCtx>,
               genesis: &IrysNodeTest<IrysNodeCtx>|
               -> eyre::Result<(CommitmentTransaction, CommitmentTransaction)> {
            let stake_tx = peer.post_stake_commitment(H256::zero()).await;
            genesis
                .wait_for_mempool(stake_tx.id, seconds_to_wait)
                .await?;
            let pledge_tx = peer.post_pledge_commitment(H256::zero()).await;
            genesis
                .wait_for_mempool(pledge_tx.id, seconds_to_wait)
                .await?;
            Ok((stake_tx, pledge_tx))
        };

    post_wait_stake_commitment(&peer1, &genesis).await?;
    post_wait_stake_commitment(&peer2, &genesis).await?;

    // Mine a block to get the commitments included
    genesis.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    genesis.mine_block().await.unwrap();

    // Wait for peers to sync and start packing
    peer1.wait_until_height(2, seconds_to_wait).await?;
    peer2.wait_until_height(2, seconds_to_wait).await?;
    peer1.wait_for_packing(seconds_to_wait).await;
    peer2.wait_for_packing(seconds_to_wait).await;

    // Create EVM transactions that will be used in the fork scenario
    let rich_signer: LocalSigner<SigningKey> = rich_account.clone().into();

    // Transaction 1: Send to recipient1 (will be in peer1's fork)
    let evm_tx_req1 = TransactionRequest {
        to: Some(TxKind::Call(recipient1.address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(1)),
        nonce: Some(1),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let tx_env1 = TransactionTestContext::sign_tx(rich_signer.clone(), evm_tx_req1).await;
    let signed_tx1: Bytes = tx_env1.encoded_2718().into();

    // Transaction 2: Send to recipient2 (will be in peer2's fork)
    // TODO: remove manual nonce calculations (tricky when dealing with forks...)
    let evm_tx_req2 = TransactionRequest {
        to: Some(TxKind::Call(recipient2.address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(1000000000000000_u128)),
        nonce: Some(1),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let tx_env2 = TransactionTestContext::sign_tx(rich_signer.clone(), evm_tx_req2).await;
    let signed_tx2: Bytes = tx_env2.encoded_2718().into();

    // Shared transaction that should be gossiped to all peers
    let shared_evm_tx_req = TransactionRequest {
        to: Some(TxKind::Call(recipient1.address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(123)),
        nonce: Some(0),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let shared_tx_env = TransactionTestContext::sign_tx(rich_signer, shared_evm_tx_req).await;
    let shared_signed_tx: Bytes = shared_tx_env.encoded_2718().into();

    // Inject the shared EVM transaction to genesis node (should gossip to peers)
    genesis_reth_context
        .rpc
        .inject_tx(shared_signed_tx)
        .await
        .expect("shared tx should be accepted");

    // mine a block
    // genesis.mine_block().await?;
    let (_block, reth_exec_env) = mine_block(&genesis.node_ctx).await?.unwrap();

    assert_eq!(reth_exec_env.block().transaction_count(), 1 + 1); // +1 for block reward

    peer1.wait_until_height(3, seconds_to_wait).await?;
    peer2.wait_until_height(3, seconds_to_wait).await?;

    // ensure the change is there
    let mut expected_recipient1_balance = U256::from(123);
    let mut expected_recipient2_balance = U256::from(0);

    let recipient1_balance = genesis_reth_context
        .rpc
        .get_balance(recipient1.address(), Some(BlockId::latest()))?;

    let recipient2_balance = genesis_reth_context
        .rpc
        .get_balance(recipient2.address(), None)?;

    assert_eq!(recipient1_balance, expected_recipient1_balance);
    assert_eq!(recipient2_balance, expected_recipient2_balance);

    // disconnect peers
    let disconnect_all = async |ctx: &IrysRethNodeAdapter| -> eyre::Result<Vec<PeerInfo>> {
        let all_peers1 = ctx.inner.network.get_all_peers().await?;
        for peer in all_peers1.iter() {
            ctx.inner.network.disconnect_peer(peer.remote_id);
        }

        while !ctx.inner.network.get_all_peers().await?.is_empty() {
            sleep(Duration::from_millis(100)).await;
        }

        let all_peers2 = ctx.inner.network.get_all_peers().await?;
        assert!(
            all_peers2.is_empty(),
            "the peer should be completely disconnected",
        );

        Ok(all_peers1)
    };

    let genesis_peers = disconnect_all(&genesis_reth_context).await?;
    let peer1_peers = disconnect_all(&peer1_reth_context).await?;
    let peer2_peers = disconnect_all(&peer2_reth_context).await?;

    let wait_for_evm_tx = async |ctx: &IrysRethNodeAdapter, hash: &B256| -> eyre::Result<()> {
        // wait until the tx shows up
        let rpc = ctx.rpc_client().unwrap();
        loop {
            match EthApiClient::<Transaction, Block, Receipt, Header>::transaction_by_hash(
                &rpc, *hash,
            )
            .await?
            {
                Some(_tx) => {
                    return Ok(());
                }
                None => sleep(Duration::from_millis(200)).await,
            }
        }
    };

    peer1_reth_context
        .rpc
        .inject_tx(signed_tx1.clone())
        .await
        .expect("peer1 tx should be accepted");

    wait_for_evm_tx(&peer1_reth_context, tx_env1.hash()).await?;

    expected_recipient1_balance += U256::from(1);

    peer2_reth_context
        .rpc
        .inject_tx(signed_tx2.clone())
        .await
        .expect("peer2 tx should be accepted");

    wait_for_evm_tx(&peer2_reth_context, tx_env2.hash()).await?;

    expected_recipient2_balance += U256::from(1000000000000000_u128);

    // Mine blocks on both peers in parallel to create a fork
    let (result1, result2) = tokio::join!(
        peer1.mine_blocks_without_gossip(1),
        peer2.mine_blocks_without_gossip(1)
    );

    // Fail the test on any error results
    result1?;
    result2?;

    // validate the peer blocks create forks with different EVM transactions

    let peer1_recipient1_balance = peer1_reth_context
        .rpc
        .get_balance(recipient1.address(), None)?;

    let peer1_recipient2_balance = peer1_reth_context
        .rpc
        .get_balance(recipient2.address(), None)?;

    let peer2_recipient1_balance = peer2_reth_context
        .rpc
        .get_balance(recipient1.address(), None)?;

    let peer2_recipient2_balance = peer2_reth_context
        .rpc
        .get_balance(recipient2.address(), None)?;

    // verify the fork
    assert_eq!(peer1_recipient1_balance, expected_recipient1_balance);
    assert_ne!(peer1_recipient2_balance, expected_recipient2_balance);

    assert_eq!(peer2_recipient2_balance, expected_recipient2_balance);
    assert_ne!(peer2_recipient1_balance, expected_recipient1_balance);

    // reconnect the peers

    let reconnect_all = async |ctx: &IrysRethNodeAdapter, peers: &Vec<PeerInfo>| {
        for peer in peers {
            ctx.inner
                .network
                .connect_peer(peer.remote_id, peer.remote_addr);
        }
    };

    reconnect_all(&genesis_reth_context, &genesis_peers).await;
    reconnect_all(&peer1_reth_context, &peer1_peers).await;
    reconnect_all(&peer2_reth_context, &peer2_peers).await;

    // try to insert a storage tx that is only valid on peer2's fork
    // then try to fetch best txs from peer2 once it reorgs to peer1's fork
    // which should fail/not include the TX

    let chunks = [[40; 32], [50; 32], [60; 32]];
    let data: Vec<u8> = chunks.concat();

    let _peer2_tx = peer2
        .post_storage_tx_without_gossip(H256::zero(), data, &recipient2)
        .await;

    // call get best txs from the mempool

    let (tx, rx) = oneshot::channel();

    peer2
        .node_ctx
        .service_senders
        .mempool
        .send(MempoolServiceMessage::GetBestMempoolTxs(
            Some(BlockId::number(peer2.get_height().await - 1)),
            tx,
        ))?;

    let best_previous = rx.await?;
    // previous block does not have the fund tx, the tx should not be present
    assert_eq!(
        best_previous.storage_tx.len(),
        0,
        "there should not be a storage tx (lack of funding due to changed parent EVM block)"
    );

    let (tx, rx) = oneshot::channel();
    // latest
    peer2
        .node_ctx
        .service_senders
        .mempool
        .send(MempoolServiceMessage::GetBestMempoolTxs(None, tx))?;
    let best_current = rx.await?;
    // latest block has the fund tx, so it should be present
    assert_eq!(
        best_current.storage_tx.len(),
        1,
        "There should be a storage tx"
    );

    // mine another block on peer1, so it's the longest chain (with gossip)
    let height = peer1.get_height().await;
    peer1.mine_block().await?;
    // peers should be able to sync
    let (gen, p2) = tokio::join!(
        genesis.wait_until_height(height + 1, 20),
        peer2.wait_until_height(height + 1, 20)
    );

    gen?;
    p2?;

    // the storage tx shouldn't be in the best mempool txs due to the fork change

    let (tx, rx) = oneshot::channel();
    peer2
        .node_ctx
        .service_senders
        .mempool
        .send(MempoolServiceMessage::GetBestMempoolTxs(None, tx))?;
    let best_current = rx.await?;

    assert_eq!(
        best_current.storage_tx.len(),
        0,
        "There shouldn't be a storage tx"
    );

    tokio::join!(genesis.stop(), peer1.stop(), peer2.stop());

    Ok(())
}
