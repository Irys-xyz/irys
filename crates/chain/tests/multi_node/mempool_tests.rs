use crate::utils::*;
use alloy_core::primitives::{Bytes, TxKind, B256, U256};
use alloy_eips::{BlockId, Encodable2718 as _};
use alloy_genesis::GenesisAccount;
use alloy_signer_local::LocalSigner;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_chain::IrysNodeCtx;
use irys_database::tables::IngressProofs;
use irys_reth_node_bridge::{
    ext::IrysRethRpcTestContextExt as _, reth_e2e_test_utils::transaction::TransactionTestContext,
    IrysRethNodeAdapter,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, DataLedger, IngressProofsList, IrysBlockHeader,
    IrysTransaction, NodeConfig, TxIngressProof, H256,
};
use k256::ecdsa::SigningKey;
use rand::Rng as _;
use reth::{
    network::{PeerInfo, Peers as _},
    primitives::{Receipt, Transaction},
    rpc::{
        api::EthApiClient,
        types::{Block, Header, TransactionRequest},
    },
};
use reth_db::transaction::DbTx as _;
use reth_db::Database as _;
use std::time::Duration;
use tokio::{sync::oneshot, time::sleep};
use tracing::debug;

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

    // retrieve block_migration_depth for use later
    let mut consensus = genesis_node.cfg.consensus.clone();
    let block_migration_depth = consensus.get_mut().block_migration_depth;

    // chunks
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

    // Then post the tx (deliberately after the chunks)
    post_data_tx(&app, &tx).await;

    // wait for chunks to be in CachedChunks table
    genesis_node.wait_for_chunk_cache_count(3, 10).await?;

    // Mine some blocks to trigger block and chunk migration
    genesis_node
        .mine_blocks((1 + block_migration_depth).try_into()?)
        .await?;
    genesis_node.wait_until_block_index_height(1, 5).await?;

    // Finally verify the chunks didn't get dropped
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 0, 5)
        .await?;
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 1, 5)
        .await?;
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 2, 5)
        .await?;

    // teardown
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
        .post_data_tx_without_gossip(H256::zero(), data, &signer)
        .await;

    // Restart the node
    tracing::info!("Restarting node");
    let restarted_node = genesis_node.stop().await.start().await;

    // confirm the mempool data tx have appeared back in the mempool after a restart
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let get_tx_msg = MempoolServiceMessage::GetDataTxs(vec![storage_tx.header.id], oneshot_tx);
    if let Err(err) = restarted_node
        .node_ctx
        .service_senders
        .mempool
        .send(get_tx_msg)
    {
        tracing::error!("error sending message to mempool: {:?}", err);
    }
    let data_tx_from_mempool = oneshot_rx.await.expect("expected result");
    assert!(data_tx_from_mempool
        .first()
        .expect("expected a data tx")
        .is_some());

    // confirm the commitment tx has appeared back in the mempool after a restart
    let result = restarted_node
        .wait_for_mempool_commitment_txs(vec![pledge_tx.id], 10)
        .await;
    assert!(result.is_ok());

    restarted_node.stop().await;

    Ok(())
}

