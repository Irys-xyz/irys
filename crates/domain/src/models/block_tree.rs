use irys_config::StorageSubmodulesConfig;
use irys_database::{block_header_by_hash, commitment_tx_by_txid, tx_header_by_txid};
use irys_types::{
    BlockBody, BlockHash, BlockTransactions, CommitmentTransaction, Config, ConsensusConfig,
    DataLedger, DataTransactionHeader, DatabaseProvider, H256, IrysBlockHeader, SealedBlock, U256,
};
use reth_db::Database as _;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::SystemTime,
};
use tracing::debug;

use crate::{
    BlockIndexReadGuard, BlockTreeReadGuard, CommitmentSnapshot, EmaSnapshot, EpochReplayData,
    EpochSnapshot, create_ema_snapshot_from_chain_history,
};

#[derive(Debug)]
pub struct BlockTreeEntry(pub Arc<SealedBlock>);

impl BlockTreeEntry {
    pub fn block_hash(&self) -> BlockHash {
        self.0.header().block_hash
    }

    pub fn height(&self) -> u64 {
        self.0.header().height
    }

    pub fn header(&self) -> &Arc<IrysBlockHeader> {
        self.0.header()
    }

    pub fn transactions(&self) -> &Arc<BlockTransactions> {
        self.0.transactions()
    }

    pub fn sealed_block(&self) -> &Arc<SealedBlock> {
        &self.0
    }
}

impl Clone for BlockTreeEntry {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl PartialEq for BlockTreeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.block_hash() == other.block_hash()
    }
}

impl Eq for BlockTreeEntry {}

#[derive(Debug)]
pub struct BlockTree {
    // Main block storage
    pub blocks: HashMap<BlockHash, BlockMetadata>,

    // Track solutions -> block hashes
    solutions: HashMap<H256, HashSet<BlockHash>>,

    // Current tip
    pub tip: BlockHash,

    // Track max cumulative difficulty
    max_cumulative_difficulty: (U256, BlockHash), // (difficulty, block_hash)

    // Height -> Hash mapping
    height_index: BTreeMap<u64, HashSet<BlockHash>>,

    // Cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    longest_chain_cache: (Vec<BlockTreeEntry>, usize),

    // Consensus configuration containing cache depth
    consensus_config: ConsensusConfig,
}

#[derive(Debug)]
pub struct BlockMetadata {
    pub block: Arc<SealedBlock>,
    pub chain_state: ChainState,
    pub timestamp: SystemTime,
    pub children: HashSet<H256>,
    pub epoch_snapshot: Arc<EpochSnapshot>,
    pub commitment_snapshot: Arc<CommitmentSnapshot>,
    pub ema_snapshot: Arc<EmaSnapshot>,
}

/// Represents the `ChainState` of a block, is it Onchain? or a valid fork?
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ChainState {
    /// Block is confirmed (by another block) and part of the main chain
    Onchain,
    /// Block is Validated but may not be on the main chain
    /// (used for blocks that are extending a fork / non canonical chain)
    Validated(BlockState),
    /// Block exists but is not conformed by any other block
    NotOnchain(BlockState),
}

/// Represents the validation state of a block, independent of its `ChainState`
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BlockState {
    /// Initial state, validation not yet started
    Unknown,
    /// Validation has been requested but not completed
    ValidationScheduled,
    /// Block has passed all validation checks
    ValidBlock,
}

/// The result of marking a new tip for the canonical chain
#[derive(Debug, Clone)]
pub enum TipChangeResult {
    NoChange,
    Extension,
    Reorg {
        orphaned_blocks: Vec<BlockHash>,
        fork_height: u64,
    },
}

impl BlockTree {
    /// Create a new cache initialized with a starting block. The block is marked as
    /// on-chain and set as the tip. Only used in testing that doesn't intersect
    /// the commitment snapshot so it stubs one out
    #[cfg(any(feature = "test-utils", test))]
    pub fn new(genesis_block_header: &IrysBlockHeader, consensus_config: ConsensusConfig) -> Self {
        use crate::dummy_epoch_snapshot;

        let block_hash = genesis_block_header.block_hash;
        let solution_hash = genesis_block_header.solution_hash;
        let height = genesis_block_header.height;
        let cumulative_diff = genesis_block_header.cumulative_diff;

        let mut blocks = HashMap::new();
        let mut solutions = HashMap::new();
        let mut height_index = BTreeMap::new();

        // Create a dummy commitment snapshot
        let commitment_snapshot = Arc::new(CommitmentSnapshot::default());

        // Create EMA cache for genesis block
        let ema_snapshot = EmaSnapshot::genesis(genesis_block_header);

        // Create SealedBlock for genesis (empty body since genesis has no transactions)
        let genesis_body = BlockBody {
            block_hash,
            ..Default::default()
        };
        let sealed_genesis = Arc::new(
            SealedBlock::new(genesis_block_header.clone(), genesis_body)
                .expect("genesis block must seal successfully"),
        );

        // Initialize longest chain cache to contain the genesis block
        let entry = make_block_tree_entry(Arc::clone(&sealed_genesis));
        let longest_chain_cache = (vec![(entry)], 0);

        // Create initial block entry for genesis block, marking it as confirmed
        // and part of the canonical chain

        let block_entry = BlockMetadata {
            block: sealed_genesis,
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
            epoch_snapshot: dummy_epoch_snapshot(),
            commitment_snapshot,
            ema_snapshot,
        };

        // Initialize all indices
        blocks.insert(block_hash, block_entry);
        solutions.insert(solution_hash, HashSet::from([block_hash]));
        height_index.insert(height, HashSet::from([block_hash]));

        Self {
            blocks,
            solutions,
            tip: block_hash,
            max_cumulative_difficulty: (cumulative_diff, block_hash),
            height_index,
            longest_chain_cache,
            consensus_config,
        }
    }

