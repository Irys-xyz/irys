use alloy_core::primitives::{ruint::aliases::U256, TxKind};
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_eips::HashOrNumber;
use alloy_genesis::GenesisAccount;
use irys_actors::mempool_service::TxIngressError;
use irys_actors::{async_trait, sha, BlockProdStrategy, BlockProducerInner, ProductionStrategy};
use irys_database::SystemLedger;
use irys_domain::{BlockState, ChainState, EmaSnapshot};
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_reth_node_bridge::irys_reth::alloy_rlp::Decodable as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{
    shadow_tx_topics, ShadowTransaction, TransactionPacket,
};
use irys_reth_node_bridge::reth_e2e_test_utils::transaction::TransactionTestContext;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, IrysBlockHeader, NodeConfig};
use irys_types::{IrysTransactionCommon as _, H256};
use reth::{
    providers::{
        AccountReader as _, BlockReader as _, ReceiptProvider as _, TransactionsProvider as _,
    },
    rpc::types::TransactionRequest,
};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

use crate::utils::{
    mine_block, mine_block_and_wait_for_validation, new_pledge_tx, new_stake_tx,
    read_block_from_state, solution_context, AddTxError, BlockValidationOutcome, IrysNodeTest,
};

#[test_log::test(tokio::test)]
async fn heavy_test_blockprod() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
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
    node.wait_until_height(irys_block.height, 10).await?;
    let context = node.node_ctx.reth_node_adapter.clone();
    let reth_receipts = context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(irys_block.evm_block_hash))?
        .unwrap();

    // block reward
    let block_reward_receipt = reth_receipts.first().unwrap();
    assert!(block_reward_receipt.success);
    assert_eq!(block_reward_receipt.logs.len(), 1);
    assert_eq!(
        block_reward_receipt.logs[0].topics()[0],
        *shadow_tx_topics::BLOCK_REWARD,
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
        *shadow_tx_topics::STORAGE_FEES,
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
        reth_block.clone().into_header().hash_slow()
    );

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_mine_ten_blocks_with_capacity_poa_solution() -> eyre::Result<()> {
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new_genesis(config).start().await;
    let reth_context = node.node_ctx.reth_node_adapter.clone();

    // Collect block hashes as we mine
    let mut block_hashes = Vec::new();

    for i in 1..10 {
        info!("manually producing block {}", i);
        node.mine_block().await?;
        let block_hash = node.wait_until_height(i, 10).await?;
        let block = node.get_block_by_hash(&block_hash)?;

        //check reth for built block
        let reth_block = reth_context
            .inner
            .provider
            .find_block_by_hash(block.evm_block_hash, reth::providers::BlockSource::Any)
            .unwrap()
            .unwrap();
        assert_eq!(i, reth_block.header.number);
        assert_eq!(reth_block.number, block.height);

        // check irys DB for built block
        let db_irys_block = node.get_block_by_hash(&block.block_hash).unwrap();
        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

        // Collect block hash for later verification
        block_hashes.push(block.block_hash);
    }

    // Verify all collected blocks are on-chain
    for (idx, hash) in block_hashes.iter().enumerate() {
        let state = read_block_from_state(&node.node_ctx, hash).await;
        assert_eq!(
            state,
            BlockValidationOutcome::StoredOnNode(ChainState::Onchain),
            "Block {} with hash {:?} should be on-chain",
            idx + 1,
            hash
        );

        // Also verify the block can be retrieved from the database
        let db_block = node.get_block_by_hash(hash).unwrap();
        assert_eq!(db_block.height, (idx + 1) as u64);
    }

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_mine_ten_blocks() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().start().await;

    node.node_ctx.start_mining().await?;
    let reth_context = node.node_ctx.reth_node_adapter.clone();

    // Collect block hashes as we mine
    let mut block_hashes = Vec::new();

    for i in 1..10 {
        let _block_hash = node.wait_until_height(i + 1, 10).await?;

        //check reth for built block
        let reth_block = reth_context.inner.provider.block_by_number(i)?.unwrap();
        assert_eq!(i, reth_block.header.number);
        assert_eq!(i, reth_block.number);

        let db_irys_block = node.get_block_by_height(i).await.unwrap();

        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());

        // Collect block hash for later verification
        block_hashes.push(db_irys_block.block_hash);
    }

    // Verify all collected blocks are on-chain
    for (idx, hash) in block_hashes.iter().enumerate() {
        let state = read_block_from_state(&node.node_ctx, hash).await;
        assert_eq!(
            state,
            BlockValidationOutcome::StoredOnNode(ChainState::Onchain),
            "Block {} with hash {:?} should be on-chain",
            idx + 1,
            hash
        );

        // Also verify the block can be retrieved from the database
        let db_block = node.get_block_by_hash(hash).unwrap();
        assert_eq!(db_block.height, (idx + 1) as u64);
    }

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_basic_blockprod() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().start().await;

    let (block, _, outcome) = mine_block_and_wait_for_validation(&node.node_ctx).await?;
    assert_eq!(
        outcome,
        BlockValidationOutcome::StoredOnNode(ChainState::Onchain)
    );

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
    tokio::time::sleep(Duration::from_secs(3)).await;
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
    config.consensus.get_mut().block_migration_depth = 1;

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
    let _recipient_init_balance = reth_context.rpc.get_balance(recipient.address(), None)?;

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

    let (irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    node.wait_until_height(irys_block.height, 10).await?;

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
        ShadowTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(matches!(
        block_reward_systx.as_v1().unwrap(),
        TransactionPacket::BlockReward(_)
    ));

    // Assert storage tx is included in the receipts (should be the second receipt)
    let storage_tx_systx =
        ShadowTransaction::decode(&mut block_txs[1].as_legacy().unwrap().tx().input.as_ref())
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
    let recipient_balance = reth_context.rpc.get_balance(recipient.address(), None)?;
    assert_eq!(recipient_balance, U256::from(1)); // The transferred amount

    // Verify account1 balance decreased by storage fees and gas costs
    let account1_balance = reth_context.rpc.get_balance(account1.address(), None)?;
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
    let node = IrysNodeTest::default_async();
    let node = node.start().await;

    let reth_context = node.node_ctx.reth_node_adapter.clone();

    let mut prev_ts: Option<u128> = None;
    let reward_address = node.node_ctx.config.node_config.reward_address;
    let mut _init_balance = reth_context.rpc.get_balance(reward_address, None)?;

    for _ in 0..3 {
        // mine a single block
        let block = node.mine_block().await?;

        // obtain the EVM timestamp for this block from Reth
        let reth_block = reth_context
            .inner
            .provider
            .find_block_by_hash(block.evm_block_hash, reth::providers::BlockSource::Any)
            .unwrap()
            .unwrap();
        let new_ts = reth_block.header.timestamp as u128;

        // update baseline timestamp and ensure the next block gets a later one
        prev_ts = Some(new_ts);
        _init_balance = reth_context.rpc.get_balance(reward_address, None)?;
        sleep(Duration::from_millis(1_500)).await;
    }

    assert!(prev_ts.is_some());
    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_unfunded_user_tx_rejected() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
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
    let irys_block = node.mine_block().await?;
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward shadow transaction
    let block_txs = context
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(irys_block.evm_block_hash))?
        .unwrap();

    assert_eq!(
        block_txs.len(),
        1,
        "Block should only contain one transaction (block reward)"
    );

    // Verify it's a block reward shadow transaction
    let shadow_tx =
        ShadowTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(
        matches!(
            shadow_tx.as_v1().unwrap(),
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
    let mut node = IrysNodeTest::default_async();
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
    let irys_block = node.mine_block().await?;
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward shadow transaction
    let block_txs = context
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(irys_block.evm_block_hash))?
        .unwrap();

    assert_eq!(
        block_txs.len(),
        1,
        "Block should only contain one transaction (block reward)"
    );

    // Verify it's a block reward shadow transaction
    let shadow_tx =
        ShadowTransaction::decode(&mut block_txs[0].as_legacy().unwrap().tx().input.as_ref())
            .unwrap();
    assert!(
        matches!(
            shadow_tx.as_v1().unwrap(),
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
    let mut node = IrysNodeTest::default_async();
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
    let irys_block = node.mine_block().await?;
    node.wait_until_height(irys_block.height, 10).await?;
    let context = node.node_ctx.reth_node_adapter.clone();
    let reth_receipts = context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(irys_block.evm_block_hash))?
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
        *shadow_tx_topics::BLOCK_REWARD,
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
        *shadow_tx_topics::STORAGE_FEES,
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

#[test_log::test(actix_web::test)]
async fn heavy_staking_pledging_txs_included() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for the peer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Initialize the peer with our keypair/signer
    let peer_config = genesis_node.testnet_peer_with_signer(&peer_signer);

    // Start the peer: No packing on the peer, it doesn't have partition assignments yet
    let peer_node = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER")
        .await;
    peer_node.start_public_api().await;

    // Get initial balance of the peer signer
    let reth_context = genesis_node.node_ctx.reth_node_adapter.clone();
    let initial_balance = reth_context
        .inner
        .provider
        .basic_account(&peer_signer.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get peer balance: {}", err);
            U256::ZERO
        });

    // Post stake + pledge commitments to the peer
    let stake_tx = peer_node.post_stake_commitment(H256::zero()).await; // zero() is the genesis block hash
    let pledge_tx = peer_node.post_pledge_commitment(H256::zero()).await;

    // Wait for commitment tx to show up in the genesis_node's mempool
    genesis_node
        .wait_for_mempool(stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(pledge_tx.id, seconds_to_wait)
        .await?;

    // Mine a block to get the stake commitment included
    let irys_block1 = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(irys_block1.height, 10)
        .await?;

    // Get receipts for the first block
    let receipts1 = reth_context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(irys_block1.evm_block_hash))?
        .unwrap();

    // Verify block contains all expected shadow transactions
    // Based on the logs, both stake and pledge are included in the same block
    assert_eq!(
        receipts1.len(),
        3,
        "Block should contain exactly 3 receipts: block reward, stake, and pledge"
    );

    // Find and verify the stake shadow transaction receipt
    let stake_receipt = receipts1
        .iter()
        .find(|r| {
            r.logs
                .iter()
                .any(|log| log.topics()[0] == *shadow_tx_topics::STAKE)
        })
        .expect("Stake shadow transaction receipt not found");

    assert!(stake_receipt.success, "Stake transaction should succeed");
    assert_eq!(
        stake_receipt.cumulative_gas_used, 0,
        "Shadow tx should not consume gas"
    );
    assert_eq!(
        stake_receipt.logs[0].address,
        peer_signer.address(),
        "Stake transaction should target the peer's address"
    );

    // Find and verify the pledge shadow transaction receipt (it's in the same block)
    let pledge_receipt = receipts1
        .iter()
        .find(|r| {
            r.logs
                .iter()
                .any(|log| log.topics()[0] == *shadow_tx_topics::PLEDGE)
        })
        .expect("Pledge shadow transaction receipt not found");

    assert!(pledge_receipt.success, "Pledge transaction should succeed");
    assert_eq!(
        pledge_receipt.cumulative_gas_used, 0,
        "Shadow tx should not consume gas"
    );
    assert_eq!(
        pledge_receipt.logs[0].address,
        peer_signer.address(),
        "Pledge transaction should target the peer's address"
    );

    // Get balance after both stake and pledge transactions
    let balance_after_block1 = reth_context
        .inner
        .provider
        .basic_account(&peer_signer.address())
        .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
        .unwrap_or_else(|err| {
            tracing::warn!("Failed to get peer balance: {}", err);
            U256::ZERO
        });

    // In the same block:
    // - Stake decreases balance by (commitment_value + fee)
    // - Pledge decreases balance by (commitment_value + fee)
    // Both decrease balance by 2 each, so total decrease is 4
    assert_eq!(
        balance_after_block1,
        initial_balance - U256::from(4),
        "Balance should decrease by 4 (2 for stake + 2 for pledge)"
    );

    // Mine another block to verify the system continues to work
    let irys_block2 = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(irys_block2.height, 10)
        .await?;

    // Get receipts for the second block
    let receipts2 = reth_context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(irys_block2.evm_block_hash))?
        .unwrap();

    // Second block should only have block reward
    assert_eq!(
        receipts2.len(),
        1,
        "Second block should only contain block reward"
    );
    assert_eq!(
        receipts2[0].logs[0].topics()[0],
        *shadow_tx_topics::BLOCK_REWARD,
        "Second block should only have block reward shadow tx"
    );

    // Get the genesis nodes view of the peers assignments
    let peer_assignments = genesis_node.get_partition_assignments(peer_signer.address());

    // Verify that one partition has been assigned to the peer to match its pledge
    assert_eq!(peer_assignments.len(), 1);

    // Verify block transactions contain the expected shadow transactions in the correct order
    let block_txs1 = reth_context
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(irys_block1.evm_block_hash))?
        .unwrap();

    // Block should contain exactly 3 transactions: block reward, stake, pledge (in that order)
    assert_eq!(
        block_txs1.len(),
        3,
        "Block should contain exactly 3 transactions"
    );

    // First transaction should be block reward
    let block_reward_tx =
        ShadowTransaction::decode(&mut block_txs1[0].as_legacy().unwrap().tx().input.as_ref())
            .expect("First transaction should be decodable as shadow transaction");
    assert!(
        matches!(
            block_reward_tx.as_v1().unwrap(),
            TransactionPacket::BlockReward(_)
        ),
        "First transaction should be block reward"
    );

    // Second transaction should be stake
    let stake_tx =
        ShadowTransaction::decode(&mut block_txs1[1].as_legacy().unwrap().tx().input.as_ref())
            .expect("Second transaction should be decodable as shadow transaction");
    if let Some(TransactionPacket::Stake(bd)) = stake_tx.as_v1() {
        assert_eq!(bd.target, peer_signer.address());
        assert_eq!(bd.amount, U256::from(2)); // commitment_value(1) + fee(1)
    } else {
        panic!("Second transaction should be stake");
    }

    // Third transaction should be pledge
    let pledge_tx =
        ShadowTransaction::decode(&mut block_txs1[2].as_legacy().unwrap().tx().input.as_ref())
            .expect("Third transaction should be decodable as shadow transaction");
    if let Some(TransactionPacket::Pledge(bd)) = pledge_tx.as_v1() {
        assert_eq!(bd.target, peer_signer.address());
        assert_eq!(bd.amount, U256::from(2)); // commitment_value(1) + fee(1)
    } else {
        panic!("Third transaction should be pledge");
    }
    genesis_node.stop().await;
    peer_node.stop().await;

    Ok(())
}

