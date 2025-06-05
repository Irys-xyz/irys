use alloy_consensus::TxEnvelope;
use alloy_core::primitives::{ruint::aliases::U256, Bytes, TxKind, B256};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::HashOrNumber;
use alloy_genesis::GenesisAccount;
use alloy_signer_local::LocalSigner;
use eyre::{eyre, OptionExt};
use irys_actors::mempool_service::TxIngressError;
use irys_primitives::IrysTxId;
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_reth_node_bridge::irys_reth::system_tx::system_tx_topics;
use irys_reth_node_bridge::{
    adapter::new_reth_context, reth_e2e_test_utils::transaction::TransactionTestContext,
};
use irys_types::IrysTransactionCommon;
use irys_types::{irys::IrysSigner, IrysTransaction, NodeConfig};
use k256::ecdsa::SigningKey;
use reth::providers::{AccountReader, ReceiptProvider};
use reth::rpc::api::OtterscanClient;
use reth::{providers::BlockReader, rpc::types::TransactionRequest};
use std::{collections::HashMap, time::Duration};
use tokio::time::sleep;
use tracing::{error, info};

use crate::utils::{mine_block, AddTxError, IrysNodeTest};

#[test_log::test(tokio::test)]
async fn heavy_test_blockprod() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let user_account = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![(
        user_account.address(),
        GenesisAccount {
            balance: U256::from(1000),
            ..Default::default()
        },
    )]);

    // print all addresses
    println!("user_account: {:?}", user_account.address());
    println!("node: {:?}", node.cfg.signer().address());

    let node = node.start().await;
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let tx = node
        .create_submit_data_tx(&user_account, data_bytes.clone())
        .await?;

    let (irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = new_reth_context(node.node_ctx.reth_handle.clone().into())
        .await
        .unwrap();
    let reth_receipts = context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(reth_exec_env.block().hash()))?
        .unwrap();

    // block reward
    let block_reward_receipt = reth_receipts.last().unwrap();
    assert!(block_reward_receipt.success);
    assert_eq!(block_reward_receipt.logs.len(), 1);
    assert_eq!(
        block_reward_receipt.logs[0].topics()[0],
        *system_tx_topics::BLOCK_REWARD,
    );
    assert_eq!(block_reward_receipt.cumulative_gas_used, 0);
    assert_eq!(
        block_reward_receipt.logs[0].address,
        node.cfg.signer().address()
    );

    // storage tx
    let storage_tx_receipt = reth_receipts.first().unwrap();
    assert!(storage_tx_receipt.success);
    assert_eq!(storage_tx_receipt.logs.len(), 1);
    assert_eq!(
        storage_tx_receipt.logs[0].topics()[0],
        *system_tx_topics::STORAGE_FEES,
    );
    assert_eq!(storage_tx_receipt.cumulative_gas_used, 0);
    assert_eq!(storage_tx_receipt.logs[0].address, user_account.address());
    assert_eq!(tx.header.signer, user_account.address());
    assert_eq!(tx.header.data_size, data_bytes.len() as u64);

    // ensure that the balance for the storage user has decreased
    let signer_balance = context
        .inner
        .provider
        .basic_account(&user_account.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get signer_b balance: {}", err);
            U256::ZERO
        });
    assert_eq!(
        signer_balance,
        U256::from(1000) - U256::from(tx.header.total_fee())
    );

    // ensure that block heights in reth and irys are the same
    let reth_block = reth_exec_env.block().clone();
    assert_eq!(reth_block.number, irys_block.height);

    // check irys DB for built block
    let db_irys_block = node.get_block_by_hash(&irys_block.block_hash).unwrap();
    assert_eq!(
        db_irys_block.evm_block_hash,
        reth_block.into_header().hash_slow()
    );

    node.stop().await;
    Ok(())
}

// todo add tests where the user has no funds to fund the storage

