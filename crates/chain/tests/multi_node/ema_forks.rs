use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, reth_ethereum_primitives, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::{
    storage_pricing::Amount, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader,
    NodeConfig, OracleConfig,
};
use reth::payload::EthBuiltPayload;
use rust_decimal_macros::dec;

#[test_log::test(actix_web::test)]
async fn heavy_ema_states_valid_across_forks() -> eyre::Result<()> {
    // setup
    let num_blocks_in_epoch = 10;
    let seconds_to_wait = 20;
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = 3;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let node_1 = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    node_1.start_public_api().await;
    let mut peer_config = node_1.testnet_peer_with_signer(&peer_signer);
    peer_config.oracle = OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.01)).unwrap(),
        percent_change: Amount::percentage(dec!(0.005)).unwrap(),
        smoothing_interval: 3,
    };

    let node_2 = node_1
        .testnet_peer_with_assignments_and_name(peer_config, "PEER")
        .await;

    node_1.mine_blocks_without_gossip(7).await?;
    node_2.mine_blocks_without_gossip(9).await?;

    // todo assert that all the blocks are unique
    let chain_node_1 = node_1
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;
    let chain_node_2 = node_2
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;

    // compare all unique blocks
    let blocks_to_compare = chain_node_1.iter().zip(chain_node_2.iter());

    let mut blocks_validated = 0;
    for (block_1, block_2) in blocks_to_compare {
        if block_1.block_hash == block_2.block_hash {
            continue;
        }
        blocks_validated += 1;
        assert_eq!(block_1.height, block_2.height, "heights must be the same");
        assert_ne!(
            block_1.block_hash, block_2.block_hash,
            "block hashes must differ"
        );
        let ema_1 = node_1
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_1.block_hash)
            .unwrap();
        let ema_2 = node_2
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_2.block_hash)
            .unwrap();
        let block_raw_1 = node_1.get_block_by_hash(&block_1.block_hash).unwrap();
        let block_raw_2 = node_2.get_block_by_hash(&block_2.block_hash).unwrap();
        assert_ne!(
            block_raw_1.block_hash, block_raw_2.block_hash,
            "block hashes must differ"
        );
        dbg!();
        dbg!();
        dbg!(
            block_1.height,
            // block_raw_1.oracle_irys_price,
            // block_raw_2.oracle_irys_price,
            ema_1.ema_price_current_interval,
            ema_2.ema_price_current_interval
        );
        // assert_ne!(ema_1, ema_2, "ema snapshot values must differ");
    }
    assert_eq!(
        blocks_validated, 7,
        "expect to have compared the len of the shortest fork"
    );
    panic!();

    node_2.stop().await;
    node_1.stop().await;

    Ok(())
}