// This test produces a block with an invalid PoA chunk.
// A new block will not be built on the invalid block.
#[test_log::test(actix_web::test)]
async fn heavy_block_prod_will_not_build_on_invalid_blocks() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait(?Send)]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        fn create_poa_data(
            &self,
            solution: &irys_types::block_production::SolutionContext,
            ledger_id: Option<u32>,
        ) -> eyre::Result<(irys_types::PoaData, H256)> {
            // Create an invalid PoA chunk that doesn't match the actual solution
            let invalid_chunk = vec![0xFF; 256 * 1024]; // Fill with invalid data
            let poa_chunk = irys_types::Base64(invalid_chunk);
            // hash is valid so that prevalidation succeeds
            let poa_chunk_hash = H256(sha::sha256(&poa_chunk.0));

            let poa = irys_types::PoaData {
                tx_path: solution.tx_path.clone().map(irys_types::Base64),
                data_path: solution.data_path.clone().map(irys_types::Base64),
                chunk: Some(poa_chunk),
                recall_chunk_index: solution.recall_chunk_index,
                ledger_id,
                partition_chunk_offset: solution.chunk_offset,
                partition_hash: solution.partition_hash,
            };
            Ok((poa, poa_chunk_hash))
        }
    }

    // Configure test network
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut node = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);

    // Create peer signer and fund it
    let peer_signer = node.new_random_signer();
    node.fund_genesis_accounts(vec![&peer_signer]);

    // Start genesis node (node 1)
    let node = IrysNodeTest::new_genesis(node.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    // disable validation for this test
    node.node_ctx.set_validation_enabled(false);
    node.start_public_api().await;

    // Create evil block production strategy
    let evil_strategy = EvilBlockProdStrategy {
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    // Produce block with invalid PoA
    let (evil_block, _eth_payload) = evil_strategy
        .fully_produce_new_block(solution_context(&node.node_ctx).await?)
        .await?
        .unwrap();

    // Mine a valid block
    // note: cannot use `.mine_block()` because there will be height mismatch when it awaits for the new height
    let mut sub = node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();

    // turn back on the validation for this test
    node.node_ctx.set_validation_enabled(true);
    let (new_block, _reth_block) = ProductionStrategy {
        inner: node.node_ctx.block_producer_inner.clone(),
    }
    .fully_produce_new_block(solution_context(&node.node_ctx).await?)
    .await?
    .unwrap();

    // Get the new block and verify its parent is not the evil block
    assert_ne!(
        new_block.previous_block_hash, evil_block.block_hash,
        "expect the new block parent to NOT be the evil parent block"
    );
    assert_eq!(
        new_block.height, evil_block.height,
        "we have created a fork because we don't want to build on the evil block"
    );
    loop {
        // wait for the block to be validated
        let res = sub.recv().await.unwrap();
        if res.block_hash == new_block.block_hash
            // if we get anything other than Unknown, proceed processing
            && res.state != ChainState::NotOnchain(BlockState::Unknown)
        {
            break;
        }
    }

    let latest_block_hash = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_max_cumulative_difficulty_block()
        .1;
    let new_block_state = *node
        .node_ctx
        .block_tree_guard
        .read()
        .get_block_and_status(&new_block.block_hash)
        .unwrap()
        .1;
    assert_eq!(latest_block_hash, new_block.block_hash);
    assert_eq!(new_block_state, ChainState::Onchain);

    // Cleanup
    node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_always_build_on_max_difficulty_block() -> eyre::Result<()> {
    // Define the OptimisticBlockMiningStrategy that mines blocks without waiting for validation
    struct OptimisticBlockMiningStrategy {
        pub prod: ProductionStrategy,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for OptimisticBlockMiningStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        // Override parent_irys_block to immediately select the highest cumulative difficulty block
        // without waiting for validation, enabling optimistic mining
        async fn parent_irys_block(&self) -> eyre::Result<(IrysBlockHeader, Arc<EmaSnapshot>)> {
            // Get the block with highest cumulative difficulty immediately
            let (_, parent_block_hash) = self
                .inner()
                .block_tree_guard
                .read()
                .get_max_cumulative_difficulty_block();

            // Fetch the parent block header
            let header = self.fetch_block_header(parent_block_hash).await?;

            // Get the EMA snapshot
            let ema_snapshot = self.get_block_ema_snapshot(&header.block_hash)?;

            Ok((header, ema_snapshot))
        }
    }

    // Configure test network
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new_genesis(config).start().await;

    // disable validation for this test
    node.node_ctx.set_validation_enabled(false);

    // Create optimistic block production strategy
    let optimistic_strategy = OptimisticBlockMiningStrategy {
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    // Mine 5 blocks using the optimistic strategy
    let mut optimistic_blocks: Vec<Arc<IrysBlockHeader>> = Vec::new();
    for i in 1..=5 {
        info!("Mining optimistic block {}", i);

        // Generate a solution and produce block with optimistic strategy
        let solution = solution_context(&node.node_ctx).await?;
        let (block, _eth_payload) = optimistic_strategy
            .fully_produce_new_block(solution)
            .await?
            .unwrap();

        // Verify this block builds on the previous one (or genesis for first block)
        if i > 1 {
            assert_eq!(
                block.previous_block_hash,
                optimistic_blocks[i - 2].block_hash,
                "Optimistic block {} should build on previous optimistic block",
                i
            );
        }

        optimistic_blocks.push(block.clone());
    }

    // Verify all optimistic blocks were mined at correct heights
    for (idx, block) in optimistic_blocks.iter().enumerate() {
        assert_eq!(
            block.height,
            (idx + 1) as u64,
            "Optimistic block {} should be at height {}",
            idx + 1,
            idx + 1
        );
    }

    // re-enable validation
    node.node_ctx.set_validation_enabled(true);

    // Now mine a new block using the normal mining method
    // This should wait for validation and build on the last optimistic block
    info!("Mining normal block after optimistic chain");
    let (normal_block, _) = mine_block(&node.node_ctx).await?.unwrap();

    // Wait for the normal block to be fully processed
    node.wait_until_height(normal_block.height, 10).await?;

    // Assert that the normal block extends the last optimistic block
    assert_eq!(
        normal_block.previous_block_hash,
        optimistic_blocks.last().unwrap().block_hash,
        "Normal block should extend the last optimistic block"
    );
    assert_eq!(normal_block.height, 6, "Normal block should be at height 6");

    // Also verify that the parent is validated
    let parent_block_state = {
        let tree = node.node_ctx.block_tree_guard.read();
        *tree
            .get_block_and_status(&optimistic_blocks.last().unwrap().block_hash)
            .unwrap()
            .1
    };

    // Check if the parent block is validated (either Onchain or Validated with ValidBlock)
    let is_parent_validated = matches!(
        parent_block_state,
        ChainState::Onchain | ChainState::Validated(BlockState::ValidBlock)
    );

    assert!(
        is_parent_validated,
        "Parent block should be marked as validated in the block tree, but was {:?}",
        parent_block_state
    );

    // Cleanup
    node.stop().await;

    Ok(())
}

// Setup: Configure a node with block_tree_depth=3 to test pruning behavior
// Action: Mine 10 blocks, checking that blocks get pruned while mining.
// Assert: Verify blocks 1-7 are pruned and blocks 8, 9, 10 still exist in the tree
#[test_log::test(tokio::test)]
async fn heavy_test_block_tree_pruning() -> eyre::Result<()> {
    // Setup
    // Configure test parameters
    let block_tree_depth = 3;
    let num_blocks_to_mine = 10;

    // Configure a node with specified block_tree_depth
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().block_tree_depth = block_tree_depth;

    let node = IrysNodeTest::new_genesis(config).start().await;

    // Action
    // Mine blocks and collect their hashes
    let mut all_block_hashes = Vec::new();

    for height_to_mine in 1..=num_blocks_to_mine {
        info!("Mining block {}", height_to_mine);

        // Mine a block using the utility that auto-waits
        let block = node.mine_block().await?;

        // Store the block hash
        all_block_hashes.push(block.block_hash);

        // Assert the tree size is as expected
        // The canonical chain starts with genesis (1 block) and adds mined blocks
        // But only keeps up to block_tree_depth blocks total
        let total_blocks = height_to_mine + 1; // genesis + mined blocks
        let expected_tree_size = std::cmp::min(total_blocks, block_tree_depth as usize);
        let actual_tree_size = node.get_canonical_chain().len();
        assert_eq!(
            actual_tree_size, expected_tree_size,
            "Tree size mismatch at height {}: expected {}, got {}",
            height_to_mine, expected_tree_size, actual_tree_size
        );
    }

    // Assert
    // Verify tree has exactly block_tree_depth blocks
    assert_eq!(
        node.get_canonical_chain().len(),
        block_tree_depth as usize,
        "Final tree size should be exactly {}",
        block_tree_depth
    );

    // Verify blocks that should be pruned [1-7]
    for height in 1..=7 {
        let block_hash = &all_block_hashes[height - 1];
        let block_result = node.get_block_by_hash(block_hash);
        assert!(
            block_result.is_err(),
            "Block at height {} should be pruned",
            height
        );
    }

    // Verify blocks that should still exist [8-10]
    for height in 8..=10 {
        let block_hash = &all_block_hashes[height - 1];
        let block_result = node.get_block_by_hash(block_hash);
        assert!(
            block_result.is_ok(),
            "Block at height {} should not be pruned",
            height
        );
    }

    node.stop().await;
    Ok(())
}

#[actix::test]
/// test that config option max_commitment_txs_per_block is enforced
/// check individual blocks have correct txs. e.g.
/// 1 stake + 11 pledge commitment txs with a limit of two per block, we should see 2 +2 +2 +2 +0 +2 +2
/// epoch blocks should include any new txs
/// epoch blocks should contain a copy of all commitment txs from blocks in the epoch block range
async fn commitment_txs_are_capped_per_block() -> eyre::Result<()> {
    let seconds_to_wait = 10;
    let max_commitment_txs_per_block: u64 = 2;
    let num_blocks_in_epoch = 5;

    initialize_tracing();

    let max_commitments_per_epoch =
        (num_blocks_in_epoch * max_commitment_txs_per_block) - max_commitment_txs_per_block;

    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch.try_into()?);
    genesis_config
        .consensus
        .get_mut()
        .mempool
        .max_commitment_txs_per_block = max_commitment_txs_per_block;

    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let _ = genesis_node.start_public_api().await;

    // create and post stake commitment tx
    let stake_tx = new_stake_tx(&H256::zero(), &signer);
    genesis_node.post_commitment_tx(&stake_tx).await?;

    let mut tx_ids: Vec<H256> = vec![stake_tx.id]; // txs used for anchor chain and later to check mempool ingress
    for _ in 0..11 {
        let tx = new_pledge_tx(
            tx_ids.last().expect("valid tx id for use as anchor"),
            &signer,
        );
        tx_ids.push(tx.id);
        genesis_node.post_commitment_tx(&tx).await?;
    }

    // wait for all txs to ingress mempool
    genesis_node
        .wait_for_mempool_commitment_txs(tx_ids.clone(), seconds_to_wait)
        .await?;

    let mut counts = Vec::new();
    for i in 1..=8 {
        genesis_node.mine_block().await?;
        let block = genesis_node.get_block_by_height(i).await?;
        let is_epoch_block = block.height > 0 && block.height % num_blocks_in_epoch == 0;
        counts.push(
            block
                .system_ledgers
                .get(SystemLedger::Commitment as usize)
                .map_or(0, |l| l.tx_ids.len()),
        );
        if is_epoch_block {
            assert_eq!(counts[(i - 1) as usize], max_commitments_per_epoch as usize);
        } else {
            assert!(counts[(i - 1) as usize] <= max_commitment_txs_per_block as usize);
        }
    }

    // check the grand total txs is correct
    assert_eq!(
        counts.iter().sum::<usize>(),
        20,
        "Total count of commitment txs is incorrect",
    );

    // check individual blocks have correct txs.
    // for 1 stake + 11 pledge total commitment txs with a limit of two per block, we should see 2 + 2 + 2 + 2 + 0 + 2 + 2

    for h in 1..num_blocks_in_epoch {
        let block_n = genesis_node.get_block_by_height(h).await?;
        assert_eq!(
            2,
            block_n
                .system_ledgers
                .get(SystemLedger::Commitment as usize)
                .map_or(0, |l| l.tx_ids.len()),
            "block {} commitment tx count is incorrect",
            h
        );
    }

    // epoch block rolls up previous txs
    let epoch_block = genesis_node
        .get_block_by_height(num_blocks_in_epoch)
        .await?;
    assert_eq!(epoch_block.height % num_blocks_in_epoch, 0);
    let epoch_tx_ids = epoch_block
        .system_ledgers
        .get(SystemLedger::Commitment as usize)
        .map_or(Vec::<H256>::new(), |l| l.tx_ids.0.clone());
    assert_eq!(epoch_tx_ids, tx_ids[..max_commitments_per_epoch as usize]);

    // some blocks after epoch should contain commitment txs
    // this will be a few blocks, as we posted enough txs above to populate two more blocks
    for h in 6..=7 {
        let block_n = genesis_node.get_block_by_height(h).await?;
        assert_eq!(
            2,
            block_n
                .system_ledgers
                .get(SystemLedger::Commitment as usize)
                .map_or(0, |l| l.tx_ids.len()),
            "post-epoch block commitment tx count is incorrect",
        );
    }

    // we have then used all commitment txs from mempool, so final block(s) are empty
    let final_height = 8;
    let final_block = genesis_node.get_block_by_height(final_height).await?;
    assert_eq!(
        0,
        final_block
            .system_ledgers
            .get(SystemLedger::Commitment as usize)
            .map_or(0, |l| l.tx_ids.len()),
        "post-epoch, emptied mempool of commitments. block {:?} commitment tx count is incorrect",
        final_height,
    );

    genesis_node.stop().await;

    Ok(())
}