#[actix_web::test]
async fn heavy_mempool_submit_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=off,storage::db::mdbx=off,reth=off,irys_p2p::server=off,irys_actors::mining=error",
    );

    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch: u64 = 3;
    let seconds_to_wait = 15;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;
    // TODO: change anchor
    genesis_config
        .consensus
        .get_mut()
        .mempool
        .anchor_expiry_depth = 100; // don't care about anchor expiry
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    // Create a signer (keypair) for the peer and fund it
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();

    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis_node.testnet_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testnet_peer_with_signer(&peer2_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;
    peer1_node.start_public_api().await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;
    peer2_node.start_public_api().await;

    // Post stake + pledge commitments to peer1
    let peer1_stake_tx = peer1_node.post_stake_commitment(H256::zero()).await; // zero() is the genesis block hash
    let peer1_pledge_tx = peer1_node.post_pledge_commitment(H256::zero()).await;

    // Post stake + pledge commitments to peer2
    let peer2_stake_tx = peer2_node.post_stake_commitment(H256::zero()).await;
    let peer2_pledge_tx = peer2_node.post_pledge_commitment(H256::zero()).await;

    // Wait for all commitment tx to show up in the genesis_node's mempool
    genesis_node
        .wait_for_mempool(peer1_stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer1_pledge_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_pledge_tx.id, seconds_to_wait)
        .await?;

    let mut expected_height = num_blocks_in_epoch;

    // Mine blocks to get the commitments included, epoch tasks performed, and assignments of partition_hash's to the peers
    genesis_node.mine_blocks(expected_height as usize).await?;

    // wait for block mining to reach tree height
    genesis_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;

    // wait for migration to reach index height
    genesis_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Get the genesis nodes view of the peers assignments
    let peer1_assignments = genesis_node.get_partition_assignments(peer1_signer.address());
    let peer2_assignments = genesis_node.get_partition_assignments(peer2_signer.address());

    // Verify that one partition has been assigned to each peer to match its pledge
    assert_eq!(peer1_assignments.len(), 1);
    assert_eq!(peer2_assignments.len(), 1);

    // Wait for the peers to receive & process the epoch block
    peer1_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Wait for them to pack their storage modules with the partition_hashes
    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    struct ForkFnReturn {
        // shared_tx: (IrysTransaction, [[u8; 32]; 3]),
        peer1_tx: (IrysTransaction /*  [[u8; 32]; 3] */,),
        peer2_tx: (IrysTransaction /*  [[u8; 32]; 3] */,),
        peer1_block: IrysBlockHeader,
        peer2_block: IrysBlockHeader,
    }

    let fork_fn = async |expected_height: &mut u64| -> eyre::Result<ForkFnReturn> {
        let mut rng = rand::thread_rng();
        let chunks1: [[u8; 32]; 3] = [[rng.gen(); 32], [rng.gen(); 32], [rng.gen(); 32]];
        let data1: Vec<u8> = chunks1.concat();

        let chunks2 = [[rng.gen(); 32], [rng.gen(); 32], [rng.gen(); 32]];
        let data2: Vec<u8> = chunks2.concat();

        let chunks3 = [[rng.gen(); 32], [rng.gen(); 32], [rng.gen(); 32]];
        let data3: Vec<u8> = chunks3.concat();

        // Post a transaction that should be gossiped to all peers
        let shared_tx = genesis_node
            .post_data_tx(
                H256::zero(),
                data3,
                &genesis_node.node_ctx.config.irys_signer(),
            )
            .await;

        // Wait for the transaction to gossip

        let txid = shared_tx.header.id;
        debug!("JESSEDEBUG2 SHARED TX {}", &txid);

        peer1_node.wait_for_mempool(txid, seconds_to_wait).await?;
        peer2_node.wait_for_mempool(txid, seconds_to_wait).await?;
        debug!("JESSEDEBUG2 WFMP DONE");

        // Post a unique storage transaction to each peer
        let peer1_tx = peer1_node
            .post_data_tx_without_gossip(H256::zero(), data1, &peer1_signer)
            .await;
        let peer2_tx = peer2_node
            .post_data_tx_without_gossip(H256::zero(), data2, &peer2_signer)
            .await;
        let p1_height = peer1_node.get_height().await;
        let p2_height = peer2_node.get_height().await;
        debug!("JESSEDEBUG2 HEIGHTS {} {}", &p1_height, &p2_height);

        // Mine mine blocks on both peers in parallel
        let (result1, result2) = tokio::join!(
            peer1_node.mine_blocks_without_gossip(1),
            peer2_node.mine_blocks_without_gossip(1)
        );

        // Fail the test on any error results
        result1?;
        result2?;

        *expected_height += 1;

        // wait for block mining to reach tree height
        peer1_node
            .wait_until_height(*expected_height, seconds_to_wait)
            .await?;
        peer2_node
            .wait_until_height(*expected_height, seconds_to_wait)
            .await?;
        // wait for migration to reach index height
        peer1_node
            .wait_until_block_index_height(
                *expected_height - block_migration_depth,
                seconds_to_wait,
            )
            .await?;
        peer2_node
            .wait_until_block_index_height(
                *expected_height - block_migration_depth,
                seconds_to_wait,
            )
            .await?;

        // Validate the peer blocks create forks with different transactions
        let peer1_block = peer1_node.get_block_by_height(*expected_height).await?;
        let peer2_block = peer2_node.get_block_by_height(*expected_height).await?;

        debug!(
            "JESSEDEBUG2 FRK LDGR BLK {} {:?}",
            expected_height, &peer1_block.data_ledgers
        );
        debug!(
            "JESSEDEBUG2 FRK LDGR BLK2 {} {:?}",
            expected_height, &peer2_block.data_ledgers
        );

        let peer1_block_txids = &peer1_block.data_ledgers[DataLedger::Submit].tx_ids.0;
        assert!(
            peer1_block_txids.contains(&txid),
            "block {} {} should include submit tx {}",
            &peer1_block.block_hash,
            &peer1_block.height,
            &txid
        );
        assert!(peer1_block_txids.contains(&peer1_tx.header.id));

        let peer2_block_txids = &peer2_block.data_ledgers[DataLedger::Submit].tx_ids.0;
        assert!(peer2_block_txids.contains(&txid));
        assert!(peer2_block_txids.contains(&peer2_tx.header.id));

        // Assert both blocks have the same cumulative difficulty this will ensure
        // that the peers prefer the first block they saw with this cumulative difficulty,
        // their own.
        assert_eq!(peer1_block.cumulative_diff, peer2_block.cumulative_diff);

        peer2_node.gossip_block(&peer2_block)?;
        peer1_node.gossip_block(&peer1_block)?;

        // Wait for gossip, to send blocks to opposite peers
        peer1_node
            .wait_for_block(&peer2_block.block_hash, 10)
            .await?;
        peer2_node
            .wait_for_block(&peer1_block.block_hash, 10)
            .await?;
        peer1_node.get_block_by_hash(&peer2_block.block_hash)?;
        peer2_node.get_block_by_hash(&peer1_block.block_hash)?;

        let peer1_block_after = peer1_node.get_block_by_height(*expected_height).await?;
        let peer2_block_after = peer2_node.get_block_by_height(*expected_height).await?;

        // Verify neither peer changed their blocks after receiving the other peers block
        // for the same height.
        assert_eq!(peer1_block_after.block_hash, peer1_block.block_hash);
        assert_eq!(peer2_block_after.block_hash, peer2_block.block_hash);
        debug!(
        "\nPEER1\n    before: {} c_diff: {}\n    after:  {} c_diff: {}\nPEER2\n    before: {} c_diff: {}\n    after:  {} c_diff: {}",
        peer1_block.block_hash,
        peer1_block.cumulative_diff,
        peer1_block_after.block_hash,
        peer1_block_after.cumulative_diff,
        peer2_block.block_hash,
        peer2_block.cumulative_diff,
        peer2_block_after.block_hash,
        peer2_block_after.cumulative_diff,
    );

        Ok(ForkFnReturn {
            // shared_tx: (shared_tx, chunks3),
            peer1_tx: (peer1_tx /* chunks1 */,),
            peer2_tx: (peer2_tx /* chunks2 */,),
            peer1_block,
            peer2_block,
        })
    };

    let fork = fork_fn(&mut expected_height).await?;

    let ForkFnReturn {
        peer1_tx: (peer1_tx /*  chunks1 */,),
        peer2_tx: (peer2_tx /* chunks2 */,),
        peer1_block,
        peer2_block,
        ..
    } = fork;

    let _block_hash = genesis_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;
    let genesis_block = genesis_node.get_block_by_height(expected_height).await?;

    debug!(
        "\nGENESIS: {:?} height: {}",
        genesis_block.block_hash, genesis_block.height
    );

    enum ForkWinner {
        Peer1,
        Peer2,
    }

    let reorg_fn = async |peer1_tx: IrysTransaction,
                          peer2_tx: IrysTransaction,
                          expected_height: &mut u64|
           -> eyre::Result<(ForkWinner, IrysBlockHeader, IrysTransaction)> {
        let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);
        let canon_before = genesis_node
            .node_ctx
            .block_tree_guard
            .read()
            .get_canonical_chain();

        // Determine which peer lost the fork race and extend the other peer's chain
        // to trigger a reorganization. The losing peer's transaction will be evicted
        // and returned to the mempool.
        let reorg_tx: IrysTransaction;
        let _reorg_block_hash: H256;
        let mut fork_winner = ForkWinner::Peer1;
        let reorg_block = if genesis_block.block_hash == peer1_block.block_hash {
            debug!(
                "GENESIS: should ignore {} and should already be on {} height: {}",
                peer2_block.block_hash, peer1_block.block_hash, genesis_block.height
            );
            reorg_tx = peer1_tx; // Peer1 won initially, so peer2's chain will overtake it
            peer2_node.mine_block().await?;
            *expected_height += 1;
            peer2_node.get_block_by_height(*expected_height).await?
        } else {
            debug!(
                "GENESIS: should ignore {} and should already be on {} height: {}",
                peer1_block.block_hash, peer2_block.block_hash, genesis_block.height
            );
            fork_winner = ForkWinner::Peer2;
            reorg_tx = peer2_tx; // Peer2 won initially, so peer1's chain will overtake it
            peer1_node.mine_block().await?;
            *expected_height += 1;
            peer1_node.get_block_by_height(*expected_height).await?
        };

        let reorg_event = reorg_future.await?;
        let _genesis_block = genesis_node.get_block_by_height(*expected_height).await?;

        debug!("{:?}", reorg_event);
        let canon = genesis_node
            .node_ctx
            .block_tree_guard
            .read()
            .get_canonical_chain();

        let old_fork_hashes: Vec<_> = reorg_event.old_fork.iter().map(|b| b.block_hash).collect();
        let new_fork_hashes: Vec<_> = reorg_event.new_fork.iter().map(|b| b.block_hash).collect();

        debug!(
            "ReorgEvent:\n fork_parent: {:?}\n old_fork: {:?}\n new_fork:{:?}",
            reorg_event.fork_parent.block_hash, old_fork_hashes, new_fork_hashes
        );

        debug!("reorg_tx: {:?}", reorg_tx.header.id);
        debug!("canonical_before: {:?}", &canon_before.0);

        debug!("canonical_after: {:?}", &canon.0);

        // Validate the ReorgEvent with the canonical chains
        let old_fork: Vec<_> = reorg_event
            .old_fork
            .iter()
            .map(|bh| bh.block_hash)
            .collect();

        let new_fork: Vec<_> = reorg_event
            .new_fork
            .iter()
            .map(|bh| bh.block_hash)
            .collect();

        debug!("fork_parent: {:?}", reorg_event.fork_parent.block_hash);
        debug!("old_fork:  {:?}", old_fork);
        debug!("new_fork:  {:?}", new_fork);

        assert_eq!(old_fork, vec![canon_before.0.last().unwrap().block_hash]);
        assert_eq!(
            new_fork,
            vec![
                canon.0[canon.0.len() - 2].block_hash,
                canon.0.last().unwrap().block_hash
            ]
        );

        assert_eq!(reorg_event.new_tip, *new_fork.last().unwrap());
        Ok((fork_winner, reorg_block, reorg_tx))
    };

    // Make sure the reorg_tx is back in the mempool ready to be included in the next block
    // NOTE: It turns out the reorg_tx is actually in the block because all tx are gossiped
    //       along with their blocks even if they are a fork, so when the peer
    //       extends their fork, they have the fork tx in their mempool already
    //       and it gets included in the block.
    // let pending_tx = genesis_node.get_best_mempool_tx(None).await;
    // let tx = pending_tx
    //     .submit_tx
    //     .iter()
    //     .find(|tx| tx.id == reorg_tx.header.id);
    // assert_eq!(tx, Some(&reorg_tx.header));

    // with that ^ in mind, validate that the reorg tip block has the fork submit tx included
    let (_fork_winner, reorg_block, reorg_tx) =
        reorg_fn(peer1_tx, peer2_tx, &mut expected_height).await?;

    assert!(reorg_block.data_ledgers[DataLedger::Submit]
        .tx_ids
        .contains(&reorg_tx.header.id));

    // Wind down test
    tokio::join!(genesis_node.stop(), peer1_node.stop(), peer2_node.stop());
    Ok(())
}

