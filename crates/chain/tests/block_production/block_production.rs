use alloy_consensus::TxEnvelope;
use alloy_core::primitives::{ruint::aliases::U256, Bytes, TxKind, B256};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer_local::LocalSigner;
use eyre::{eyre, OptionExt};
use irys_actors::mempool_service::TxIngressError;
use irys_reth_node_bridge::adapter::{node::RethNodeContext, transaction::TransactionTestContext};
use irys_types::{irys::IrysSigner, IrysTransaction, NodeConfig};
use k256::ecdsa::SigningKey;
use reth::{providers::BlockReader, rpc::types::TransactionRequest};
use reth_primitives::{
    irys_primitives::{IrysTxId, ShadowResult, ShadowTxType},
    GenesisAccount,
};
use std::{collections::HashMap, time::Duration};
use tokio::time::sleep;
use tracing::info;

use crate::utils::{mine_block, AddTxError, IrysNodeTest};

#[tokio::test]
async fn heavy_test_blockprod() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let account1 = IrysSigner::random_signer(&node.cfg.consensus_config());
    let account2 = IrysSigner::random_signer(&node.cfg.consensus_config());
    let account3 = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![
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
    let irys_node = node.start().await;

    let mut txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    for a in [&account1, &account2, &account3] {
        let data_bytes = "Hello, world!".as_bytes().to_vec();
        match irys_node.create_submit_data_tx(&a, data_bytes).await {
            Ok(tx) => {
                txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
            }
            Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
                assert_eq!(a.address(), account1.address(), "account1 should fail");
            }
            Err(e) => panic!("unexpected error {:?}", e),
        }
    }

    let (block, reth_exec_env) = mine_block(&irys_node.node_ctx).await?.unwrap();

    for receipt in reth_exec_env.shadow_receipts {
        match receipt.tx_type {
            ShadowTxType::BlockReward(_block_reward_shadow) => {
                assert_eq!(receipt.result, ShadowResult::Success);
            }
            ShadowTxType::Data(_data_shadow) => {
                let og_tx = txs.get(&receipt.tx_id).unwrap();
                assert_eq!(receipt.result, ShadowResult::Success);
                assert_ne!(og_tx.header.signer, account1.address()); // account1 has no funds
            }
            _ => {
                panic!("test does not expect this shadow type")
            }
        }
    }

    let reth_context = RethNodeContext::new(irys_node.node_ctx.reth_handle.clone().into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = irys_node.get_block_by_hash(&block.block_hash).unwrap();
    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    irys_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_mine_ten_blocks_with_capacity_poa_solution() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;
    let reth_context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;

    for i in 1..10 {
        info!("manually producing block {}", i);
        let (block, _reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();

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
        let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();
        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        // MAGIC: we wait more than 1s so that the block timestamps (evm block timestamps are seconds) don't overlap
        sleep(Duration::from_millis(1500)).await;
    }
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_mine_ten_blocks() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;

    node.node_ctx.actor_addresses.start_mining()?;
    let reth_context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;

    for i in 1..10 {
        node.wait_until_height(i + 1, 60).await?;

        //check reth for built block
        let reth_block = reth_context.inner.provider.block_by_number(i)?.unwrap();
        assert_eq!(i, reth_block.header.number);
        assert_eq!(i, reth_block.number);

        let db_irys_block = node.get_block_by_height(i as u64).await.unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    }
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_basic_blockprod() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;

    let (block, _) = mine_block(&node.node_ctx).await?.unwrap();

    let reth_context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    // assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();
    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_test_blockprod_with_evm_txs() -> eyre::Result<()> {
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.storage.num_writes_before_sync = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().chunk_migration_depth = 1;

    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let account2 = IrysSigner::random_signer(&config.consensus_config());
    let account3 = IrysSigner::random_signer(&config.consensus_config());
    let chain_id = config.consensus_config().chain_id;
    let mining_signer_addr = config.miner_address();
    let reward_address = config.reward_address;
    let recipient = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![
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
    let node = IrysNodeTest::new_genesis(config).await.start().await;
    let reth_context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;
    let miner_init_balance = reth_context
        .rpc
        .get_balance(mining_signer_addr, None)
        .await?;
    let reward_init_balance = reth_context.rpc.get_balance(reward_address, None).await?;
    let recipient_init_balance = reth_context
        .rpc
        .get_balance(recipient.address(), None)
        .await?;

    let mut irys_txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    let mut evm_txs: HashMap<B256, TxEnvelope> = HashMap::new();
    for (i, a) in [(1, &account1), (2, &account2), (3, &account3)] {
        let es: LocalSigner<SigningKey> = a.clone().into();
        let evm_tx_req = TransactionRequest {
            to: Some(TxKind::Call(recipient.address())),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            gas: Some(21000),
            value: Some(U256::from(1)),
            nonce: Some(0),
            chain_id: Some(chain_id),
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
        match node.create_submit_data_tx(&a, data_bytes).await {
            Ok(tx) => {
                irys_txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
            }
            Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
                assert_eq!(
                    a.address(),
                    account1.address(),
                    "account1 should be unfunded"
                );
            }
            Err(e) => panic!("unexpected error {:?}", e),
        }
    }

    let (block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();

    let mut block_reward = U256::from(0);
    for receipt in reth_exec_env.shadow_receipts {
        match receipt.tx_type {
            ShadowTxType::BlockReward(block_reward_shadow) => {
                assert_eq!(receipt.result, ShadowResult::Success);
                block_reward = block_reward_shadow.reward;
            }
            ShadowTxType::Data(_data_shadow) => {
                let og_tx = irys_txs.get(&receipt.tx_id).unwrap();
                assert_eq!(receipt.result, ShadowResult::Success);
                assert_ne!(og_tx.header.signer, account1.address()); // account1 has no funds
            }
            _ => {
                panic!("test does not expect this shadow type")
            }
        }
    }
    assert_ne!(block_reward, U256::from(0), "block reward cannot be 0");

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    assert!(evm_txs.contains_key(&reth_block.body.transactions.first().unwrap().hash()));
    assert_eq!(
        reth_context
            .rpc
            .get_balance(recipient.address(), None)
            .await?,
        recipient_init_balance + U256::from(1)
    );
    // check irys DB for built block
    let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();

    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_rewards_get_calculated_correctly() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;
    let reth_context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;

    let mut prev_ts: Option<u128> = None;
    let reward_address = node.node_ctx.config.node_config.reward_address;
    let mut init_balance = reth_context.rpc.get_balance(reward_address, None).await?;
    for _ in 0..3 {
        // mine a single block
        let (block, reth_exec_env) = mine_block(&node.node_ctx)
            .await?
            .ok_or_eyre("block was not mined")?;

        // obtain the EVM timestamp for this block from Reth
        let reth_block = reth_context
            .inner
            .provider
            .block_by_hash(block.evm_block_hash)?
            .unwrap();
        let new_ts = reth_block.header.timestamp as u128;

        // on every block *after* genesis, validate the reward shadow
        if let Some(old_ts) = prev_ts {
            // expected reward according to the protocol’s reward curve
            let expected_reward = node
                .node_ctx
                .reward_curve
                .reward_between(old_ts, new_ts)
                .unwrap();

            // find the BlockReward shadow receipt and check correctness
            let mut reward_shadow_found = false;
            for receipt in reth_exec_env.shadow_receipts {
                if let ShadowTxType::BlockReward(br_shadow) = receipt.tx_type {
                    let expected_new_balance = init_balance + br_shadow.reward;
                    let new_balance = reth_context.rpc.get_balance(reward_address, None).await?;
                    assert_eq!(new_balance, expected_new_balance);
                    assert_eq!(
                        receipt.result,
                        ShadowResult::Success,
                        "block-reward shadow must succeed"
                    );
                    assert_eq!(
                        br_shadow.reward,
                        expected_reward.amount.into(),
                        "incorrect block-reward amount recorded in shadow"
                    );
                    reward_shadow_found = true;
                    break;
                }
            }
            assert!(
                reward_shadow_found,
                "BlockReward shadow transaction not found in receipts"
            );
        }

        // update baseline timestamp and ensure the next block gets a later one
        prev_ts = Some(new_ts);
        init_balance = reth_context.rpc.get_balance(reward_address, None).await?;
        sleep(Duration::from_millis(1_500)).await;
    }

    assert!(prev_ts.is_some());
    node.stop().await;
    Ok(())
}
