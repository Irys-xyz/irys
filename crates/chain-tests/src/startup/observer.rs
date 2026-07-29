use crate::utils::IrysNodeTest;
use irys_types::{NodeConfig, NodeMode};

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