/// this test tests reorging in the context of confirmed publish ledger transactions
/// goals:
/// - ensure orphaned publish ledger txs are included in the mempool & in blocks post-reorg
/// - ensure reorged publish txs are always associated with the canonical ingress proof(s)
/// Steps:
/// create 3 nodes: A (genesis), B and C
/// mine commitments for B and C using A
/// make sure all the nodes are synchronised
/// prevent all gossip/P2P
/// send A a storage Tx, ready for promotion
/// send B a storage Tx
/// prime a fork - mine one block on A and two on B
///  the second B block should promote B's storage tx
/// assert A and B's blocks include their respective promoted tx
/// trigger a reorg - gossip B's txs & blocks to A
/// assert that A has a reorg event
/// assert that A's tx is returned to the mempool
/// gossip A's tx to B & prepare it for promotion
/// mine a block on B, assert A's tx is included correctly
/// gossip B's block back to A, assert mempool state ingress proofs etc are correct
// TODO: once longer forks are stable & if it's worthwhile:
/// mine 4 blocks on C
/// gossip these to A
/// assert all txs return to mempool
/// gossip C's blocks to B
/// assert txs return to mempool
/// gossip returned txs to C
/// mine a block on C, assert that all reorgd txs are present

#[actix_web::test]
async fn heavy_mempool_publish_fork_recovery_test() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=off,storage::db::mdbx=off,reth=off,irys_p2p::server=off,irys_actors::mining=error",
    );
    initialize_tracing();

    // config variables
    let num_blocks_in_epoch = 5; // test currently mines 4 blocks, and expects txs to remain in mempool
    let seconds_to_wait = 15;

    // setup config
    let block_migration_depth: u64 = num_blocks_in_epoch - 1;
    let mut a_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch.try_into().unwrap());
    a_config.consensus.get_mut().chunk_size = 32;
    a_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    // signers
    // Create a signer (keypair) for the peer and fund it
    let mut b_signer = a_config.new_random_signer();
    b_signer.signer = SigningKey::from_slice(
        hex::decode("b360d276e1a5a26c59d46e5b12e0cec0f5166cb69552b5ba282a661bf2f8fe3e")?.as_slice(),
    )?;
    let mut c_signer = a_config.new_random_signer();
    c_signer.signer = SigningKey::from_slice(
        hex::decode("ff235e7114eb975ca3bef4210db9aab2443c422497021e452f9dd4a327c562dc")?.as_slice(),
    )?;

    a_config.fund_genesis_accounts(vec![&b_signer, &c_signer]);

    let a_node = IrysNodeTest::new_genesis(a_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    let a_signer = a_node.node_ctx.config.irys_signer();

    //  additional configs for peers
    let config_1 = a_node.testnet_peer_with_signer(&c_signer);
    let config_2 = a_node.testnet_peer_with_signer(&b_signer);

    // start peer nodes
    let b_node = IrysNodeTest::new(config_1)
        .start_and_wait_for_packing("NODE_B", seconds_to_wait)
        .await;
    let c_node = IrysNodeTest::new(config_2)
        .start_and_wait_for_packing("NODE_C", seconds_to_wait)
        .await;

    let mut network_height = 0;
    {
        // Post stake + pledge commitments to b
        let b_stake_tx = b_node.post_stake_commitment(H256::zero()).await; // zero() is the a block hash
        let b_pledge_tx = b_node.post_pledge_commitment(H256::zero()).await;

        // Post stake + pledge commitments to c
        let c_stake_tx = c_node.post_stake_commitment(H256::zero()).await;
        let c_pledge_tx = c_node.post_pledge_commitment(H256::zero()).await;

        // Wait for all commitment tx to show up in the node_a's mempool
        a_node
            .wait_for_mempool(b_stake_tx.id, seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(b_pledge_tx.id, seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_stake_tx.id, seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_pledge_tx.id, seconds_to_wait)
            .await?;

        // Mine blocks to get the commitments included, epoch tasks performed, and assignments of partition_hash's to the peers
        a_node
            .mine_blocks(num_blocks_in_epoch.try_into().unwrap())
            .await?;
        network_height += num_blocks_in_epoch;
        // wait for block mining to reach tree height
        a_node
            .wait_until_height(network_height, seconds_to_wait)
            .await?;

        // wait for migration to reach index height
        a_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Get the a nodes view of the peers assignments
        let b_assignments = a_node.get_partition_assignments(b_signer.address());
        let c_assignments = a_node.get_partition_assignments(c_signer.address());

        // Verify that one partition has been assigned to each peer to match its pledge
        assert_eq!(b_assignments.len(), 1);
        assert_eq!(c_assignments.len(), 1);

        // Wait for the peers to receive & process the epoch block
        b_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;
        c_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Wait for them to pack their storage modules with the partition_hashes
        b_node.wait_for_packing(seconds_to_wait).await;
        c_node.wait_for_packing(seconds_to_wait).await;
    }

    // check peer heights match a - i.e. that we are all in sync
    network_height = a_node.get_height().await;

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;
    c_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    // disable P2P/gossip
    {
        a_node.gossip_disable();
        b_node.gossip_disable();
        c_node.gossip_disable();
    }

    // A: create tx & mine block 1 (relative)

    let a_blk1_tx1 = a_node
        .post_data_tx(
            H256::zero(),
            [[1; 32], [1; 32], [1; 32]].concat(),
            &a_signer,
        )
        .await;

    a_node.upload_chunks(&a_blk1_tx1).await?;
    a_node
        .wait_for_ingress_proofs_no_mining(vec![a_blk1_tx1.header.id], seconds_to_wait)
        .await?;

    a_node.mine_block().await?;
    network_height += 1;
    a_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    let a_blk1 = a_node.get_block_by_height(network_height).await?;
    // check that a_blk1 contains a_blk1_tx1 in both publish and submit ledgers
    assert_eq!(
        a_blk1.data_ledgers[DataLedger::Submit].tx_ids,
        vec![a_blk1_tx1.header.id]
    );
    assert_eq!(
        a_blk1.data_ledgers[DataLedger::Publish].tx_ids,
        vec![a_blk1_tx1.header.id]
    );

    // B: mine block 1

    let b_blk1_tx1 = {
        let b_blk1_tx1 = b_node
            .post_data_tx(
                H256::zero(),
                [[2; 32], [2; 32], [2; 32]].concat(),
                &b_signer,
            )
            .await;

        b_node.upload_chunks(&b_blk1_tx1).await?;
        b_node
            .wait_for_ingress_proofs_no_mining(vec![b_blk1_tx1.header.id], seconds_to_wait)
            .await?;

        b_blk1_tx1
    };

    b_node.mine_block().await?;

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    let b_blk1 = b_node.get_block_by_height(network_height).await?;

    assert_eq!(
        b_blk1.data_ledgers[DataLedger::Submit].tx_ids,
        vec![b_blk1_tx1.header.id]
    );

    assert_eq!(
        b_blk1.data_ledgers[DataLedger::Publish].tx_ids,
        vec![b_blk1_tx1.header.id]
    );

    // B: Tx & Block 2

    // don't upload chunks, we want this in the submit ledger
    let b_blk2_tx1 = b_node
        .post_data_tx(
            H256::zero(),
            [[3; 32], [3; 32], [3; 32]].concat(),
            &b_signer,
        )
        .await;

    b_node.mine_block().await?;
    network_height += 1;

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    let b_blk2 = b_node.get_block_by_height(network_height).await?;
    assert_eq!(
        b_blk2.data_ledgers[DataLedger::Submit].tx_ids,
        vec![b_blk2_tx1.header.id]
    );
    assert_eq!(
        b_blk2.data_ledgers[DataLedger::Publish].tx_ids, // should not be promoted
        vec![]
    );

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    // Gossip B1&2 to A, causing a reorg
    let a1_b2_reorg_fut = a_node.wait_for_reorg(seconds_to_wait);

    a_node.post_data_tx_raw(&b_blk1_tx1.header).await;
    b_node.send_block_to_peer(&a_node, &b_blk1).await?;

    a_node.post_data_tx_raw(&b_blk2_tx1.header).await;
    b_node.send_block_to_peer(&a_node, &b_blk2).await?;

    a_node
        .wait_for_block(&b_blk1.block_hash, seconds_to_wait)
        .await?;

    a_node
        .wait_for_block(&b_blk2.block_hash, seconds_to_wait)
        .await?;

    debug!(
        "JESSEDEBUG2 B BLOCKS {} {} {}",
        &b_blk1.block_hash,
        &b_blk2.block_hash,
        &b_node.get_height().await
    );

    // wait for a reorg event

    let _a1_b2_reorg = a1_b2_reorg_fut.await?;

    a_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    assert_eq!(
        a_node.get_block_by_height(network_height).await?,
        b_node.get_block_by_height(network_height).await?
    );

    // assert that a_blk1_tx1 is back in a's mempool
    let a1_b2_reorg_mempool_txs = a_node.get_best_mempool_tx(None).await?;

    assert_eq!(
        a1_b2_reorg_mempool_txs.submit_tx,
        vec![a_blk1_tx1.header.clone()]
    );

    let a_blk1_tx1_proof1 = {
        let tx = a_node.node_ctx.db.tx()?;
        // TODO: why do we have two structs? TxIngressProof and IngressProof?
        // probably not worth worrying about given ingress proofs need a proper impl, and this should be handled then
        tx.get::<IngressProofs>(a_blk1_tx1.header.data_root)?
            .expect("Able to get a_blk1_tx1's ingress proof from DB")
    };

    let mut a_blk1_tx1_published = a_blk1_tx1.header.clone();
    a_blk1_tx1_published.ingress_proofs = Some(TxIngressProof {
        proof: a_blk1_tx1_proof1.proof,
        signature: a_blk1_tx1_proof1.signature,
    });

    // assert that a_blk1_tx1 shows back up in get_best_mempool_txs (treated as if it wasn't promoted)
    assert_eq!(
        a1_b2_reorg_mempool_txs.publish_tx.0,
        vec![a_blk1_tx1_published]
    );

    let a_blk1_tx1_mempool = {
        let (tx, rx) = oneshot::channel();
        a_node
            .node_ctx
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetDataTxs(
                vec![a_blk1_tx1.header.id],
                tx,
            ))?;
        let mempool_txs = rx.await?;
        let a_blk1_tx1_mempool = mempool_txs.first().unwrap().clone().unwrap();
        a_blk1_tx1_mempool
    };

    // ensure a_blk1_tx1 was orphaned back into the mempool, *without* an ingress proof
    // note: as [`get_publish_txs_and_proofs`] resolves ingress proofs, calling get_best_mempool_txs will return the header with an ingress proof.
    // so we have a separate path & assert to ensure the ingress proof is being removed when the tx is orphaned
    assert_eq!(a_blk1_tx1_mempool, a_blk1_tx1.header);

    // gossip A's orphaned tx to B
    // get it ready for promotion, and then mine a block on B to include it

    b_node.post_data_tx_raw(&a_blk1_tx1.header).await;
    b_node.upload_chunks(&a_blk1_tx1).await?;
    b_node
        .wait_for_ingress_proofs_no_mining(vec![a_blk1_tx1.header.id], seconds_to_wait)
        .await?;

    // B: Mine B3

    b_node.mine_block().await?;
    network_height += 1;

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    let b_blk3 = b_node.get_block_by_height(network_height).await?;

    // ensure a_blk1_tx1 is included
    assert_eq!(
        b_blk3.data_ledgers[DataLedger::Submit].tx_ids,
        vec![a_blk1_tx1.header.id]
    );
    assert_eq!(
        b_blk3.data_ledgers[DataLedger::Publish].tx_ids, // should be promoted
        vec![a_blk1_tx1.header.id]
    );

    assert!(b_blk3.data_ledgers[DataLedger::Publish].proofs.is_some());

    // a_blk1_tx1 should have a new ingress proof (assert it's not the original from a_blk2)
    assert!(b_blk3.data_ledgers[DataLedger::Publish]
        .proofs
        .clone()
        .unwrap()
        .ne(&IngressProofsList(vec![TxIngressProof {
            proof: a_blk1_tx1_proof1.proof,
            signature: a_blk1_tx1_proof1.signature
        }])));

    // now we gossip B3 back to A
    // it shouldn't reorg, and should accept the block
    // as well as overriding the ingress proof it has locally with the one from the block

    b_node.send_block_to_peer(&a_node, &b_blk3).await?;

    a_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    assert_eq!(
        a_node.get_block_by_height(network_height).await?,
        b_node.get_block_by_height(network_height).await?
    );

    // assert that a_blk1_tx1 is no longer present in the mempool
    // (nothing should be in the mempool)
    let a_b_blk3_mempool_txs = a_node.get_best_mempool_tx(None).await?;
    assert!(a_b_blk3_mempool_txs.submit_tx.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.0.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.1.is_empty());
    assert!(a_b_blk3_mempool_txs.commitment_tx.is_empty());

    // get a_blk1_tx1 from a, it should have b_blk3's ingress proof
    let a_blk1_tx1_b_blk3_tx1 = a_node
        .get_storage_tx_header_from_mempool(&a_blk1_tx1.header.id)
        .await?;

    assert_eq!(
        a_blk1_tx1_b_blk3_tx1.ingress_proofs,
        Some(
            b_blk3.data_ledgers[DataLedger::Publish]
                .proofs
                .clone()
                .unwrap()
                .first()
                .unwrap()
                .clone()
        )
    );

    // tada!

    // gracefully shutdown nodes
    tokio::join!(a_node.stop(), b_node.stop(), c_node.stop(),);
    debug!("DONE!");
    Ok(())
}

