use crate::utils::IrysNodeTest;
use alloy_core::primitives::B256;
use alloy_genesis::GenesisAccount;
use irys_chain::IrysNodeCtx;
use irys_types::{
    fee_distribution::TermFeeCharges, irys::IrysSigner, Address, ConsensusConfig, DataLedger,
    DataTransaction, IrysBlockHeader, NodeConfig, U256,
};
use reth::providers::TransactionsProvider;
use reth::rpc::types::TransactionTrait;
use std::ops::{Deref, DerefMut};
use tracing::info;

// Test 1: Many sparse blocks with single transaction per block
#[test_log::test(actix_web::test)]
async fn heavy_ledger_expiry_many_blocks_sparse_txs() -> eyre::Result<()> {
    info!("Testing ledger expiry with many sparse blocks (1 tx per block)");
    
    // Expected behavior:
    // - Each tx is 32 bytes = 1 chunk
    // - Partition size: 10 chunks * 32 bytes = 320 bytes
    // - 11 transactions = 11 chunks total
    // - The system dynamically allocates multiple slots as data arrives
    // - All slots are created within the same epoch (blocks 1-3)
    // - When expiry occurs after 2 epochs, all slots expire together
    // - Therefore, all 11 transactions expire
    
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 10,   // 320 bytes per partition
        submit_ledger_epoch_length: 2, // expires after 2 epochs to ensure proper setup
        num_blocks_in_epoch: 3,        // 3 blocks per epoch
        num_transactions: 11,          // 11 txs to create 2 partitions
        txs_per_block: 4,              // batch more txs per block (11 txs / 3 blocks ≈ 4)
        data_size_per_tx: 32,          // 1 chunk per tx
        expected_expired_tx_count: 11, // All 11 txs expire (multiple slots created at similar times)
    })
    .await
}

// Test 2: Multiple transactions per block filling partitions
#[test_log::test(actix_web::test)]
async fn heavy_ledger_expiry_multiple_txs_per_block() -> eyre::Result<()> {
    info!("Testing ledger expiry with multiple transactions per block");
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 5, // 160 bytes per partition
        submit_ledger_epoch_length: 2,
        num_blocks_in_epoch: 3,
        num_transactions: 7,
        txs_per_block: 2,
        data_size_per_tx: 32,         // 1 chunk per tx
        expected_expired_tx_count: 5, // first partition
    })
    .await
}

// Test 3: Large transactions spanning multiple partitions
#[test_log::test(actix_web::test)]
async fn heavy_ledger_expiry_large_txs_spanning_partitions() -> eyre::Result<()> {
    info!("Testing ledger expiry with large transactions spanning partitions");
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 5, // 160 bytes per partition - larger partitions
        submit_ledger_epoch_length: 1,
        num_blocks_in_epoch: 3,
        num_transactions: 4,          // 4 transactions to ensure >1 partition
        txs_per_block: 2,             // 2 txs per block for stability
        data_size_per_tx: 96,         // 3 chunks per tx, some span boundaries
        expected_expired_tx_count: 2, // first 2 txs start in first partition (chunks 0-4)
    })
    .await
}

// Test 4: Multiple partitions fully filled and all expire
#[test_log::test(actix_web::test)]
async fn heavy_ledger_expiry_multiple_partitions_expire() -> eyre::Result<()> {
    info!("Testing ledger expiry with multiple partitions all expiring");
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 3, // 96 bytes per partition - small partitions
        submit_ledger_epoch_length: 1, // expires after 1 epoch
        num_blocks_in_epoch: 6,
        num_transactions: 16, // 16 txs * 32 bytes = 16 chunks = 6 partitions (need >5 for 5 to expire)
        txs_per_block: 4,     // 4 txs per block
        data_size_per_tx: 32, // 1 chunk per tx
        expected_expired_tx_count: 15, // first 15 transactions expire (5 full partitions)
    })
    .await
}

/// Test context for ledger expiry testing that wraps the test node
struct LedgerExpiryTestContext {
    node: IrysNodeTest<IrysNodeCtx>,
    signer: IrysSigner,