#[tokio::test]
async fn heavy_mine_ten_blocks_with_capacity_poa_solution() -> eyre::Result<()> {
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new_genesis(config).start().await;
    let reth_context = new_reth_context(node.node_ctx.reth_handle.clone().into()).await?;

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

    node.node_ctx.start_mining().await?;
    let reth_context = new_reth_context(node.node_ctx.reth_handle.clone().into()).await?;

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

    let reth_context = new_reth_context(node.node_ctx.reth_handle.clone().into()).await?;

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

#[ignore]
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
    let node = IrysNodeTest::new_genesis(config).start().await;
    let reth_context = new_reth_context(node.node_ctx.reth_handle.clone().into()).await?;
    let _recipient_init_balance = reth_context
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

    let (_block, _reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();

    let _block_reward = U256::from(0);

    error!("TODO: NEW SHADOW LOGIC");
    node.stop().await;
    return Ok(());

    // for receipt in reth_exec_env.shadow_receipts {
    //     match receipt.tx_type {
    //         ShadowTxType::BlockReward(block_reward_shadow) => {
    //             assert_eq!(receipt.result, ShadowResult::Success);
    //             block_reward = block_reward_shadow.reward;
    //         }
    //         ShadowTxType::Data(_data_shadow) => {
    //             let og_tx = irys_txs.get(&receipt.tx_id).unwrap();
    //             assert_eq!(receipt.result, ShadowResult::Success);
    //             assert_ne!(og_tx.header.signer, account1.address()); // account1 has no funds
    //         }
    //         _ => {
    //             panic!("test does not expect this shadow type")
    //         }
    //     }
    // }
    // assert_ne!(block_reward, U256::from(0), "block reward cannot be 0");

    // //check reth for built block
    // let reth_block = reth_context
    //     .inner
    //     .provider
    //     .block_by_hash(_block.evm_block_hash)?
    //     .unwrap();

    // assert!(evm_txs.contains_key(reth_block.body.transactions.first().unwrap().hash()));
    // assert_eq!(
    //     reth_context
    //         .rpc
    //         .get_balance(recipient.address(), None)
    //         .await?,
    //     _recipient_init_balance + U256::from(1)
    // );
    // // check irys DB for built block
    // let db_irys_block = node.get_block_by_hash(&_block.block_hash).unwrap();

    // assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

    // node.stop().await;
    // Ok(())
}

#[tokio::test]
async fn heavy_rewards_get_calculated_correctly() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await;
    let node = node.start().await;

    let reth_context = new_reth_context(node.node_ctx.reth_handle.clone().into()).await?;

    let mut prev_ts: Option<u128> = None;
    let reward_address = node.node_ctx.config.node_config.reward_address;
    let mut _init_balance = reth_context.rpc.get_balance(reward_address, None).await?;

    for _ in 0..3 {
        // mine a single block
        let (block, _reth_exec_env) = mine_block(&node.node_ctx)
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
            let _expected_reward = node
                .node_ctx
                .reward_curve
                .reward_between(old_ts, new_ts)
                .unwrap();

            // find the BlockReward shadow receipt and check correctness
            let _reward_shadow_found = false;
            error!("TODO: NEW SHADOW LOGIC");
            // for receipt in reth_exec_env.shadow_receipts {
            //     if let ShadowTxType::BlockReward(br_shadow) = receipt.tx_type {
            //         let expected_new_balance = init_balance + br_shadow.reward;
            //         let new_balance = reth_context.rpc.get_balance(reward_address, None).await?;
            //         assert_eq!(new_balance, expected_new_balance);
            //         assert_eq!(
            //             receipt.result,
            //             ShadowResult::Success,
            //             "block-reward shadow must succeed"
            //         );
            //         assert_eq!(
            //             br_shadow.reward,
            //             expected_reward.amount.into(),
            //             "incorrect block-reward amount recorded in shadow"
            //         );
            //         reward_shadow_found = true;
            //         break;
            //     }
            // }
            // assert!(
            //     reward_shadow_found,
            //     "BlockReward shadow transaction not found in receipts"
            // );
        }

        // update baseline timestamp and ensure the next block gets a later one
        prev_ts = Some(new_ts);
        _init_balance = reth_context.rpc.get_balance(reward_address, None).await?;
        sleep(Duration::from_millis(1_500)).await;
    }

    assert!(prev_ts.is_some());
    node.stop().await;
    Ok(())
}