    /// Restores the block tree cache from the database and `block_index` during startup.
    ///
    /// Rebuilds the block tree by loading the most recent blocks from the database (up to
    /// `block_tree_depth` blocks). The restoration process involves:
    ///
    /// 1. **Determines block range**: Calculates start/end positions based on cache depth
    /// 2. **Replays epoch history**: Initializes genesis epoch snapshot and replays historical
    ///    epoch transitions to reconstruct epoch state at the start block
    /// 3. **Loads start block**: Creates the initial block entry with commitment, epoch and EMA snapshots
    /// 4. **Processes remaining blocks**: For each subsequent block:
    ///    - Loads commitment transactions from database
    ///    - Creates EMA snapshot based on previous block
    ///    - Handles epoch transitions for epoch blocks or accumulates commitments for mid-epoch blocks
    ///    - Creates commitment snapshot incorporating new transactions
    ///    - Adds block to cache with all associated snapshots
    /// 5. **Sets chain tip**: Marks the latest block as tip and notifies Reth service
    ///
    /// ## Arguments
    /// * `block_index_guard` - Read guard for accessing the block index
    /// * `epoch_replay_data` - Genesis state and historical epoch blocks for state reconstruction
    /// * `reth_service_actor` - Actor handle for sending fork choice updates to Reth
    /// * `db` - Database provider for querying block and transaction data
    /// * `storage_submodules_config` - Storage configuration for epoch snapshot creation (only ever used with `map_storage_modules_to_partition_assignments()`, could probably be refactored out)
    /// * `config` - Full node configuration including consensus and epoch settings
    ///
    /// ## Returns
    /// Fully initialized block tree cache with all snapshots ready for use (Self)
    ///
    /// ## Errors
    /// Returns an error if the block index is empty or if database queries fail unexpectedly
    pub fn restore_from_db(
        block_index_guard: BlockIndexReadGuard,
        epoch_replay_data: EpochReplayData,
        db: DatabaseProvider,
        storage_submodules_config: &StorageSubmodulesConfig,
        config: Config,
    ) -> eyre::Result<Self> {
        let consensus_config = &config.consensus;
        // Extract block range and start block info
        let (start, end, start_block_hash) = {
            let block_index = block_index_guard.read();
            eyre::ensure!(block_index.num_blocks() > 0, "Block list must not be empty");

            let start = block_index
                .num_blocks()
                .saturating_sub(consensus_config.block_tree_depth - 1);
            let end = block_index.num_blocks();
            let start_block_hash = block_index
                .get_item(start)
                .ok_or_else(|| eyre::eyre!("missing block index entry at start height {}", start))?
                .block_hash;
            (start, end, start_block_hash)
        };

        // Initialize epoch snapshot from genesis state to establish the baseline
        // for replaying historical epoch transitions up to our target block
        let mut epoch_snapshot = EpochSnapshot::new(
            storage_submodules_config,
            epoch_replay_data.genesis_block_header,
            epoch_replay_data.genesis_commitments,
            &config,
        );

        // Filter epoch blocks to only include those at or below our start height,
        // ensuring we don't replay beyond our intended starting point
        let mut epoch_blocks = epoch_replay_data.epoch_blocks;
        epoch_blocks.retain(|epoch_block_data| epoch_block_data.epoch_block.height <= start);

        // Replay filtered epoch transitions to reconstruct the epoch state
        // right before the start block height
        epoch_snapshot
            .replay_epoch_data(epoch_blocks)
            .expect("epoch_replay_data to be replayed");
        let arc_epoch_snapshot = Arc::new(epoch_snapshot);

        let tx = db
            .tx()
            .map_err(|e| eyre::eyre!("failed to open db tx for restore_from_db: {}", e))?;
        // note: we need the poa chunk so that it's available in the tree for gossip purposes
        let start_block = block_header_by_hash(&tx, &start_block_hash, true)
            .map_err(|e| eyre::eyre!("db error loading start block {}: {}", start_block_hash, e))?
            .ok_or_else(|| {
                eyre::eyre!("start block header not found for hash {}", start_block_hash)
            })?;

        debug!(
            "block tree start block - hash: {} height: {}",
            start_block_hash, start_block.height
        );

        // Load transactions for the start block from DB and construct SealedBlock
        let start_block_commitment_txs = load_commitment_transactions(&start_block, &db)?;
        let start_block_data_txs = load_data_transactions(&start_block, &db)?;
        let start_block_body = BlockBody {
            block_hash: start_block.block_hash,
            data_transactions: start_block_data_txs.into_values().flatten().collect(),
            commitment_transactions: start_block_commitment_txs,
        };
        let sealed_start_block = Arc::new(SealedBlock::new(start_block.clone(), start_block_body)?);

        // Initialize cache with start block
        let entry = make_block_tree_entry(Arc::clone(&sealed_start_block));
        let mut block_tree_cache = Self {
            blocks: HashMap::new(),
            solutions: HashMap::new(),
            tip: start_block_hash,
            max_cumulative_difficulty: (start_block.cumulative_diff, start_block_hash),
            height_index: BTreeMap::new(),
            longest_chain_cache: (vec![entry], 0),
            consensus_config: consensus_config.clone(),
        };

        // Initialize commitment snapshot and add start block
        let mut commitment_snapshot: CommitmentSnapshot =
            build_current_commitment_snapshot_from_index(
                block_index_guard.clone(),
                start_block_hash,
                arc_epoch_snapshot.clone(),
                db.clone(),
                consensus_config,
            );

        let arc_commitment_snapshot = Arc::new(commitment_snapshot.clone());

        // Create EMA cache for start block
        let ema_snapshot =
            build_current_ema_snapshot_from_index(&start_block, db.clone(), consensus_config);

        // Create a Block Entry for the start block
        let block_entry = BlockMetadata {
            block: sealed_start_block,
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
            epoch_snapshot: arc_epoch_snapshot,
            commitment_snapshot: arc_commitment_snapshot.clone(),
            ema_snapshot: ema_snapshot.clone(),
        };

        block_tree_cache
            .blocks
            .insert(start_block_hash, block_entry);
        block_tree_cache
            .solutions
            .insert(start_block.solution_hash, HashSet::from([start_block_hash]));
        block_tree_cache
            .height_index
            .insert(start_block.height, HashSet::from([start_block_hash]));

        let mut prev_commitment_snapshot = arc_commitment_snapshot;
        let mut prev_ema_snapshot = ema_snapshot;
        let mut prev_block = start_block;

        // Process remaining blocks
        for block_height in (start + 1)..end {
            let block_hash = {
                let block_index = block_index_guard.read();
                block_index.get_item(block_height).unwrap().block_hash
            };

            // note: we need the poa chunk so that it's available in the tree for gossip purposes
            let block = block_header_by_hash(&tx, &block_hash, true)
                .unwrap()
                .unwrap();

            // Load commitment transactions (from DB during startup)
            let commitment_txs =
                load_commitment_transactions(&block, &db).expect("to load transactions from db");

            let arc_ema_snapshot = prev_ema_snapshot
                .next_snapshot(&block, &prev_block, consensus_config)
                .expect("failed to create EMA snapshot");

            // Start with the previous block's epoch snapshot as the baseline
            let parent_block_entry = block_tree_cache.blocks.get(&prev_block.block_hash).unwrap();
            let is_epoch_block = block.height % consensus_config.epoch.num_blocks_in_epoch == 0;

            let epoch_snapshot = if is_epoch_block {
                // Epoch boundary reached: create a fresh epoch snapshot that incorporates
                // all commitments from the commitment cache and computes the epoch state for the new epoch
                create_epoch_snapshot_for_block(&block, parent_block_entry, consensus_config)?
            } else {
                // Just copy the previous blocks epoch_snapshot reference
                let epoch_snapshot = parent_block_entry.epoch_snapshot.clone();
                // Mid-epoch blocks: accumulate new commitment transactions into the existing
                // commitment snapshot without triggering epoch state transitions
                for commitment_tx in &commitment_txs {
                    let _status =
                        commitment_snapshot.add_commitment(commitment_tx, &epoch_snapshot);
                    // TODO: we should be asserting the status here... should it always be `Accepted`??
                }

                epoch_snapshot
            };

            // Create commitment snapshot for this block
            let arc_commitment_snapshot = create_commitment_snapshot_for_block(
                &block,
                &commitment_txs,
                &prev_commitment_snapshot,
                epoch_snapshot.clone(),
                consensus_config,
            );

            // Load data transactions for this block from DB and construct SealedBlock
            let data_txs = load_data_transactions(&block, &db)?;
            let block_body = BlockBody {
                block_hash: block.block_hash,
                data_transactions: data_txs.into_values().flatten().collect(),
                commitment_transactions: commitment_txs.clone(),
            };
            let sealed_block = Arc::new(SealedBlock::new(block.clone(), block_body)?);

            prev_commitment_snapshot = arc_commitment_snapshot.clone();
            prev_ema_snapshot = arc_ema_snapshot.clone();
            prev_block = block.clone();
            block_tree_cache
                .add_common(
                    block.block_hash,
                    &sealed_block,
                    arc_commitment_snapshot,
                    epoch_snapshot,
                    arc_ema_snapshot,
                    ChainState::Validated(BlockState::ValidBlock),
                )
                .unwrap();
        }

        let tip_hash = {
            let block_index = block_index_guard.read();
            block_index.get_latest_item().unwrap().block_hash
        };

        block_tree_cache.mark_tip(&tip_hash).map_err(|e| {
            eyre::eyre!(
                "failed to mark tip {} during restore_from_db: {}",
                tip_hash,
                e
            )
        })?;

        // TODO: if we restart when we have a very large amount of blocks,
        // then actually we would be storing almost all of the block index in memory -
        // this is because pruning is only called at the very end

        // Prune the cache after restoration to ensure correct depth
        // Subtract 1 to ensure we keep exactly `depth` blocks.
        // The cache.prune() implementation does not count `tip` into the depth
        // equation, so it's always tip + `depth` that's kept around
        block_tree_cache.prune(consensus_config.block_tree_depth.saturating_sub(1));

        Ok(block_tree_cache)
    }

    pub fn add_common(
        &mut self,
        hash: BlockHash,
        block: &Arc<SealedBlock>,
        commitment_snapshot: Arc<CommitmentSnapshot>,
        epoch_snapshot: Arc<EpochSnapshot>,
        ema_snapshot: Arc<EmaSnapshot>,
        chain_state: ChainState,
    ) -> eyre::Result<()> {
        let prev_hash = block.header().previous_block_hash;

        // Get parent
        let prev_entry = self
            .blocks
            .get_mut(&prev_hash)
            .ok_or_else(|| eyre::eyre!("Previous block not found"))?;

        // Update indices
        prev_entry.children.insert(hash);
        self.solutions
            .entry(block.header().solution_hash)
            .or_default()
            .insert(hash);
        self.height_index
            .entry(block.header().height)
            .or_default()
            .insert(hash);

        debug!(
            "adding block: max_cumulative_difficulty: {} block.cumulative_diff: {} {}, commitment_snapshot_hash: {}, epoch_snapshot_hash: {}, ema_snapshot_hash: {}",
            self.max_cumulative_difficulty.0,
            block.header().cumulative_diff,
            block.header().block_hash,
            &commitment_snapshot.get_hash(),
            &epoch_snapshot.get_hash(),
            &ema_snapshot.get_hash()
        );

        if block.header().cumulative_diff > self.max_cumulative_difficulty.0 {
            debug!(
                "setting max_cumulative_difficulty ({}, {}) for height: {}",
                block.header().cumulative_diff,
                hash,
                block.header().height
            );
            self.max_cumulative_difficulty = (block.header().cumulative_diff, hash);
        }

        self.blocks.insert(
            hash,
            BlockMetadata {
                block: Arc::clone(block),
                chain_state,
                timestamp: SystemTime::now(),
                children: HashSet::new(),
                epoch_snapshot,
                commitment_snapshot,
                ema_snapshot,
            },
        );

        self.update_longest_chain_cache();
        Ok(())
    }