    // Tracking
    transactions: Vec<DataTransaction>,
    blocks_mined: Vec<IrysBlockHeader>,

    // Fee accounting
    total_term_fees: U256,
    total_perm_fees: U256,
    immediate_term_rewards: U256,
    expected_expiry_fees: U256,
    total_block_rewards: U256,

    // Balances
    initial_balance: U256,
    miner_address: Address,

    // Config
    consensus_config: ConsensusConfig,
    submit_ledger_epoch_length: u64,
    num_blocks_in_epoch: u64,
}

impl Deref for LedgerExpiryTestContext {
    type Target = IrysNodeTest<IrysNodeCtx>;

    fn deref(&self) -> &Self::Target {
        &self.node
    }
}

impl DerefMut for LedgerExpiryTestContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.node
    }
}

impl LedgerExpiryTestContext {
    /// Setup test with ledger expiry configuration
    async fn setup(
        chunk_size: u64,
        num_chunks_in_partition: u64,
        submit_ledger_epoch_length: u64,
        num_blocks_in_epoch: u64,
    ) -> eyre::Result<Self> {
        // Configure node
        let mut config = NodeConfig::testing();
        config.consensus.get_mut().block_migration_depth = 1;
        config.consensus.get_mut().chunk_size = chunk_size;
        config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
        config.consensus.get_mut().epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        config.consensus.get_mut().epoch.num_blocks_in_epoch = num_blocks_in_epoch;

        // Create funded signer
        let signer = IrysSigner::random_signer(&config.consensus_config());
        config.consensus.extend_genesis_accounts(vec![(
            signer.address(),
            GenesisAccount {
                balance: U256::from(10_000_000_000_000_000_000_u128).into(), // 10 IRYS
                ..Default::default()
            },
        )]);

        let consensus_config = config.consensus_config();

        // Start node
        let node = IrysNodeTest::new_genesis(config.clone())
            .start_and_wait_for_packing("test", 30)
            .await;

        // Get initial balance
        let miner_address = node.node_ctx.config.node_config.miner_address();
        let genesis_block = node.get_block_by_height(0).await?;
        let initial_balance = U256::from_be_bytes(
            node.get_balance(miner_address, genesis_block.evm_block_hash)
                .to_be_bytes(),
        );

        info!("Initial miner balance: {}", initial_balance);

        Ok(Self {
            node,
            signer,
            transactions: Vec::new(),
            blocks_mined: Vec::new(),
            total_term_fees: U256::from(0),
            total_perm_fees: U256::from(0),
            immediate_term_rewards: U256::from(0),
            expected_expiry_fees: U256::from(0),
            total_block_rewards: U256::from(0),
            initial_balance,
            miner_address,
            consensus_config,
            submit_ledger_epoch_length,
            num_blocks_in_epoch,
        })
    }

