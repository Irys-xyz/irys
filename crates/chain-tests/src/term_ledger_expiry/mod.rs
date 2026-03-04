mod perm_refund;

use crate::utils::IrysNodeTest;
use alloy_core::primitives::B256;
use alloy_genesis::GenesisAccount;
use alloy_primitives::Address;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_chain::IrysNodeCtx;
use irys_types::{
    fee_distribution::TermFeeCharges, irys::IrysSigner, ConsensusConfig, DataLedger,
    DataTransaction, IrysAddress, IrysBlockHeader, NodeConfig, U256,
};
use std::ops::{Deref, DerefMut};
use tracing::info;

// Test 0: Complete slot expiry with 0 transactions

#[test_log::test(tokio::test)]
async fn heavy_ledger_expiry_many_blocks_no_txs() -> eyre::Result<()> {
    info!("Testing ledger expiry with no transactions at all");

    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 10,   // 320 bytes per partition
        submit_ledger_epoch_length: 2, // expires after 2 epochs to ensure proper setup
        num_blocks_in_epoch: 3,        // 3 blocks per epoch
        num_transactions: 0,           // 0 txs to create 0 partitions
        txs_per_block: 0,              // batch more txs per block (11 txs / 3 blocks ≈ 4)
        data_size_per_tx: 32,          // 1 chunk per tx
        expected_expired_tx_count: 0,  // No transactions posted
    })
    .await
}

// Test 1: Complete slot expiry with 11 transactions
//
// ═══ DATA LAYOUT ═══
// • 11 txs × 32 bytes = 11 chunks total
// • Partition size = 10 chunks (320 bytes)
// • Creates 2 slots: Slot 0 (10 txs), Slot 1 (1 tx)
//
// Chunk Distribution:
// ┌─────────────────────────┬──────────┐
// │    Slot 0 (10 chunks)   │  Slot 1  │
// │    Txs 0-9              │  Tx 10   │
// └─────────────────────────┴──────────┘
//
// ═══ TIMELINE ═══
// Epoch 1 (blocks 1-3):  All 11 txs submitted
// Epoch 2 (blocks 4-6):  Waiting period
// Epoch 3 (block 7):     EXPIRY → All 11 txs expire
//
// Expiry = Block 7 - (2 epochs × 3 blocks) = 1
// All slots created in blocks 1-3 expire when checked at block 7
//
#[test_log::test(tokio::test)]
async fn heavy_ledger_expiry_many_blocks_sparse_txs() -> eyre::Result<()> {
    info!("Testing ledger expiry with many sparse blocks (1 tx per block)");

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

// Test 2: Partial slot expiry with 7 transactions
//
// ═══ DATA LAYOUT ═══
// • 7 txs × 32 bytes = 7 chunks total
// • Partition size = 5 chunks (160 bytes)
// • Creates 2 slots: Slot 0 (5 txs), Slot 1 (2 txs)
//
// Chunk Distribution:
// ┌─────────────────────────┬──────────────┐
// │    Slot 0 (5 chunks)    │   Slot 1     │
// │    Txs 0-4              │   Txs 5-6    │
// └─────────────────────────┴──────────────┘
//
// ═══ TIMELINE ═══
// Epoch 1: 7 txs submitted (2 txs per block)
// Epoch 2: Waiting period
// Epoch 3: EXPIRY → Only Slot 0 expires (5 txs)
//         Slot 1 remains active
//
#[test_log::test(tokio::test)]
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
        expected_expired_tx_count: 5, // first slot expires
    })
    .await
}