    /// Adds a block to the block tree.
    ///
    /// Peer blocks undergo strict validation before acceptance:
    /// 1. **Full validation sequence** - Full validation rules must be run and pass
    /// 2. **Confirmation required** - Block must be confirmed by another block building on it
    /// 3. **Block Index Migration** - Only then is the block added to the block_index
    pub fn add_block(
        &mut self,
        block: &Arc<SealedBlock>,
        commitment_snapshot: Arc<CommitmentSnapshot>,
        epoch_snapshot: Arc<EpochSnapshot>,
        ema_snapshot: Arc<EmaSnapshot>,
    ) -> eyre::Result<()> {
        let hash = block.header().block_hash;

        debug!(
            "add_block() - {} height: {}",
            block.header().block_hash,
            block.header().height
        );

        if matches!(
            self.blocks.get(&hash).map(|b| b.chain_state),
            Some(ChainState::Onchain)
        ) {
            debug!(block.hash = ?hash, "already part of the main chian state");
            return Ok(());
        }

        self.add_common(
            hash,
            block,
            commitment_snapshot,
            epoch_snapshot,
            ema_snapshot,
            ChainState::NotOnchain(BlockState::Unknown),
        )
    }

    /// Helper function to delete a single block without recursion
    fn delete_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let solution_hash = block_entry.block.header().solution_hash;
        let height = block_entry.block.header().height;
        let prev_hash = block_entry.block.header().previous_block_hash;

        // Update parent's children set
        if let Some(prev_entry) = self.blocks.get_mut(&prev_hash) {
            prev_entry.children.remove(block_hash);
        }

        // Update height index
        if let Some(height_set) = self.height_index.get_mut(&height) {
            height_set.remove(block_hash);
            if height_set.is_empty() {
                self.height_index.remove(&height);
            }
        }

        // Update solutions map
        if let Some(solutions) = self.solutions.get_mut(&solution_hash) {
            solutions.remove(block_hash);
            if solutions.is_empty() {
                self.solutions.remove(&solution_hash);
            }
        }

        // Remove the block
        self.blocks.remove(block_hash);

        // Update max_cumulative_difficulty if necessary
        if self.max_cumulative_difficulty.1 == *block_hash {
            self.max_cumulative_difficulty = self.find_max_difficulty();
        }

