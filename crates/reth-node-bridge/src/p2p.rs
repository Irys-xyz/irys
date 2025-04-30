use irys_types::{Address, NodeConfig};
use reth::rpc::eth::EthApiServer as _;
// use crate::utils::eth_payload_attributes;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{setup, transaction::TransactionTestContext};
use reth_node_ethereum::EthereumNode;
use revm_primitives::B256;
use std::{
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::warn;

use crate::{
    node::{run_node3, run_node4},
    run_node,
};

mod t {
    use irys_types::Address;
    use reth::{payload::EthPayloadBuilderAttributes, rpc::types::engine::PayloadAttributes};
    use revm_primitives::B256;

    pub fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: None, /* Some(B256::ZERO) */
            shadows: None,
        };
        EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
    }
}

#[tokio::test]
async fn can_sync() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = setup::<EthereumNode>(
        2,
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                .cancun_activated()
                .build(),
        ),
        false,
    )
    .await?;

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let second_node = nodes.pop().unwrap();
    let mut first_node = nodes.pop().unwrap();

    // Make the first node advance
    // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

    // make the node advance
    let (payload, _) = first_node
        .advance_block(vec![], t::eth_payload_attributes)
        .await?;

    let block_hash = payload.block().hash();
    let block_number = payload.block().number;

    // assert the block has been committed to the blockchain
    // first_node.assert_new_block(tx_hash, block_hash, block_number).await?;
    first_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    // first_node.wait_block(block_number, block_hash, false).await?;

    // only send forkchoice update to second node
    second_node
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip
    // second_node.wait_block(block_number, block_hash, false).await?;
    second_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    Ok(())
}

#[tokio::test]
async fn can_sync2() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = setup::<EthereumNode>(
        2,
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                .cancun_activated()
                .build(),
        ),
        false,
    )
    .await?;

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let second_node = nodes.pop().unwrap();
    let mut first_node = nodes.pop().unwrap();

    // Make the first node advance
    // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

    // make the node advance
    // let (payload, _) = first_node
    //     .advance_block(vec![], t::eth_payload_attributes)
    //     .await?;

    // let block_hash = payload.block().hash();
    // let block_number = payload.block().number;

    let (block_hash, block_number) = {
        let p1_latest = first_node
            .rpc
            .inner
            .eth_api()
            .block_by_number(alloy_eips::BlockNumberOrTag::Latest, false)
            .await
            .unwrap()
            .unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let payload_attrs = reth::rpc::types::engine::PayloadAttributes {
            timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            shadows: None,
        };

        let (exec_payload, built, attrs) = first_node
            .new_payload_irys2(p1_latest.header.hash, payload_attrs)
            .await?;

        let block_hash = first_node
            .engine_api
            .submit_payload(
                built.clone(),
                attrs.clone(),
                alloy_rpc_types::engine::PayloadStatusEnum::Valid,
                vec![],
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        first_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        (
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_hash,
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_number,
        )

        // let payload = first_node
        //     .engine_api
        //     .build_payload_v1_irys(/* gen_latest.header.hash */ B256::ZERO, payload_attrs)
        //     .await?;

        // // (payload.block().hash(), payload.block().number)

        // let block_hash = first_node
        //     .engine_api
        //     .submit_payload(
        //         payload.clone(),
        //         payload_attrs.clone(),
        //         PayloadStatusEnum::Valid,
        //         versioned_hashes,
        //     )
        //     .await?;

        // (
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_hash,
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_number,
        // )

        // let (payload, _) = first_node
        //     .advance_block(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)

        // let (payload, _) = first_node
        //     .advance_block_irys(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)
    };

    // assert the block has been committed to the blockchain
    // first_node.assert_new_block(tx_hash, block_hash, block_number).await?;
    first_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    // first_node.wait_block(block_number, block_hash, false).await?;

    // only send forkchoice update to second node
    second_node
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip
    // second_node.wait_block(block_number, block_hash, false).await?;
    second_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    Ok(())
}

