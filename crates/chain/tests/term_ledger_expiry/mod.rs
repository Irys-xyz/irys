use crate::utils::IrysNodeTest;
use alloy_core::primitives::B256;
use alloy_genesis::GenesisAccount;
use irys_chain::IrysNodeCtx;
use irys_types::{
    fee_distribution::TermFeeCharges, irys::IrysSigner, Address, ConsensusConfig, DataLedger,
    DataTransaction, IrysBlockHeader, NodeConfig, U256,
};
use std::ops::{Deref, DerefMut};
use tracing::info;

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

    /// Post transactions and mine blocks in batches
    async fn post_transactions_and_mine(
        &mut self,
        num_transactions: usize,
        txs_per_block: usize,
    ) -> eyre::Result<()> {
        let genesis_block = self.get_block_by_height(0).await?;
        let anchor = genesis_block.block_hash;
        let mut pending_txs = Vec::new();

        for i in 0..num_transactions {
            info!("Posting transaction {}", i);

            // Create and post transaction
            let data = vec![i as u8; 32];
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
        let target_expiry_height =
            ((self.submit_ledger_epoch_length + 1) * self.num_blocks_in_epoch) as u64;
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
            info!(
                "Block {}: Submit: {} txs, Publish: {} txs",
                height,
                tx_ids
                    .get(&DataLedger::Submit)
                    .map(|v| v.len())
                    .unwrap_or(0),
                tx_ids
                    .get(&DataLedger::Publish)
                    .map(|v| v.len())
                    .unwrap_or(0)
            );

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

        for i in 0..expired_tx_count {
            let tx = &self.transactions[i];
            let term_charges = TermFeeCharges::new(tx.header.term_fee, &self.consensus_config)?;

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
        let final_block = self.blocks_mined.last().expect("Should have final block");

        let expected = self
            .initial_balance
            .saturating_add(self.total_block_rewards)
            .saturating_add(self.immediate_term_rewards)
            .saturating_add(self.expected_expiry_fees);

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

        info!("✅ Submit ledger expiry fee distribution verified with EXACT balance matching!");
        Ok(())
    }
}

#[test_log::test(actix_web::test)]
async fn heavy_submit_ledger_expiry_single_partition() -> eyre::Result<()> {
    // Setup
    let mut ctx = LedgerExpiryTestContext::setup(
        32, // chunk_size
        5,  // num_chunks_in_partition
        2,  // submit_ledger_epoch_length
        3,  // num_blocks_in_epoch
    )
    .await?;

    // Post transactions and mine initial blocks
    ctx.post_transactions_and_mine(7, 2).await?;
    ctx.calculate_immediate_rewards()?;
    ctx.verify_initial_balance().await?;

    // Mine blocks to trigger expiry
    ctx.mine_to_trigger_expiry().await?;
    ctx.calculate_expiry_fees(5)?; // First 5 transactions expire

    // Verify final balance
    ctx.verify_final_balance().await?;

    // Cleanup
    ctx.node.node_ctx.stop().await;
    Ok(())
}
