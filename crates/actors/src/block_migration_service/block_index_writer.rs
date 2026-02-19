use eyre::eyre;
use irys_domain::{BlockIndex, SupplyState};
use irys_types::{BlockHash, ConsensusConfig, DataLedger, SealedBlock};
use reth_db::transaction::{DbTx, DbTxMut};
use std::sync::Arc;
use tracing::{info, instrument};

/// Writes block index entries and updates supply state during migration.
///
/// The block index write is performed within a caller-provided DB transaction
/// (via [`write_block_index`](Self::write_block_index)), allowing it to be
/// atomic with other block persistence operations. Supply state is updated
/// separately after the transaction commits.
#[derive(Debug)]
pub struct BlockIndexWriter {
    supply_state: Option<Arc<SupplyState>>,
    chunk_size: u64,
    last_received_block: Option<(u64, BlockHash)>,
}

impl BlockIndexWriter {
    pub fn new(supply_state: Option<Arc<SupplyState>>, consensus_config: &ConsensusConfig) -> Self {
        Self {
            supply_state,
            chunk_size: consensus_config.chunk_size,
            last_received_block: None,
        }
    }

    /// Writes a block's index entry using the provided database transaction.
    ///
    /// Enforces strict sequential ordering: each block's height must be
    /// exactly one more than the previous, and its `previous_block_hash`
    /// must match.
    #[instrument(level = "trace", skip_all, err, fields(block.height = %sealed_block.header().height, block.hash = %sealed_block.header().block_hash))]
    pub fn write_block_index(
        &mut self,
        tx: &(impl DbTxMut + DbTx),
        sealed_block: &SealedBlock,
    ) -> eyre::Result<()> {
        let block = sealed_block.header();
        let transactions = sealed_block.transactions();

        // ── ordering invariant ──────────────────────────────────────────
        if let Some((prev_height, prev_hash)) = &self.last_received_block {
            if block.height != prev_height + 1 || &block.previous_block_hash != prev_hash {
                return Err(eyre!(
                    "Block migration out of order or with a gap: prev_height={}, prev_hash={}, current_height={}, current_hash={}",
                    prev_height, prev_hash, block.height, block.block_hash
                ));
            }
        } else {
            info!(
                "BlockIndexWriter received its first block: height {}, hash {:x}",
                block.height, block.block_hash
            );
        }

        let mut all_txs = Vec::new();
        all_txs.extend_from_slice(transactions.get_ledger_txs(DataLedger::Publish));
        all_txs.extend_from_slice(transactions.get_ledger_txs(DataLedger::Submit));

        BlockIndex::push_block(tx, block, &all_txs, self.chunk_size)?;

        self.last_received_block = Some((block.height, block.block_hash));

        Ok(())
    }

    /// Updates supply state after the block index transaction has committed.
    ///
    /// Must be called after [`write_block_index`](Self::write_block_index) succeeds
    /// and the enclosing transaction commits, since the backfill task reads from
    /// block_index to determine which blocks need historical reward summing.
    pub fn update_supply_state(&self, sealed_block: &SealedBlock) -> eyre::Result<()> {
        if let Some(supply_state) = &self.supply_state {
            let block = sealed_block.header();
            supply_state
                .add_block_reward(block.height, block.reward_amount)
                .map_err(|e| eyre!("Supply state update failed during migration: {}", e))?;
        }
        Ok(())
    }
}
