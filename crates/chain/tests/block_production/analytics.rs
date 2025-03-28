use std::str::from_utf8;
use std::time::Duration;

use actix_http::StatusCode;
use alloy_core::primitives::{ruint::aliases::U256, Bytes, TxKind};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::EthereumWallet;
use alloy_provider::Provider;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::LocalSigner;
use alloy_signer_local::PrivateKeySigner;
use irys_types::Config;
use irys_types::TxChunkOffset;
use irys_types::UnpackedChunk;
use rand::Rng;

use crate::utils::mine_block;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::{node::RethNodeContext, transaction::TransactionTestContext};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, serialization::*, IrysTransaction, SimpleRNG, StorageConfig};
use k256::ecdsa::SigningKey;
use reth::rpc::types::TransactionRequest;
use reth_primitives::GenesisAccount;
use tokio::time::sleep;
use tracing::info;

// network simulation test for analytics
#[ignore]
#[actix_web::test]
async fn test_blockprod_with_evm_txs() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");

    let temp_dir = setup_tracing_and_temp_dir(Some("test_blockprod"), false);
    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();

    let account1 = IrysSigner::random_signer(&testnet_config);
    let account2 = IrysSigner::random_signer(&testnet_config);
    let account3 = IrysSigner::random_signer(&testnet_config);
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
            chain_id: testnet_config.chain_id,
        },
        testnet_config.clone(),
    )
    .await?;
    let _reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

    let http_url = format!("http://127.0.0.1:{}", node.config.port);

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::builder()
        .timeout(Duration::from_secs(10_000))
        .finish();

    let _response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    let generate_tx = |a: &IrysSigner| -> (IrysTransaction, Vec<u8>) {
        let data_size = rand::thread_rng().gen_range(1..=100);
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let tx = a.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        (tx, data_bytes)
    };

    let upload_header = |tx: &IrysTransaction| {
        client
            .post(format!("{}/v1/tx", http_url))
            .send_json(&tx.header)
    };

    let mut pending_txs = [
        (generate_tx(&account1), 0),
        (generate_tx(&account2), 0),
        (generate_tx(&account3), 0),
    ];
    upload_header(&pending_txs[0].0 .0).await.unwrap();
    upload_header(&pending_txs[1].0 .0).await.unwrap();
    upload_header(&pending_txs[2].0 .0).await.unwrap();

    let accounts = [account1, account2, account3];

    let alloy_providers = accounts
        .iter()
        .map(|a| {
            let signer: PrivateKeySigner = a.signer.clone().into();
            ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(EthereumWallet::from(signer))
                .on_http(
                    format!("http://127.0.0.1:{}/v1/execution-rpc", node.config.port)
                        .parse()
                        .unwrap(),
                )
        })
        .collect::<Vec<_>>();

    for i in 0..20 {
        // let mut irys_txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
        // let mut evm_txs: HashMap<B256, TxEnvelope> = HashMap::new();

        let mut simple_rng = SimpleRNG::new(i);

        for (i, a) in accounts.iter().enumerate() {
            let es: LocalSigner<SigningKey> = a.clone().into();
            let to_index: usize =
                <u32 as TryInto<usize>>::try_into(simple_rng.next_range(3)).unwrap();

            let alloy_provider = alloy_providers.get(to_index).unwrap();

            let evm_tx_req = TransactionRequest {
                to: Some(TxKind::Call(
                    accounts.get(to_index).unwrap().address(), /* config.mining_signer.address() */
                )),
                max_fee_per_gas: Some(20e9 as u128),
                max_priority_fee_per_gas: Some(20e9 as u128),
                gas: Some(21000),
                value: Some(U256::from(simple_rng.next_range(20_000))),
                nonce: Some(alloy_provider.get_transaction_count(a.address()).await?),
                chain_id: Some(testnet_config.chain_id),
                ..Default::default()
            };

            let tx_env = TransactionTestContext::sign_tx(es, evm_tx_req).await;
            let signed_tx: Bytes = tx_env.encoded_2718().into();
            let _ = alloy_provider
                .send_raw_transaction(&signed_tx)
                .await
                .unwrap();

            // reth_context
            //     .rpc
            //     .inject_tx(signed_tx)
            //     .await
            //     .expect("tx should be accepted");
            // evm_txs.insert(*tx_env.tx_hash(), tx_env.clone());

            let ((ref mut tx, ref mut data_bytes), ref mut num_chunks_uploaded) =
                pending_txs.get_mut(i).unwrap();
            // let chunks_left = tx.chunks.len() - *num_chunks_uploaded;

            // upload the remaining chunks
            for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
                if tx_chunk_offset < *num_chunks_uploaded && tx_chunk_offset != *num_chunks_uploaded
                {
                    continue;
                }

                let data_root = tx.header.data_root;
                let data_size = tx.header.data_size;
                let min = chunk_node.min_byte_range;
                let max = chunk_node.max_byte_range;
                let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

                let chunk = UnpackedChunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    tx_offset: TxChunkOffset::from(tx_chunk_offset as u32),
                };

                // Make a POST request with JSON payload

                let mut resp = client
                    .post(format!("{}/v1/chunk", http_url))
                    .send_json(&chunk)
                    .await
                    .unwrap();
                let body = resp.body().await?;
                let body_str = from_utf8(&body)?;
                dbg!(body_str);
                assert_eq!(resp.status(), StatusCode::OK);
            }

            // create a new tx, upload *some* of it's chunks
            (*tx, *data_bytes) = generate_tx(&a);

            *num_chunks_uploaded = simple_rng
                .next_range((tx.chunks.len() + 1).try_into().unwrap())
                .try_into()
                .unwrap();

            upload_header(&tx).await.unwrap();

            for (tx_chunk_offset, chunk_node) in
                tx.chunks.iter().take(*num_chunks_uploaded).enumerate()
            {
                let data_root = tx.header.data_root;
                let data_size = tx.header.data_size;
                let min = chunk_node.min_byte_range;
                let max = chunk_node.max_byte_range;
                let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

                let chunk = UnpackedChunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    tx_offset: (tx_chunk_offset as u32).into(),
                };

                let resp = client
                    .post(format!("{}/v1/chunk", http_url))
                    .send_json(&chunk)
                    .await
                    .unwrap();

                assert_eq!(resp.status(), StatusCode::OK);
            }
        }

        mine_block(&node).await?;
        info!("Finished step {}", &i);
    }

    sleep(Duration::from_secs(u64::MAX)).await;

    Ok(())
}
