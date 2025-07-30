//! Active validations management module.
//!
//! Priority-based concurrent validation task management using a two-tier system:
//! 1. `BlockState`
//!     1.1. **CanonicalExtension**: Blocks extending canonical tip (highest priority)
//!     1.2. **Canonical**: Blocks already on canonical chain (medium priority)
//!     1.3. **Fork**: Alternative chain blocks (low priority)
//!     1.4. **Unknown**: Blocks with unknown state - generally orphans (lowest priority)
//! 2. `height`
//!     Blocks with lower heights have a higher priority
//! 2. `vdf_step_count`
//!     Blocks with the same BlockState and height priority level will be ordered by their vdf_step_count, favouring blocks with fewer steps
//!
//! ## Implementation
//! - Uses priority queue for deriving polling priorities
//! - Non-blocking processing with `poll_immediate` for completion checks
//! - Canonical extension detection walks parent chain to canonical tip
//! - Lower block heights processed first within each priority tier
//! - Completed tasks immediately removed to free resources
use futures::future::poll_immediate;
use futures::FutureExt as _;
use irys_domain::{BlockTree, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, IrysBlockHeader};
use irys_vdf::state::CancelEnum;
use priority_queue::PriorityQueue;
use std::future::Future;
use std::mem::replace;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::{cmp::Reverse, sync::Arc};
use tracing::{debug, error, info, instrument};

use crate::block_tree_service::ValidationResult;
use crate::validation_service::block_validation_task::BlockValidationTask;
use crate::validation_service::VdfValidationResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BlockPriority {
    /// Canonical extensions that extend from the canonical tip (highest priority)
    CanonicalExtension,
    /// Canonical blocks already on chain (middle priority)  
    Canonical,
    /// Fork blocks that don't extend the canonical tip (low priority)
    Fork,
    /// Unknown/orphan blocks (not tracked by the block tree) (Lowest priority)
    Unknown,
}

#[derive(Debug, Clone)]
/// Metadata struct that is used to inform block validation priority decisions
pub(crate) struct BlockPriorityMeta {
    pub height: u64,
    pub state: BlockPriority,
    pub vdf_step_count: u64,
    pub block: Arc<IrysBlockHeader>,
}

/// Define how ordering for BlockPriorityMeta structs works
impl Ord for BlockPriorityMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // if two blocks have the same `BlockState` (primary ordering)
        self.state
            .cmp(&other.state)
            // and the same height (prefer lower height blocks)
            .then_with(|| self.height.cmp(&other.height))
            // prefer the one with the fewest VDF steps
            .then_with(|| self.vdf_step_count.cmp(&other.vdf_step_count))
    }
}

impl From<(BlockPriority, Arc<IrysBlockHeader>)> for BlockPriorityMeta {
    fn from(value: (BlockPriority, Arc<IrysBlockHeader>)) -> Self {
        let (state, block) = value;
        Self {
            height: block.height,
            state,
            vdf_step_count: block.vdf_limiter_info.steps.len() as u64, // safe, usize <= u64 (for now...)
            block,
        }
    }
}

impl PartialOrd for BlockPriorityMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// needed due to the capture of `block`
impl PartialEq for BlockPriorityMeta {
    fn eq(&self, other: &Self) -> bool {
        self.height == other.height
            && self.state == other.state
            && self.vdf_step_count == other.vdf_step_count
    }
}

impl Eq for BlockPriorityMeta {}

pub(crate) struct VdfValidationTask {
    pub block_hash: BlockHash,
    pub fut: Pin<Box<dyn Future<Output = VdfValidationResult> + Send>>,
    pub cancel: Arc<AtomicU8>,
}

/// Wrapper around active validations with capacity management and priority ordering
pub(crate) struct ActiveValidations {
    /// Priority queue for blocks pending VDF validation
    pub(crate) vdf_pending_queue: PriorityQueue<BlockHash, Reverse<BlockValidationTask>>,

    /// the currently executing VDF task
    /// VDF validation is not concurrent
    pub(crate) vdf_task: Option<VdfValidationTask>,

    /// Priority queue of (block_hash, meta) with  priority ordering of tasks that are ready for concurrent validation
    pub(crate) concurrent_queue: PriorityQueue<BlockHash, Reverse<BlockPriorityMeta>>,

    /// Map from block hash to the concurrent tasks
    pub(crate) concurrent_tasks:
        std::collections::HashMap<BlockHash, Pin<Box<dyn Future<Output = ()> + Send>>>,
    pub(crate) block_tree_guard: BlockTreeReadGuard,
}