    /// Post transactions and mine blocks in batches with custom data sizes
    async fn post_transactions_and_mine(
        &mut self,
        num_transactions: usize,
        txs_per_block: usize,
        data_size_per_tx: usize,
    ) -> eyre::Result<()> {
        let genesis_block = self.get_block_by_height(0).await?;
        let anchor = genesis_block.block_hash;
        let mut pending_txs = Vec::new();
        
        // Calculate expected partition layout
        let chunk_size = self.consensus_config.chunk_size;
        let chunks_per_partition = self.consensus_config.num_chunks_in_partition;
        let partition_size = chunk_size * chunks_per_partition;
        let chunks_per_tx = data_size_per_tx.div_ceil(chunk_size as usize) as u64;
        
        info!("Partition layout:");
        info!("  Chunk size: {} bytes", chunk_size);
        info!("  Chunks per partition: {}", chunks_per_partition);
        info!("  Partition size: {} bytes", partition_size);
        info!("  Data size per tx: {} bytes", data_size_per_tx);
        info!("  Chunks per tx: {}", chunks_per_tx);

        for i in 0..num_transactions {
            info!("Posting transaction {} with {} bytes", i, data_size_per_tx);

            // Create transaction with custom data size
            let data = vec![i as u8; data_size_per_tx];
            let tx = self.post_data_tx(anchor, data, &self.signer).await;

            // Track fees
            self.total_term_fees = self.total_term_fees.saturating_add(tx.header.term_fee);
            if let Some(perm_fee) = tx.header.perm_fee {
                self.total_perm_fees = self.total_perm_fees.saturating_add(perm_fee);
            }

            self.transactions.push(tx.clone());
            pending_txs.push(tx.header.id);

            // Wait for mempool
            self.wait_for_mempool(tx.header.id, 10).await?;

            // Mine block after batch or last transaction
            if pending_txs.len() >= txs_per_block || i == num_transactions - 1 {
                info!("Mining block with {} transactions", pending_txs.len());
                let block = self.mine_block().await?;

                // Verify inclusion
                let tx_ids_map = block.get_data_ledger_tx_ids();
                let submit_txs = tx_ids_map
                    .get(&DataLedger::Submit)
                    .expect("Submit ledger should have transactions");

                for tx_id in &pending_txs {
                    assert!(
                        submit_txs.contains(tx_id),
                        "Transaction {:?} should be included in block {}",
                        tx_id,
                        block.height
                    );
                }

                info!(
                    "Block {} mined with {} transactions",
                    block.height,
                    pending_txs.len()
                );

                // Track block reward
                self.total_block_rewards =
                    self.total_block_rewards.saturating_add(block.reward_amount);
                self.blocks_mined.push(block);
                pending_txs.clear();
            }
        }

        info!(
            "Posted {} transactions with total term_fees: {}, perm_fees: {}",
            num_transactions, self.total_term_fees, self.total_perm_fees
        );

        Ok(())
    }

    /// Mine blocks to trigger Submit ledger expiry
    async fn mine_to_trigger_expiry(&mut self) -> eyre::Result<()> {
        let last_block = self.blocks_mined.last().expect("Should have mined blocks");
        let current_height = last_block.height;

        // Calculate target height for expiry
        let target_expiry_height = (self.submit_ledger_epoch_length + 1) * self.num_blocks_in_epoch;
        let expiry_block_height = target_expiry_height.max(current_height + 3);

        info!(
            "Current height: {}, targeting expiry at block {}",
            current_height, expiry_block_height
        );

        // Mine blocks to reach expiry height
        for height in (current_height + 1)..=expiry_block_height {
            self.mine_block().await?;
            let block = self.get_block_by_height(height).await?;

            // Track block reward
            self.total_block_rewards = self.total_block_rewards.saturating_add(block.reward_amount);
            info!("Block {} reward: {}", height, block.reward_amount);

            let tx_ids = block.get_data_ledger_tx_ids();
            let submit_count = tx_ids
                .get(&DataLedger::Submit)
                .map(std::collections::HashSet::len)
                .unwrap_or(0);
            let publish_count = tx_ids
                .get(&DataLedger::Publish)
                .map(std::collections::HashSet::len)
                .unwrap_or(0);
            
            info!(
                "Block {}: Submit: {} txs, Publish: {} txs",
                height, submit_count, publish_count
            );
            
            // Log which transactions moved to publish (expired)
            if publish_count > 0 {
                if let Some(publish_txs) = tx_ids.get(&DataLedger::Publish) {
                    info!("Block {} expired transactions: {:?}", height, publish_txs);
                    
                    // Check which of our transactions expired
                    let mut expired_count = 0;
                    for (idx, tx) in self.transactions.iter().enumerate() {
                        if publish_txs.contains(&tx.header.id) {
                            info!("Transaction {} (index {}) expired in block {}", tx.header.id, idx, height);
                            expired_count += 1;
                        }
                    }
                    info!("Total of our transactions expired in block {}: {}", height, expired_count);
                }
            }

            self.blocks_mined.push(block);
        }

        self.wait_until_height(expiry_block_height, 30).await?;

        let expiry_block = &self.blocks_mined.last().unwrap();
        info!("Reached expiry block at height {}", expiry_block_height);

        let expiry_tx_ids = expiry_block.get_data_ledger_tx_ids();
        info!(
            "Expiry block ledgers: Submit: {:?}, Publish: {:?}",
            expiry_tx_ids.get(&DataLedger::Submit),
            expiry_tx_ids.get(&DataLedger::Publish)
        );

        Ok(())
    }

