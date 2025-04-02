use crate::utils::mine_block;
use irys_actors::BlockFinalizedMessage;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::{Address, Config, IrysTransactionHeader, Signature, H256};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[actix_web::test]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let testnet_config_genesis = Config {
        port: 8080,
        ..Config::testnet()
    };
    let ctx_genesis_node =
        setup_with_config(testnet_config_genesis, "heavy_sync_chain_state_genesis")
            .await
            .expect("found invalid genesis ctx");

    // start mining
    // advance one block
    let (_header, _payload) = mine_block(&ctx_genesis_node.node).await?.unwrap();
    // advance one block, finalizing the previous block
    let (header, _payload) = mine_block(&ctx_genesis_node.node).await?.unwrap();
    let mock_header = IrysTransactionHeader {
        id: H256::from([255u8; 32]),
        anchor: H256::from([1u8; 32]),
        signer: Address::default(),
        data_root: H256::from([3u8; 32]),
        data_size: 1024,
        term_fee: 100,
        perm_fee: Some(200),
        ledger_id: 1,
        bundle_format: None,
        chain_id: ctx_genesis_node.config.chain_id,
        version: 0,
        ingress_proofs: None,
        signature: Signature::test_signature().into(),
    };
    let _block_finalized_message = BlockFinalizedMessage {
        block_header: header,
        all_txs: Arc::new(vec![mock_header]),
    };

    //start two additional peers, instructing them to use the genesis peer as their trusted peer

    //start peer1
    let testnet_config_peer1 = Config {
        port: 8081,
        ..Config::testnet()
    };
    let ctx_peer1_node = setup_with_config(testnet_config_peer1, "heavy_sync_chain_state_peer1")
        .await
        .expect("found invalid genesis ctx for peer1");

    //start peer2
    let testnet_config_peer2 = Config {
        port: 8082,
        ..Config::testnet()
    };
    let ctx_peer2_node = setup_with_config(testnet_config_peer2, "heavy_sync_chain_state_peer2")
        .await
        .expect("found invalid genesis ctx for peer2");

    //FIXME: magic number could be a constant e.g. 3 blocks worth of time?
    sleep(Duration::from_millis(10000)).await;

    //run asserts. http requests to peer1 and peer2 index after x seconds to ensure they have begun syncing the blocks

    Ok(())
}

struct TestCtx {
    config: Config,
    node: IrysNodeCtx,
    #[expect(
        dead_code,
        reason = "to prevent drop() being called and cleaning up resources"
    )]
    temp_dir: TempDir,
}

async fn setup_with_config(testnet_config: Config, node_name: &str) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some(node_name), false);
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    Ok(TestCtx {
        config: testnet_config,
        node,
        temp_dir,
    })
}
