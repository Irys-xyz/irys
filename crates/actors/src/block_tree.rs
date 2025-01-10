use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock},
    time::SystemTime,
};

use crate::{
    block_discovery::BlockPreValidatedMessage,
    block_index::BlockIndexActor,
    block_producer::{BlockConfirmedMessage, BlockProducerActor, RegisterBlockProducerMessage},
    mempool::MempoolActor,
};
use actix::prelude::*;
use eyre::ensure;
use irys_database::Ledger;
use irys_types::{BlockHash, IrysBlockHeader, IrysTransactionId, H256, U256};

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeActor {
    /// Shared access to the block index used for validation.
    pub block_index: Addr<BlockIndexActor>,
    /// Reference to the mempool actor, which maintains the validity of pending transactions.
    pub mempool: Addr<MempoolActor>,
    /// Needs to know the current block to build on
    block_producer: Option<Addr<BlockProducerActor>>,
    /// Block tree internal state
    cache: Arc<RwLock<BlockTreeCache>>,
}

impl Actor for BlockTreeActor {
    type Context = Context<Self>;
}

impl BlockTreeActor {
    /// Initializes a BlockTreeActor without a block_producer address
    pub fn new(
        block_index: Addr<BlockIndexActor>,
        mempool: Addr<MempoolActor>,
        genesis_block: &IrysBlockHeader,
    ) -> Self {
        Self {
            block_index,
            mempool,
            block_producer: None,
            cache: Arc::new(RwLock::new(BlockTreeCache::new(genesis_block))),
        }
    }
}

impl Handler<RegisterBlockProducerMessage> for BlockTreeActor {
    type Result = ();
    fn handle(
        &mut self,
        msg: RegisterBlockProducerMessage,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.block_producer = Some(msg.0);
    }
}

impl Handler<BlockPreValidatedMessage> for BlockTreeActor {
    type Result = ();
    fn handle(&mut self, msg: BlockPreValidatedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let pre_validated_block = msg.0;
        let all_txs = msg.1;
        // TODO: Check and see if this block represents a new head for the canonical chain
        // or if it should be added to a fork.

        // For now, because there are no forks, we'll just auto confirm the block
        let block_confirm_message = BlockConfirmedMessage(pre_validated_block, all_txs);

        self.block_index.do_send(block_confirm_message.clone());
        if let Some(block_producer) = &self.block_producer {
            block_producer.do_send(block_confirm_message.clone());
        }
        self.mempool.do_send(block_confirm_message);

        // TODO: Kick off a full validation process on the confirmed block that checks the VDF steps
        // and other heavy validation tasks, so it can be fully validated before we risk producing
        // a block on top of it.
    }
}

/// Number of blocks to retain in cache from chain head
const BLOCK_CACHE_DEPTH: u64 = 50;

type ChainCacheEntry = (BlockHash, Vec<IrysTransactionId>, Vec<IrysTransactionId>); // (block_hash, publish_txs, submit_txs)

#[derive(Debug)]
struct BlockTreeCache {
    // Main block storage
    blocks: HashMap<BlockHash, BlockEntry>,

    // Track solutions -> block hashes
    solutions: HashMap<H256, HashSet<BlockHash>>,

    // Current tip
    tip: BlockHash,

    // Track max cumulative difficulty
    max_cumulative_difficulty: (U256, BlockHash), // (difficulty, block_hash)

    // Height -> Hash mapping
    height_index: BTreeMap<u64, HashSet<BlockHash>>,

    // Cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    longest_chain_cache: (Vec<ChainCacheEntry>, usize),
}

#[derive(Debug)]
struct BlockEntry {
    block: IrysBlockHeader,
    status: BlockStatus,
    timestamp: SystemTime,
    children: HashSet<H256>,
}

#[derive(Debug, PartialEq, Clone)]
enum BlockStatus {
    OnChain,
    Validated,
    NotValidated(ValidationState),
}

#[derive(Debug, PartialEq, Clone)]
enum ValidationState {
    AwaitingValidation,
    AwaitingVdfStepValidation,
    VdfStepValidationScheduled,
    VdfStepsValidated,
}