// Test 3: Large transactions spanning partition boundaries
//
// ═══ DATA LAYOUT ═══
// • 4 txs × 96 bytes = 12 chunks total (3 chunks per tx)
// • Partition size = 5 chunks (160 bytes)
// • Transactions span across partition boundaries
//
// Chunk Distribution:
// ┌──────────────────────┬──────────────────┬──────────────────┐
// │   Partition 0        │   Partition 1    │   Partition 2    │
// │   Chunks 0-4         │   Chunks 5-9     │   Chunks 10-11   │
// ├──────────────────────┼──────────────────┼──────────────────┤
// │ Tx0: [0,1,2]        │                  │                  │
// │ Tx1: [3,4]──────────│─>[5]             │                  │
// │                     │ Tx2: [6,7,8]     │                  │
// │                     │ Tx3: [9]─────────│─>[10,11]         │
// └──────────────────────┴──────────────────┴──────────────────┘
//
// ═══ EXPIRY ═══
// After 1 epoch: Only Tx0 and Tx1 expire (start in Partition 0)
// Tx2 and Tx3 start in later partitions, don't expire
//
#[test_log::test(tokio::test)]
async fn heavy_ledger_expiry_large_txs_spanning_partitions() -> eyre::Result<()> {
    info!("Testing ledger expiry with large transactions spanning partitions");
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 5,
        submit_ledger_epoch_length: 1,
        num_blocks_in_epoch: 3,
        num_transactions: 4,
        txs_per_block: 2,
        data_size_per_tx: 96,         // 3 chunks per tx
        expected_expired_tx_count: 2, // Tx0 and Tx1 expire
    })
    .await
}

