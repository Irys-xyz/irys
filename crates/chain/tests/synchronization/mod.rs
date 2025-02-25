use std::time::Duration;

use alloy_core::primitives::{ruint::aliases::U256};
use alloy_provider::Provider;
use rand::Rng;

use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::{node::RethNodeContext};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    irys::IrysSigner, serialization::*, IrysTransaction, StorageConfig,
};

use reth_primitives::GenesisAccount;

// network simulation test for analytics
#[ignore]
#[actix_web::test]
async fn should_resume_from_the_same_block() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");

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

    let node = start_irys_node(
        config.clone(),
        StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 1000,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: config.mining_signer.address(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1_000,
            chunk_migration_depth: 1, // Testnet / single node config
        },
    )
        .await?;
    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    let http_url = "http://127.0.0.1:8080";

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::builder()
        .timeout(Duration::from_secs(10_000))
        .finish();

    let mut response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    let heh = response.body().await.unwrap();

    let generate_tx = |a: &IrysSigner| -> (IrysTransaction, Vec<u8>) {
        let data_size = rand::thread_rng().gen_range(1..=100);
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let tx = a.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        (tx, data_bytes)
    };

    let upload_transaction_header = |tx: &IrysTransaction| {
        client
            .post(format!("{}/v1/tx", http_url))
            .send_json(&tx.header)
    };

    // /tx/{tx_id}

    let mut pending_txs = [
        (generate_tx(&account1), 0),
        (generate_tx(&account2), 0),
        (generate_tx(&account3), 0),
    ];

    upload_transaction_header(&pending_txs[0].0 .0).await.unwrap();
    upload_transaction_header(&pending_txs[1].0 .0).await.unwrap();
    upload_transaction_header(&pending_txs[2].0 .0).await.unwrap();

    let accounts = [account1, account2, account3];

    Ok(())
}