        self.update_longest_chain_cache();
        Ok(())
    }

    #[cfg(feature = "test-utils")]
    pub fn test_delete(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        self.delete_block(block_hash)
    }

    /// Removes a block and all its descendants recursively
    pub fn remove_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        // Get children before deleting the block
        let children = self
            .blocks
            .get(block_hash)
            .map(|entry| entry.children.iter().copied().collect::<Vec<_>>())
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        // Recursively remove all children first
        for child in children {
            self.remove_block(&child)?;
        }

        // Delete this block
        self.delete_block(block_hash)
    }

    // Helper to find new max difficulty when current max is removed
    fn find_max_difficulty(&self) -> (U256, BlockHash) {
        self.blocks
            .iter()
            .map(|(hash, entry)| (entry.block.header().cumulative_diff, *hash))
            .max_by_key(|(diff, _)| *diff)
            .unwrap_or((U256::zero(), BlockHash::default()))
    }

    /// Returns the canonical chain as a cached sequence of block entries.
    ///
    /// The canonical chain represents the longest valid chain from the earliest cached block
    /// to the current tip. The chain is maintained as a cached tuple containing:
    /// - **Block entries**: Ordered sequence from oldest to newest block in cache
    /// - **Non-onchain count**: Number of blocks not yet fully validated
    ///
    /// ## Canonical Chain Structure
    /// * **First element**: Genesis block or the oldest block within `block_tree_depth`
    /// * **Last element**: Current chain tip (highest cumulative difficulty)
    /// * **Ordering**: Chronological from oldest to newest block
    ///
    /// ## Returns
    /// `(Vec<BlockTreeEntry>, usize)` - Tuple of (block entries, count of non-onchain blocks)
    ///
    /// ## Note
    /// This returns a cloned copy of the cached chain for thread-safe access. The cache
    /// is updated whenever the canonical chain changes due to new blocks or reorganizations.
    #[must_use]
    pub fn get_canonical_chain(&self) -> (Vec<BlockTreeEntry>, usize) {
        self.longest_chain_cache.clone()
    }

    #[must_use]
    pub fn get_latest_canonical_entry(&self) -> &BlockTreeEntry {
        self.longest_chain_cache
            .0
            .last()
            .expect("canonical chain must always have an entry in it")
    }

    fn update_longest_chain_cache(&mut self) {
        let pairs = {
            self.longest_chain_cache.0.clear();
            &mut self.longest_chain_cache.0
        };
        let mut not_onchain_count = 0;

        let mut current = self.max_cumulative_difficulty.1;
        let mut blocks_to_collect = self.consensus_config.block_tree_depth;
        debug!(
            "updating canonical chain cache latest_cache_tip: {}",
            current
        );

        while let Some(entry) = self.blocks.get(&current) {
            match &entry.chain_state {
                // For blocks awaiting initial validation, restart chain from parent
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled) => {
                    // Reset everything and continue from parent block
                    pairs.clear();
                    not_onchain_count = 0;
                    current = entry.block.header().previous_block_hash;
                    blocks_to_collect = self.consensus_config.block_tree_depth;
                    continue;
                }

                ChainState::Onchain => {
                    // Include OnChain blocks in pairs
                    let chain_cache_entry = make_block_tree_entry(Arc::clone(&entry.block));
                    pairs.push(chain_cache_entry);

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                ChainState::Validated(_) => {
                    let chain_cache_entry = make_block_tree_entry(Arc::clone(&entry.block));
                    pairs.push(chain_cache_entry);

                    not_onchain_count += 1;

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                ChainState::NotOnchain(block_state) => {
                    let chain_cache_entry = make_block_tree_entry(Arc::clone(&entry.block));
                    pairs.push(chain_cache_entry);

                    // We only count this as not onchain if it's validated
                    if *block_state == BlockState::ValidBlock {
                        not_onchain_count += 1;
                    }

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }
            }

            if entry.block.header().height == 0 {
                break;
            } else {
                current = entry.block.header().previous_block_hash;
            }
        }

        pairs.reverse();
        self.longest_chain_cache.1 = not_onchain_count;
    }

    /// Helper to mark off-chain blocks in a set
    fn mark_off_chain(&mut self, children: HashSet<H256>, current: &BlockHash) {
        for child in children {
            if child == *current {
                continue;
            }
            if let Some(entry) = self.blocks.get_mut(&child)
                && matches!(entry.chain_state, ChainState::Onchain)
            {
                entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                // Recursively mark children of this block
                let children = entry.children.clone();
                self.mark_off_chain(children, current);
            }
        }
    }

    /// Helper to recursively mark blocks on-chain
    fn mark_on_chain(&mut self, block_header: &IrysBlockHeader) -> eyre::Result<()> {
        let prev_hash = block_header.previous_block_hash;

        match self.blocks.get(&prev_hash) {
            None => Ok(()), // Reached the end
            Some(prev_entry) => {
                let prev_header = prev_entry.block.header().clone();
                let prev_children = prev_entry.children.clone();

                match prev_entry.chain_state {
                    ChainState::Onchain => {
                        // Mark other branches as not onchain (but preserve their validation state)
                        self.mark_off_chain(prev_children, &block_header.block_hash);
                        Ok(())
                    }
                    ChainState::NotOnchain(BlockState::ValidBlock) | ChainState::Validated(_) => {
                        // Update previous block to on_chain
                        if let Some(entry) = self.blocks.get_mut(&prev_hash) {
                            entry.chain_state = ChainState::Onchain;
                        }
                        // Recursively mark previous blocks
                        self.mark_on_chain(&prev_header)
                    }
                    ChainState::NotOnchain(_) => Err(eyre::eyre!("invalid_tip")),
                }
            }
        }
    }

    /// Marks a block as the new tip
    pub fn mark_tip(&mut self, block_hash: &BlockHash) -> eyre::Result<bool> {
        debug!("mark_tip({})", block_hash);
        // Get the current block
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found in cache"))?;

        let block_header = block_entry.block.header().clone();
        let old_tip = self.tip;

        let (canonical_diff, canonical_block_hash) = self.max_cumulative_difficulty;

        if block_header.cumulative_diff == canonical_diff && canonical_block_hash != *block_hash {
            // "Cannot move tip away from canonical for another block with same cumulative_diff"
            return Ok(false);
        }

        // Recursively mark previous blocks
        self.mark_on_chain(&block_header)?;

        // Mark the tip block as on_chain
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            entry.chain_state = ChainState::Onchain;
        }

        self.tip = *block_hash;
        self.update_longest_chain_cache();

        debug!(
            "\u{001b}[32mmark tip: hash:{} height: {}\u{001b}[0m",
            block_hash, block_header.height
        );

        Ok(old_tip != *block_hash)
    }

    pub fn mark_block_as_validation_scheduled(
        &mut self,
        block_hash: &BlockHash,
    ) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if entry.chain_state == ChainState::NotOnchain(BlockState::Unknown) {
                entry.chain_state = ChainState::NotOnchain(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            } else if entry.chain_state == ChainState::Validated(BlockState::Unknown) {
                entry.chain_state = ChainState::Validated(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    pub fn mark_block_as_valid(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            match entry.chain_state {
                ChainState::NotOnchain(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::NotOnchain(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                // When we add blocks to the block tree that we produced locally, they are added
                // as ChainState::Validated but we can still schedule them for full validation
                // with their BlockState.
                ChainState::Validated(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                _ => Err(eyre::eyre!(
                    "unable to mark block as valid: chain_state {:?} {}",
                    entry.chain_state,
                    entry.block.header().block_hash,
                )),
            }
        } else {
            Err(eyre::eyre!(
                "unable to mark block as valid: block not found"
            ))
        }
    }

    /// Gets block by hash
    #[must_use]
    pub fn get_block(&self, block_hash: &BlockHash) -> Option<&IrysBlockHeader> {
        self.blocks
            .get(block_hash)
            .map(|entry| -> &IrysBlockHeader { entry.block.header() })
    }

    /// Gets the sealed block (header + body) by hash
    #[must_use]
    pub fn get_sealed_block(&self, block_hash: &BlockHash) -> Option<Arc<SealedBlock>> {
        self.blocks
            .get(block_hash)
            .map(|entry| Arc::clone(&entry.block))
    }

    /// Get the N most recent blocks walking backwards from the tip.
    #[must_use]
    pub fn get_recent_blocks_from_tip(&self, count: usize) -> Vec<&IrysBlockHeader> {
        let mut blocks = Vec::new();
        let mut current_hash = self.tip;

        while blocks.len() < count {
            let block = match self.get_block(&current_hash) {
                Some(b) => b,
                None => break,
            };

            blocks.push(block);
            current_hash = block.previous_block_hash;
        }

        blocks.reverse(); // oldest-first
        blocks
    }

    pub fn canonical_commitment_snapshot(&self) -> Arc<CommitmentSnapshot> {
        let head_entry = self
            .longest_chain_cache
            .0
            .last()
            .expect("at least one block in the longest chain");

        self.blocks
            .get(&head_entry.block_hash())
            .expect("commitment snapshot for block")
            .commitment_snapshot
            .clone()
    }

    pub fn canonical_epoch_snapshot(&self) -> Arc<EpochSnapshot> {
        let head_entry = self
            .longest_chain_cache
            .0
            .last()
            .expect("at least one block in the longest chain");

        self.blocks
            .get(&head_entry.block_hash())
            .expect("commitment snapshot for block")
            .epoch_snapshot
            .clone()
    }

    pub fn get_commitment_snapshot(
        &self,
        block_hash: &BlockHash,
    ) -> eyre::Result<Arc<CommitmentSnapshot>> {
        match self.blocks.get(block_hash) {
            Some(entry) => Ok(entry.commitment_snapshot.clone()),
            None => Err(eyre::eyre!("Block not found: {}", block_hash)),
        }
    }

    /// Get EMA cache for a specific block
    pub fn get_ema_snapshot(&self, block_hash: &BlockHash) -> Option<Arc<EmaSnapshot>> {
        self.blocks
            .get(block_hash)
            .map(|entry| entry.ema_snapshot.clone())
    }

    pub fn get_epoch_snapshot(&self, block_hash: &BlockHash) -> Option<Arc<EpochSnapshot>> {
        self.blocks
            .get(block_hash)
            .map(|entry| entry.epoch_snapshot.clone())
    }

    /// Returns the current possible set of candidate hashes for a given height.
    pub fn get_hashes_for_height(&self, height: u64) -> Option<&HashSet<BlockHash>> {
        self.height_index.get(&height)
    }

    /// Get the block with maximum cumulative difficulty
    pub fn get_max_cumulative_difficulty_block(&self) -> (U256, BlockHash) {
        self.max_cumulative_difficulty
    }

    /// Returns max-difficulty block info: (hash, height, can_build_upon)
    ///
    /// # Panics
    /// Panics if the max difficulty block is not in the tree (invariant violation).
    pub fn get_max_block_info(&self) -> (BlockHash, u64, bool) {
        let (_, hash) = self.max_cumulative_difficulty;
        let entry = self
            .blocks
            .get(&hash)
            .expect("max difficulty block must exist in tree");
        (
            hash,
            entry.block.header().height,
            self.can_be_built_upon(&hash),
        )
    }

    /// Check if a block can be built upon
    pub fn can_be_built_upon(&self, block_hash: &BlockHash) -> bool {
        self.blocks.get(block_hash).is_some_and(|entry| {
            matches!(
                entry.chain_state,
                ChainState::Onchain | ChainState::Validated(BlockState::ValidBlock)
            )
        })
    }

    /// Gets block and its current validation status
    #[must_use]
    pub fn get_block_and_status(
        &self,
        block_hash: &BlockHash,
    ) -> Option<(&IrysBlockHeader, &ChainState)> {
        self.blocks
            .get(block_hash)
            .map(|entry| -> (&IrysBlockHeader, &ChainState) {
                (entry.block.header(), &entry.chain_state)
            })
    }

    /// Walk backwards from a block to find the validation status of the chain
    /// Returns: (blocks_awaiting_validation, last_validated_block_hash)
    pub fn get_validation_chain_status(
        &self,
        from_block_hash: &BlockHash,
    ) -> (usize, Option<BlockHash>) {
        let mut current_hash = *from_block_hash;
        let mut blocks_awaiting = 0;
        let mut last_validated = None;

        while let Some(entry) = self.blocks.get(&current_hash) {
            match entry.chain_state {
                ChainState::Onchain | ChainState::Validated(BlockState::ValidBlock) => {
                    // Found a validated block
                    if last_validated.is_none() {
                        last_validated = Some(current_hash);
                    }
                }
                ChainState::NotOnchain(BlockState::ValidBlock) => {
                    // This block passed validation but isn't on chain yet
                    if last_validated.is_none() {
                        blocks_awaiting += 1;
                    }
                }
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled)
                | ChainState::Validated(BlockState::Unknown | BlockState::ValidationScheduled) => {
                    // This block is awaiting validation
                    blocks_awaiting += 1;
                }
            }

            if entry.block.header().height == 0 {
                break; // Reached genesis
            }

            current_hash = entry.block.header().previous_block_hash;
        }

        (blocks_awaiting, last_validated)
    }

    /// Collect previous blocks up to the last on-chain block
    pub fn get_fork_blocks(&self, previous_block_hash: BlockHash) -> Vec<Arc<SealedBlock>> {
        let mut prev_hash = previous_block_hash;
        let mut fork_blocks: Vec<Arc<SealedBlock>> = Vec::new();

        while let Some(entry) = self.blocks.get(&prev_hash) {
            match entry.chain_state {
                ChainState::Onchain => {
                    fork_blocks.push(Arc::clone(&entry.block));
                    break;
                }
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    fork_blocks.push(Arc::clone(&entry.block));
                    prev_hash = entry.block.header().previous_block_hash;
                }
            }
        }

        fork_blocks.reverse();
        fork_blocks
    }

    /// Finds the earliest not validated block, walking back the chain
    /// until finding a validated block, reaching block height 0, or exceeding cache depth
    #[must_use]
    fn get_earliest_not_onchain<'a>(
        &'a self,
        block: &'a BlockMetadata,
    ) -> Option<(&'a BlockMetadata, Vec<Arc<SealedBlock>>, SystemTime)> {
        let mut current_entry = block;
        let mut prev_header: &IrysBlockHeader = current_entry.block.header();
        let mut depth_count = 0;

        while prev_header.height > 0 && depth_count < self.consensus_config.block_tree_depth {
            let prev_hash = prev_header.previous_block_hash;
            let prev_entry = self.blocks.get(&prev_hash)?;
            debug!(
                "get_earliest_not_onchain: prev_entry.chain_state: {:?} {} height: {}",
                prev_entry.chain_state,
                prev_hash,
                prev_entry.block.header().height
            );
            match prev_entry.chain_state {
                ChainState::Validated(BlockState::ValidBlock) | ChainState::Onchain => {
                    return Some((
                        current_entry,
                        self.get_fork_blocks(prev_header.previous_block_hash),
                        current_entry.timestamp,
                    ));
                }
                ChainState::NotOnchain(_) | ChainState::Validated(_) => {
                    current_entry = prev_entry;
                    prev_header = current_entry.block.header();
                    depth_count += 1;
                }
            }
        }

        // If we've reached height 0 or exceeded cache depth, return None
        None
    }

    /// Get the earliest unvalidated block from the longest/canonical chain
    /// Relies on the `longest_chain_cache`
    /// TODO: rename to "...in_canonical_chain"
    /// TODO: replace with lookup from canonical chain not_on_chain_count & lookup
    #[must_use]
    pub fn get_earliest_not_onchain_in_longest_chain(
        &self,
    ) -> Option<(&BlockMetadata, Vec<Arc<SealedBlock>>, SystemTime)> {
        // Get the block with max cumulative difficulty
        let (_max_cdiff, max_diff_hash) = self.max_cumulative_difficulty;

        // Get the tip's cumulative difficulty
        let tip_entry = self.blocks.get(&self.tip)?;
        let tip_cdiff = tip_entry.block.header().cumulative_diff;

        // Check if tip's difficulty exceeds max difficulty
        if tip_cdiff >= self.max_cumulative_difficulty.0 {
            return None;
        }

        // Get the block with max difficulty
        let entry = self.blocks.get(&max_diff_hash)?;

        debug!(
            "get_earliest_not_onchain_in_longest_chain() with max_diff_hash: {} height: {} state: {:?}",
            max_diff_hash,
            entry.block.header().height,
            entry.chain_state
        );

        // Check if it's part of a fork and get the start of the fork
        if let ChainState::NotOnchain(_) | ChainState::Validated(_) = &entry.chain_state {
            self.get_earliest_not_onchain(entry)
        } else {
            None
        }
    }

    // TODO: replace with reading canonical chain, minusing head block by not on chain count
    pub fn get_earliest_unvalidated_block_height(&self) -> Option<u64> {
        // Get the block with max cumulative difficulty
        let (_entry, blocks, _time) = self.get_earliest_not_onchain_in_longest_chain()?;
        blocks
            .iter()
            .min_by_key(|b| b.header().height)
            .map(|block| block.header().height)
    }

    pub fn can_process_height(&self, height: u64) -> bool {
        let max_tree_depth = self.consensus_config.block_migration_depth as u64;
        if height < max_tree_depth {
            return true;
        }
        let earliest_unvalidated_height = self.get_earliest_unvalidated_block_height();

        earliest_unvalidated_height
            .map(|earliest_height| {
                let max_height = earliest_height + (max_tree_depth / 2);
                height <= max_height
            })
            .unwrap_or(true)
    }

    /// Gets block with matching solution hash, excluding specified block.
    /// Returns a block meeting these requirements:
    /// - Has matching `solution_hash`
    /// - Is not the excluded block
    /// - Either has same `cumulative_diff` as input or meets double-signing criteria
    #[must_use]
    pub fn get_by_solution_hash(
        &self,
        solution_hash: &H256,
        excluding: &BlockHash,
        cumulative_difficulty: U256,
        previous_cumulative_difficulty: U256,
    ) -> Option<&IrysBlockHeader> {
        // Get set of blocks with this solution hash
        let block_hashes = self.solutions.get(solution_hash)?;

        let mut best_block = None;

        // Examine each block hash
        for &hash in block_hashes {
            // Skip the excluded block
            if hash == *excluding {
                continue;
            }

            if let Some(entry) = self.blocks.get(&hash) {
                let header: &IrysBlockHeader = entry.block.header();

                // Case 1: Exact cumulative_diff match - return immediately
                if header.cumulative_diff == cumulative_difficulty {
                    return Some(header);
                }

                // Case 2: Double signing case - return immediately
                if header.cumulative_diff > previous_cumulative_difficulty
                    && cumulative_difficulty > header.previous_cumulative_diff
                {
                    return Some(header);
                }

                // Store as best block seen so far if we haven't found one yet
                if best_block.is_none() {
                    best_block = Some(header);
                }
            }
        }

        // Return best block found (if any)
        best_block
    }

    /// Prunes blocks below specified depth from tip. When pruning an on-chain block,
    /// removes all its non-on-chain children regardless of their height.
    pub fn prune(&mut self, depth: u64) {
        let tip_height = self
            .blocks
            .get(&self.tip)
            .expect("Tip block not found")
            .block
            .header()
            .height;
        let min_keep_height = tip_height.saturating_sub(depth);

        let min_height = match self.height_index.keys().min() {
            Some(&h) => h,
            None => return,
        };

        let mut current_height = min_height;
        while current_height < min_keep_height {
            let Some(hashes) = self.height_index.get(&current_height) else {
                current_height += 1;
                continue;
            };

            // Clone hashes to avoid borrow issues during removal
            let hashes: Vec<_> = hashes.iter().copied().collect();

            for hash in hashes {
                if let Some(entry) = self.blocks.get(&hash)
                    && matches!(entry.chain_state, ChainState::Onchain)
                {
                    // First remove all non-on-chain children
                    let children = entry.children.clone();
                    for child in children {
                        if let Some(child_entry) = self.blocks.get(&child)
                            && !matches!(child_entry.chain_state, ChainState::Onchain)
                        {
                            let child_state = child_entry.chain_state;
                            if let Err(err) = self.remove_block(&child) {
                                tracing::error!(
                                    custom.error = ?err,
                                    custom.child_hash = ?child,
                                    custom.child_state = ?child_state,
                                    "Failed to remove non-on-chain child block"
                                );
                            }
                        }
                    }

                    // Now remove just this block
                    if let Err(err) = self.delete_block(&hash) {
                        tracing::error!(
                            custom.error = ?err,
                            block.hash = ?hash,
                            "Failed to delete block"
                        );
                    }
                }
            }

            current_height += 1;
        }
    }

    /// Returns true if solution hash exists in cache
    #[must_use]
    pub fn is_known_solution_hash(&self, solution_hash: &H256) -> bool {
        self.solutions.contains_key(solution_hash)
    }
}

fn build_current_ema_snapshot_from_index(
    latest_block_from_index: &IrysBlockHeader,
    db: DatabaseProvider,
    config: &ConsensusConfig,
) -> Arc<EmaSnapshot> {
    let tx = db.tx().unwrap();

    // Start from the provided latest block
    let mut current_block = latest_block_from_index.clone();

    // Collect blocks by walking up the chain
    let mut chain_blocks = vec![current_block.clone()];
    let blocks_to_collect = config.ema.price_adjustment_interval * 3;

    // Walk backwards through the chain, collecting blocks
    while chain_blocks.len() < blocks_to_collect as usize && current_block.height > 0 {
        current_block = block_header_by_hash(&tx, &current_block.previous_block_hash, false)
            .unwrap()
            .expect("previous block must be in database");
        chain_blocks.push(current_block.clone());
    }

    // Reverse to have oldest blocks first
    chain_blocks.reverse();

    // Pass the collected blocks to create_ema_snapshot_from_chain_history
    create_ema_snapshot_from_chain_history(&chain_blocks, config)
        .expect("Failed to create EMA snapshot from chain history")
}

pub async fn get_optimistic_chain(tree: BlockTreeReadGuard) -> eyre::Result<Vec<(H256, u64)>> {
    let canonical_chain = tokio::task::spawn_blocking(move || {
        let cache = tree.read();

        let mut blocks_to_collect = cache.consensus_config.block_tree_depth;
        let mut chain_cache = Vec::with_capacity(
            blocks_to_collect
                .try_into()
                .expect("u64 must fit into usize"),
        );
        let mut current = cache.max_cumulative_difficulty.1;
        debug!("get_optimistic_chain with latest_cache_tip: {}", current);

        while let Some(entry) = cache.blocks.get(&current) {
            chain_cache.push((current, entry.block.header().height));

            if blocks_to_collect == 0 {
                break;
            }
            blocks_to_collect -= 1;

            if entry.block.header().height == 0 {
                break;
            } else {
                current = entry.block.header().previous_block_hash;
            }
        }

        chain_cache.reverse();
        chain_cache
    })
    .await?;
    Ok(canonical_chain)
}

/// Returns the canonical chain where the first item in the Vec is the oldest block
/// Uses spawn_blocking to prevent the read operation from blocking the async executor
/// and locking other async tasks while traversing the block tree.
/// Notably useful in single-threaded tokio based unittests.
pub async fn get_canonical_chain(
    tree: BlockTreeReadGuard,
) -> eyre::Result<(Vec<BlockTreeEntry>, usize)> {
    let canonical_chain =
        tokio::task::spawn_blocking(move || tree.read().get_canonical_chain()).await?;
    Ok(canonical_chain)
}

/// Reconstructs the commitment snapshot for the current epoch by loading all commitment
/// transactions from blocks since the last epoch boundary.
///
/// Iterates through all blocks from the first block after the most recent epoch block
/// up to the latest block, collecting and applying all commitment transactions to build
/// the current epoch's commitment state. This is typically used during startup or when
/// the commitment snapshot needs to be rebuilt from persistent storage.
///
/// # Returns
/// Initialized commitment snapshot containing all commitments from the current epoch
pub fn build_current_commitment_snapshot_from_index(
    block_index_guard: BlockIndexReadGuard,
    start_block_hash: BlockHash,
    epoch_snapshot: Arc<EpochSnapshot>,
    db: DatabaseProvider,
    consensus_config: &ConsensusConfig,
) -> CommitmentSnapshot {
    let num_blocks_in_epoch = consensus_config.epoch.num_blocks_in_epoch;
    let block_index = block_index_guard.read();

    let mut snapshot = CommitmentSnapshot::default();

    let tx = db.tx().unwrap();

    let latest = block_header_by_hash(&tx, &start_block_hash, false)
        .unwrap()
        .expect("block_index block to be in database");
    let last_epoch_block_height = latest.height - (latest.height % num_blocks_in_epoch);

    let start = last_epoch_block_height + 1;

    // Loop though all the blocks starting with the first block following the last epoch block
    for height in start..=latest.height {
        // Query each block to see if they have commitment txids
        let block_item = block_index.get_item(height).unwrap();
        let block = block_header_by_hash(&tx, &block_item.block_hash, false)
            .unwrap()
            .expect("block_index block to be in database");

        let commitment_tx_ids = block.commitment_tx_ids();
        if !commitment_tx_ids.is_empty() {
            // If so, retrieve the full commitment transactions
            for txid in commitment_tx_ids {
                let commitment_tx = commitment_tx_by_txid(&tx, txid)
                    .unwrap()
                    .expect("commitment transactions to be in database");

                // Apply them to the commitment snapshot
                let _status = snapshot.add_commitment(&commitment_tx, &epoch_snapshot);
                // TODO: we should be asserting the status here... should it always be `Accepted`??
            }
        }
    }

    // Return the initialized commitment snapshot
    snapshot
}

/// Creates a new commitment snapshot for the given block based on commitment transactions
/// and the previous commitment snapshot.
///
/// ## Behavior
/// - Returns a fresh empty snapshot if this is an epoch block (height divisible by num_blocks_in_epoch)
/// - Returns a clone of the previous snapshot if no commitment transactions are present in the new block
/// - Otherwise, creates a new commitment snapshot by adding all commitment transactions to a copy of the
/// previous snapshot
///
/// ## Arguments
/// * `block` - The block header to create a commitment snapshot for
/// * `commitment_txs` - Slice of commitment transactions to process for this block (should match txids in the block)
/// * `prev_commitment_snapshot` - The commitment snapshot from the previous block
/// * `consensus_config` - Configuration containing epoch settings
///
/// # Returns
/// Arc-wrapped commitment snapshot for the new block
pub fn create_commitment_snapshot_for_block(
    block: &IrysBlockHeader,
    commitment_txs: &[CommitmentTransaction],
    prev_commitment_snapshot: &Arc<CommitmentSnapshot>,
    epoch_snapshot: Arc<EpochSnapshot>,
    consensus_config: &ConsensusConfig,
) -> Arc<CommitmentSnapshot> {
    let is_epoch_block = block
        .height
        .is_multiple_of(consensus_config.epoch.num_blocks_in_epoch);

    if is_epoch_block {
        return Arc::new(CommitmentSnapshot::default());
    }

    if commitment_txs.is_empty() {
        return prev_commitment_snapshot.clone();
    }

    let mut new_commitment_snapshot = (**prev_commitment_snapshot).clone();
    for commitment_tx in commitment_txs {
        new_commitment_snapshot.add_commitment(commitment_tx, &epoch_snapshot);
    }
    Arc::new(new_commitment_snapshot)
}

pub fn create_epoch_snapshot_for_block(
    block: &IrysBlockHeader,
    parent_block_entry: &BlockMetadata,
    consensus_config: &ConsensusConfig,
) -> eyre::Result<Arc<EpochSnapshot>> {
    let is_epoch_block = block
        .height
        .is_multiple_of(consensus_config.epoch.num_blocks_in_epoch);

    if is_epoch_block {
        let prev_epoch_snapshot = parent_block_entry.epoch_snapshot.clone();
        let commitments = parent_block_entry
            .commitment_snapshot
            .get_epoch_commitments();
        let mut new_snapshot = (*prev_epoch_snapshot).clone();
        let prev_epoch_block = new_snapshot.epoch_block.clone();
        new_snapshot.perform_epoch_tasks(&Some(prev_epoch_block), block, commitments)?;
        Ok(Arc::new(new_snapshot))
    } else {
        Ok(parent_block_entry.epoch_snapshot.clone())
    }
}

/// Loads commitment transactions from the database for the given block's commitment ledger transaction IDs.
fn load_commitment_transactions(
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let commitment_tx_ids = block.commitment_tx_ids();
    if commitment_tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Startup: query database directly
    let mut txs = Vec::new();
    let db_tx = db.tx().expect("to create a read only tx for the db");
    for tx_id in commitment_tx_ids {
        if let Some(header) =
            commitment_tx_by_txid(&db_tx, tx_id).expect("to retrieve tx header from db")
        {
            txs.push(header);
        }
    }

    txs.sort();

    Ok(txs)
}

/// Loads data transactions from the database for the given block's data ledger transaction IDs.
fn load_data_transactions(
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> eyre::Result<HashMap<DataLedger, Vec<DataTransactionHeader>>> {
    let data_tx_ids_by_ledger = block.get_data_ledger_tx_ids();
    if data_tx_ids_by_ledger.is_empty() {
        return Ok(HashMap::new());
    }

    let mut data_txs: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
    let db_tx = db.tx().expect("to create a read only tx for the db");

    for (ledger, tx_ids) in data_tx_ids_by_ledger {
        let mut ledger_txs = Vec::new();
        for tx_id in tx_ids {
            let header = tx_header_by_txid(&db_tx, &tx_id)
                .expect("to retrieve tx header from db")
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Missing data transaction in DB: tx_id={}, ledger={:?}. \
                        This may indicate DB corruption.",
                        tx_id,
                        ledger
                    )
                })?;
            ledger_txs.push(header);
        }
        if !ledger_txs.is_empty() {
            data_txs.insert(ledger, ledger_txs);
        }
    }

    Ok(data_txs)
}

