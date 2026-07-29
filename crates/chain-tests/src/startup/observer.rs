use crate::utils::IrysNodeTest;
use irys_types::{NodeConfig, NodeMode};
use std::sync::Arc;

/// An `Observer` node must boot into a working state and follow the chain
/// without any mining apparatus: no storage modules, no partition mining
/// controllers, and no submodules config file on disk.
#[test_log::test(tokio::test)]
async fn heavy_test_observer_boots_and_follows_without_mining() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    let mut genesis_config = NodeConfig::testing();
    let observer_account = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&observer_account]);

    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let mut observer_config = genesis.testing_peer_with_signer(&observer_account);
    observer_config.node_mode = NodeMode::Observer;
    // Already the default, but the mode's precondition is worth stating.
    observer_config.stake_pledge_drives = false;

    let observer = IrysNodeTest::new(observer_config)
        .start_with_name("OBSERVER")
        .await;

    // No storage modules, no partition mining controllers.
    let sm_count = observer.node_ctx.storage_modules_guard.read().len();
    let ctrl_count = observer.node_ctx.partition_controllers.len();
    assert_eq!(sm_count, 0, "observer should have no storage modules");
    assert_eq!(
        ctrl_count, 0,
        "observer should have no partition mining controllers"
    );

    // Genesis mines; the observer must follow.
    genesis.mine_blocks(3).await?;
    observer.wait_until_height(3, seconds_to_wait).await?;

    let observer_height = observer.get_canonical_chain_height().await;
    assert!(observer_height >= 3);

    let submodule_file = observer
        .node_ctx
        .config
        .node_config
        .base_directory
        .join(".irys_submodules.toml");
    assert!(
        !submodule_file.exists(),
        "observer must not write a submodules config"
    );

    observer.stop().await;
    genesis.stop().await;
    Ok(())
}

/// An `Observer` converted from a `Miner` is the opposite structural case to the
/// fresh Observer above: its `.irys_submodules.toml` already exists, and an
/// existing file is always honored, so the mode gate on default submodule
/// creation never fires. Storage modules are built and one partition mining
/// controller is spawned per module. What keeps such a node from mining is not
/// the mode gate but the construction default — controllers start with mining
/// off, and only the `main.rs` miner path ever turns it on. This test pins that
/// default, so removing it cannot pass silently.
#[test_log::test(tokio::test)]
async fn heavy_test_converted_observer_has_controllers_but_does_not_mine() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    let num_blocks_in_epoch = 3;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // Small chunks keep the packing this test needs fast.
    genesis_config.consensus.get_mut().chunk_size = 32;
    let node_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&node_signer]);

    let genesis = IrysNodeTest::new_genesis(genesis_config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Phase 1: run the node as a Miner that pledges its drives, so the
    // conversion starts from a node with real on-chain partition assignments.
    let mut miner_config = genesis.testing_peer_with_signer(&node_signer);
    miner_config.stake_pledge_drives = true;

    // Mine first so the auto-generated stake/pledge txs anchor to a real block.
    let anchor_block = genesis.mine_block().await?;
    let miner = IrysNodeTest::new(miner_config)
        .start_with_name("MINER")
        .await;

    // Push the tip so the miner reaches the chain head and auto-stake proceeds.
    genesis.gossip_block_to_peers(&Arc::new(anchor_block.clone()))?;
    miner
        .wait_for_block_at_height(anchor_block.height, seconds_to_wait)
        .await?;

    // One stake plus one pledge per default submodule.
    genesis
        .wait_for_mempool_best_txs_shape(0, 0, 4, seconds_to_wait as u32)
        .await?;

    // Assignments are handed out at the epoch boundary.
    let (_, epoch_height) = genesis.mine_until_next_epoch().await?;
    miner
        .wait_for_block_at_height(epoch_height, seconds_to_wait)
        .await?;
    let assignments = miner.get_partition_assignments(node_signer.address());
    assert_eq!(
        assignments.len(),
        3,
        "the miner must hold on-chain partition assignments before conversion, \
         otherwise the converted node has nothing to build storage modules from"
    );
    miner.wait_for_packing(seconds_to_wait).await;

    let submodules_path = miner
        .node_ctx
        .config
        .node_config
        .base_directory
        .join(".irys_submodules.toml");
    let submodules_before = std::fs::read_to_string(&submodules_path)?;

    // Phase 2: the operator flips the mode and restarts the same directory.
    let mut stopped = miner.stop().await;
    stopped.cfg.node_mode = NodeMode::Observer;
    // `Observer` with `stake_pledge_drives = true` is rejected by
    // `Config::validate`, so conversion must clear it.
    stopped.cfg.stake_pledge_drives = false;
    let observer = stopped.start_with_name("OBSERVER").await;

    // The structural difference from a fresh Observer: the mining apparatus is
    // present. If either count were 0 the mining-off assertion below would be
    // vacuous.
    let sm_count = observer.node_ctx.storage_modules_guard.read().len();
    let controllers = observer.node_ctx.partition_controllers.clone();
    assert!(
        sm_count > 0,
        "a converted observer must keep the storage modules its existing \
         .irys_submodules.toml describes; with 0 this test is vacuous"
    );
    assert!(
        !controllers.is_empty(),
        "a converted observer must keep one partition mining controller per \
         storage module; with 0 this test is vacuous"
    );

    // The observer still follows the chain.
    genesis.mine_blocks(3).await?;
    let genesis_height = genesis.get_canonical_chain_height().await;
    observer
        .wait_until_height(genesis_height, seconds_to_wait)
        .await?;

    // The assertion this test exists for. Checked after the observer has
    // followed live blocks, so it also covers the mining broadcasts (seed,
    // difficulty) those blocks push at every controller.
    for (idx, controller) in controllers.iter().enumerate() {
        assert_eq!(
            controller.is_mining().await,
            Some(false),
            "converted observer: partition mining controller {idx} reports mining enabled"
        );
    }

    // The pre-existing submodules config was honored, not rewritten.
    assert_eq!(
        std::fs::read_to_string(&submodules_path)?,
        submodules_before,
        "a converted observer must leave the existing .irys_submodules.toml untouched"
    );

    observer.stop().await;
    genesis.stop().await;
    Ok(())
}