// This test aims to (currently) test how the EVM interacts with forks and reorgs in the context of the mempool deciding which txs it should select
// it does this by:
// 1.) creating a fork with a transfer that would allow an account (recipient2) to afford a storage transaction (& validating this tx is included by the mempool)
// 2.) checking that the mempool function called for the block before this fork would prevent their transaction from being selected
// 3.) re-connecting the peers and ensuring that the correct fork was selected, and the account cannot afford the storage transaction (the funding tx was on the shorter fork)
// This test will probably be expanded in the future - it also includes a set of primitives for managing forks on the EVM/reth side too

#[actix_web::test]
async fn heavy_evm_mempool_fork_recovery_test() -> eyre::Result<()> {
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
    genesis_config.consensus.get_mut().block_migration_depth = 1;

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
    let _block_hash = peer1.wait_until_height(2, seconds_to_wait).await?;
    let _block_hash = peer2.wait_until_height(2, seconds_to_wait).await?;
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

    let _block_hash = peer1.wait_until_height(3, seconds_to_wait).await?;
    let _block_hash = peer2.wait_until_height(3, seconds_to_wait).await?;

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
    let genesis_peers = genesis.disconnect_all_reth_peers().await?;
    let peer1_peers = peer1.disconnect_all_reth_peers().await?;
    let peer2_peers = peer2.disconnect_all_reth_peers().await?;

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
        .post_data_tx_without_gossip(H256::zero(), data, &recipient2)
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

    let best_previous = rx.await??;
    // previous block does not have the fund tx, the tx should not be present
    assert_eq!(
        best_previous.submit_tx.len(),
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
    let best_current = rx.await??;
    // latest block has the fund tx, so it should be present
    assert_eq!(
        best_current.submit_tx.len(),
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

    let _block_hash = gen?;
    let _block_hash = p2?;

    // the storage tx shouldn't be in the best mempool txs due to the fork change

    let (tx, rx) = oneshot::channel();
    peer2
        .node_ctx
        .service_senders
        .mempool
        .send(MempoolServiceMessage::GetBestMempoolTxs(None, tx))?;
    let best_current = rx.await??;

    assert_eq!(
        best_current.submit_tx.len(),
        0,
        "There shouldn't be a storage tx"
    );

    tokio::join!(genesis.stop(), peer1.stop(), peer2.stop());

    Ok(())
}