#[tokio::test]
async fn can_sync3() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    reth_tracing::init_test_tracing();
    let num_nodes = 2;

    let base_config = irys_types::NodeConfig::testnet();
    let tasks = reth_tasks::TaskManager::current();
    let exec = tasks.executor();
    let mut nodes = {
        let mut nodes: Vec<reth_e2e_test_utils::node::NodeTestContext<_, _>> =
            Vec::with_capacity(num_nodes);
        for idx in 0..num_nodes {
            // let tmp_dir = temporary_directory(None, true);
            let tmp_dir = reth_db::test_utils::tempdir_path();
            let reth_node_builder::NodeHandle {
                node,
                node_exit_future: _,
            } = run_node(
                Arc::new(
                    ChainSpecBuilder::default()
                        .chain(MAINNET.chain)
                        .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                        .cancun_activated()
                        .build(),
                ),
                exec.clone(),
                NodeConfig {
                    base_directory: tmp_dir,
                    ..base_config.clone()
                },
                Arc::new(RwLock::new(None)),
                0,
                true,
            )
            .await?;
            let node = reth_e2e_test_utils::node::NodeTestContext::new(node).await?;

            // // Connect each node in a chain.
            // if let Some(previous_node) = nodes.last_mut() {
            //     previous_node.connect(&mut node).await;
            // }

            // // Connect last node with the first if there are more than two
            // if idx + 1 == num_nodes && num_nodes > 2 {
            //     if let Some(first_node) = nodes.first_mut() {
            //         node.connect(first_node).await;
            //     }
            // }

            nodes.push(node)
        }
        nodes
    };

    warn!("JESSEDEBUG2 got nodes");

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let mut second_node = nodes.pop().unwrap();
    let mut first_node = nodes.pop().unwrap();

    first_node.connect(&mut second_node).await;

    // Make the first node advance
    // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

    // make the node advance
    // let (payload, _) = first_node
    //     .advance_block(vec![], t::eth_payload_attributes)
    //     .await?;

    // let block_hash = payload.block().hash();
    // let block_number = payload.block().number;

    let (block_hash, block_number) = {
        let p1_latest = first_node
            .rpc
            .inner
            .eth_api()
            .block_by_number(alloy_eips::BlockNumberOrTag::Latest, false)
            .await
            .unwrap()
            .unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let payload_attrs = reth::rpc::types::engine::PayloadAttributes {
            timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            shadows: None,
        };

        let (exec_payload, built, attrs) = first_node
            .new_payload_irys2(p1_latest.header.hash, payload_attrs)
            .await?;

        let block_hash = first_node
            .engine_api
            .submit_payload(
                built.clone(),
                attrs.clone(),
                alloy_rpc_types::engine::PayloadStatusEnum::Valid,
                vec![],
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        first_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        (
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_hash,
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_number,
        )

        // let payload = first_node
        //     .engine_api
        //     .build_payload_v1_irys(/* gen_latest.header.hash */ B256::ZERO, payload_attrs)
        //     .await?;

        // // (payload.block().hash(), payload.block().number)

        // let block_hash = first_node
        //     .engine_api
        //     .submit_payload(
        //         payload.clone(),
        //         payload_attrs.clone(),
        //         PayloadStatusEnum::Valid,
        //         versioned_hashes,
        //     )
        //     .await?;

        // (
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_hash,
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_number,
        // )

        // let (payload, _) = first_node
        //     .advance_block(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)

        // let (payload, _) = first_node
        //     .advance_block_irys(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)
    };

    // assert the block has been committed to the blockchain
    // first_node.assert_new_block(tx_hash, block_hash, block_number).await?;
    first_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    // first_node.wait_block(block_number, block_hash, false).await?;

    // only send forkchoice update to second node
    second_node
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip
    // second_node.wait_block(block_number, block_hash, false).await?;
    second_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    Ok(())
}