    /// Calculate immediate term fee rewards (5% for block producers)
    fn calculate_immediate_rewards(&mut self) -> eyre::Result<()> {
        self.immediate_term_rewards = U256::from(0);

        for tx in &self.transactions {
            let term_charges = TermFeeCharges::new(tx.header.term_fee, &self.consensus_config)?;
            self.immediate_term_rewards = self
                .immediate_term_rewards
                .saturating_add(term_charges.block_producer_reward);
        }

        info!(
            "Expected immediate term fee rewards: {}",
            self.immediate_term_rewards
        );
        Ok(())
    }

    /// Calculate expected expiry fees (95% treasury for expired transactions)
    fn calculate_expiry_fees(&mut self, expired_tx_count: usize) -> eyre::Result<()> {
        self.expected_expiry_fees = U256::from(0);

        info!("Calculating expiry fees for {} expired transactions", expired_tx_count);
        info!("Total transactions posted: {}", self.transactions.len());
        
        // Log which blocks contain which transactions
        let mut tx_to_block = std::collections::HashMap::new();
        for block in &self.blocks_mined {
            let tx_ids = block.get_data_ledger_tx_ids();
            if let Some(submit_txs) = tx_ids.get(&DataLedger::Submit) {
                for tx_id in submit_txs {
                    for (idx, tx) in self.transactions.iter().enumerate() {
                        if &tx.header.id == tx_id {
                            tx_to_block.insert(idx, block.height);
                        }
                    }
                }
            }
        }
        
        info!("Transaction distribution across blocks:");
        for i in 0..self.transactions.len() {
            if let Some(block_height) = tx_to_block.get(&i) {
                info!("  Tx {}: in block {}", i, block_height);
            }
        }
        
        for i in 0..expired_tx_count {
            let tx = &self.transactions[i];
            let term_charges = TermFeeCharges::new(tx.header.term_fee, &self.consensus_config)?;

            info!(
                "Tx {} (expecting to expire): term_fee={}, block_producer_reward={}, treasury={}",
                i,
                tx.header.term_fee,
                term_charges.block_producer_reward,
                term_charges.term_fee_treasury
            );
            
            // On expiry, the miner gets the treasury portion
            self.expected_expiry_fees = self
                .expected_expiry_fees
                .saturating_add(term_charges.term_fee_treasury);
        }

        info!(
            "Expected expiry fees for miner: {}",
            self.expected_expiry_fees
        );
        Ok(())
    }

    /// Get current balance for miner
    async fn get_miner_balance(&self, block_hash: B256) -> U256 {
        U256::from_be_bytes(
            self.get_balance(self.miner_address, block_hash)
                .to_be_bytes(),
        )
    }

    /// Verify balance after initial blocks
    async fn verify_initial_balance(&self) -> eyre::Result<()> {
        let last_block = self.blocks_mined.last().expect("Should have mined blocks");
        let expected = self
            .initial_balance
            .saturating_add(self.total_block_rewards)
            .saturating_add(self.immediate_term_rewards);

        let actual = self.get_miner_balance(last_block.evm_block_hash).await;

        assert_eq!(
            actual, expected,
            "Balance after initial blocks should match expected. Got {}, expected {}",
            actual, expected
        );

        Ok(())
    }