pub fn make_block_tree_entry(block: Arc<SealedBlock>) -> BlockTreeEntry {
    BlockTreeEntry(block)
}

#[cfg(test)]
mod tests {

    use crate::{EmaSnapshot, dummy_epoch_snapshot};

    use super::*;
    use assert_matches::assert_matches;
    use eyre::ensure;
    use irys_testing_utils::IrysBlockHeaderTestExt as _;

    /// Creates a SealedBlock from a header with an empty body (no transactions).
    /// Signs the header if the signature is invalid (e.g., after modifying fields).
    fn seal_block(header: &mut IrysBlockHeader) -> Arc<SealedBlock> {
        header.ensure_test_signed();
        let body = BlockBody {
            block_hash: header.block_hash,
            ..Default::default()
        };
        Arc::new(SealedBlock::new(header.clone(), body).expect("sealing block with empty body"))
    }

    /// Creates a properly signed data transaction for testing.
    fn create_signed_data_tx() -> DataTransactionHeader {
        use irys_types::irys::IrysSigner;
        use irys_types::transaction::IrysTransactionCommon as _;

        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        DataTransactionHeader::new(&config)
            .sign(&signer)
            .expect("signing test tx")
    }

    /// Creates a SealedBlock from a header with matching data transactions in the body.
    /// Signs the header if the signature is invalid (e.g., after adding tx_ids).
    fn seal_block_with_data_txs(
        header: &mut IrysBlockHeader,
        data_txs: Vec<DataTransactionHeader>,
    ) -> Arc<SealedBlock> {
        header.ensure_test_signed();
        let body = BlockBody {
            block_hash: header.block_hash,
            data_transactions: data_txs,
            commitment_transactions: vec![],
        };
        Arc::new(SealedBlock::new(header.clone(), body).expect("sealing block with txs"))
    }

