use std::{collections::HashMap, fs::remove_dir_all, time::Duration};

use alloy_consensus::TxEnvelope;
use alloy_core::primitives::{Bytes, TxKind, B256, U256};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer_local::LocalSigner;
use chain::chain::start_for_testing;
use eyre::eyre;
use irys_actors::mempool::TxIngressMessage;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::{node::RethNodeContext, transaction::TransactionTestContext};
use irys_types::{
    block_production::SolutionContext, irys::IrysSigner, Address, IrysTransaction, H256,
    IRYS_CHAIN_ID,
};
use k256::ecdsa::SigningKey;
use reth::{providers::BlockReader, rpc::types::TransactionRequest};
use reth_primitives::{
    irys_primitives::{IrysTxId, ShadowResult},
    GenesisAccount,
};
use tokio::time::sleep;
use tracing::info;

#[tokio::test]
async fn test_blockprod() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }
    let account1 = IrysSigner::random_signer();
    let account2 = IrysSigner::random_signer();
    let account3 = IrysSigner::random_signer();

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

    let node = start_for_testing(config).await?;

    // let node_signer = PrivateKeySigner::from_signing_key(node.config.mining_signer.signer);

    let mut txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    for a in [&account1, &account2, &account3] {
        let data_bytes = "Hello, world!".as_bytes().to_vec();
        let tx = a.create_transaction(data_bytes, None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        // submit to mempool
        let tx_res = node
            .actor_addresses
            .mempool
            .send(TxIngressMessage(tx.header.clone()))
            .await
            .unwrap();
        txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
        // txs.push(tx);
    }

    let (block, reth_exec_env) = node
        .actor_addresses
        .block_producer
        .send(SolutionContext {
            partition_hash: H256::random(),
            chunk_offset: 0,
            mining_address: node.config.mining_signer.address(),
        })
        .await?
        .unwrap();

    for receipt in reth_exec_env.shadow_receipts {
        let og_tx = txs.get(&receipt.tx_id).unwrap();
        if og_tx.header.signer == account1.address() {
            assert_eq!(receipt.result, ShadowResult::OutOfFunds)
        } else {
            assert_eq!(receipt.result, ShadowResult::Success)
        }
    }

    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = irys_database::block_by_hash(&node.db, block.block_hash)?.unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    Ok(())
}

#[tokio::test]
async fn mine_ten_blocks() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }
    let node = start_for_testing(config).await?;

    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    for i in 1..10 {
        info!("mining block {}", i);
        let fut = node.actor_addresses.block_producer.send(SolutionContext {
            partition_hash: H256::random(),
            chunk_offset: 0,
            mining_address: node.config.mining_signer.address(),
        });
        let (block, reth_exec_env) = fut.await?.unwrap();

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
        let db_irys_block = irys_database::block_by_hash(&node.db, block.block_hash)?.unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        // MAGIC: we wait more than 1s so that the block timestamps (evm block timestamps are seconds) don't overlap
        sleep(Duration::from_millis(1500)).await;
    }
    Ok(())
}

#[tokio::test]
async fn test_basic_blockprod() -> eyre::Result<()> {
    let config = IrysNodeConfig::default();
    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }
    let node = start_for_testing(config).await?;

    let (block, _) = node
        .actor_addresses
        .block_producer
        .send(SolutionContext {
            partition_hash: H256::random(),
            chunk_offset: 0,
            mining_address: Address::random(),
        })
        .await?
        .unwrap();

    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = irys_database::block_by_hash(&node.db, block.block_hash)?.unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    Ok(())
}

#[tokio::test]
async fn test_blockprod_with_evm_txs() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }
    let account1 = IrysSigner::random_signer();
    let account2 = IrysSigner::random_signer();
    let account3 = IrysSigner::random_signer();

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
                balance: U256::from(420000000000000 as u128),
                ..Default::default()
            },
        ),
        (
            account3.address(),
            GenesisAccount {
                balance: U256::from(690000000000000 as u128),
                ..Default::default()
            },
        ),
    ]);

    let node = start_for_testing(config.clone()).await?;
    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    let mut irys_txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    let mut evm_txs: HashMap<B256, TxEnvelope> = HashMap::new();
    for (i, a) in [(1, &account1), (2, &account2), (3, &account3)] {
        let es: LocalSigner<SigningKey> = a.clone().into();
        let evm_tx_req = TransactionRequest {
            to: Some(TxKind::Call(config.mining_signer.address())),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            gas: Some(21000),
            value: Some(U256::from(1)),
            nonce: Some(0),
            chain_id: Some(IRYS_CHAIN_ID),
            ..Default::default()
        };
        let tx_env = TransactionTestContext::sign_tx(es, evm_tx_req).await;
        let signed_tx: Bytes = tx_env.encoded_2718().into();
        // let signed_tx = TransactionTestContext::transfer_tx_bytes(IRYS_CHAIN_ID, es).await;
        match i {
            // 1 is poor, tx should fail to inject
            1 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect_err("tx should be rejected");
                ()
            }
            // 2 is less poor but should still fail
            2 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect_err("tx should be rejected");
                ()
            }
            // should succeed
            3 => {
                reth_context
                    .rpc
                    .inject_tx(signed_tx)
                    .await
                    .expect("tx should be accepted");
                ()
            }
            _ => return Err(eyre!("unknown account index")),
        }
        evm_txs.insert(*tx_env.tx_hash(), tx_env.clone());

        let data_bytes = "Hello, world!".as_bytes().to_vec();
        let tx = a.create_transaction(data_bytes, None).unwrap();
        let tx = a.sign_transaction(tx).unwrap();
        // submit to mempool
        let tx_res = node
            .actor_addresses
            .mempool
            .send(TxIngressMessage(tx.header.clone()))
            .await
            .unwrap();
        irys_txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
        // txs.push(tx);
    }

    let (block, reth_exec_env) = node
        .actor_addresses
        .block_producer
        .send(SolutionContext {
            partition_hash: H256::random(),
            chunk_offset: 0,
            mining_address: node.config.mining_signer.address(),
        })
        .await?
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
    // assert_eq!(reth_block.number, block.height);
    assert!(evm_txs.contains_key(&reth_block.body.transactions.get(0).unwrap().hash()));

    assert_eq!(
        reth_context
            .rpc
            .get_balance(config.mining_signer.address(), None)
            .await?,
        U256::from(1)
    );
    // check irys DB for built block
    let db_irys_block = irys_database::block_by_hash(&node.db, block.block_hash)?.unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    Ok(())
}