impl BlockTreeCache {
    /// Create a new cache initialized with a starting block. The block is marked as
    /// on-chain and set as the tip.
    pub fn new(block: &IrysBlockHeader) -> Self {
        let block_hash = block.block_hash;
        let solution_hash = block.solution_hash;
        let height = block.height;
        let cumulative_diff = block.cumulative_diff;

        let mut blocks = HashMap::new();
        let mut solutions = HashMap::new();
        let mut height_index = BTreeMap::new();

        // Create initial block entry
        let block_entry = BlockEntry {
            block: block.clone(),
            status: BlockStatus::OnChain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
        };

        // Initialize all indices
        blocks.insert(block_hash, block_entry);
        solutions.insert(solution_hash, HashSet::from([block_hash]));
        height_index.insert(height, HashSet::from([block_hash]));

        // Initialize longest chain cache with just the genesis block
        let longest_chain_cache = (
            vec![(
                block_hash,
                block.ledgers[Ledger::Publish].txids.0.clone(), // Publish ledger txs
                block.ledgers[Ledger::Submit].txids.0.clone(),  // Submit ledger txs
            )],
            0,
        );

        Self {
            blocks,
            solutions,
            tip: block_hash,
            max_cumulative_difficulty: (cumulative_diff, block_hash),
            height_index,
            longest_chain_cache,
        }
    }

    /// Initializes the cache from a list of validated blocks.
    /// The most recent block in the list is marked as the tip.
    /// The input blocks must be sorted in descending order, from newest to oldest.
    pub fn initialize_from_list(blocks: Vec<IrysBlockHeader>) -> Self {
        assert!(!blocks.is_empty(), "Block list must not be empty");

        // Create a new cache using the most recent block as the starting point.
        let mut cache = Self::new(&blocks[0]);
        let tip_hash = blocks.last().unwrap().block_hash;

        // Iterate through the remaining blocks (from the second newest to the oldest)
        // and add them to the cache. Ownership of each block is transferred to
        // `add_validated_block`.
        for block in blocks.into_iter().skip(1) {
            // We can safely unwrap here because `add_validated_block` assumes the
            // block's parent exists and is already validated. This was ensured
            // when the cache was initialized with the first block.
            cache.add_validated_block(block).unwrap();
        }
        cache.mark_tip(&tip_hash).unwrap();
        cache
    }

    fn add_common(
        &mut self,
        hash: BlockHash,
        block: &IrysBlockHeader,
        status: BlockStatus,
    ) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        // Get parent
        let prev_entry = self
            .blocks
            .get_mut(&prev_hash)
            .ok_or_else(|| eyre::eyre!("Previous block not found"))?;

        // Update indices
        prev_entry.children.insert(hash);
        self.solutions
            .entry(block.solution_hash)
            .or_default()
            .insert(hash);
        self.height_index
            .entry(block.height)
            .or_default()
            .insert(hash);

        if block.cumulative_diff > self.max_cumulative_difficulty.0 {
            self.max_cumulative_difficulty = (block.cumulative_diff, hash);
        }

        self.blocks.insert(
            hash,
            BlockEntry {
                block: block.clone(),
                status,
                timestamp: SystemTime::now(),
                children: HashSet::new(),
            },
        );

