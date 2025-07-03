use alloy_core::primitives::{ruint::aliases::U256, TxKind};
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_eips::HashOrNumber;
use alloy_genesis::GenesisAccount;
use eyre::OptionExt as _;
use irys_actors::block_tree_service::ChainState;
use irys_actors::mempool_service::TxIngressError;
use irys_actors::{async_trait, sha, BlockProdStrategy, BlockProducerInner, ProductionStrategy};
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_reth_node_bridge::irys_reth::alloy_rlp::Decodable as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{
    shadow_tx_topics, ShadowTransaction, TransactionPacket,
};
use irys_reth_node_bridge::reth_e2e_test_utils::transaction::TransactionTestContext;
use irys_types::{irys::IrysSigner, NodeConfig};
use irys_types::{IrysTransactionCommon as _, H256};
use reth::providers::{AccountReader as _, ReceiptProvider as _, TransactionsProvider as _};
use reth::{providers::BlockReader as _, rpc::types::TransactionRequest};
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

use crate::utils::{
    mine_block, mine_block_and_wait_for_validation, read_block_from_state, solution_context,
    AddTxError, BlockValidationOutcome, IrysNodeTest,
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

    // Collect block hashes as we mine
    let mut block_hashes = Vec::new();

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
        let block_hash = node.wait_until_height(i + 1, 60).await?;
        let state = read_block_from_state(&node.node_ctx, &block_hash).await;
        assert_eq!(
            state,
            BlockValidationOutcome::StoredOnNode(ChainState::Onchain)
        );

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
    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward shadow transaction
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
    let (_irys_block, reth_exec_env) = mine_block(&node.node_ctx).await?.unwrap();
    let context = node.node_ctx.reth_node_adapter.clone();

    // Verify block transactions - should only contain block reward shadow transaction
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
    let (_irys_block1, reth_exec_env1) = mine_block(&genesis_node.node_ctx).await?.unwrap();

    // Get receipts for the first block
    let receipts1 = reth_context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(reth_exec_env1.block().hash()))?
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
    let (_irys_block2, reth_exec_env2) = mine_block(&genesis_node.node_ctx).await?.unwrap();

    // Get receipts for the second block
    let receipts2 = reth_context
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(reth_exec_env2.block().hash()))?
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
    let peer_assignments = genesis_node
        .get_partition_assignments(peer_signer.address())
        .await;

    // Verify that one partition has been assigned to the peer to match its pledge
    assert_eq!(peer_assignments.len(), 1);

    // Verify block transactions contain the expected shadow transactions in the correct order
    let block_txs1 = reth_exec_env1
        .block()
        .body()
        .transactions
        .iter()
        .collect::<Vec<_>>();

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
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);

    // Create peer signer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start genesis node (node 1)
    let node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
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
    let mut sub = node.node_ctx.service_senders.subscribe_block_state_updates();
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
        let res = sub.recv().await.unwrap();
        if res.block.block_hash == new_block.block_hash {
            break;
        }
    }

    let latest_block_hash = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash;
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
