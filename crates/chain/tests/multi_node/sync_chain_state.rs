use crate::utils::mine_block;
use irys_actors::BlockFinalizedMessage;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::{Address, Config, IrysTransactionHeader, Signature, H256};
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

struct TestingConfigs {
    genesis: String,
    peer1: String,
    peer2: String,
}

fn write_config(config: &Config, path: &str) -> std::io::Result<()> {
    let toml_str = toml::to_string(config).expect("Failed to serialise config");
    let mut file = File::create(path)?;
    file.write_all(toml_str.as_bytes())?;
    Ok(())
}

#[actix_web::test]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let config_paths = TestingConfigs {
        genesis: String::from(".tmp/config-genesis.toml"),
        peer1: String::from(".tmp/config-peer1.toml"),
        peer2: String::from(".tmp/config-peer2.toml"),
    };

    let mut test_config = Config::testnet();
    test_config.port = 8080;

    match write_config(&test_config, &config_paths.genesis) {
        Ok(_) => {
            info!("{} config written", config_paths.genesis)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.genesis)
        }
    }

    test_config.port = 8081;
    match write_config(&test_config, &config_paths.peer1) {
        Ok(_) => {
            info!("{} config written", config_paths.peer1)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.peer1)
        }
    }

    test_config.port = 8082;
    match write_config(&test_config, &config_paths.peer2) {
        Ok(_) => {
            info!("{} config written", config_paths.peer2)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.peer2)
        }
    }

    // start genesis
    let ctx = setup().await?;

    // start mining
    // advance one block
    let (_header, _payload) = mine_block(&ctx.node).await?.unwrap();
    // advance one block, finalizing the previous block
    let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
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
        chain_id: ctx.config.chain_id,
        version: 0,
        ingress_proofs: None,
        signature: Signature::test_signature().into(),
    };
    let _block_finalized_message = BlockFinalizedMessage {
        block_header: header,
        all_txs: Arc::new(vec![mock_header]),
    };

    //FIXME: magic number could be a constant e.g. 3 blocks worth of time?
    sleep(Duration::from_millis(10000)).await;

    //start two additional peers, instructing them to use the genesis peer as their trusted peer

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

async fn setup() -> eyre::Result<TestCtx> {
    let testnet_config = Config {
        // add any overrides here
        ..Config::testnet()
    };
    setup_with_config(testnet_config).await
}

async fn setup_with_config(testnet_config: Config) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some("external_api"), false);
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