#[tokio::test]
async fn can_sync4() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    reth_tracing::init_test_tracing();
    let num_nodes = 2;

    let base_config = irys_types::NodeConfig::testnet();
    let tasks = reth_tasks::TaskManager::current();
    let exec = tasks.executor();
    let mut nodes = {
        let mut nodes: Vec<reth_e2e_test_utils::node::NodeTestContext<_, _>> =
            Vec::with_capacity(num_nodes);
        for idx in 0..num_nodes {
            // let tmp_dir = temporary_directory(None, true);
            let tmp_dir = reth_db::test_utils::tempdir_path();
            let reth_node_builder::NodeHandle {
                node,
                node_exit_future: _,
            } = run_node3(
                Arc::new(
                    ChainSpecBuilder::default()
                        .chain(MAINNET.chain)
                        .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                        .cancun_activated()
                        .build(),
                ),
                exec.clone(),
                NodeConfig {
                    base_directory: tmp_dir,
                    ..base_config.clone()
                },
                Arc::new(RwLock::new(None)),
                0,
                true,
            )
            .await?;
            let node = reth_e2e_test_utils::node::NodeTestContext::new(node).await?;

            // // Connect each node in a chain.
            // if let Some(previous_node) = nodes.last_mut() {
            //     previous_node.connect(&mut node).await;
            // }

            // // Connect last node with the first if there are more than two
            // if idx + 1 == num_nodes && num_nodes > 2 {
            //     if let Some(first_node) = nodes.first_mut() {
            //         node.connect(first_node).await;
            //     }
            // }

            nodes.push(node)
        }
        nodes
    };

    warn!("JESSEDEBUG2 got nodes");

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let mut second_node = nodes.pop().unwrap();
    let mut first_node = nodes.pop().unwrap();

    first_node.connect(&mut second_node).await;

    // Make the first node advance
    // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

    // make the node advance
    // let (payload, _) = first_node
    //     .advance_block(vec![], t::eth_payload_attributes)
    //     .await?;

    // let block_hash = payload.block().hash();
    // let block_number = payload.block().number;

    let (block_hash, block_number) = {
        let p1_latest = first_node
            .rpc
            .inner
            .eth_api()
            .block_by_number(alloy_eips::BlockNumberOrTag::Latest, false)
            .await
            .unwrap()
            .unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let payload_attrs = reth::rpc::types::engine::PayloadAttributes {
            timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            shadows: None,
        };

        let (exec_payload, built, attrs) = first_node
            .new_payload_irys2(p1_latest.header.hash, payload_attrs)
            .await?;

        let block_hash = first_node
            .engine_api
            .submit_payload(
                built.clone(),
                attrs.clone(),
                alloy_rpc_types::engine::PayloadStatusEnum::Valid,
                vec![],
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        first_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        (
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_hash,
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_number,
        )

        // let payload = first_node
        //     .engine_api
        //     .build_payload_v1_irys(/* gen_latest.header.hash */ B256::ZERO, payload_attrs)
        //     .await?;

        // // (payload.block().hash(), payload.block().number)

        // let block_hash = first_node
        //     .engine_api
        //     .submit_payload(
        //         payload.clone(),
        //         payload_attrs.clone(),
        //         PayloadStatusEnum::Valid,
        //         versioned_hashes,
        //     )
        //     .await?;

        // (
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_hash,
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_number,
        // )

        // let (payload, _) = first_node
        //     .advance_block(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)

        // let (payload, _) = first_node
        //     .advance_block_irys(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)
    };

    // assert the block has been committed to the blockchain
    // first_node.assert_new_block(tx_hash, block_hash, block_number).await?;
    first_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    // first_node.wait_block(block_number, block_hash, false).await?;

    // only send forkchoice update to second node
    second_node
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip
    // second_node.wait_block(block_number, block_hash, false).await?;
    second_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    Ok(())
}