impl ActiveValidations {
    pub(crate) fn new(block_tree_guard: BlockTreeReadGuard) -> Self {
        Self {
            vdf_pending_queue: PriorityQueue::new(),
            concurrent_queue: PriorityQueue::new(),
            concurrent_tasks: std::collections::HashMap::new(),
            block_tree_guard,
            vdf_task: None,
        }
    }

    /// Calculate the priority for a block based on its chain position and canonical status
    #[instrument(skip_all, fields(block_hash = %block.block_hash))]
    pub(crate) fn calculate_priority(
        &self,
        block: &Arc<IrysBlockHeader>,
    ) -> Reverse<BlockPriorityMeta> {
        let block_tree = self.block_tree_guard.read();
        let block_hash = block.block_hash;

        let state = match block_tree.get_block_and_status(&block_hash) {
            // Canonical blocks: middle priority tier
            Some((_block, ChainState::Onchain)) => BlockPriority::Canonical,
            Some((_block, ChainState::NotOnchain(_) | ChainState::Validated(_))) => {
                if self.is_canonical_extension(&block_hash, &block_tree) {
                    // Canonical extensions: highest priority tier
                    BlockPriority::CanonicalExtension
                } else {
                    // Fork blocks: low priority tier
                    BlockPriority::Fork
                }
            }
            // Block is unknown (lowest priority)
            None => BlockPriority::Unknown,
        };

        Reverse((state, Arc::clone(block)).into())
    }