    fn dummy_ema_snapshot() -> Arc<EmaSnapshot> {
        let config = irys_types::ConsensusConfig::testing();
        let genesis_header = IrysBlockHeader::V1(irys_types::IrysBlockHeaderV1 {
            oracle_irys_price: config.genesis.genesis_price,
            ema_irys_price: config.genesis.genesis_price,
            ..Default::default()
        });
        EmaSnapshot::genesis(&genesis_header)
    }

    #[tokio::test]
    async fn test_block_cache() {
        let b1 = random_block(U256::from(0));

        // For the purposes of these tests, the block cache will not track transaction headers
        let comm_cache = Arc::new(CommitmentSnapshot::default());

        // Initialize block tree cache from `b1`
        let mut cache = BlockTree::new(&b1, ConsensusConfig::testing());

        // Verify cache returns `None` for unknown hashes
        assert_eq!(cache.get_block(&H256::random()), None);
        assert_eq!(
            cache.get_by_solution_hash(&H256::random(), &H256::random(), U256::one(), U256::one()),
            None
        );

        // Verify cache returns the expected block
        assert_eq!(cache.get_block(&b1.block_hash), Some(&b1));
        assert_eq!(
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &H256::random(),
                U256::one(),
                U256::one()
            ),
            Some(&b1)
        );

