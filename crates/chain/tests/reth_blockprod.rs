use std::sync::Arc;

use alloy_core::primitives::{B256, U256};
use irys_reth_node_bridge::{rpc::AccountStateExtApiClient, IrysChainSpecBuilder};
use irys_types::Address;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::rpc_params;
use reth::{
    payload::EthPayloadBuilderAttributes,
    providers::BlockReaderIdExt as _,
    rpc::types::{engine::PayloadAttributes, Block},
    tasks::TaskManager,
};
use reth_primitives::{
    irys_primitives::{DiffShadow, IrysTxId, NewAccountState, ShadowTx, ShadowTxType, Shadows},
    BlockNumberOrTag, Genesis, GenesisAccount,
};
use tempfile::TempDir;
use tokio::runtime::Handle;

use reth_e2e_test_utils::node::NodeTestContext;

#[tokio::test]
pub async fn block_building_test() -> eyre::Result<()> {
    // TODO: eventually we'll want to produce a nice testing API like ext/reth/crates/e2e-test-utils/src/lib.rs
    // but for now due to runtime weirdness we just yoink the parts of the reth API we need out
    let temp_dir = TempDir::with_prefix("reth-genesis-test").unwrap();
    let temp_dir_path = temp_dir.path().to_path_buf();
    // dbg!(format!("temp dir path: {:?}", &temp_dir_path));
    // remove_dir_all("../../.reth")?;
    let builder = IrysChainSpecBuilder::mainnet();
    let chainspec_builder = builder.reth_builder;

    // TODO @JesseTheRobot - make sure logging is initialized before we get here as this uses logging macros
    // use the existing reth code to handle blocking & graceful shutdown

    let addr1 = Address::random();
    let balance1 = U256::from(1000000000);
    let alloc = vec![(
        addr1,
        GenesisAccount {
            balance: balance1,
            ..Default::default()
        },
    )];
    let genesis = Genesis::default().extend_accounts(alloc);
    let chain_spec = chainspec_builder.genesis(genesis).build();

    let handle = Handle::current();
    let task_manager = TaskManager::new(handle);
    let task_executor = task_manager.executor();

    let node_handle = irys_reth_node_bridge::run_node(
        Arc::new(chain_spec.clone()),
        task_executor,
        temp_dir_path.clone(),
    )
    .await?;

    let mut test_context = NodeTestContext::new(node_handle.node.clone()).await?;

    // create block info
    let http_rpc_client = node_handle.node.rpc_server_handle().http_client().unwrap();

    // TODO @JesseTheRobot figure out how to pass EngineTypes so we can use the direct calls
    // look at the test context for guidance
    // let genesis_blk = http_rpc_client
    //     .block_by_number::<Option<Block>, _>(reth_primitives::BlockNumberOrTag::Number(0), false)
    //     .await?;

    // see ext/reth/crates/rpc/rpc-builder/tests/it/http.rs:43
    let genesis_blk = http_rpc_client
        .request::<Option<Block>, _>("eth_getBlockByNumber", rpc_params!["0x0", false])
        .await?
        .unwrap();

    dbg!(&genesis_blk);
    assert!(genesis_blk.header.hash == chain_spec.genesis_hash());

    // let test_wallet = Wallet::default().with_chain_id(chain_spec.chain.id());

    let (payload, attrs) = test_context
        .new_payload(|ts| {
            // let mut pa = eth_payload_attributes(ts);

            let attributes = PayloadAttributes {
                timestamp: ts,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
                shadows: Some(Shadows::new(vec![ShadowTx {
                    tx_id: IrysTxId::random(),
                    address: addr1, /* test_wallet.inner.address(), */
                    // tx: ShadowTxType::Transfer(TransferShadow {
                    //     to: Address::random(),
                    //     amount: U256::from(99999999),
                    // }),
                    tx: ShadowTxType::Diff(DiffShadow {
                        new_state: NewAccountState {
                            balance: Some(U256::from(123456789)),
                            ..Default::default()
                        },
                    }),
                    fee: U256::from(1000),
                }])),
            };

            return EthPayloadBuilderAttributes::new(B256::ZERO, attributes.clone());
        })
        .await?;

    dbg!(&payload, &attrs);
    let block_hash = payload.block().hash();

    let block_number = payload.block().number;

    // assert the block has been committed to the blockchain
    // test_context
    //     .assert_new_block(B256::ZERO, block_hash, block_number)
    //     .await?;

    test_context
        .engine_api
        .update_forkchoice_payload_attr(block_hash, block_hash, None)
        .await?;
    loop {
        // wait for the block to commit
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(latest_block) = test_context
            .inner
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)?
        {
            if latest_block.number == block_number {
                // make sure the block hash we submitted via FCU engine api is the new latest
                // block using an RPC call
                assert_eq!(latest_block.hash_slow(), block_hash);
                break;
            }
        }
    }
    // original: let res2 = http_rpc_client.get_account(addr1, None).await?;
    // disambiguated:
    let res2 = AccountStateExtApiClient::get_account(&http_rpc_client, addr1, None).await?;
    assert!(res2.is_some_and(|a| a.balance == U256::from(123456789)));

    Ok(())
}

