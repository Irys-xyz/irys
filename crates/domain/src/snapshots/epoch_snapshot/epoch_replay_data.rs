use irys_database::db::IrysDatabaseExt as _;
use irys_database::{block_header_by_hash, commitment_tx_by_txid, SystemLedger};
use irys_storage::RecoveredMempoolState;
use irys_types::{CommitmentTransaction, Config, DatabaseProvider, IrysBlockHeader};
use reth_db::Database as _;
use std::collections::VecDeque;

use crate::block_index_guard::BlockIndexReadGuard;

#[derive(Debug, Clone)]
/// Represents an epoch block and its associated commitment transactions
pub struct EpochBlockData {
    pub epoch_block: IrysBlockHeader,
    pub commitments: Vec<CommitmentTransaction>,
}

#[derive(Debug, Clone)]
/// Represents the complete historical epoch data needed for replay
/// Contains the genesis block, genesis commitments, and all subsequent epoch blocks
pub struct EpochReplayData {
    pub genesis_block_header: IrysBlockHeader,
    pub genesis_commitments: Vec<CommitmentTransaction>,
    pub epoch_blocks: Vec<EpochBlockData>,
}

impl EpochReplayData {
    /// Retrieves historical epoch data from the blockchain
    ///
    /// Queries all epoch blocks and their commitments from the database,
    /// returning a complete structure containing the genesis block, its commitments,
    /// and all subsequent epoch blocks for replaying the chain state.
    ///
    /// # Arguments
    /// * `db` - Database access provider
    /// * `block_index_guard` - Read guard for the block index
    /// * `config` - Configuration for epoch parameters
    ///
    /// # Returns
    /// * EpochReplayData containing all historical epoch information
    pub async fn query_replay_data(
        db: &DatabaseProvider,
        block_index_guard: &BlockIndexReadGuard,
        config: &Config,
    ) -> eyre::Result<Self> {
        // Recover any mempool commitment transactions that were persisted
        let recovered =
            RecoveredMempoolState::load_from_disk(&config.node_config.mempool_dir(), false).await;

        let block_index = block_index_guard.read();

        // Calculate how many epoch blocks should exist in the chain
        let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
        let latest_height = block_index.latest_height();
        let num_epoch_blocks = (latest_height / num_blocks_in_epoch) + 1;
        let mut epoch_block_data: VecDeque<EpochBlockData> = VecDeque::new();
        // Process each epoch block from genesis to the latest
        for i in 0..num_epoch_blocks {
            let block_height = i * num_blocks_in_epoch;

            // Retrieve the epoch block header from the index hash; if index missing, skip
            let Some(item) = block_index.get_item(block_height) else {
                // missing epoch block index entry. Panic!
                panic!("Missing epoch block @ {} from block index", block_height);
            };
            let block = db
                .view_eyre(|tx| block_header_by_hash(tx, &item.block_hash, false))?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Expected to find block header in database for indexed hash {}",
                        item.block_hash
                    )
                })?;

            // Ensure block height matches expected position in the chain
            if block.height != block_height {
                return Err(eyre::eyre!(
                    "Block height mismatch: stored={}, expected={} for hash={}",
                    block.height,
                    block_height,
                    block.block_hash
                ));
            }

            // Find the commitment ledger in the epoch block's system ledgers
            let commitment_ledger = match block
                .system_ledgers
                .iter()
                .find(|b| b.ledger_id == SystemLedger::Commitment)
            {
                Some(v) => v,
                None => {
                    // skip the commitment specific logic
                    epoch_block_data.push_back(EpochBlockData {
                        epoch_block: block,
                        commitments: vec![],
                    });
                    continue;
                }
            };

            // Retrieve all commitment transactions referenced by this epoch block
            let read_tx = db
                .tx()
                .expect("Expected to create a valid database transaction");
            let commitments_tx = commitment_ledger
                .tx_ids
                .iter()
                .map(|txid| {
                    // First try to get the commitment tx from the DB
                    let opt = commitment_tx_by_txid(&read_tx, txid)?;
                    opt.or_else(|| recovered.commitment_txs.get(txid).cloned())
                        .ok_or_else(|| {
                            // If we can't find it, there's no continuing
                            eyre::eyre!("Commitment transaction not found: txid={}", txid)
                        })
                })
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_else(|_| panic!("Able to fetch all commitment transactions from database for epoch block {} at height {}", block.block_hash, block.height));

            epoch_block_data.push_back(EpochBlockData {
                epoch_block: block,
                commitments: commitments_tx,
            });
        }

        // Separate genesis data from subsequent epoch blocks
        let genesis_data = {
            let genesis_block_data = epoch_block_data
                .pop_front()
                .expect("Expected at least one epoch block (genesis) in the replay data");

            if genesis_block_data.epoch_block.height > 0 {
                panic!(
                    "Missing genesis block from epoch blocks, first block was hash {} height {}",
                    genesis_block_data.epoch_block.block_hash,
                    genesis_block_data.epoch_block.height
                );
            }

            genesis_block_data
        };

        Ok(Self {
            genesis_block_header: genesis_data.epoch_block,
            genesis_commitments: genesis_data.commitments,
            epoch_blocks: epoch_block_data.into(),
        })
    }
}