        // Verify getting by `solution_hash` excludes the expected block
        assert_matches!(
            cache.get_by_solution_hash(&b1.solution_hash, &b1.block_hash, U256::one(), U256::one()),
            None
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Not sure how much sense this test makes now - it's impossible to
        // have a different block with the same hash
        // Re-adding `b1` shouldn't change the state because it is
        // confirmed onchain
        let mut b1_dup = b1.clone();
        let sealed_b1_dup = seal_block(&mut b1_dup);
        assert_matches!(
            cache.add_block(
                &sealed_b1_dup,
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block(&b1.block_hash).unwrap().data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b1.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Same as above, `get_deepest_unvalidated_in_longest_chain` should not
        // modify state
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Add b2 block (with a transaction) as not_validated
        let b2_tx = create_signed_data_tx();
        let txid = b2_tx.id;
        let mut b2 = extend_chain(random_block(U256::from(1)), &b1);
        b2.data_ledgers[DataLedger::Submit].tx_ids.push(txid);
        b2.test_sign();
        assert_matches!(
            cache.add_block(
                &seal_block_with_data_txs(&mut b2, vec![b2_tx.clone()]),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b2.block_hash
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Verify b2's transaction is in the cache
        assert_eq!(
            cache.get_block(&b2.block_hash).unwrap().data_ledgers[DataLedger::Submit].tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &b1.block_hash, U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );

        // Remove b2_1
        assert_matches!(cache.remove_block(&b2.block_hash), Ok(()));
        assert_eq!(cache.get_block(&b2.block_hash), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Remove b2_1 again
        assert_matches!(cache.remove_block(&b2.block_hash), Err(_));

        // Re-add b2_1 and add a competing b2 block called b1_2, it will be built
        // on b1 but share the same solution_hash
        // b2 has tx IDs from earlier modification, so need matching transactions
        assert_matches!(
            cache.add_block(
                &seal_block_with_data_txs(&mut b2, vec![b2_tx.clone()]),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        let mut b1_2 = extend_chain(random_block(U256::from(2)), &b1);
        b1_2.solution_hash = b1.solution_hash;
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b1_2),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );

        println!(
            "b1:   {} cdiff: {} solution_hash: {}",
            b1.block_hash, b1.cumulative_diff, b1.solution_hash
        );
        println!(
            "b2:   {} cdiff: {} solution_hash: {}",
            b2.block_hash, b2.cumulative_diff, b2.solution_hash
        );
        println!(
            "b1_2: {} cdiff: {} solution_hash: {}",
            b1_2.block_hash, b1_2.cumulative_diff, b1_2.solution_hash
        );

        // Verify if we exclude b1_2 we wont get it back
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &b1_2.block_hash,
                    U256::one(),
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );

        // Verify that we do get b1_2 back when not excluding it
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    b1_2.cumulative_diff,
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1_2.block_hash
        );

        // Get result with empty excluded hash
        let result = cache
            .get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::default(), // Empty/zeroed hash
                U256::one(),
                U256::one(),
            )
            .unwrap();

        // Assert result is either b1 or b1_2
        assert!(
            result.block_hash == b1.block_hash || result.block_hash == b1_2.block_hash,
            "Expected either b1 or b1_2 to be returned"
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Even though b2 is marked as a tip, it is still lower difficulty than b1_2 so will
        // not be included in the longest chain
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        assert_eq!(Some(&b1_2), cache.get_block(&b1_2.block_hash));
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(()));

        // Remove b1_2, causing b2 to now be the tip of the heaviest chain
        assert_matches!(cache.remove_block(&b1_2.block_hash), Ok(()));
        assert_eq!(cache.get_block(&b1_2.block_hash), None);
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(()));

        // Prune to a depth of 1 behind the tip
        cache.prune(1);
        assert_eq!(Some(&b1), cache.get_block(&b1.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(()));

        // Prune at the tip, removing all ancestors (verify b1 is pruned)
        cache.prune(0);
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(()));

        // Again, this time to make sure b1_2 is really gone
        cache.prune(0);
        assert_eq!(None, cache.get_block(&b1_2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1_2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(()));

        // <Reset the cache>
        // b1_2->b1 fork is the heaviest, but only b1 is validated. b2_2->b2->b1 is longer but
        // has a lower cdiff.
        let mut cache = BlockTree::new(&b1, ConsensusConfig::testing());
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b1_2),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        // b2 still has tx IDs from earlier modification
        assert_matches!(
            cache.add_block(
                &seal_block_with_data_txs(&mut b2, vec![b2_tx]),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        let mut b2_2 = extend_chain(random_block(U256::one()), &b2);
        println!(
            "b2_2: {} cdiff: {} solution_hash: {}",
            b2_2.block_hash, b2_2.cumulative_diff, b2_2.solution_hash
        );
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b2_2),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b1_2.block_hash
        );

        // b2_3->b2_2->b2->b1 is longer and heavier but only b2->b1 are validated.
        let mut b2_3 = extend_chain(random_block(U256::from(3)), &b2_2);
        println!(
            "b2_3: {} cdiff: {} solution_hash: {}",
            b2_3.block_hash, b2_3.cumulative_diff, b2_3.solution_hash
        );
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b2_3),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Err(_));
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(()));

        // Now b2_2->b2->b1 are validated.
        assert_matches!(
            cache.add_common(
                b2_2.block_hash,
                &seal_block(&mut b2_2),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b2_2.block_hash).unwrap(),
            (&b2_2, &ChainState::Validated(BlockState::ValidBlock))
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b2_2], 1, &cache), Ok(()));

        // Now the b3->b2->b1 fork is heaviest
        let mut b3 = extend_chain(random_block(U256::from(4)), &b2);
        println!(
            "b3:   {} cdiff: {} solution_hash: {}",
            b3.block_hash, b3.cumulative_diff, b3.solution_hash
        );
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b3),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_matches!(
            cache.add_common(
                b3.block_hash,
                &seal_block(&mut b3),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        assert_matches!(cache.mark_tip(&b3.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 0, &cache), Ok(()));

        // b3->b2->b1 fork is still heaviest
        assert_matches!(cache.mark_tip(&b2_2.block_hash), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(()));

        // add not validated b4, b3->b2->b1 fork is still heaviest
        let mut b4 = extend_chain(random_block(U256::from(5)), &b3);
        println!(
            "b4:   {} cdiff: {} solution_hash: {}",
            b4.block_hash, b4.cumulative_diff, b4.solution_hash
        );
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b4),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b4.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(()));

        // Prune to a depth of 1 past the tip and verify b1 and the b1_2 branch are pruned
        cache.prune(1);
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2, &b3], 1, &cache), Ok(()));

        // Mark a new tip for the longest chain, validating b2_3, and pruning again
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Ok(_));
        cache.prune(1);
        assert_eq!(None, cache.get_block(&b2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // Also make sure b3 (the old tip) got pruned
        cache.prune(1);
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // and that the not yet validated b4 was also pruned
        cache.prune(1);
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // Now make sure b2_2 and b2_3 are still in the cache/state
        cache.prune(1);
        assert_eq!(Some(&b2_2), cache.get_block(&b2_2.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_2.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        cache.prune(1);
        assert_eq!(Some(&b2_3), cache.get_block(&b2_3.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_3.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // Verify previously pruned b3 can be safely removed again, with longest chain cache staying stable
        assert_matches!(cache.remove_block(&b3.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // Same safety check for b4 - removing already pruned block shouldn't affect chain state
        assert_matches!(cache.remove_block(&b4.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(()));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTree::new(&b11, ConsensusConfig::testing());
        let mut b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b12),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        let mut b13 = extend_chain(random_block(U256::one()), &b11);

        println!("---");
        println!(
            "b11: {} cdiff: {} solution_hash: {}",
            b11.block_hash, b11.cumulative_diff, b11.solution_hash
        );
        println!(
            "b12: {} cdiff: {} solution_hash: {}",
            b12.block_hash, b12.cumulative_diff, b12.solution_hash
        );
        println!(
            "b13: {} cdiff: {} solution_hash: {}",
            b13.block_hash, b13.cumulative_diff, b13.solution_hash
        );
        println!("tip: {} before mark_tip()", cache.tip);

        assert_matches!(
            cache.add_common(
                b13.block_hash,
                &seal_block(&mut b13),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();

        println!("tip: {} after mark_tip()", cache.tip);
        // Verify the change doesn't switch tip to b13
        assert!(!reorg);

        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b12.block_hash
        );
        // Block tree state:
        //
        //                     [B13] cdiff=1, Validated(ValidBlock)
        //                    / ⚠ not counted as onchain
        //                   /
        //                  /
        // [B11] cdiff=0 --+-- [B12] cdiff=1, NotValidated (first added)
        // (genesis - tip).    ⚡ stays marked as tip but on longest chain
        //                       because while B13 has same cdiff & was second
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(()));

        // Extend the b13->b11 chain
        let mut b14 = extend_chain(random_block(U256::from(2)), &b13);
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b14),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .header()
                .block_hash,
            b14.block_hash
        );
        // by adding b14 we've now made the b13->b11 chain heavier and because
        // b13 is isn't already validated so it counts towards not_onchain_count
        // b14 isn't validated so it doesn't count towards the not_onchain_count
        assert_matches!(check_longest_chain(&[&b11, &b13], 1, &cache), Ok(()));

        // Now mark b13 as the tip, it should succeed as b14 made it canonical
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();
        assert!(reorg);

        // Try to mutate the state of the cache with some random validations
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&BlockHash::random()),
            Ok(())
        );
        assert_matches!(cache.mark_block_as_valid(&BlockHash::random()), Err(_));
        // Attempt to mark the already onchain b13 to prior vdf states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b13.block_hash),
            Ok(())
        );
        assert_matches!(cache.mark_block_as_valid(&b13.block_hash), Err(_));
        // Verify its state wasn't changed
        assert_eq!(
            cache.get_block_and_status(&b13.block_hash).unwrap(),
            (&b13, &ChainState::Onchain)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::Unknown))
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(()));

        // Move b14 though the vdf validation states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b14.block_hash),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &ChainState::NotOnchain(BlockState::ValidationScheduled)
            )
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(())
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(()));

        // now mark b14 as vdf validated
        assert_matches!(cache.mark_block_as_valid(&b14.block_hash), Ok(()));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(())
        );
        // Now that b14 is vdf validated it can be considered a NotOnchain
        // part of the longest chain
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(()));

        // add a b15 block
        let mut b15 = extend_chain(random_block(U256::from(3)), &b14);
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b15),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(()));

        // Validate b14
        assert_matches!(
            cache.add_common(
                b14.block_hash,
                &seal_block(&mut b14),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Validated(BlockState::ValidBlock))
        );
        // b14 is validated, but wont be onchain until is tip_height or lower
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(()));

        // add a b16 block
        let mut b16 = extend_chain(random_block(U256::from(4)), &b15);
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b16),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b16.block_hash),
            Ok(())
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(()));

        // Mark b16 as vdf validated eve though b15 is not
        assert_matches!(cache.mark_block_as_valid(&b16.block_hash), Ok(()));
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b16.block_hash).unwrap(),
            (&b16, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(()));

        // Make b14 the tip (making it OnChain)
        assert_matches!(cache.mark_tip(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Onchain)
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 0, &cache), Ok(()));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTree::new(&b11, ConsensusConfig::testing());
        let mut b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_block(
                &seal_block(&mut b12),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot()
            ),
            Ok(())
        );
        let mut b13 = extend_chain(random_block(U256::from(2)), &b12);
        println!("---");
        assert_matches!(cache.mark_tip(&b12.block_hash), Ok(_));

        // Verify the longest chain state isn't changed by adding b12
        assert_matches!(check_longest_chain(&[&b11, &b12], 0, &cache), Ok(()));

        // Now add the subsequent block, but as ValidationScheduled
        assert_matches!(
            cache.add_common(
                b13.block_hash,
                &seal_block(&mut b13),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidationScheduled),
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b12, &b13], 1, &cache), Ok(()));

        // When a locally produced block is added as validated "onchain" but it
        // hasn't yet been validated by the validation_service
        assert_matches!(
            check_earliest_not_onchain(
                &b13.block_hash,
                &ChainState::Validated(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(())
        );

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTree::new(&b11, ConsensusConfig::testing());
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        let mut b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_common(
                b12.block_hash,
                &seal_block(&mut b12),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        assert_matches!(cache.mark_tip(&b12.block_hash), Ok(_));

        assert_matches!(check_longest_chain(&[&b11, &b12], 0, &cache), Ok(()));

        // Create a fork at b12
        let mut b13a = extend_chain(random_block(U256::from(2)), &b12);
        let mut b13b = extend_chain(random_block(U256::from(2)), &b12);

        assert_matches!(
            cache.add_common(
                b13a.block_hash,
                &seal_block(&mut b13a),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );
        assert_matches!(
            cache.add_common(
                b13b.block_hash,
                &seal_block(&mut b13b),
                comm_cache.clone(),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );

        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 1, &cache), Ok(()));

        assert_matches!(cache.mark_tip(&b13a.block_hash), Ok(_));
        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 0, &cache), Ok(()));

        // extend the fork to make it canonical
        let mut b14b = extend_chain(random_block(U256::from(3)), &b13b);
        assert_matches!(
            cache.add_common(
                b14b.block_hash,
                &seal_block(&mut b14b),
                comm_cache,
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock),
            ),
            Ok(())
        );

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 2, &cache),
            Ok(())
        );

        // Mark the new tip
        assert_matches!(cache.mark_tip(&b14b.block_hash), Ok(_));

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 0, &cache),
            Ok(())
        );
    }

    fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new_mock_header();
        block.solution_hash = H256::random(); // Ensure unique solution hash
        block.height = 0; // Default to genesis
        block.cumulative_diff = cumulative_diff;
        block.test_sign();
        block
    }

    fn extend_chain(
        mut new_block: IrysBlockHeader,
        previous_block: &IrysBlockHeader,
    ) -> IrysBlockHeader {
        new_block.previous_block_hash = previous_block.block_hash();
        new_block.height = previous_block.height() + 1;
        new_block.previous_cumulative_diff = previous_block.cumulative_diff;
        // Re-sign after modifying fields that affect the signature hash
        new_block.test_sign();
        new_block
    }

    fn check_earliest_not_onchain(
        block_hash: &BlockHash,
        chain_state: &ChainState,
        cache: &BlockTree,
    ) -> eyre::Result<()> {
        let _x = 1;
        if let Some((block_entry, _, _)) = cache.get_earliest_not_onchain_in_longest_chain() {
            let c_s = &block_entry.chain_state;

            ensure!(
                block_entry.block.header().block_hash == *block_hash,
                "Wrong unvalidated block found: {} expected:{}",
                block_entry.block.header().block_hash,
                block_hash
            );

            ensure!(
                chain_state == c_s,
                "Wrong validation_state found: {:?}",
                c_s
            );
        } else if let ChainState::NotOnchain(_) = chain_state {
            return Err(eyre::eyre!("No unvalidated blocks found in longest chain"));
        }

        Ok(())
    }

    fn check_longest_chain(
        expected_blocks: &[&IrysBlockHeader],
        expected_not_onchain: usize,
        cache: &BlockTree,
    ) -> eyre::Result<()> {
        let (canonical_blocks, not_onchain_count) = cache.get_canonical_chain();
        let actual_blocks: Vec<_> = canonical_blocks
            .iter()
            .map(super::BlockTreeEntry::block_hash)
            .collect();

        let expected = expected_blocks
            .iter()
            .map(|b| b.block_hash())
            .collect::<Vec<_>>();

        println!("actual: {:?}\nexpected: {:?}", actual_blocks, expected);

        ensure!(
            actual_blocks
                == expected_blocks
                    .iter()
                    .map(|b| b.block_hash)
                    .collect::<Vec<_>>(),
            "Canonical chain does not match expected blocks"
        );
        ensure!(
            not_onchain_count == expected_not_onchain,
            format!(
                "Number of not-onchain blocks ({}) does not match expected ({})",
                not_onchain_count, expected_not_onchain
            )
        );
        Ok(())
    }
}
