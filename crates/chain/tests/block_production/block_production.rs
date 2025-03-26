use std::{collections::HashMap, time::Duration};

use alloy_consensus::TxEnvelope;
use alloy_core::primitives::{ruint::aliases::U256, Bytes, TxKind, B256};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer_local::LocalSigner;
use eyre::eyre;
use irys_actors::{block_producer::SolutionFoundMessage, mempool_service::TxIngressMessage};
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::{node::RethNodeContext, transaction::TransactionTestContext};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, Config, IrysTransaction};
use k256::ecdsa::SigningKey;
use reth::{providers::BlockReader, rpc::types::TransactionRequest};
use reth_db::Database;
use reth_primitives::{
    irys_primitives::{IrysTxId, ShadowResult},
    GenesisAccount,
};
use tokio::time::sleep;
use tracing::info;

use crate::utils::capacity_chunk_solution;
/// Create a valid capacity PoA solution

#[tokio::test]
async fn heavy_test_blockprod() -> eyre::Result<()> {
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
                balance: U256::from(1),
                ..Default::default()
            },
        ),
        (
            account2.address(),
            GenesisAccount {
                balance: U256::from(2),
                ..Default::default()
            },
        ),
        (
            account3.address(),
            GenesisAccount {
                balance: U256::from(1000),
                ..Default::default()
            },
        ),
    ]);

    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let mut txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    for a in [&account1, &account2, &account3] {
        let data_bytes = "Hello, world!".as_bytes().to_vec();
        let tx = a.create_transaction(data_bytes, None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        // submit to mempool
        let _tx_res = node
            .actor_addresses
            .mempool
            .send(TxIngressMessage(tx.header.clone()))
            .await
            .unwrap();
        txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
        // txs.push(tx);
    }

    let poa_solution = capacity_chunk_solution(
        node.node_config.mining_signer.address(),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await;

    let (block, reth_exec_env) = node
        .actor_addresses
        .block_producer
        .send(SolutionFoundMessage(poa_solution))
        .await??
        .unwrap();

    for receipt in reth_exec_env.shadow_receipts {
        let og_tx = txs.get(&receipt.tx_id).unwrap();
        if og_tx.header.signer == account1.address() {
            assert_eq!(receipt.result, ShadowResult::OutOfFunds)
        } else {
            assert_eq!(receipt.result, ShadowResult::Success)
        }
    }

    let reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block

    let db_irys_block = &node
        .db
        .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash, false))?
        .unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_mine_ten_blocks_with_capacity_poa_solution() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_blockprod"), false);
    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

    for i in 1..10 {
        info!("manually producing block {}", i);
        let poa_solution = capacity_chunk_solution(
            node.node_config.mining_signer.address(),
            node.vdf_steps_guard.clone(),
            &node.vdf_config,
            &node.storage_config,
        )
        .await;
        let fut = node
            .actor_addresses
            .block_producer
            .send(SolutionFoundMessage(poa_solution.clone()));
        let (block, _reth_exec_env) = fut.await??.unwrap();

        //check reth for built block
        let reth_block = reth_context
            .inner
            .provider
            .block_by_hash(block.evm_block_hash)?
            .unwrap();
        assert_eq!(i, reth_block.header.number as u32);
        // height is hardcoded at 42 right now
        // assert_eq!(reth_block.number, block.height);

        // check irys DB for built block
        let db_irys_block = &node
            .db
            .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash, false))?
            .unwrap();
        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        // MAGIC: we wait more than 1s so that the block timestamps (evm block timestamps are seconds) don't overlap
        sleep(Duration::from_millis(1500)).await;
    }
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_mine_ten_blocks() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_blockprod"), false);
    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    node.actor_addresses.start_mining()?;

    let reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

    for i in 1..10 {
        info!("waiting block {}", i);

        let mut retries = 0;
        while node.block_index_guard.read().num_blocks() < i + 1 && retries < 60_u64 {
            sleep(Duration::from_secs(1)).await;
            retries += 1;
        }

        info!("got block after {} seconds/retries", &retries);

        let block = node
            .block_index_guard
            .read()
            .get_item(i as usize)
            .unwrap()
            .clone();

        //check reth for built block
        let reth_block = reth_context.inner.provider.block_by_number(i)?.unwrap();
        assert_eq!(i, reth_block.header.number);
        assert_eq!(i, reth_block.number);

        // check irys DB for built block
        let db_irys_block = &node
            .db
            .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash, false))?
            .unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    }
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_basic_blockprod() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_blockprod"), false);

    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();

    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let poa_solution = capacity_chunk_solution(
        node.node_config.mining_signer.address(),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await;

    let (block, _) = node
        .actor_addresses
        .block_producer
        .send(SolutionFoundMessage(poa_solution))
        .await??
        .unwrap();

    let reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = &node
        .db
        .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash, false))?
        .unwrap();
    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_blockprod_with_evm_txs() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_blockprod"), false);
    let testnet_config = Config {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        num_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        chunk_migration_depth: 1,
        ..Config::testnet()
    };
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);

    let mining_signer_addr = config.mining_signer.address();
    let account1 = IrysSigner::random_signer(&testnet_config);
    let account2 = IrysSigner::random_signer(&testnet_config);
    let account3 = IrysSigner::random_signer(&testnet_config);

    config.extend_genesis_accounts(vec![
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(1),
                ..Default::default()
            },
        ),
        (
            account2.address(),
            GenesisAccount {
                balance: U256::from(420000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account3.address(),
            GenesisAccount {
                balance: U256::from(690000000000000_u128),
                ..Default::default()
            },
        ),
    ]);

    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    let reth_context = RethNodeContext::new(node.reth_handle.clone().into()).await?;
    let miner_init_balance = reth_context
        .rpc
        .get_balance(mining_signer_addr, None)
        .await?;

    let mut irys_txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    let mut evm_txs: HashMap<B256, TxEnvelope> = HashMap::new();
    for (i, a) in [(1, &account1), (2, &account2), (3, &account3)] {
        let es: LocalSigner<SigningKey> = a.clone().into();
        let evm_tx_req = TransactionRequest {
            to: Some(TxKind::Call(mining_signer_addr)),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            gas: Some(21000),
            value: Some(U256::from(1)),
            nonce: Some(0),
            chain_id: Some(testnet_config.chain_id),
            ..Default::default()
        };
        let tx_env = TransactionTestContext::sign_tx(es, evm_tx_req).await;
        let signed_tx: Bytes = tx_env.encoded_2718().into();
        // let signed_tx = TransactionTestContext::transfer_tx_bytes(CONFIG.irys_chain_id, es).await;
        match i {
            // 1 is poor, tx should fail to inject
            1 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect_err("tx should be rejected");
            }
            // 2 is less poor but should still fail
            2 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect_err("tx should be rejected");
            }
            // should succeed
            3 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect("tx should be accepted");
            }
            _ => return Err(eyre!("unknown account index")),
        }
        evm_txs.insert(*tx_env.tx_hash(), tx_env.clone());

        let data_bytes = "Hello, world!".as_bytes().to_vec();
        let tx = a.create_transaction(data_bytes, None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        // submit to mempool
        let _tx_res = node
            .actor_addresses
            .mempool
            .send(TxIngressMessage(tx.header.clone()))
            .await
            .unwrap();
        irys_txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
    }

    let poa_solution = capacity_chunk_solution(
        node.node_config.mining_signer.address(),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await;

    let (block, reth_exec_env) = node
        .actor_addresses
        .block_producer
        .send(SolutionFoundMessage(poa_solution))
        .await??
        .unwrap();

    for receipt in reth_exec_env.shadow_receipts {
        let og_tx = irys_txs.get(&receipt.tx_id).unwrap();
        if og_tx.header.signer == account1.address() {
            assert_eq!(receipt.result, ShadowResult::OutOfFunds)
        } else {
            assert_eq!(receipt.result, ShadowResult::Success)
        }
    }

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    assert!(evm_txs.contains_key(&reth_block.body.transactions.first().unwrap().hash()));

    assert_eq!(
        reth_context
            .rpc
            .get_balance(mining_signer_addr, None)
            .await?,
        miner_init_balance + U256::from(1)
    );
    // check irys DB for built block
    let db_irys_block = &node
        .db
        .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash, false))?
        .unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    node.stop().await;
    Ok(())
}