#[tokio::test]
async fn can_sync5() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");
    reth_tracing::init_test_tracing();
    let num_nodes = 2;

    let base_config = irys_types::NodeConfig::testnet();
    let tasks = reth_tasks::TaskManager::current();
    let exec = tasks.executor();
    let mut nodes = {
        let mut nodes: Vec<reth_e2e_test_utils::node::NodeTestContext<_, _>> =
            Vec::with_capacity(num_nodes);
        for idx in 0..num_nodes {
            // let tmp_dir = temporary_directory(None, true);
            let tmp_dir = reth_db::test_utils::tempdir_path();
            let reth_node_builder::NodeHandle {
                node,
                node_exit_future: _,
            } = run_node4(
                Arc::new(
                    ChainSpecBuilder::default()
                        .chain(MAINNET.chain)
                        .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                        .cancun_activated()
                        .build(),
                ),
                exec.clone(),
                NodeConfig {
                    base_directory: tmp_dir,
                    ..base_config.clone()
                },
                Arc::new(RwLock::new(None)),
                0,
                true,
            )
            .await?;
            let node = reth_e2e_test_utils::node::NodeTestContext::new(node).await?;

            // // Connect each node in a chain.
            // if let Some(previous_node) = nodes.last_mut() {
            //     previous_node.connect(&mut node).await;
            // }

            // // Connect last node with the first if there are more than two
            // if idx + 1 == num_nodes && num_nodes > 2 {
            //     if let Some(first_node) = nodes.first_mut() {
            //         node.connect(first_node).await;
            //     }
            // }

            nodes.push(node)
        }
        nodes
    };

    warn!("JESSEDEBUG2 got nodes");

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let mut second_node = nodes.pop().unwrap();
    let mut first_node = nodes.pop().unwrap();

    first_node.connect(&mut second_node).await;

    // Make the first node advance
    // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

    // make the node advance
    // let (payload, _) = first_node
    //     .advance_block(vec![], t::eth_payload_attributes)
    //     .await?;

    // let block_hash = payload.block().hash();
    // let block_number = payload.block().number;

    let (block_hash, block_number) = {
        let p1_latest = first_node
            .rpc
            .inner
            .eth_api()
            .block_by_number(alloy_eips::BlockNumberOrTag::Latest, false)
            .await
            .unwrap()
            .unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let payload_attrs = reth::rpc::types::engine::PayloadAttributes {
            timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            shadows: None,
        };

        let (exec_payload, built, attrs) = first_node
            .new_payload_irys2(p1_latest.header.hash, payload_attrs)
            .await?;

        let block_hash = first_node
            .engine_api
            .submit_payload(
                built.clone(),
                attrs.clone(),
                alloy_rpc_types::engine::PayloadStatusEnum::Valid,
                vec![],
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        first_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        (
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_hash,
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_number,
        )

        // let payload = first_node
        //     .engine_api
        //     .build_payload_v1_irys(/* gen_latest.header.hash */ B256::ZERO, payload_attrs)
        //     .await?;

        // // (payload.block().hash(), payload.block().number)

        // let block_hash = first_node
        //     .engine_api
        //     .submit_payload(
        //         payload.clone(),
        //         payload_attrs.clone(),
        //         PayloadStatusEnum::Valid,
        //         versioned_hashes,
        //     )
        //     .await?;

        // (
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_hash,
        //     payload
        //         .execution_payload
        //         .payload_inner
        //         .payload_inner
        //         .payload_inner
        //         .block_number,
        // )

        // let (payload, _) = first_node
        //     .advance_block(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)

        // let (payload, _) = first_node
        //     .advance_block_irys(vec![], t::eth_payload_attributes)
        //     .await?;

        // (payload.block().hash(), payload.block().number)
    };

    // assert the block has been committed to the blockchain
    // first_node.assert_new_block(tx_hash, block_hash, block_number).await?;
    first_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    // first_node.wait_block(block_number, block_hash, false).await?;

    // only send forkchoice update to second node
    second_node
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip
    // second_node.wait_block(block_number, block_hash, false).await?;
    second_node
        .assert_new_block2(block_hash, block_number)
        .await?;

    Ok(())
}
