use alloy_core::primitives::{ruint::aliases::U256, Bytes, TxKind};
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_genesis::GenesisAccount;
use alloy_network::EthereumWallet;
use alloy_provider::Provider as _;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::LocalSigner;
use alloy_signer_local::PrivateKeySigner;
use irys_reth_node_bridge::reth_e2e_test_utils::transaction::TransactionTestContext;
use k256::ecdsa::SigningKey;
use rand::Rng as _;
use reth::rpc::types::TransactionRequest;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

use irys_types::NodeConfig;
use irys_types::TxChunkOffset;
use irys_types::UnpackedChunk;
use irys_types::{irys::IrysSigner, serialization::*, DataTransaction, SimpleRNG};

use crate::utils::IrysNodeTest;

// network simulation test for analytics
#[ignore]
#[tokio::test]
async fn test_blockprod_with_evm_txs() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 1000;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.storage.num_writes_before_sync = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().block_migration_depth = 1; // Testnet / single node config;
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let account2 = IrysSigner::random_signer(&config.consensus_config());
    let account3 = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![
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
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // server should be running
    // check with request to `/v1/info`
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10_000))
        .build()
        .expect("failed to build reqwest client");

    let _response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    let anchor = node.get_anchor().await?;

    let generate_tx_with_size = |a: &IrysSigner,
                                 data_size: usize,
                                 perm_fee: irys_types::U256,
                                 term_fee: irys_types::U256|
     -> eyre::Result<(DataTransaction, Vec<u8>)> {
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let tx = a.create_publish_transaction(
            data_bytes.clone(),
            anchor,
            perm_fee.into(),
            term_fee.into(),
        )?;
        let tx = a.sign_transaction(tx)?;
        Ok((tx, data_bytes))
    };

    let upload_header = |tx: &DataTransaction| {
        client
            .post(format!("{}/v1/tx", http_url))
            .json(&tx.header)
            .send()
    };

    // Generate initial transactions with correct pricing
    let mut pending_txs = vec![];
    for account in [&account1, &account2, &account3] {
        let data_size = rand::thread_rng().gen_range(1..=100);
        let price_info = node
            .get_data_price(irys_types::DataLedger::Publish, data_size as u64)
            .await
            .expect("Failed to get price");
        let tx_with_data =
            generate_tx_with_size(account, data_size, price_info.perm_fee, price_info.term_fee)?;
        pending_txs.push((tx_with_data, 0));
    }
    upload_header(&pending_txs[0].0 .0).await.unwrap();
    upload_header(&pending_txs[1].0 .0).await.unwrap();
    upload_header(&pending_txs[2].0 .0).await.unwrap();

    let accounts = [account1, account2, account3];

    let alloy_providers = accounts
        .iter()
        .map(|a| {
            let signer: PrivateKeySigner = a.signer.clone().into();
            ProviderBuilder::new()
                .wallet(EthereumWallet::from(signer))
                .connect_http(
                    format!(
                        "http://127.0.0.1:{}/v1/execution-rpc",
                        node.node_ctx.config.node_config.http.bind_port
                    )
                    .parse()
                    .unwrap(),
                )
        })
        .collect::<Vec<_>>();

    for i in 0..20 {
        let mut simple_rng = SimpleRNG::new(i);

        for (i, a) in accounts.iter().enumerate() {
            let es: LocalSigner<SigningKey> = a.clone().into();
            let to_index: usize =
                <u32 as TryInto<usize>>::try_into(simple_rng.next_range(3)).unwrap();

            let alloy_provider = alloy_providers.get(to_index).unwrap();

            let evm_tx_req = TransactionRequest {
                to: Some(TxKind::Call(
                    accounts.get(to_index).unwrap().alloy_address(), /* config.mining_signer.address() */
                )),
                max_fee_per_gas: Some(20e9 as u128),
                max_priority_fee_per_gas: Some(20e9 as u128),
                gas: Some(21000),
                value: Some(U256::from(simple_rng.next_range(20_000))),
                nonce: Some(
                    alloy_provider
                        .get_transaction_count(a.alloy_address())
                        .await?,
                ),
                chain_id: Some(node.node_ctx.config.consensus.chain_id),
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
                let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());

                let chunk = UnpackedChunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    tx_offset: TxChunkOffset::from(
                        TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
                    ),
                };

                // Make a POST request with JSON payload
                let resp = client
                    .post(format!("{}/v1/chunk", http_url))
                    .json(&chunk)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), reqwest::StatusCode::OK);
            }

            // create a new tx, upload *some* of it's chunks
            let new_data_size = rand::thread_rng().gen_range(1..=100);
            let new_price_info = node
                .get_data_price(irys_types::DataLedger::Publish, new_data_size as u64)
                .await
                .expect("Failed to get price");
            (*tx, *data_bytes) = generate_tx_with_size(
                a,
                new_data_size,
                new_price_info.perm_fee,
                new_price_info.term_fee,
            )
            .expect("Failed to generate tx");

            *num_chunks_uploaded = simple_rng
                .next_range((tx.chunks.len() + 1).try_into().unwrap())
                .try_into()
                .unwrap();

            upload_header(tx).await.unwrap();

            for (tx_chunk_offset, chunk_node) in
                tx.chunks.iter().take(*num_chunks_uploaded).enumerate()
            {
                let data_root = tx.header.data_root;
                let data_size = tx.header.data_size;
                let min = chunk_node.min_byte_range;
                let max = chunk_node.max_byte_range;
                let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());

                let chunk = UnpackedChunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    tx_offset: (tx_chunk_offset as u32).into(),
                };

                let resp = client
                    .post(format!("{}/v1/chunk", http_url))
                    .json(&chunk)
                    .send()
                    .await
                    .unwrap();

                assert_eq!(resp.status(), reqwest::StatusCode::OK);
            }
        }

        node.mine_block().await?;
        info!("Finished step {}", &i);
    }

    sleep(Duration::from_secs(u64::MAX)).await;

    Ok(())
}