        self.update_longest_chain_cache();
        Ok(())
    }

    pub fn add_block(&mut self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let hash = block.block_hash;
        if matches!(
            self.blocks.get(&hash).map(|b| b.status.clone()),
            Some(BlockStatus::OnChain)
        ) {
            return Ok(());
        }
        self.add_common(
            hash,
            block,
            BlockStatus::NotValidated(ValidationState::AwaitingVdfStepValidation),
        )
    }

    pub fn add_validated_block(&mut self, block: IrysBlockHeader) -> eyre::Result<()> {
        let hash = block.block_hash;
        let prev_hash = block.previous_block_hash;

        // Verify parent is validated
        ensure!(
            !matches!(
                self.blocks.get(&prev_hash).map(|b| b.status.clone()),
                Some(BlockStatus::NotValidated(_))
            ),
            "Previous block not validated"
        );

        self.add_common(hash, &block, BlockStatus::Validated)
    }

    /// Helper function to delete a single block without recursion
    fn delete_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let solution_hash = block_entry.block.solution_hash;
        let height = block_entry.block.height;
        let prev_hash = block_entry.block.previous_block_hash;

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

    /// Removes a block and all its descendants recursively
    pub fn remove_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        // Get children before deleting the block
        let children = self
            .blocks
            .get(block_hash)
            .map(|entry| entry.children.iter().cloned().collect::<Vec<_>>())
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
            .map(|(hash, entry)| (entry.block.cumulative_diff, *hash))
            .max_by_key(|(diff, _)| *diff)
            .unwrap_or((U256::zero(), BlockHash::default()))
    }

    fn get_canonical_chain(&self) -> (Vec<ChainCacheEntry>, usize) {
        self.longest_chain_cache.clone()
    }

    fn update_longest_chain_cache(&mut self) {
        let mut pairs = Vec::new();
        let mut not_onchain_count = 0;

        let mut current = self.max_cumulative_difficulty.1;
        let mut blocks_to_collect = BLOCK_CACHE_DEPTH;

        while let Some(entry) = self.blocks.get(&current) {
            match &entry.status {
                // For blocks awaiting initial validation, restart chain from parent
                BlockStatus::NotValidated(ValidationState::AwaitingVdfStepValidation)
                | BlockStatus::NotValidated(ValidationState::VdfStepValidationScheduled) => {
                    // Reset everything and continue from parent block
                    pairs.clear();
                    not_onchain_count = 0;
                    current = entry.block.previous_block_hash;
                    blocks_to_collect = BLOCK_CACHE_DEPTH;
                    continue;
                }

                BlockStatus::OnChain => {
                    // Include OnChain blocks in pairs
                    let publish_txs = entry.block.ledgers[Ledger::Publish].txids.0.clone();
                    let submit_txs = entry.block.ledgers[Ledger::Submit].txids.0.clone();
                    pairs.push((current, publish_txs, submit_txs));

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                // For validated or other not_validated states
                BlockStatus::Validated | BlockStatus::NotValidated(_) => {
                    let publish_txs = entry.block.ledgers[Ledger::Publish].txids.0.clone();
                    let submit_txs = entry.block.ledgers[Ledger::Submit].txids.0.clone();
                    pairs.push((current, publish_txs, submit_txs));
                    not_onchain_count += 1;

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }
            }

            current = entry.block.previous_block_hash;
        }

        pairs.reverse();
        self.longest_chain_cache = (pairs, not_onchain_count);
    }

    /// Helper function to mark a chain of blocks as validated
    fn mark_chain_validated(&mut self, start_hash: BlockHash) {
        let mut current = start_hash;
        while let Some(entry) = self.blocks.get_mut(&current) {
            if matches!(entry.status, BlockStatus::OnChain) {
                entry.status = BlockStatus::Validated;
                current = entry.block.previous_block_hash;
            } else {
                break;
            }
        }
    }

    /// Helper to mark off-chain blocks in a set
    fn mark_off_chain(&mut self, children: HashSet<H256>, current: &BlockHash) {
        for child in children {
            if child == *current {
                continue;
            }
            if let Some(entry) = self.blocks.get_mut(&child) {
                if matches!(entry.status, BlockStatus::OnChain) {
                    entry.status = BlockStatus::Validated;
                    // Recursively mark children of this block
                    let children = entry.children.clone();
                    self.mark_off_chain(children, current);
                }
            }
        }
    }

    /// Helper to recursively mark blocks on-chain
    fn mark_on_chain(&mut self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        match self.blocks.get(&prev_hash) {
            None => Ok(()), // Reached the end
            Some(prev_entry) => {
                let prev_block = prev_entry.block.clone();
                let prev_children = prev_entry.children.clone();

                match prev_entry.status {
                    BlockStatus::NotValidated(_) => Err(eyre::eyre!("invalid_tip")),
                    BlockStatus::OnChain => {
                        // Mark other branches as validated
                        self.mark_off_chain(prev_children, &block.block_hash);
                        Ok(())
                    }
                    BlockStatus::Validated => {
                        // Update previous block to on_chain
                        if let Some(entry) = self.blocks.get_mut(&prev_hash) {
                            entry.status = BlockStatus::OnChain;
                        }
                        // Recursively mark previous blocks
                        self.mark_on_chain(&prev_block)
                    }
                }
            }
        }
    }

    /// Marks a block as the new tip
    pub fn mark_tip(&mut self, block_hash: &BlockHash) -> eyre::Result<bool> {
        // Get the current block
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let block = block_entry.block.clone();
        let old_tip = self.tip;

        // Recursively mark previous blocks
        self.mark_on_chain(&block)?;

        // Mark the tip block as on_chain
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            entry.status = BlockStatus::OnChain;
        }

        self.tip = *block_hash;
        self.update_longest_chain_cache();
        Ok(old_tip != *block_hash)
    }

    pub fn mark_vdf_validation_scheduled(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if let BlockStatus::NotValidated(ValidationState::AwaitingVdfStepValidation) =
                entry.status
            {
                entry.status =
                    BlockStatus::NotValidated(ValidationState::VdfStepValidationScheduled);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    pub fn mark_vdf_steps_validated(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if let BlockStatus::NotValidated(ValidationState::VdfStepValidationScheduled) =
                entry.status
            {
                entry.status = BlockStatus::NotValidated(ValidationState::VdfStepsValidated);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    /// Gets block by hash
    pub fn get_block(&self, block_hash: &BlockHash) -> Option<&IrysBlockHeader> {
        self.blocks.get(block_hash).map(|entry| &entry.block)
    }

    /// Gets block and its current validation status    
    pub fn get_block_and_status(
        &self,
        block_hash: &BlockHash,
    ) -> Option<(&IrysBlockHeader, &BlockStatus)> {
        self.blocks
            .get(block_hash)
            .map(|entry| (&entry.block, &entry.status))
    }

    /// Collect previous blocks up to the last on-chain block
    fn get_fork_blocks(&self, block: &IrysBlockHeader) -> Vec<&IrysBlockHeader> {
        let mut prev_hash = block.previous_block_hash;
        let mut fork_blocks = Vec::new();

        while let Some(prev_entry) = self.blocks.get(&prev_hash) {
            match prev_entry.status {
                BlockStatus::OnChain => {
                    fork_blocks.push(&prev_entry.block);
                    break;
                }
                BlockStatus::Validated | BlockStatus::NotValidated(_) => {
                    fork_blocks.push(&prev_entry.block);
                    prev_hash = prev_entry.block.previous_block_hash;
                }
            }
        }

        fork_blocks
    }

    /// Finds the earliest not validated block, walking back the chain
    /// until finding a validated block, reaching block height 0, or exceeding cache depth
    pub fn get_earliest_not_validated<'a>(
        &'a self,
        block: &'a BlockEntry,
    ) -> Option<(
        &'a BlockEntry,
        Vec<&'a IrysBlockHeader>,
        (ValidationState, SystemTime),
    )> {
        let mut current_entry = block;
        let mut prev_block = &current_entry.block;
        let mut depth_count = 0;

        while prev_block.height > 0 && depth_count < BLOCK_CACHE_DEPTH {
            let prev_hash = prev_block.previous_block_hash;
            let prev_entry = self.blocks.get(&prev_hash)?;

            match prev_entry.status {
                BlockStatus::Validated | BlockStatus::OnChain => {
                    return Some((
                        current_entry,
                        self.get_fork_blocks(prev_block),
                        (
                            match &current_entry.status {
                                BlockStatus::NotValidated(state) => state.clone(),
                                _ => return None,
                            },
                            current_entry.timestamp,
                        ),
                    ));
                }
                BlockStatus::NotValidated(_) => {
                    current_entry = prev_entry;
                    prev_block = &current_entry.block;
                    depth_count += 1;
                }
            }
        }

        // If we've reached height 0 or exceeded cache depth, return None
        None
    }

    /// Get the earliest unvalidated block from the longest chain
    /// Relies on the longest_chain_cache
    pub fn get_earliest_not_validated_in_longest_chain(
        &self,
    ) -> Option<(
        &BlockEntry,
        Vec<&IrysBlockHeader>,
        (ValidationState, SystemTime),
    )> {
        // Get the block with max cumulative difficulty
        let (_, max_diff_hash) = self.max_cumulative_difficulty;

        // Get the tip's cumulative difficulty
        let tip_entry = self.blocks.get(&self.tip)?;
        let tip_cdiff = tip_entry.block.cumulative_diff;

        // Check if tip's difficulty exceeds max difficulty
        if tip_cdiff >= self.max_cumulative_difficulty.0 {
            return None;
        }

        // Get the block with max difficulty
        let entry = self.blocks.get(&max_diff_hash)?;

        // Check if it's not validated
        if let BlockStatus::NotValidated(state) = &entry.status {
            // Get earliest not validated walking back the chain
            self.get_earliest_not_validated(entry)
        } else {
            None
        }
    }

    /// Gets block with matching solution hash, excluding specified block.
    /// Returns a block meeting these requirements:
    /// - Has matching solution_hash
    /// - Is not the excluded block
    /// - Either has same cumulative_diff as input or meets double-signing criteria
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
                let block = &entry.block;

                // Case 1: Exact cumulative_diff match - return immediately
                if block.cumulative_diff == cumulative_difficulty {
                    return Some(block);
                }

                // Case 2: Double signing case - return immediately
                if block.cumulative_diff > previous_cumulative_difficulty
                    && cumulative_difficulty > block.previous_cumulative_diff
                {
                    return Some(block);
                }

                // Store as best block seen so far if we haven't found one yet
                if best_block.is_none() {
                    best_block = Some(block);
                }
            }
        }

        // Return best block found (if any)
        best_block
    }

    /// Prunes blocks below specified depth from tip. When pruning an on-chain block,
    /// removes all its non-on-chain children regardless of their height.
    pub fn prune(&mut self, depth: u64) -> eyre::Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }

        let tip_height = self
            .blocks
            .get(&self.tip)
            .ok_or_else(|| eyre::eyre!("Tip block not found"))?
            .block
            .height;
        let min_keep_height = tip_height.saturating_sub(depth);

        let min_height = match self.height_index.keys().min() {
            Some(&h) => h,
            None => return Ok(()),
        };

        let mut current_height = min_height;
        while current_height < min_keep_height {
            let Some(hashes) = self.height_index.get(&current_height) else {
                current_height += 1;
                continue;
            };

            // Clone hashes to avoid borrow issues during removal
            let hashes: Vec<_> = hashes.iter().cloned().collect();

            for hash in hashes {
                if let Some(entry) = self.blocks.get(&hash) {
                    if matches!(entry.status, BlockStatus::OnChain) {
                        // First remove all non-on-chain children
                        let children = entry.children.clone();
                        for child in children {
                            if let Some(child_entry) = self.blocks.get(&child) {
                                if !matches!(child_entry.status, BlockStatus::OnChain) {
                                    self.remove_block(&child)?;
                                }
                            }
                        }

                        // Now remove just this block
                        self.delete_block(&hash)?;
                    }
                }
            }

            current_height += 1;
        }

        Ok(())
    }

    /// Returns true if solution hash exists in cache
    pub fn is_known_solution_hash(&self, solution_hash: &H256) -> bool {
        self.solutions.contains_key(solution_hash)
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use eyre::ensure;
    use irys_database::Ledger;

    #[actix::test]
    async fn test_block_cache() {
        let b1 = random_block(U256::from(0));

        // Initialize block tree cache from `b1`
        let mut cache = BlockTreeCache::new(&b1);

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

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Adding `b1` again shouldn't change the state because it is confirmed
        // onchain
        let mut b1_test = b1.clone();
        b1_test.ledgers[Ledger::Submit].txids.push(H256::random());
        assert_matches!(cache.add_block(&b1_test), Ok(_));
        assert_eq!(
            cache.get_block(&b1.block_hash).unwrap().ledgers[Ledger::Submit]
                .txids
                .len(),
            0
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b1.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids
                .len(),
            0
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Same as above, `get_deepest_unvalidated_in_longest_chain` should not
        // modify state
        assert_matches!(cache.get_earliest_not_validated_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add b2 block as not_validated
        let mut b2 = extend_chain(random_block(U256::from(1)), &b1);
        assert_matches!(cache.add_block(&b2), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2.block_hash
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add a TXID to b2, and re-add it to the cache, but still don't mark as validated
        let txid = H256::random();
        b2.ledgers[Ledger::Submit].txids.push(txid.clone());
        assert_matches!(cache.add_block(&b2), Ok(_));
        assert_eq!(
            cache.get_block(&b2.block_hash).unwrap().ledgers[Ledger::Submit].txids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &b1.block_hash, U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids[0],
            txid
        );

        // Remove b2_1
        assert_matches!(cache.remove_block(&b2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b2.block_hash), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b2_1 again
        assert_matches!(cache.remove_block(&b2.block_hash), Err(_));

        // Re-add b2_1 and add a competing b2 block called b1_2, it will be built
        // on b1 but share the same solution_hash
        assert_matches!(cache.add_block(&b2), Ok(_));
        let mut b1_2 = extend_chain(random_block(U256::from(2)), &b1);
        b1_2.solution_hash = b1.solution_hash;
        assert_matches!(cache.add_block(&b1_2), Ok(_));

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
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Even though b2 is marked as a tip, it is still lower difficulty than B1_2 so will
        // not be included in the longest chain
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        assert_eq!(Some(&b1_2), cache.get_block(&b1_2.block_hash));
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b1_2, causing b2 to now be the tip of the heaviest chain
        assert_matches!(cache.remove_block(&b1_2.block_hash), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune to a depth of 1 behind the tip
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune at the tip, removing all ancestors (verify b1 is pruned)
        assert_matches!(cache.prune(0), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // Again, this time to make sure b1_2 is really gone
        assert_matches!(cache.prune(0), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // <Reset the cache>
        // b1_2->b1 fork is the heaviest, but only b1 is validated. b2_2->b2->b1 is longer but
        // has a lower cdiff.
        let mut cache = BlockTreeCache::new(&b1);
        assert_matches!(cache.add_block(&b1_2), Ok(_));
        assert_matches!(cache.add_block(&b2), Ok(_));
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        let b2_2 = extend_chain(random_block(U256::one()), &b2);
        println!(
            "b2_2: {} cdiff: {} solution_hash: {}",
            b2_2.block_hash, b2_2.cumulative_diff, b2_2.solution_hash
        );
        assert_matches!(cache.add_block(&b2_2), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b1_2.block_hash
        );

        // b2_3->b2_2->b2->b1 is longer and heavier but only b2->b1 are validated.
        let b2_3 = extend_chain(random_block(U256::from(3)), &b2_2);
        println!(
            "b2_3: {} cdiff: {} solution_hash: {}",
            b2_3.block_hash, b2_3.cumulative_diff, b2_3.solution_hash
        );
        assert_matches!(cache.add_block(&b2_3), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Err(_));
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Now b2_2->b2->b1 are validated.
        assert_matches!(cache.add_validated_block(b2_2.clone()), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b2_2.block_hash).unwrap(),
            (&b2_2, &BlockStatus::Validated)
        );
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b2_2], 1, &cache), Ok(_));

        // Now the b3->b2->b1 fork is heaviest
        let b3 = extend_chain(random_block(U256::from(4)), &b2);
        println!(
            "b3:   {} cdiff: {} solution_hash: {}",
            b3.block_hash, b3.cumulative_diff, b3.solution_hash
        );
        assert_matches!(cache.add_block(&b3), Ok(_));
        assert_matches!(cache.add_validated_block(b3.clone()), Ok(_));
        assert_matches!(cache.mark_tip(&b3.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_validated_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 0, &cache), Ok(_));

        // b3->b2->b1 fork is still heaviest
        assert_matches!(cache.mark_tip(&b2_2.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_validated_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // add not validated b4, b3->b2->b1 fork is still heaviest
        let b4 = extend_chain(random_block(U256::from(5)), &b3);
        println!(
            "b4:   {} cdiff: {} solution_hash: {}",
            b4.block_hash, b4.cumulative_diff, b4.solution_hash
        );
        assert_matches!(cache.add_block(&b4), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b4.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // Prune to a depth of 1 past the tip and verify b1 and the b1_2 branch are pruned
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2, &b3], 1, &cache), Ok(_));

        // Mark a new tip for the longest chain, validating b2_3, and pruning again
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Ok(_));
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Also make sure b3 (the old tip) got pruned
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // and that the not yet validated b4 was also pruned
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Now make sure b2_2 and b2_3 are still in the cache/state
        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        assert_matches!(cache.prune(1), Ok(_));
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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

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
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(cache.add_block(&b12), Ok(_));
        let b13 = extend_chain(random_block(U256::one()), &b11);

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

        assert_matches!(cache.add_validated_block(b13.clone()), Ok(_));
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();

        // The tip does change here, even though it's not part of the longest
        // chain, this seems like a bug
        println!("tip: {} after mark_tip()", cache.tip);
        assert_eq!(reorg, true);

        assert_matches!(cache.get_earliest_not_validated_in_longest_chain(), None);
        // Although b13 becomes the tip, it's not included in the longest_chain_cache.
        // This is because the cache follows blocks from max_cumulative_difficulty, which
        // was set to b12 when it was first added. When multiple blocks have the same
        // cumulative difficulty, max_cumulative_difficulty preserves the first one seen.
        // Since b13's difficulty equals b12's (rather than exceeds it), b12 remains the
        // reference point for longest chain calculations.

        // Block tree state:
        //
        //                     [B13] cdiff=1, Validated
        //                    /  ⚡ marked as tip but not in longest chain
        //                   /     because B12 has same cdiff & was first
        //                  /
        // [B11] cdiff=0 --+-- [B12] cdiff=1, NotValidated (first added)
        // (genesis)             ⚠ not counted as onchain due to not being validated
        //
        // ▶ Longest chain contains: [B11]
        // ▶ Not on chain count: 0
        // ▶ First added wins longest_chain with equal cdiff

        // DMac's Note:
        // Issue: tip and longest chain can become misaligned when marking a tip that has
        //   equal difficulty to an earlier block. The tip could point to b13 while the
        //   longest chain contains b12, as b12 was seen first with same difficulty.
        // Fix: mark_tip() should reject attempts to change tip to a block that has equal
        //   (rather than greater) difficulty compared to the current max_difficulty block.
        //   This would ensure tip always follows the longest chain.
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Extend the b13->b11 chain
        let b14 = extend_chain(random_block(U256::from(2)), &b13);
        assert_matches!(cache.add_block(&b14), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_validated_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b14.block_hash
        );
        // by adding b14 we've now made the b13->b11 chain heavier and because
        // b13 is already validated it is included in the longest chain
        // b14 isn't validated so it doesn't count towards the not_onchain_count
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Try to mutate the state of the cache with some validations
        assert_matches!(
            cache.mark_vdf_validation_scheduled(&BlockHash::random()),
            Ok(_)
        );
        assert_matches!(cache.mark_vdf_steps_validated(&BlockHash::random()), Ok(_));
        // Attempt to mark the already onchain b13 to prior vdf states
        assert_matches!(cache.mark_vdf_validation_scheduled(&b13.block_hash), Ok(_));
        assert_matches!(cache.mark_vdf_steps_validated(&b13.block_hash), Ok(_));
        // Verify its state wasn't changed
        assert_eq!(
            cache.get_block_and_status(&b13.block_hash).unwrap(),
            (&b13, &BlockStatus::OnChain)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &BlockStatus::NotValidated(ValidationState::AwaitingVdfStepValidation)
            )
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Move b14 though the vdf validation states
        assert_matches!(cache.mark_vdf_validation_scheduled(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &BlockStatus::NotValidated(ValidationState::VdfStepValidationScheduled)
            )
        );
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                ValidationState::VdfStepValidationScheduled,
                &cache
            ),
            Ok(_)
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // now mark b14 as vdf validated
        assert_matches!(cache.mark_vdf_steps_validated(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &BlockStatus::NotValidated(ValidationState::VdfStepsValidated)
            )
        );
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                ValidationState::VdfStepsValidated,
                &cache
            ),
            Ok(_)
        );
        // Now that b14 is vdf validated it can be considered a not_onchain
        // part of the longest chain
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b15 block
        let b15 = extend_chain(random_block(U256::from(3)), &b14);
        assert_matches!(cache.add_block(&b15), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                ValidationState::VdfStepsValidated,
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Validate b14
        assert_matches!(cache.add_validated_block(b14.clone()), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                ValidationState::AwaitingVdfStepValidation,
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &BlockStatus::Validated)
        );
        // b14 is validated, but wont be onchain until is tip_height or lower
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b16 block
        let b16 = extend_chain(random_block(U256::from(4)), &b15);
        assert_matches!(cache.add_block(&b16), Ok(_));
        assert_matches!(cache.mark_vdf_validation_scheduled(&b16.block_hash), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                ValidationState::AwaitingVdfStepValidation,
                &cache
            ),
            Ok(_)
        );
        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Mark b16 as vdf validated eve though b15 is not
        assert_matches!(cache.mark_vdf_steps_validated(&b16.block_hash), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                ValidationState::AwaitingVdfStepValidation,
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b16.block_hash).unwrap(),
            (
                &b16,
                &BlockStatus::NotValidated(ValidationState::VdfStepsValidated)
            )
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Make b14 the tip (making it OnChain)
        assert_matches!(cache.mark_tip(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &BlockStatus::OnChain)
        );
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                ValidationState::AwaitingVdfStepValidation,
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 0, &cache), Ok(_));
    }

    fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new();
        block.block_hash = BlockHash::random();
        block.solution_hash = H256::random(); // Ensure unique solution hash
        block.height = 0; // Default to genesis
        block.cumulative_diff = cumulative_diff;
        block
    }

    fn extend_chain(
        mut new_block: IrysBlockHeader,
        previous_block: &IrysBlockHeader,
    ) -> IrysBlockHeader {
        new_block.previous_block_hash = previous_block.block_hash;
        new_block.height = previous_block.height + 1;
        new_block.previous_cumulative_diff = previous_block.cumulative_diff;
        // Don't modify solution_hash - keep the random one from block creation
        new_block
    }

    fn check_earliest_not_validated(
        block_hash: &BlockHash,
        validation_state: ValidationState,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let (block_entry, _, (v_state, _)) = cache
            .get_earliest_not_validated_in_longest_chain()
            .ok_or_else(|| eyre::eyre!("No unvalidated blocks found in longest chain"))?;

        ensure!(
            block_entry.block.block_hash == *block_hash,
            "Wrong unvalidated block found: {}",
            block_entry.block.block_hash
        );

        ensure!(
            v_state == validation_state,
            "Wrong validation_state found: {:?}",
            validation_state
        );

        Ok(())
    }

    fn check_longest_chain(
        expected_blocks: &[&IrysBlockHeader],
        expected_not_onchain: usize,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let (canonical_blocks, not_onchain_count) = cache.get_canonical_chain();
        let actual_blocks: Vec<_> = canonical_blocks.iter().map(|(hash, _, _)| *hash).collect();

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
            "Number of not-onchain blocks does not match expected"
        );
        Ok(())
    }
}
