use crate::utils::IrysNodeTest;
use irys_testing_utils::initialize_tracing_with_backtrace;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, H256};

#[actix_web::test]
/// demonstrate that duplicate txs are allowed into mempool, to allow for forks, but not returned by handle_get_best_mempool_txs()
/// demonstrate that duplicate txs are blocked from mempool ingress when tx is in database after block migration
async fn heavy_double_spend_rejection_after_block_migration() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug,irys_database=off,irys_p2p::gossip_service=off,irys_actors::storage_module_service=off,trie=off,irys_reth::evm=off,engine::root=off,irys_p2p::peer_list=off,storage::db::mdbx=off,reth_basic_payload_builder=off,irys_gossip_service=off,providers::db=off,reth_payload_builder::service=off,irys_actors::broadcast_mining_service=off,reth_ethereum_payload_builder=off,provider::static_file=off,engine::persistence=off,provider::storage_writer=off,reth_engine_tree::persistence=off,irys_actors::cache_service=off,irys_vdf=off,irys_actors::block_tree_service=off,irys_actors::vdf_service=off,rys_gossip_service::service=off,eth_ethereum_payload_builder=off,reth_node_events::node=off,reth::cli=off,reth_engine_tree::tree=off,irys_actors::ema_service=off,irys_efficient_sampling=off,hyper_util::client::legacy::connect::http=off,hyper_util::client::legacy::pool=off,irys_database::migration::v0_to_v1=off,irys_storage::storage_module=off,actix_server::worker=off,irys::packing::update=off,engine::tree=off,irys_actors::mining=error,payload_builder=off,irys_actors::reth_service=off,irys_actors::packing=off,irys_actors::reth_service=off,irys::packing::progress=off,irys_chain::vdf=off,irys_vdf::vdf_state=off");
    initialize_tracing_with_backtrace();

    // basic node config
    let seconds_to_wait: usize = 10;
    let seconds_to_wait_u32: u32 = seconds_to_wait.try_into()?;
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().chunk_size = 32;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    // start node
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    //
    // TEST CASE 1: create a submit tx for later migration to block index,
    //              testing it cannot be resumbitted back into mempool after migration
    //

    // create and submit a data transaction
    let tx_data = vec![1_u8; 64];
    let anchor = H256::zero();
    let tx_for_migration = node.post_data_tx(anchor, tx_data, &signer).await;
    let txid = tx_for_migration.header.id;
    node.wait_for_mempool(txid, seconds_to_wait).await?;

    // mine block including tx_for_migration
    node.mine_block().await?;
    let block1 = node.get_block_by_height(1).await?;
    assert!(block1
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .unwrap()
        .contains(&txid));

    // mine enough blocks to cause block with tx_for_migration to migrate to index
    node.mine_blocks(
        config
            .consensus
            .get_mut()
            .block_migration_depth
            .try_into()?,
    )
    .await?;

    let block2 = node.get_block_by_height(2).await?;
    // check the shape of the mempool equates to empty
    node.wait_for_mempool_best_txs_shape(0, 0, 0, seconds_to_wait_u32)
        .await?;
    // create commitment tx that will be allowed into mempool, but not included in a block as this node is already staked
    let stake_for_mempool = node.post_stake_commitment(block2.block_hash).await;

    // create commitment tx that will remain in the mempool
    let pledge_for_mempool = node.post_pledge_commitment(block2.block_hash).await;
    node.wait_for_mempool_commitment_txs(
        vec![stake_for_mempool.id, pledge_for_mempool.id],
        seconds_to_wait,
    )
    .await?;

    // check the shape of the mempool now contains one commitment txs
    node.wait_for_mempool_best_txs_shape(0, 0, 1, seconds_to_wait_u32)
        .await?;

    //
    // TEST CASE 2: create a tx for the mempool,
    //              testing it cannot be resubmitted to the mempool after mining
    //

    // create and submit a data transaction
    let tx_data = vec![1_u8; 64];
    let anchor = txid; //chain from prior tx
    let tx_for_mempool = node.post_data_tx(anchor, tx_data, &signer).await;
    let txid = tx_for_mempool.header.id;
    node.wait_for_mempool(txid, seconds_to_wait).await?;

    // mempool should still have a commitment tx
    // submit tx should be present, as it's anchor is a canonical tx
    node.wait_for_mempool_best_txs_shape(1, 0, 1, seconds_to_wait_u32)
        .await?;

    // ensure block with tx_for_migration is now in index (from TEST CASE 1)
    node.wait_until_block_index_height(block1.height, seconds_to_wait)
        .await?;

    // resubmit tx header that already exists in the database (as it has migrated)
    node.post_data_tx_raw(&tx_for_migration.header).await;

    // resubmit tx header that already exists in the mempool
    node.post_data_tx_raw(&tx_for_mempool.header).await;

    // mempool should provide the submit as part of best txs
    node.wait_for_mempool_best_txs_shape(1, 0, 1, seconds_to_wait_u32)
        .await?;

    // resubmit commitment transactions that were already seen
    node.post_commitment_tx(&stake_for_mempool).await?;
    node.post_commitment_tx(&pledge_for_mempool).await?;

    node.wait_for_mempool_best_txs_shape(1, 0, 1, seconds_to_wait_u32)
        .await?;

    // ensure mempool does not accept any duplicate tx
    // mempool will have skipped new stake and the duplicate pledge
    node.wait_for_mempool_best_txs_shape(1, 0, 1, seconds_to_wait_u32)
        .await?;

    // mine another block to migrate block 2 into the index
    node.mine_block().await?;
    // block 2 should now be in the index
    node.wait_until_block_index_height(2, seconds_to_wait)
        .await?;
    // retrieve block 2 once again
    let block2 = node.get_block_by_height(2).await?;
    assert!(!block2
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .unwrap()
        .contains(&txid));

    let final_block = node
        .get_block_by_height(config.consensus.get_mut().block_migration_depth as u64 + 2)
        .await?;
    let commitment_ids = final_block.get_commitment_ledger_tx_ids();
    assert_eq!(commitment_ids, vec![pledge_for_mempool.id]);

    //
    // TEST CASE 3: Post stake txs that were staked in previous epoch and see they are skipped
    //

    // mine enough blocks to trigger epoch
    node.mine_blocks(
        config
            .consensus
            .get_mut()
            .block_migration_depth
            .try_into()?,
    )
    .await?;

    // block 8 should now be in the index and recent enough to use as an anchor
    node.wait_until_block_index_height(8, seconds_to_wait)
        .await?;

    // retrieve block 8 for use as a unique, recent and previously unused anchor
    let block8 = node.get_block_by_height(8).await?;
    node.wait_for_mempool_best_txs_shape(0, 0, 0, seconds_to_wait_u32)
        .await?;

    // re post existing stake commitment, that also uses the same anchor as the previous stake tx
    // this should be rejected by the mempool and not ingress the mempool
    let _duplicate_stake_for_mempool = node.post_stake_commitment(block2.block_hash).await;

    // re post existing stake commitment tx that will be skipped by mempool ingress as this node is already staked
    // use a recent anchor, or we will hit an invalid anchor error due to anchor timeout
    let _new_anchor_stake_for_mempool = node.post_stake_commitment(block8.block_hash).await;
    // ensure mempool does not accept either of the above two txs
    // i.e. mempool should have rejected both stakes as the node has been staked since epoch
    node.wait_for_mempool_best_txs_shape(0, 0, 0, seconds_to_wait_u32)
        .await?;

    // finally, mine block to ensure block is valid
    node.mine_block().await?;

    Ok(())
}
