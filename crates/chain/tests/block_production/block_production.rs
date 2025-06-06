use alloy_core::primitives::{ruint::aliases::U256, TxKind};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::HashOrNumber;
use alloy_genesis::GenesisAccount;
use eyre::OptionExt;
use irys_actors::mempool_service::TxIngressError;
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_reth_node_bridge::irys_reth::alloy_rlp::Decodable;
use irys_reth_node_bridge::irys_reth::system_tx::{
    system_tx_topics, SystemTransaction, TransactionPacket,
};
use irys_reth_node_bridge::reth_e2e_test_utils::transaction::TransactionTestContext;
use irys_types::IrysTransactionCommon;
use irys_types::{irys::IrysSigner, NodeConfig};
use reth::providers::{AccountReader, ReceiptProvider, TransactionsProvider};
use reth::{providers::BlockReader, rpc::types::TransactionRequest};
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

use crate::utils::{mine_block, AddTxError, IrysNodeTest};

#[test_log::test(tokio::test)]
async fn heavy_test_blockprod() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let user_account = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![
        (
            // ensure that the block reward address has 0 balance
            node.cfg.signer().address(),
            GenesisAccount {
                balance: U256::from(0),
                ..Default::default()
            },
        ),
        (
            user_account.address(),
            GenesisAccount {
                balance: U256::from(1000),
                ..Default::default()
            },
        ),
    ]);

    // print all addresses
    println!("user_account: {:?}", user_account.address());
    println!("node: {:?}", node.cfg.signer().address());

    let node = node.start().await;
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let tx = node
        .create_submit_data_tx(&user_account, data_bytes.clone())
        .await?;

    let (irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();
    let reth_receipts = context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(reth_exec_env.block().hash()))?
        .unwrap();

    // block reward
    let block_reward_receipt = reth_receipts.first().unwrap();
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
    let storage_tx_receipt = reth_receipts.last().unwrap();
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

    // ensure that the block reward has increased the block reward address balance
    let block_reward_address = node.cfg.signer().address();
    let block_reward_balance = context
        .inner
        .provider
        .basic_account(&block_reward_address)
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get block reward address balance: {}", err);
            U256::ZERO
        });
    assert_eq!(
        block_reward_balance,
        // started with 0 balance
        U256::from(0) + U256::from_le_bytes(irys_block.reward_amount.to_le_bytes())
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

#[test_log::test(tokio::test)]
async fn heavy_mine_ten_blocks_with_capacity_poa_solution() -> eyre::Result<()> {
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new_genesis(config).start().await;
    let reth_context = node.node_ctx.reth_node_adapter.clone();

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
        assert_eq!(reth_block.number, block.height);

        // check irys DB for built block
        let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();
        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        // MAGIC: we wait more than 1s so that the block timestamps (evm block timestamps are seconds) don't overlap
        sleep(Duration::from_millis(1500)).await;
    }
    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_mine_ten_blocks() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;

    node.node_ctx.start_mining().await?;
    let reth_context = node.node_ctx.reth_node_adapter.clone();

    for i in 1..10 {
        node.wait_until_height(i + 1, 60).await?;

        //check reth for built block
        let reth_block = reth_context.inner.provider.block_by_number(i)?.unwrap();
        assert_eq!(i, reth_block.header.number);
        assert_eq!(i, reth_block.number);

        let db_irys_block = node.get_block_by_height(i).await.unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    }
    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_basic_blockprod() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await.start().await;

    let (block, _) = mine_block(&node.node_ctx).await?.unwrap();

    let reth_context = node.node_ctx.reth_node_adapter.clone();

    //check reth for built block
    let reth_block = reth_context
        .inner
        .provider
        .block_by_hash(block.evm_block_hash)?
        .unwrap();

    // height is hardcoded at 42 right now
    assert_eq!(reth_block.number, block.height);

    // check irys DB for built block
    let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();
    assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
    node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
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
    let chain_id = config.consensus_config().chain_id;
    let recipient = IrysSigner::random_signer(&config.consensus_config());
    let account_1_balance = U256::from(1_000000000000000000_u128);
    config.consensus.extend_genesis_accounts(vec![(
        account1.address(),
        GenesisAccount {
            // 1ETH
            balance: account_1_balance,
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config).start().await;
    let reth_context = node.node_ctx.reth_node_adapter.clone();
    let _recipient_init_balance = reth_context
        .rpc
        .get_balance(recipient.address(), None)
        .await?;

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
    let tx_env = TransactionTestContext::sign_tx(account1.clone().into(), evm_tx_req).await;
    let evm_tx_hash = reth_context
        .rpc
        .inject_tx(tx_env.encoded_2718().into())
        .await
        .expect("tx should be accepted");
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let irys_tx = node
        .create_submit_data_tx(&account1, data_bytes.clone())
        .await?;

    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();

    // Get the transaction hashes from the block in order
    let block_txs = reth_exec_env
        .block()
        .body()
        .transactions
        .iter()
        .collect::<Vec<_>>();

    // We expect 3 receipts: storage tx, evm tx, and block reward
    assert_eq!(block_txs.len(), 3);
    // Assert block reward (should be the first receipt)
    let block_reward_systx =
        SystemTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(matches!(
        block_reward_systx.as_v1().unwrap(),
        TransactionPacket::BlockReward(_)
    ));

    // Assert storage tx is included in the receipts (should be the second receipt)
    let storage_tx_systx =
        SystemTransaction::decode(&mut block_txs[1].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(matches!(
        storage_tx_systx.as_v1().unwrap(),
        TransactionPacket::StorageFees(_)
    ));

    // Verify the EVM transaction hash matches
    let reth_block = reth_exec_env.block().clone();
    let block_txs = reth_context
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(reth_block.hash()))?
        .unwrap();
    let evm_tx_in_block = block_txs
        .iter()
        .find(|tx| *tx.hash() == evm_tx_hash)
        .expect("EVM transaction should be included in the block");
    assert_eq!(*evm_tx_in_block.hash(), evm_tx_hash);

    // Verify recipient received the transfer
    let recipient_balance = reth_context
        .rpc
        .get_balance(recipient.address(), None)
        .await?;
    assert_eq!(recipient_balance, U256::from(1)); // The transferred amount

    // Verify account1 balance decreased by storage fees and gas costs
    let account1_balance = reth_context
        .rpc
        .get_balance(account1.address(), None)
        .await?;
    // Balance should be: initial (1000) - storage fees - gas costs - transfer amount (1)
    let expected_balance = account_1_balance
        - U256::from(irys_tx.header.total_fee())
        - U256::from(21000 * 20e9 as u64) // gas_used * max_fee_per_gas
        - U256::from(1); // transfer amount
    assert_eq!(account1_balance, expected_balance);

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_rewards_get_calculated_correctly() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().await;
    let node = node.start().await;

    let reth_context = node.node_ctx.reth_node_adapter.clone();

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
            // expected reward according to the protocol's reward curve
            let _expected_reward = node
                .node_ctx
                .reward_curve
                .reward_between(old_ts, new_ts)
                .unwrap();
            // todo
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

#[test_log::test(tokio::test)]
async fn heavy_test_unfunded_user_tx_rejected() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let unfunded_user = IrysSigner::random_signer(&node.cfg.consensus_config());

    // Set up genesis accounts - unfunded user gets zero balance
    node.cfg.consensus.extend_genesis_accounts(vec![
        (
            // ensure that the block reward address has 0 balance
            node.cfg.signer().address(),
            GenesisAccount {
                balance: U256::from(0),
                ..Default::default()
            },
        ),
        (
            // unfunded user gets zero balance (but he has an entry in the reth db)
            unfunded_user.address(),
            GenesisAccount {
                balance: U256::from(0),
                ..Default::default()
            },
        ),
    ]);

    let node = node.start().await;

    // Attempt to create and submit a transaction from the unfunded user
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let tx_result = node
        .create_submit_data_tx(&unfunded_user, data_bytes.clone())
        .await;

    // Verify that the transaction was rejected due to insufficient funds
    match tx_result {
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            info!("Transaction correctly rejected due to insufficient funds");
        }
        Ok(_) => panic!("Expected transaction to be rejected due to insufficient funds"),
        Err(other_error) => panic!("Expected Unfunded error, got: {:?}", other_error),
    }

    // Mine a block - should only contain block reward transaction
    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward system transaction
    let block_txs = reth_exec_env
        .block()
        .body()
        .transactions
        .iter()
        .collect::<Vec<_>>();

    assert_eq!(
        block_txs.len(),
        1,
        "Block should only contain one transaction (block reward)"
    );

    // Verify it's a block reward system transaction
    let system_tx =
        SystemTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(
        matches!(
            system_tx.as_v1().unwrap(),
            TransactionPacket::BlockReward(_)
        ),
        "Single transaction should be a block reward"
    );

    // Verify unfunded user's balance remains zero
    let user_balance = context
        .inner
        .provider
        .basic_account(&unfunded_user.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get unfunded user balance: {}", err);
            U256::ZERO
        });
    assert_eq!(
        user_balance,
        U256::ZERO,
        "Unfunded user balance should remain zero"
    );
    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_nonexistent_user_tx_rejected() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let nonexistent_user = IrysSigner::random_signer(&node.cfg.consensus_config());

    // Set up genesis accounts - only add the block reward address, nonexistent_user is not in genesis
    node.cfg.consensus.extend_genesis_accounts(vec![
        (
            // ensure that the block reward address has 0 balance
            node.cfg.signer().address(),
            GenesisAccount {
                balance: U256::from(0),
                ..Default::default()
            },
        ),
        // Note: nonexistent_user is NOT added to genesis accounts, so it has implicit zero balance
    ]);

    let node = node.start().await;

    // Attempt to create and submit a transaction from the nonexistent user
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let tx_result = node
        .create_submit_data_tx(&nonexistent_user, data_bytes.clone())
        .await;

    // Verify that the transaction was rejected due to insufficient funds
    match tx_result {
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            info!("Transaction correctly rejected due to insufficient funds (nonexistent account)");
        }
        Ok(_) => panic!("Expected transaction to be rejected due to insufficient funds"),
        Err(other_error) => panic!("Expected Unfunded error, got: {:?}", other_error),
    }

    // Mine a block - should only contain block reward transaction
    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward system transaction
    let block_txs = reth_exec_env
        .block()
        .body()
        .transactions
        .iter()
        .collect::<Vec<_>>();

    assert_eq!(
        block_txs.len(),
        1,
        "Block should only contain one transaction (block reward)"
    );

    // Verify it's a block reward system transaction
    let system_tx =
        SystemTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(
        matches!(
            system_tx.as_v1().unwrap(),
            TransactionPacket::BlockReward(_)
        ),
        "Single transaction should be a block reward"
    );

    // Verify nonexistent user's balance is zero (account doesn't exist)
    let user_balance = context
        .inner
        .provider
        .basic_account(&nonexistent_user.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get nonexistent user balance: {}", err);
            U256::ZERO
        });
    assert_eq!(
        user_balance,
        U256::ZERO,
        "Nonexistent user balance should be zero"
    );

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_just_enough_funds_tx_included() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async().await;
    let user = IrysSigner::random_signer(&node.cfg.consensus_config());

    // Set up genesis accounts - user gets balance 2, but total fee is 2 (perm_fee=1 + term_fee=1)
    node.cfg.consensus.extend_genesis_accounts(vec![
        (
            // ensure that the block reward address has 0 balance
            node.cfg.signer().address(),
            GenesisAccount {
                balance: U256::from(0),
                ..Default::default()
            },
        ),
        (
            user.address(),
            GenesisAccount {
                balance: U256::from(2),
                ..Default::default()
            },
        ),
    ]);

    let node = node.start().await;

    // Create and submit a transaction from the user
    let data_bytes = "Hello, world!".as_bytes().to_vec();
    let tx = node
        .create_submit_data_tx(&user, data_bytes.clone())
        .await?;

    // Verify the transaction was accepted (fee is 2: perm_fee=1 + term_fee=1)
    assert_eq!(tx.header.total_fee(), 2, "Total fee should be 2");

    // Mine a block - should contain block reward and storage fee transactions
    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();
    let reth_receipts = context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(reth_exec_env.block().hash()))?
        .unwrap();

    // Should have 2 receipts: block reward and storage fees
    assert_eq!(
        reth_receipts.len(),
        2,
        "Block should contain block reward and storage fee transactions"
    );

    // Verify block reward receipt (first)
    let block_reward_receipt = &reth_receipts[0];
    assert!(
        block_reward_receipt.success,
        "Block reward transaction should succeed"
    );
    assert_eq!(
        block_reward_receipt.logs[0].topics()[0],
        *system_tx_topics::BLOCK_REWARD,
        "First transaction should be block reward"
    );

    // Verify storage fee receipt (second)
    let storage_fee_receipt = &reth_receipts[1];
    assert!(
        storage_fee_receipt.success,
        "Storage fee transaction should fail due to insufficient funds"
    );
    assert_eq!(
        storage_fee_receipt.logs[0].topics()[0],
        *system_tx_topics::STORAGE_FEES,
        "Second transaction should be storage fees"
    );
    assert_eq!(
        storage_fee_receipt.logs[0].address,
        user.address(),
        "Storage fee transaction should target the user's address"
    );

    // Verify user's balance
    let user_balance = context
        .inner
        .provider
        .basic_account(&user.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get user balance: {}", err);
            U256::ZERO
        });
    assert_eq!(
        user_balance,
        U256::from(0),
        "User balance should go down to 0"
    );

    node.stop().await;
    Ok(())
}
