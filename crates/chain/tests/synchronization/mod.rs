use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use alloy_core::primitives::ruint::aliases::U256;
use alloy_provider::Provider;
use rand::Rng;

use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, serialization::*, BlockHash, IrysTransaction, StorageConfig};

use crate::utils::mine_block;
use reth_primitives::GenesisAccount;
use tokio::task::JoinHandle;
use tracing::debug;

async fn get_info_bytes(http_url: &str) -> Vec<u8> {
    let client = awc::Client::builder()
        .timeout(Duration::from_secs(10_000))
        .finish();

    let mut response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    response.body().await.unwrap().to_vec()
}

async fn mine_three_blocks(node: &IrysNodeCtx) -> BlockHash {
    let (block_header, _execution_payload) = mine_block(node).await.unwrap().unwrap();
    let first_block_hash = block_header.block_hash;
    let (second_block_header, _execution_payload) = mine_block(node).await.unwrap().unwrap();
    let second_block_hash = second_block_header.block_hash;
    let (third_block_header, _execution_payload) = mine_block(node).await.unwrap().unwrap();
    let third_block_hash = third_block_header.block_hash;

    assert_ne!(first_block_hash, second_block_hash);
    assert_ne!(second_block_hash, third_block_hash);

    third_block_hash
}

// network simulation test for analytics
#[ignore]
#[actix_web::test]
async fn should_resume_from_the_same_block() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");

    let client = awc::Client::builder()
        .timeout(Duration::from_secs(10_000))
        .finish();
    let http_url = "http://127.0.0.1:8080";

    let temp_dir = setup_tracing_and_temp_dir(Some("test_node_resume_block"), false);
    let mut config = IrysNodeConfig::default();
    config.base_directory = temp_dir.path().to_path_buf();

    let account1 = IrysSigner::random_signer_with_chunk_size(32);
    let account2 = IrysSigner::random_signer_with_chunk_size(32);
    let account3 = IrysSigner::random_signer_with_chunk_size(32);
    config.extend_genesis_accounts(vec![
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(69000000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account2.address(),
            GenesisAccount {
                balance: U256::from(4200000000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account3.address(),
            GenesisAccount {
                balance: U256::from(6900000000000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);

    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 1000,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: config.mining_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        chunk_migration_depth: 1, // Testnet / single node config
    };

    let generate_tx = |a: &IrysSigner| -> (IrysTransaction, Vec<u8>) {
        let data_size = rand::thread_rng().gen_range(1..=100);
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let tx = a.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        (tx, data_bytes)
    };

    debug!("Starting Irys Node");

    let node = start_irys_node(config.clone(), storage_config.clone())
        .await
        .unwrap();

    // let db = node.db.clone();

    // Wait a little bit till all services start
    tokio::time::sleep(Duration::from_secs(1)).await;

    // let last_block_hash = mine_three_blocks(&node).await;

    // let weak_count = Arc::weak_count(&db);
    // let strong_count = Arc::strong_count(&db);

    node.stop().await;

    debug!("Stopping the node after 3 blocks");
    // debug!("The last block hash is {:?}", last_block_hash);

    debug!("Checking that the node is stopped");
    let mut response = client.get(format!("{}/v1/info", http_url)).send().await;

    match response {
        Ok(_) => panic!("expected error"),
        Err(e) => {
            assert!(matches!(e, awc::error::SendRequestError::Connect(_)));
        }
    };
    
    // debug!(
    //     "Amount of weak references to the database prior to node stop: {}",
    //     weak_count
    // );
    // debug!(
    //     "Amount of strong references to the database: {}",
    //     strong_count
    // );
    // debug!(
    //     "Amount of weak references to the database: {}",
    //     Arc::weak_count(&db)
    // );
    debug!("Waitin a little");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // debug!(
    //     "Amount of strong references to the database: {}",
    //     Arc::strong_count(&db)
    // );

    // assert!(matches!(strong_count, 1));

    // Checking that the data directory is still there
    let data_directory = config.irys_consensus_data_dir();
    debug!("Data directory: {:?}", data_directory);

    let reth_data_directory = config.reth_data_dir();
    debug!("Reth data directory: {:?}", reth_data_directory);

    let entries = std::fs::read_dir(reth_data_directory)?;
    for entry in entries {
        let entry = entry?;
        debug!("{:?}", entry.path());
    }

    let restarted_node = start_irys_node(config.clone(), storage_config.clone()).await?;

    restarted_node.stop().await;

    Ok(())
}