    /// Verify final balance matches all expected fees
    async fn verify_final_balance(&self) -> eyre::Result<()> {
        // Get the reth context to examine shadow transactions
        let reth_context = self.node.node_ctx.reth_node_adapter.clone();
        
        // Look for expired ledger fee shadow transactions in ALL blocks we mined
        let mut actual_expiry_fees = U256::from(0);
        
        // Check each block for TermFeeReward shadow transactions
        for block in &self.blocks_mined {
            // Get all transactions from this block
            let block_txs = reth_context
                .inner
                .provider
                .transactions_by_block(alloy_eips::HashOrNumber::Hash(block.evm_block_hash))?
                .unwrap_or_default();
            
            info!("Block {} has {} transactions", block.height, block_txs.len());
            
            for tx in &block_txs {
                // Decode the shadow transaction
                if let Ok(shadow_tx) = irys_reth_node_bridge::irys_reth::shadow_tx::ShadowTransaction::decode(&mut tx.input().as_ref()) {
                    if let Some(packet) = shadow_tx.as_v1() {
                        // Check if this is a TermFeeReward transaction (from expired ledger)
                        if let irys_reth_node_bridge::irys_reth::shadow_tx::TransactionPacket::TermFeeReward(reward) = packet {
                            info!("Found TermFeeReward shadow tx in block {}: target={}, amount={}, irys_ref={:?}", 
                                block.height, reward.target, reward.amount, reward.irys_ref);
                            if reward.target == self.miner_address {
                                let amount = U256::from_le_bytes(reward.amount.to_le_bytes());
                                actual_expiry_fees = actual_expiry_fees.saturating_add(amount);
                            }
                        }
                    }
                }
            }
        }
        
        info!("Total expiry fees from shadow transactions: {}", actual_expiry_fees);
        info!("Expected expiry fees: {}", self.expected_expiry_fees);
        
        // Verify the shadow transactions match our expectations
        assert_eq!(
            actual_expiry_fees, self.expected_expiry_fees,
            "Shadow transaction expiry fees should match expected. Got {}, expected {}",
            actual_expiry_fees, self.expected_expiry_fees
        );

        let expected = self
            .initial_balance
            .saturating_add(self.total_block_rewards)
            .saturating_add(self.immediate_term_rewards)
            .saturating_add(self.expected_expiry_fees);

        let final_block = self.blocks_mined.last().expect("Should have final block");
        let actual = self.get_miner_balance(final_block.evm_block_hash).await;

        info!("Balance breakdown:");
        info!("  Initial balance:           {}", self.initial_balance);
        info!("  Block rewards:             {}", self.total_block_rewards);
        info!(
            "  Immediate term fees (5%):  {}",
            self.immediate_term_rewards
        );
        info!("  Expiry fees (95%):         {}", self.expected_expiry_fees);
        info!("  Expected final balance:    {}", expected);
        info!("  Actual final balance:      {}", actual);

        assert_eq!(
            actual, expected,
            "Final balance should match exactly. Got {}, expected {}",
            actual, expected
        );

        info!("Submit ledger expiry fee distribution verified with EXACT balance matching!");
        Ok(())
    }
}

/// Test parameters for ledger expiry scenarios
struct LedgerExpiryTestParams {
    // Node configuration
    chunk_size: u64,
    num_chunks_in_partition: u64,
    submit_ledger_epoch_length: u64,
    num_blocks_in_epoch: u64,

    // Test scenario
    num_transactions: usize,
    txs_per_block: usize,
    data_size_per_tx: usize,

    // Expected results
    expected_expired_tx_count: usize,
}

/// Parametrized ledger expiry test
async fn ledger_expiry_test(params: LedgerExpiryTestParams) -> eyre::Result<()> {
    // Setup
    let mut ctx = LedgerExpiryTestContext::setup(
        params.chunk_size,
        params.num_chunks_in_partition,
        params.submit_ledger_epoch_length,
        params.num_blocks_in_epoch,
    )
    .await?;

    // Post transactions and mine initial blocks
    ctx.post_transactions_and_mine(
        params.num_transactions,
        params.txs_per_block,
        params.data_size_per_tx,
    )
    .await?;
    ctx.calculate_immediate_rewards()?;
    ctx.verify_initial_balance().await?;

    // Mine blocks to trigger expiry
    ctx.mine_to_trigger_expiry().await?;
    ctx.calculate_expiry_fees(params.expected_expired_tx_count)?;

    // Verify final balance
    ctx.verify_final_balance().await?;

    // Cleanup
    ctx.node.node_ctx.stop().await;
    Ok(())
}
