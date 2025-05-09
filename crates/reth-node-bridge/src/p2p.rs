#[cfg(test)]
mod tests {

    use irys_types::{Address, NodeConfig};
    use reth::rpc::eth::EthApiServer as _;
    use reth_chainspec::{ChainSpecBuilder, MAINNET};
    use reth_e2e_test_utils::setup;
    use reth_node_ethereum::EthereumNode;
    use revm_primitives::B256;
    use std::{
        sync::{Arc, RwLock},
        time::{SystemTime, UNIX_EPOCH},
    };

    use reth::{payload::EthPayloadBuilderAttributes, rpc::types::engine::PayloadAttributes};

    use crate::node::run_node;

    const TEST_GENESIS: &str =
        include_str!("../../../ext/reth/crates/ethereum/node/tests/assets/genesis.json");

    pub fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: None, // NEVER SET THIS TO Some(B256::ZERO), IT SHOULD ALWAYS BE NONE
            shadows: None,
        };
        EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
    }

    #[tokio::test]
    async fn can_sync() -> eyre::Result<()> {
        std::env::set_var("RUST_LOG", "debug");
        reth_tracing::init_test_tracing();

        let (mut nodes, _tasks, _wallet) = setup::<EthereumNode>(
            2,
            Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(serde_json::from_str(TEST_GENESIS).unwrap())
                    .cancun_activated()
                    .build(),
            ),
            false,
        )
        .await?;

        // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
        let second_node = nodes.pop().unwrap();
        let mut first_node = nodes.pop().unwrap();

        // let tx_hash = first_node.rpc.inject_tx(raw_tx).await?;

        // make the node advance
        let (payload, _) = first_node
            .advance_block(vec![], eth_payload_attributes)
            .await?;

        let block_hash = payload.block().hash();
        let block_number = payload.block().number;

        // assert the block has been committed to the blockchain
        first_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        second_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        // expect second node advanced via p2p gossip
        second_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn can_sync_diff_rpc() -> eyre::Result<()> {
        std::env::set_var("RUST_LOG", "debug");
        reth_tracing::init_test_tracing();

        let (mut nodes, _tasks, _wallet) = setup::<EthereumNode>(
            2,
            Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(serde_json::from_str(TEST_GENESIS).unwrap())
                    .cancun_activated()
                    .build(),
            ),
            false,
        )
        .await?;

        let second_node = nodes.pop().unwrap();
        let mut first_node = nodes.pop().unwrap();

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
            //     .advance_block(vec![], eth_payload_attributes)
            //     .await?;

            // (payload.block().hash(), payload.block().number)

            // let (payload, _) = first_node
            //     .advance_block_irys(vec![], eth_payload_attributes)
            //     .await?;

            // (payload.block().hash(), payload.block().number)
        };

        // assert the block has been committed to the blockchain
        first_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        second_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        // expect second node advanced via p2p gossip
        second_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn can_sync_run_node() -> eyre::Result<()> {
        std::env::set_var("RUST_LOG", "debug");
        reth_tracing::init_test_tracing();
        let num_nodes = 2;

        let base_config = irys_types::NodeConfig::testnet();
        let tasks = reth_tasks::TaskManager::current();
        let exec = tasks.executor();
        let mut nodes = {
            let mut nodes: Vec<reth_e2e_test_utils::node::NodeTestContext<_, _>> =
                Vec::with_capacity(num_nodes);
            for _ in 0..num_nodes {
                // let tmp_dir = temporary_directory(None, true);
                let tmp_dir = reth_db::test_utils::tempdir_path();
                let reth_node_builder::NodeHandle {
                    node,
                    node_exit_future: _,
                } = run_node(
                    Arc::new(
                        ChainSpecBuilder::default()
                            .chain(MAINNET.chain)
                            .genesis(serde_json::from_str(TEST_GENESIS).unwrap())
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

                nodes.push(node)
            }
            nodes
        };

        // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
        let mut second_node = nodes.pop().unwrap();
        let mut first_node = nodes.pop().unwrap();

        first_node.connect(&mut second_node).await;

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
        };

        // assert the block has been committed to the blockchain
        first_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        second_node
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        // expect second node advanced via p2p gossip
        second_node
            .assert_new_block2(block_hash, block_number)
            .await?;

        Ok(())
    }
}