// Test 4: Transactions spanning boundaries with multiple slot expiry
//
// ═══ DATA LAYOUT ═══
// • 10 txs × 64 bytes = 20 chunks total (2 chunks per tx)
// • Partition size = 3 chunks (96 bytes) - very small
// • Creates ~7 partitions, txs span boundaries
//
// Chunk Distribution (txs span partition boundaries):
// ┌────────┬────────┬────────┬────────┬────────┬────────┬────────┐
// │ Part 0 │ Part 1 │ Part 2 │ Part 3 │ Part 4 │ Part 5 │ Part 6 │
// │ [0-2]  │ [3-5]  │ [6-8]  │ [9-11] │ [12-14]│ [15-17]│ [18-19]│
// ├────────┼────────┼────────┼────────┼────────┼────────┼────────┤
// │Tx0:[0,1]        │        │        │        │        │        │
// │Tx1:[2]─│─>[3]   │        │        │        │        │        │
// │        │Tx2:[4,5]        │        │        │        │        │
// │        │Tx3:[5]─│─>[6]   │        │        │        │        │
// │        │        │Tx4:[7,8]        │        │        │        │
// │        │        │Tx5:[8]─│─>[9]   │        │        │        │
// │        │        │        │Tx6:[10,│11]     │        │        │
// │        │        │        │Tx7:[11]│─>[12]  │        │        │
// │        │        │        │        │Tx8:[13,│14]     │        │
// │        │        │        │        │Tx9:[14]│─>[15]  │        │
// └────────┴────────┴────────┴────────┴────────┴────────┴────────┘
//
// ═══ TIMELINE ═══
// Epoch 1 (blocks 1-3): 10 txs submitted, creating slots 0-2
// Epoch 2 (block 4-6): Waiting period
// Epoch 3 (block 7):   EXPIRY → First 3 slots expire (6 txs)
//                      Txs 0-5 expire (start in partitions 0-2)
//
#[test_log::test(tokio::test)]
async fn heavy_ledger_expiry_multiple_partitions_expire() -> eyre::Result<()> {
    info!("Testing ledger expiry with multiple partitions all expiring");
    ledger_expiry_test(LedgerExpiryTestParams {
        chunk_size: 32,
        num_chunks_in_partition: 3,    // Small 96-byte partitions
        submit_ledger_epoch_length: 2, // Changed to 2 epochs for proper expiry
        num_blocks_in_epoch: 3,        // Changed to 3 blocks per epoch
        num_transactions: 10,          // 10 txs that span boundaries
        txs_per_block: 4,
        data_size_per_tx: 64,         // 2 chunks per tx to span boundaries
        expected_expired_tx_count: 6, // First 6 txs expire (slots 0-2)
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
    miner_address: IrysAddress,

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
        config.consensus.get_mut().num_chunks_in_recall_range = 1;
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
                .await
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
        num_blocks_in_epoch: u64,
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

        // Just mine two epochs if there are no transactions in this test
        if num_transactions == 0 {
            for _ in 0..(num_blocks_in_epoch * 2) {
                let block = self.mine_block().await?;
                self.total_block_rewards =
                    self.total_block_rewards.saturating_add(block.reward_amount);
                self.blocks_mined.push(block);
            }

            return Ok(());
        }

        for i in 0..num_transactions {
            info!("Posting transaction {} with {} bytes", i, data_size_per_tx);

            // Create transaction with custom data size
            let data = vec![i as u8; data_size_per_tx];
            let tx = self.post_data_tx(anchor, data, &self.signer).await;

            // Track fees
            self.total_term_fees = self
                .total_term_fees
                .saturating_add(tx.header.term_fee.into());
            if let Some(perm_fee) = tx.header.perm_fee {
                self.total_perm_fees = self.total_perm_fees.saturating_add(perm_fee.into());
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
                .map(std::vec::Vec::len)
                .unwrap_or(0);
            let publish_count = tx_ids
                .get(&DataLedger::Publish)
                .map(std::vec::Vec::len)
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
                            info!(
                                "Transaction {} (index {}) expired in block {}",
                                tx.header.id, idx, height
                            );
                            expired_count += 1;
                        }
                    }
                    info!(
                        "Total of our transactions expired in block {}: {}",
                        height, expired_count
                    );
                }
            }

            self.blocks_mined.push(block);
        }

        self.wait_for_block_at_height(expiry_block_height, 30)
            .await?;

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

        info!(
            "Calculating expiry fees for {} expired transactions",
            expired_tx_count
        );
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
    async fn get_miner_balance(&self, block_hash: B256) -> eyre::Result<U256> {
        // Canonical Irys height can advance before the corresponding EVM header
        // is queryable via reth RPC under CI load.
        self.wait_for_evm_block(block_hash, 30).await?;

        Ok(U256::from_be_bytes(
            self.get_balance(self.miner_address, block_hash)
                .await
                .to_be_bytes(),
        ))
    }

    /// Verify balance after initial blocks
    async fn verify_initial_balance(&self) -> eyre::Result<()> {
        let last_block = self.blocks_mined.last().expect("Should have mined blocks");
        let expected = self
            .initial_balance
            .saturating_add(self.total_block_rewards)
            .saturating_add(self.immediate_term_rewards);

        let actual = self.get_miner_balance(last_block.evm_block_hash).await?;

        assert_eq!(
            actual, expected,
            "Balance after initial blocks should match expected. Got {}, expected {}",
            actual, expected
        );

        Ok(())
    }

    /// Verify final balance matches all expected fees
    async fn verify_final_balance(&self) -> eyre::Result<()> {
        // Look for expired ledger fee shadow transactions in ALL blocks we mined
        let mut actual_expiry_fees = U256::from(0);

        let miner_address: Address = self.miner_address.into();

        // Check each block for TermFeeReward shadow transactions
        for block in &self.blocks_mined {
            let evm_block = self.wait_for_evm_block(block.evm_block_hash, 30).await?;
            let block_txs = evm_block.body.transactions;

            info!(
                "Block {} has {} transactions",
                block.height,
                block_txs.len()
            );

            for tx in &block_txs {
                // Decode the shadow transaction
                let mut input = tx.input().as_ref();
                if let Ok(shadow_tx) =
                    irys_reth_node_bridge::irys_reth::shadow_tx::ShadowTransaction::decode(
                        &mut input,
                    )
                {
                    if let Some(irys_reth_node_bridge::irys_reth::shadow_tx::TransactionPacket::TermFeeReward(reward)) = shadow_tx.as_v1() {
                        info!("Found TermFeeReward shadow tx in block {}: target={}, amount={}, irys_ref={:?}",
                            block.height, reward.target, reward.amount, reward.irys_ref);
                        if reward.target == miner_address {
                            let amount = U256::from_le_bytes(reward.amount.to_le_bytes());
                            actual_expiry_fees = actual_expiry_fees.saturating_add(amount);
                        }
                    }
                }
            }
        }

        info!(
            "Total expiry fees from shadow transactions: {}",
            actual_expiry_fees
        );
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
        let actual = self.get_miner_balance(final_block.evm_block_hash).await?;

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
        params.num_blocks_in_epoch,
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
    ctx.node.stop().await;
    Ok(())
}