    /// Check if a block is a canonical extension (extends from the canonical tip)
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTree) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash;

        // Walk up from the block to see if we reach the canonical tip
        let mut current_hash = *block_hash;
        while let Some((block, _)) = block_tree.get_block_and_status(&current_hash) {
            if current_hash == canonical_tip {
                return true;
            }
            current_hash = block.previous_block_hash;

            // Stop if we reach a canonical block (avoid infinite walking)
            if let Some((_, ChainState::Onchain)) = block_tree.get_block_and_status(&current_hash) {
                // Check if this canonical block is the canonical tip
                return current_hash == canonical_tip;
            }
        }
        false
    }

    #[instrument(skip_all, fields(block_hash = %block.block_hash))]
    pub(crate) fn push_concurrent_fut(
        &mut self,
        block: Arc<IrysBlockHeader>,
        future: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) {
        let priority = self.calculate_priority(&block);
        debug!(
            "adding concurrent validation task with priority: {:?}",
            priority.0.state
        );
        self.concurrent_tasks.insert(block.block_hash, future);
        self.concurrent_queue.push(block.block_hash, priority);
    }

    pub(crate) fn concurrent_len(&self) -> usize {
        self.concurrent_queue.len()
    }

    pub(crate) fn concurrent_is_empty(&self) -> bool {
        self.concurrent_queue.is_empty()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.concurrent_is_empty() && self.vdf_pending_queue.is_empty()
    }

    /// Process completed concurrent validations and remove them from the active set
    /// returns `true` if any of the block validation tasks succeeded
    #[instrument(skip_all, fields(active_count = self.concurrent_len()))]
    pub(crate) async fn process_completed_concurrent(&mut self) -> bool {
        let mut completed_blocks = Vec::new();

        assert_eq!(
            self.concurrent_queue.len(),
            self.concurrent_tasks.len(),
            "validations and futures out of sync"
        );

        if self.concurrent_queue.is_empty() {
            return false;
        }

        // Check futures in priority order using poll_immediate for non-blocking check
        for (block_hash, _priority) in self.concurrent_queue.clone().iter() {
            if let Some(future) = self.concurrent_tasks.get_mut(block_hash) {
                // Use poll_immediate to check if future is ready without blocking
                if poll_immediate(future).await.is_some() {
                    completed_blocks.push(*block_hash);
                }
            }
        }

        // Remove completed validations
        for block_hash in &completed_blocks {
            debug!(block_hash = %block_hash, "validation task completed");
            self.concurrent_queue.remove(block_hash);
            self.concurrent_tasks.remove(block_hash);
        }
        let tasks_completed = !completed_blocks.is_empty();
        if tasks_completed {
            debug!(
                completed_count = completed_blocks.len(),
                remaining_count = self.concurrent_len(),
                "processed completed validations"
            );
        }
        tasks_completed
    }

    /// Gets the current VDF task, or creates a new one.
    /// also handles cancellation/preempting
    pub(crate) fn get_or_create_vdf_task(&mut self) -> Option<VdfValidationTask> {
        let peek = self.vdf_pending_queue.peek();

        // if we have an existing task, figure out if it's being cancelled
        // if not, check if we need to cancel it (to replace it with a higher priority task)
        let task = if let Some(task) = self.vdf_task.take() {
            // if cancelling, return current task (it'll poll to completion once cancellation completes)
            let current_cancel_state = task.cancel.load(Ordering::Relaxed);
            if current_cancel_state != CancelEnum::Continue as u8 {
                debug!(
                    "VDF task {} is being cancelled ({:?})",
                    &task.block_hash, &current_cancel_state
                );
                task
            } else if let Some((high_prio_hash, high_prio_task)) = peek {
                // check if task needs to be replaced by a higher priority task
                // check the hash of the highest priority according to the queue against the hash of the task
                if *high_prio_hash != task.block_hash {
                    info!(
                        "Cancelling in-progress VDF validation for block {} in favour of block {:?} {}",
                        &task.block_hash,&high_prio_task.0.priority.state, &high_prio_hash,
                    );
                    // Cancel only if currently set to Continue
                    if let Err(e) = task.cancel.compare_exchange(
                        CancelEnum::Continue as u8,
                        CancelEnum::Cancelled as u8,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        error!("Error cancelling task {} - {}", &task.block_hash, e)
                    }
                }
                task
            } else {
                task
            }
            // if there is no active task, and we have a pending task in the queue, add it
        } else if let Some((pending_hash, pending_task)) = peek {
            // Create new task from highest priority pending task
            debug!("Created VDF validation task for  {}", &pending_hash);
            let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

            VdfValidationTask {
                block_hash: *pending_hash,
                fut: pending_task
                    .0
                    .clone()
                    .execute_vdf(Arc::clone(&cancel))
                    .boxed(),
                cancel,
            }
        } else {
            // Nothing to process
            return None;
        };
        Some(task)
    }

    pub(crate) fn handle_vdf_validation_result(
        &mut self,
        task: &VdfValidationTask,
        result: VdfValidationResult,
    ) {
        match result {
            VdfValidationResult::Valid => {
                // remove task from the vdf_pending queue
                let (hash, task) = self.vdf_pending_queue.remove(&task.block_hash).unwrap_or_else(|| panic!("Expected processing task for {} to have an entry in the vdf_pending queue",
                        &task.block_hash));
                // do NOT send anything to the block tree

                debug!(
                    "Processed VDF task for block {}, spawning concurrent validation task",
                    &hash
                );

                // add to active concurrent validations (this also adds to the concurrent queue)
                self.push_concurrent_fut(task.0.block.clone(), task.0.execute_concurrent().boxed())
            }
            VdfValidationResult::Invalid(err) => {
                // remove task from the vdf_pending queue
                let (invalid_hash, invalid_item) = self
                    .vdf_pending_queue
                    .remove(&task.block_hash)
                    .expect("Expected processing task to have an entry in the vdf_pending queue");
                error!(block_hash = %invalid_hash, "Error validating VDF - {}", &err);
                // notify the block tree
                invalid_item
                    .0
                    .send_validation_result(ValidationResult::Invalid);
            }
            VdfValidationResult::Cancelled => {
                debug!("VDF task {} was cancelled", &task.block_hash);
                // do nothing, leave the task in the pending queue
            }
        };
    }

    /// Process the vdf task
    /// returns `true` if the current VDF task polled to completion and we should be run again
    #[instrument(skip_all, fields(pending = self.vdf_pending_queue.len()))]
    pub(crate) async fn process_completed_vdf(&mut self) -> bool {
        // get the VDF task we should poll, or early return if there's nothing to process
        let mut task = match self.get_or_create_vdf_task() {
            Some(task) => task,
            None => return false, // Nothing to do
        };

        // process the provided task
        // either 1.) a previously produced task, 2.) a previously produced task that is getting cancelled, or 3.) a new task
        // we still poll cancelling tasks to ensure they stop correctly
        let poll_res = poll_immediate(&mut task.fut).await;

        if let Some(result) = poll_res {
            // handle the result of the VDF validation task
            self.handle_vdf_validation_result(&task, result);
            true
        } else {
            // task hasn't completed
            self.vdf_task = Some(task);
            false
        }
    }

    /// Reevaluate priorities for all active validations after a reorg
    /// This recalculates priorities based on the new canonical chain state
    #[instrument(skip_all, fields(validation_count = self.concurrent_len()))]
    pub(crate) fn reevaluate_priorities(&mut self) {
        debug!("reevaluating priorities after reorg");

        {
            // swap the old queue out of `self` to we can take full ownership
            let old_queue = replace(&mut self.vdf_pending_queue, PriorityQueue::new());

            // Recalculate priority for each block hash and update the queue
            for (block_hash, mut task) in old_queue {
                let new_priority = self.calculate_priority(&task.0.block);
                task.0.priority = new_priority.0;
                self.vdf_pending_queue.push(block_hash, task);
            }
        }

        {
            // Create a new priority queue with updated priorities
            let mut new_validations: PriorityQueue<irys_types::H256, Reverse<BlockPriorityMeta>> =
                PriorityQueue::new();

            // Recalculate priority for each block hash and update the queue
            for (block_hash, old_priority) in self.concurrent_queue.iter() {
                let new_priority = self.calculate_priority(&old_priority.0.block);
                new_validations.push(*block_hash, new_priority);
            }

            // Replace the old priority queue with the updated one
            self.concurrent_queue = new_validations;

            debug!(
                validation_count = self.concurrent_len(),
                "completed priority reevaluation after reorg"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_tree_service::test_utils::genesis_tree;
    use futures::future::{pending, ready};
    use irys_domain::{dummy_ema_snapshot, dummy_epoch_snapshot, BlockState, CommitmentSnapshot};
    use irys_types::{IrysBlockHeader, H256};
    use itertools::Itertools as _;
    use std::collections::HashMap;
    use std::sync::Arc;
    use test_log::test;
    use tokio::time::{sleep, Duration};

    /// Create a mock future that completes immediately
    fn create_ready_future() -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(ready(()))
    }

    /// Create a mock future that never completes
    fn create_pending_future() -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(pending())
    }

    /// Create a mock future that completes after a delay (unused but kept for potential future tests)
    #[expect(dead_code)]
    fn create_delayed_future(delay_ms: u64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            sleep(Duration::from_millis(delay_ms)).await;
        })
    }

    #[track_caller]
    fn get_block_from_blocks(
        blocks: &[IrysBlockHeader],
        block_hash: BlockHash,
    ) -> Arc<IrysBlockHeader> {
        Arc::new(
            blocks
                .iter()
                .find(|e| e.block_hash == block_hash)
                .expect("Block should exist")
                .clone(),
        )
    }

    /// Setup a canonical chain scenario for testing
    fn setup_canonical_chain_scenario(
        max_height: u64,
    ) -> (BlockTreeReadGuard, Vec<IrysBlockHeader>) {
        let mut blocks = (0..=max_height)
            .map(|height| {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.cumulative_diff = height.into();
                (header, ChainState::Onchain)
            })
            .collect::<Vec<_>>();
        let guard = genesis_tree(&mut blocks);

        // Mark the last block as the canonical tip
        if max_height > 0 {
            let last_block_hash = blocks.last().unwrap().0.block_hash;
            guard.write().mark_tip(&last_block_hash).unwrap();
        }

        (guard, blocks.into_iter().map(|(blk, _)| blk).collect())
    }

    /// Tests priority ordering with blocks added in sequential height order.
    /// Setup: Canonical chain (0-50), add blocks at heights [10, 20, 30, 40] in order.
    /// Expected: Priority queue returns blocks in same order (lowest height first).
    /// Verifies: Basic priority ordering works correctly with sequential input.
    #[test(tokio::test)]
    async fn test_priority_ordering_sequential_input() {
        // Setup canonical chain with blocks at different heights
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(50);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Add blocks in sequential order
        let heights = vec![10, 20, 30, 40];
        let mut expected_hashes = Vec::new();

        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            expected_hashes.push(block_hash);
            active_validations.push_concurrent_fut(
                get_block_from_blocks(&blocks, block_hash),
                create_pending_future(),
            );
        }

        // Verify priority ordering - lower heights should have higher priority
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.concurrent_queue.pop() {
            actual_order.push(hash);
        }

        // Should be in order: height 10, 20, 30, 40 (lowest height first)
        assert_eq!(actual_order, expected_hashes);
    }

    /// Tests priority ordering with blocks added in reverse height order.
    /// Setup: Canonical chain (0-50), add blocks at heights [40, 30, 20, 10] in reverse order.
    /// Expected: Priority queue returns blocks in ascending height order [10, 20, 30, 40].
    /// Verifies: Priority ordering is independent of insertion order.
    #[test(tokio::test)]
    async fn test_priority_ordering_reverse_input() {
        // Setup canonical chain
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(50);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Add blocks in reverse order
        let heights = vec![40, 30, 20, 10];
        let mut block_hashes = Vec::new();

        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            block_hashes.push(block_hash);
            active_validations.push_concurrent_fut(
                get_block_from_blocks(&blocks, block_hash),
                create_pending_future(),
            );
        }

        // Verify priority ordering - should still be by height regardless of input order
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.concurrent_queue.pop() {
            actual_order.push(hash);
        }

        // Should be in order: height 10, 20, 30, 40 (lowest height first)
        // Which corresponds to reverse of input order
        block_hashes.reverse();
        assert_eq!(actual_order, block_hashes);
    }

    /// Tests priority ordering with blocks added in random height order.
    /// Setup: Canonical chain (0-25), add 10 blocks at random heights in arbitrary order.
    /// Expected: Priority queue returns blocks sorted by ascending height.
    /// Verifies: Priority ordering handles random insertion patterns correctly.
    #[test(tokio::test)]
    async fn test_priority_ordering_random_input() {
        // Setup canonical chain
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(25);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Create blocks at random heights
        let heights = vec![15, 3, 22, 8, 12, 1, 18, 25, 7, 14];
        let mut height_to_hash = HashMap::new();

        // Add blocks in random order
        for &height in &heights {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            height_to_hash.insert(height, block_hash);
            active_validations.push_concurrent_fut(
                get_block_from_blocks(&blocks, block_hash),
                create_pending_future(),
            );
        }

        // Verify blocks come out in height order
        let mut sorted_heights = heights;
        sorted_heights.sort();

        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.concurrent_queue.pop() {
            actual_order.push(hash);
        }

        let expected_order: Vec<BlockHash> = sorted_heights
            .iter()
            .map(|&height| height_to_hash[&height])
            .collect();

        assert_eq!(actual_order, expected_order);
    }

    /// Tests priority ordering with fork blocks vs canonical extensions.
    /// Setup: Canonical chain (0-20), fork blocks (11,12) from block 10, extensions (21,22) from block 20.
    /// Expected: Extensions have higher priority than forks: [21, 22, 11, 12].
    /// Verifies: CanonicalExtension > Fork priority, and height ordering within each type.
    #[test(tokio::test)]
    async fn test_priority_with_fork_scenarios() {
        // Setup scenario with main chain (0-20 canonical)
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(20);

        // Manually create fork blocks (11, 12) and extension blocks (21, 22)
        let (fork_block_11, fork_block_12, extension_block_21, extension_block_22) = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();

            // Get block 10 as fork parent and block 20 as extension parent
            let fork_parent = canonical_chain
                .iter()
                .find(|entry| entry.height == 10)
                .expect("Block 10 should exist");
            let extension_parent = canonical_chain
                .iter()
                .find(|entry| entry.height == 20)
                .expect("Block 20 should exist");

            // Create fork block 11 (child of block 10)
            let mut fork_block_11 = IrysBlockHeader::new_mock_header();
            fork_block_11.height = 11;
            fork_block_11.previous_block_hash = fork_parent.block_hash;
            fork_block_11.block_hash = H256::random();
            fork_block_11.cumulative_diff = 50.into(); // Lower than canonical to stay as fork

            // Create fork block 12 (child of fork block 11)
            let mut fork_block_12 = IrysBlockHeader::new_mock_header();
            fork_block_12.height = 12;
            fork_block_12.previous_block_hash = fork_block_11.block_hash;
            fork_block_12.block_hash = H256::random();
            fork_block_12.cumulative_diff = 60.into(); // Lower than canonical to stay as fork

            // Create extension block 21 (child of block 20)
            let mut extension_block_21 = IrysBlockHeader::new_mock_header();
            extension_block_21.height = 21;
            extension_block_21.previous_block_hash = extension_parent.block_hash;
            extension_block_21.block_hash = H256::random();
            extension_block_21.cumulative_diff = 10000.into(); // Higher than canonical for extension

            // Create extension block 22 (child of extension block 21)
            let mut extension_block_22 = IrysBlockHeader::new_mock_header();
            extension_block_22.height = 22;
            extension_block_22.previous_block_hash = extension_block_21.block_hash;
            extension_block_22.block_hash = H256::random();
            extension_block_22.cumulative_diff = 10001.into(); // Higher than canonical for extension

            // Add blocks to tree as NotOnchain
            tree.add_common(
                fork_block_11.block_hash,
                &fork_block_11,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                fork_block_12.block_hash,
                &fork_block_12,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                extension_block_21.block_hash,
                &extension_block_21,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();
            tree.add_common(
                extension_block_22.block_hash,
                &extension_block_22,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();

            (
                fork_block_11,
                fork_block_12,
                extension_block_21,
                extension_block_22,
            )
        };

        let mut active_validations = ActiveValidations::new(block_tree_guard);

        // Use the known block hashes from creation
        let fork_blocks = vec![(Arc::new(fork_block_11), 11), (Arc::new(fork_block_12), 12)];
        let extension_blocks = vec![
            (Arc::new(extension_block_21), 21),
            (Arc::new(extension_block_22), 22),
        ];

        // Add blocks to active validations in mixed order to test priority sorting
        for (block, _) in &fork_blocks {
            active_validations.push_concurrent_fut(block.clone(), create_pending_future());
        }
        for (block, _) in &extension_blocks {
            active_validations.push_concurrent_fut(block.clone(), create_pending_future());
        }

        // Verify priority ordering
        let mut actual_order = Vec::new();
        while let Some((hash, priority)) = active_validations.concurrent_queue.pop() {
            actual_order.push((hash, priority.0));
        }

        // Expected: extensions first (21, 22), then forks (11, 12)
        let expected_order = [
            (
                extension_blocks[0].0.clone(),
                (
                    BlockPriority::CanonicalExtension,
                    Arc::clone(&extension_blocks[0].0),
                )
                    .into(),
            ),
            (
                extension_blocks[1].0.clone(),
                (
                    BlockPriority::CanonicalExtension,
                    Arc::clone(&extension_blocks[1].0),
                )
                    .into(),
            ),
            (
                fork_blocks[0].0.clone(),
                (BlockPriority::Fork, Arc::clone(&fork_blocks[0].0)).into(),
            ),
            (
                fork_blocks[1].0.clone(),
                (BlockPriority::Fork, Arc::clone(&fork_blocks[1].0)).into(),
            ),
        ];

        assert_eq!(actual_order.len(), expected_order.len());
        for (i, ((actual_hash, actual_priority), (expected_block, expected_priority))) in
            actual_order
                .iter()
                .zip_eq(expected_order.iter())
                .enumerate()
        {
            assert_eq!(
                *actual_hash, expected_block.block_hash,
                "Hash mismatch at position {}",
                i
            );
            assert_eq!(
                actual_priority, expected_priority,
                "Priority mismatch at position {}",
                i
            );
        }
    }

    /// Tests priority ordering with large number of concurrent validations.
    /// Setup: Canonical chain (0-100), add 50 blocks in reverse order to simulate load.
    /// Expected: All blocks returned in ascending height order despite reverse insertion.
    /// Verifies: Priority queue scales correctly with many concurrent validations.
    #[test(tokio::test)]
    async fn test_many_pending_tasks_capacity_management() {
        // Setup large canonical chain
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(100);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Use the available heights from the chain
        let mut height_to_hash = HashMap::new();
        let heights: Vec<u64>;

        // Collect all block hashes first
        {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();

            // Use the first 50 heights from the actual chain
            heights = chain.iter().take(50).map(|entry| entry.height).collect();

            for entry in chain.iter().take(50) {
                height_to_hash.insert(entry.height, entry.block_hash);
            }
        }

        // Add blocks in shuffled order
        let mut shuffled_heights = heights.clone();
        shuffled_heights.reverse(); // Simple reverse as shuffle

        for &height in &shuffled_heights {
            let block_hash = height_to_hash[&height];
            active_validations.push_concurrent_fut(
                get_block_from_blocks(&blocks, block_hash),
                create_pending_future(),
            );
        }

        // Verify all blocks are present
        assert_eq!(active_validations.concurrent_len(), heights.len());

        // Verify they come out in correct priority order
        let mut actual_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.concurrent_queue.pop() {
            actual_order.push(hash);
        }

        let expected_order: Vec<BlockHash> = heights
            .iter()
            .map(|&height| height_to_hash[&height])
            .collect();

        assert_eq!(actual_order, expected_order);
    }

    /// Tests that completed validation removal preserves priority ordering.
    /// Setup: Add 4 blocks with alternating ready/pending futures at heights [5, 10, 15, 20].
    /// Expected: Ready futures removed (heights 5, 15), remaining blocks [10, 20] in priority order.
    /// Verifies: process_completed() maintains priority ordering for remaining validations.
    #[test(tokio::test)]
    async fn test_process_completed_preserves_priority() {
        // Setup canonical chain
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(30);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());
        let chain = {
            let tree = block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            chain
        };
        // Add mix of ready and pending futures
        let heights = [5, 10, 15, 20];
        let mut height_to_hash = HashMap::new();

        for (i, &height) in heights.iter().enumerate() {
            let block_hash = chain
                .iter()
                .find(|entry| entry.height == height)
                .map(|entry| entry.block_hash)
                .expect("Block should exist");

            height_to_hash.insert(height, block_hash);

            // Alternate between ready and pending futures
            let future = if i % 2 == 0 {
                create_ready_future()
            } else {
                create_pending_future()
            };

            active_validations
                .push_concurrent_fut(get_block_from_blocks(&blocks, block_hash), future);
        }

        // Process completed validations
        active_validations.process_completed_concurrent().await;

        // Should have removed the ready futures (heights 5 and 15)
        assert_eq!(active_validations.concurrent_len(), 2);

        // Remaining blocks should still be in priority order
        let mut remaining_order = Vec::new();
        while let Some((hash, _priority)) = active_validations.concurrent_queue.pop() {
            remaining_order.push(hash);
        }

        // Should be height 10, then height 20
        assert_eq!(remaining_order[0], height_to_hash[&10]);
        assert_eq!(remaining_order[1], height_to_hash[&20]);
    }

    /// Tests edge cases: empty queue and genesis block handling.
    /// Setup: Empty queue, then add genesis block (height 0).
    /// Expected: Empty operations succeed, genesis gets Canonical(0) priority.
    /// Verifies: Edge cases handled gracefully without panics or errors.
    #[test(tokio::test)]
    async fn test_edge_cases() {
        // Test with empty validation queue
        let (block_tree_guard, blocks) = setup_canonical_chain_scenario(10);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        assert!(active_validations.concurrent_is_empty());
        assert_eq!(active_validations.concurrent_len(), 0);

        // Process completed on empty queue should not panic
        active_validations.process_completed_concurrent().await;
        assert!(active_validations.concurrent_is_empty());

        // Test with genesis block
        let tree = block_tree_guard.read();
        let (chain, _) = tree.get_canonical_chain();
        let genesis_hash = chain[0].block_hash;
        let genesis_block = get_block_from_blocks(&blocks, genesis_hash);

        active_validations.push_concurrent_fut(Arc::clone(&genesis_block), create_pending_future());

        // Genesis block should have priority based on height 0 and Canonical status
        let priority = active_validations.calculate_priority(&genesis_block);
        assert_eq!(
            priority,
            std::cmp::Reverse((BlockPriority::Canonical, genesis_block).into())
        );

        assert_eq!(active_validations.concurrent_len(), 1);
        assert!(!active_validations.concurrent_is_empty());
    }

    /// Tests BlockPriority enum ordering and Reverse wrapper behavior.
    /// Setup: Compare different BlockPriority variants and heights.
    /// Expected: CanonicalExtension < Canonical < Fork, lower heights < higher heights.
    /// Verifies: Enum derives correct Ord implementation for priority queue usage.
    #[test]
    fn test_block_priority_ordering() {
        let block = Arc::new(IrysBlockHeader::new_mock_header());

        let mkprio = |state: BlockPriority, height: u64, vdf_steps: u64| BlockPriorityMeta {
            height,
            state,
            vdf_step_count: vdf_steps,
            block: Arc::clone(&block),
        };

        // Test that enum variants have correct ordering
        assert!(
            mkprio(BlockPriority::CanonicalExtension, 10, 0)
                < mkprio(BlockPriority::Canonical, 5, 0)
        );
        assert!(mkprio(BlockPriority::Canonical, 10, 0) < mkprio(BlockPriority::Fork, 5, 0));
        assert!(
            mkprio(BlockPriority::CanonicalExtension, 10, 0) < mkprio(BlockPriority::Fork, 5, 0)
        );

        // Test within same variant, lower heights have higher priority
        assert!(
            mkprio(BlockPriority::CanonicalExtension, 10, 0)
                < mkprio(BlockPriority::CanonicalExtension, 11, 0)
        );
        assert!(mkprio(BlockPriority::Canonical, 10, 0) < mkprio(BlockPriority::Canonical, 11, 0));
        assert!(mkprio(BlockPriority::Fork, 10, 0) < mkprio(BlockPriority::Fork, 11, 0));

        // Test with Reverse wrapper to ensure priority queue ordering is correct
        assert!(
            Reverse(mkprio(BlockPriority::CanonicalExtension, 10, 0))
                > Reverse(mkprio(BlockPriority::Canonical, 10, 0))
        );
        assert!(
            Reverse(mkprio(BlockPriority::Canonical, 10, 0))
                > Reverse(mkprio(BlockPriority::Fork, 10, 0))
        );
        assert!(
            Reverse(mkprio(BlockPriority::CanonicalExtension, 10, 0))
                > Reverse(mkprio(BlockPriority::Fork, 10, 0))
        );

        // In a priority queue, lower Reverse values have higher priority
        assert!(
            Reverse(mkprio(BlockPriority::CanonicalExtension, 10, 0))
                > Reverse(mkprio(BlockPriority::CanonicalExtension, 11, 0))
        );
        assert!(
            Reverse(mkprio(BlockPriority::Canonical, 10, 0))
                > Reverse(mkprio(BlockPriority::Canonical, 11, 0))
        );
        assert!(
            Reverse(mkprio(BlockPriority::Fork, 10, 0))
                > Reverse(mkprio(BlockPriority::Fork, 11, 0))
        );

        // create a priority queue
        let mut queue: PriorityQueue<BlockHash, Reverse<BlockPriorityMeta>> = PriorityQueue::new();
        let expected_order = [
            Reverse(mkprio(BlockPriority::CanonicalExtension, 9, 0)),
            Reverse(mkprio(BlockPriority::CanonicalExtension, 10, 0)),
            Reverse(mkprio(BlockPriority::CanonicalExtension, 10, 9999)),
            Reverse(mkprio(BlockPriority::Canonical, 9, 1)), // should not be prioritised despite being height 9 & having just one step
        ];
        for prio in expected_order.iter() {
            queue.push(BlockHash::random(), prio.clone());
        }

        for (idx, itm) in queue.into_sorted_iter().enumerate() {
            assert_eq!(*(expected_order.get(idx).unwrap()), itm.1)
        }
    }

    /// Tests priority reevaluation when a fork becomes the canonical chain.
    /// Setup: Canonical chain (0-3), canonical extensions (4-5), and fork chain (3-10) from height 2.
    /// Action: Make fork chain canonical by marking blocks 3-5 as canonical tip sequentially.
    /// Expected: Extension blocks (4-5) become Fork, fork blocks (3-5) become Canonical,
    ///          remaining fork blocks (6-10) become CanonicalExtension.
    /// Verifies: reevaluate_priorities() correctly recalculates all block priorities after reorg.
    #[test(tokio::test)]
    async fn test_reevaluate_priorities_after_fork_becomes_canonical() {
        // Setup: Create initial canonical chain (height 0-3)
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let mut active_validations = ActiveValidations::new(block_tree_guard.clone());

        // Create canonical extension blocks (extending from canonical tip at height 3)
        let extension_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let tip = canonical_chain.last().unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = tip.block_hash;

            for height in 4..=5 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.block_hash = H256::random();
                header.cumulative_diff = height.into();
                last_hash = header.block_hash;

                tree.add_common(
                    header.block_hash,
                    &header,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::NotOnchain(BlockState::ValidationScheduled),
                )
                .unwrap();

                blocks.push(Arc::new(header));
            }
            blocks
        };

        // Add extension blocks to active validations
        for block in &extension_blocks {
            active_validations.push_concurrent_fut(block.clone(), create_pending_future());
        }

        // Verify initial priorities - extension blocks should be CanonicalExtension
        for block in &extension_blocks {
            let priority = active_validations.calculate_priority(&block.clone());
            assert_eq!(
                priority,
                Reverse((BlockPriority::CanonicalExtension, block.clone()).into())
            );
        }

        // Create fork blocks (extending from height 2, creating alternative chain)
        let fork_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let fork_parent = canonical_chain.iter().find(|e| e.height == 2).unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = fork_parent.block_hash;

            for height in 3..=10 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.block_hash = H256::random();
                header.cumulative_diff = height.into();
                last_hash = header.block_hash;

                tree.add_common(
                    header.block_hash,
                    &header,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::NotOnchain(BlockState::ValidationScheduled),
                )
                .unwrap();

                blocks.push(Arc::new(header));
            }
            blocks
        };

        // Add fork blocks to active validations
        for block in &fork_blocks {
            active_validations.push_concurrent_fut(block.clone(), create_pending_future());
        }

        // Action: Make the fork chain canonical by marking blocks as valid and advancing tip
        // This simulates a reorganization where the fork becomes the canonical chain
        {
            let mut tree = block_tree_guard.write();

            // Mark one of the later fork blocks as valid to enable tip advancement
            tree.mark_block_as_valid(&fork_blocks[6].block_hash)
                .unwrap();

            // Advance the canonical tip through the fork chain (blocks 3-5)
            // This makes the fork chain canonical up to height 5
            for i in 0..=5 {
                tree.mark_tip(&fork_blocks[i].block_hash).unwrap();
            }
        }

        // Action: Reevaluate priorities after the reorganization
        active_validations.reevaluate_priorities();

        // Verify: Extension blocks (4-5) are now Fork priority (no longer extend canonical tip)
        for block in &extension_blocks {
            let priority = active_validations
                .concurrent_queue
                .get_priority(&block.block_hash)
                .unwrap();
            assert_eq!(
                priority,
                &Reverse((BlockPriority::Fork, block.clone()).into()),
                "Extension block at height {} should now be Fork priority",
                block.height
            );
        }

        // Verify: Fork blocks 3-5 are now Canonical priority (part of canonical chain)
        for block in &fork_blocks[..6] {
            let priority = active_validations
                .concurrent_queue
                .get_priority(&block.block_hash)
                .unwrap();
            assert_eq!(
                priority,
                &Reverse((BlockPriority::Canonical, block.clone()).into()),
                "Fork block at height {} should now be Canonical priority",
                block.height
            );
        }

        // Verify: Remaining fork blocks (6-10) are now CanonicalExtension priority
        for block in &fork_blocks[6..] {
            let priority = active_validations
                .concurrent_queue
                .get_priority(&block.block_hash)
                .unwrap();
            assert_eq!(
                priority,
                &Reverse((BlockPriority::CanonicalExtension, block.clone()).into()),
                "Fork block at height {} should now be CanonicalExtension priority",
                block.height
            );
        }
    }
}
