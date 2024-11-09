use std::{collections::HashMap, fs::remove_dir_all, time::Duration};

use actors::mempool::TxIngressMessage;
use alloy_core::primitives::U256;
use alloy_signer_local::PrivateKeySigner;
use chain::chain::start_for_testing;
use irys_config::{chain::chain::IRYS_MAINNET, IrysNodeConfig};
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_types::{block_production::SolutionContext, irys::IrysSigner, Address, IrysTransaction};
use reth::providers::BlockReader;
use reth_primitives::{
    irys_primitives::{IrysTxId, ShadowResult},
    GenesisAccount,
};
use tokio::time::sleep;
use tracing::info;

#[tokio::test]
async fn test_blockprod() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    remove_dir_all(&config.base_directory)?;
    let account1 = IrysSigner::random_signer();
    let account2 = IrysSigner::random_signer();
    let account3 = IrysSigner::random_signer();

    // TODO @JesseTheRobot this API sucks, fix it
    let m_genesis = IRYS_MAINNET.genesis.clone().extend_accounts(vec![
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

    config.chainspec_builder.reth_builder =
        config.chainspec_builder.reth_builder.genesis(m_genesis);

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
            partition_id: 0,
            chunk_index: 0,
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
    let db_irys_block = database::block_by_hash(&node.db, block.block_hash)?.unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    Ok(())
}

#[tokio::test]
async fn mine_ten_blocks() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    remove_dir_all(&config.base_directory)?;

    let node = start_for_testing(config).await?;

    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

    for i in 1..10 {
        info!("mining block {}", i);

        let (block, reth_exec_env) = node
            .actor_addresses
            .block_producer
            .send(SolutionContext {
                partition_id: 0,
                chunk_index: 0,
                mining_address: node.config.mining_signer.address(),
            })
            .await?
            .unwrap();

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
        let db_irys_block = database::block_by_hash(&node.db, block.block_hash)?.unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

        sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[tokio::test]
async fn test_basic_blockprod() -> eyre::Result<()> {
    let config = IrysNodeConfig::default();
    remove_dir_all(&config.base_directory)?;

    let node = start_for_testing(config).await?;

    let (block, _) = node
        .actor_addresses
        .block_producer
        .send(SolutionContext {
            partition_id: 0,
            chunk_index: 0,
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
    let db_irys_block = database::block_by_hash(&node.db, block.block_hash)?.unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    Ok(())
}