#[tokio::test]
pub async fn block_building_test2() -> eyre::Result<()> {
    // TODO: eventually we'll want to produce a nice testing API like ext/reth/crates/e2e-test-utils/src/lib.rs
    // but for now due to runtime weirdness we just yoink the parts of the reth API we need out
    let temp_dir = TempDir::with_prefix("reth-genesis-test").unwrap();
    let temp_dir_path = temp_dir.path().to_path_buf();
    // dbg!(format!("temp dir path: {:?}", &temp_dir_path));
    // remove_dir_all("../../.reth")?;
    let builder = IrysChainSpecBuilder::mainnet();
    let chainspec_builder = builder.reth_builder;

    // TODO @JesseTheRobot - make sure logging is initialized before we get here as this uses logging macros
    // use the existing reth code to handle blocking & graceful shutdown

    let addr1 = Address::random();
    let balance1 = U256::from(1000000000);
    let alloc = vec![(
        addr1,
        GenesisAccount {
            balance: balance1,
            ..Default::default()
        },
    )];
    let genesis = Genesis::default().extend_accounts(alloc);
    let chain_spec = chainspec_builder.genesis(genesis).build();

    let handle = Handle::current();
    let task_manager = TaskManager::new(handle);
    let task_executor = task_manager.executor();

    let node_handle = irys_reth_node_bridge::run_node(
        Arc::new(chain_spec.clone()),
        task_executor,
        temp_dir_path.clone(),
    )
    .await?;

    let mut test_context = NodeTestContext::new(node_handle.node.clone()).await?;

    // create block info
    let http_rpc_client = node_handle.node.rpc_server_handle().http_client().unwrap();

    // TODO @JesseTheRobot figure out how to pass EngineTypes so we can use the direct calls
    // look at the test context for guidance
    // let genesis_blk = http_rpc_client
    //     .block_by_number::<Option<Block>, _>(reth_primitives::BlockNumberOrTag::Number(0), false)
    //     .await?;

    // see ext/reth/crates/rpc/rpc-builder/tests/it/http.rs:43
    let genesis_blk = http_rpc_client
        .request::<Option<Block>, _>("eth_getBlockByNumber", rpc_params!["0x0", false])
        .await?
        .unwrap();

    dbg!(&genesis_blk);
    assert!(genesis_blk.header.hash == chain_spec.genesis_hash());

    // let test_wallet = Wallet::default().with_chain_id(chain_spec.chain.id());

    // let (payload, attrs) = test_context
    //     .new_payload(|ts| {
    //         // let mut pa = eth_payload_attributes(ts);

    //         let attributes = PayloadAttributes {
    //             timestamp: ts,
    //             prev_randao: B256::ZERO,
    //             suggested_fee_recipient: Address::ZERO,
    //             withdrawals: Some(vec![]),
    //             parent_beacon_block_root: Some(B256::ZERO),
    //             shadows: Some(Shadows::new(vec![ShadowTx {
    //                 tx_id: IrysTxId::random(),
    //                 address: addr1, /* test_wallet.inner.address(), */
    //                 // tx: ShadowTxType::Transfer(TransferShadow {
    //                 //     to: Address::random(),
    //                 //     amount: U256::from(99999999),
    //                 // }),
    //                 tx: ShadowTxType::Diff(DiffShadow {
    //                     new_state: NewAccountState {
    //                         balance: Some(U256::from(123456789)),
    //                         ..Default::default()
    //                     },
    //                 }),
    //                 fee: U256::from(1000),
    //             }])),
    //         };

    //         return EthPayloadBuilderAttributes::new(B256::ZERO, attributes.clone());
    //     })
    //     .await?;

    let attributes_generator = |ts| {
        // let mut pa = eth_payload_attributes(ts);

        let attributes = PayloadAttributes {
            timestamp: ts,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            shadows: Some(Shadows::new(vec![ShadowTx {
                tx_id: IrysTxId::random(),
                address: addr1, /* test_wallet.inner.address(), */
                // tx: ShadowTxType::Transfer(TransferShadow {
                //     to: Address::random(),
                //     amount: U256::from(99999999),
                // }),
                tx: ShadowTxType::Diff(DiffShadow {
                    new_state: NewAccountState {
                        balance: Some(U256::from(123456789)),
                        ..Default::default()
                    },
                }),
                fee: U256::from(1000),
            }])),
        };

        return EthPayloadBuilderAttributes::new(B256::ZERO, attributes.clone());
    };
    let eth_attr = test_context
        .payload
        .new_payload(attributes_generator)
        .await
        .unwrap();
    // first event is the payload attributes
    test_context
        .payload
        .expect_attr_event(eth_attr.clone())
        .await?;
    // wait for the payload builder to have finished building
    test_context
        .payload
        .wait_for_built_payload(eth_attr.payload_id())
        .await;
    // trigger resolve payload via engine api
    let payload = test_context
        .engine_api
        .get_payload_v1_irys(eth_attr.payload_id())
        .await?;

    let s = serde_json::to_string(&payload)?;
    dbg!(&payload, &s);

    let full_irys_header = 

    // let block_hash = payload.block().hash();

    // let block_number = payload.block().number;

    // assert the block has been committed to the blockchain
    // test_context
    //     .assert_new_block(B256::ZERO, block_hash, block_number)
    //     .await?;
    // let res = test_context
    //     .engine_api
    //     .get_payload_v1_irys_value(payload.id())
    //     .await?;

    // dbg!(&res);

    // test_context
    //     .engine_api
    //     .update_forkchoice_payload_attr(block_hash, block_hash, None)
    //     .await?;
    // loop {
    //     // wait for the block to commit
    //     tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    //     if let Some(latest_block) = test_context
    //         .inner
    //         .provider
    //         .block_by_number_or_tag(BlockNumberOrTag::Latest)?
    //     {
    //         if latest_block.number == block_number {
    //             // make sure the block hash we submitted via FCU engine api is the new latest
    //             // block using an RPC call
    //             assert_eq!(latest_block.hash_slow(), block_hash);
    //             break;
    //         }
    //     }
    // }

    // http_rpc_client.get_payload_v1_irys(payload_id)
    // original: let res2 = http_rpc_client.get_account(addr1, None).await?;
    // disambiguated:
    let res2 = AccountStateExtApiClient::get_account(&http_rpc_client, addr1, None).await?;
    assert!(res2.is_some_and(|a| a.balance == U256::from(123456789)));

    Ok(())
}
