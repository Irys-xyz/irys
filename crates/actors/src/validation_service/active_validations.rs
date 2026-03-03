//! Priority-based block validation task scheduling.
//!
//! ## High-Level Flow
//!
//! 1. New block enters VDF validation (sequential, preemptible)
//! 2. Valid blocks proceed to concurrent validation (parallel: recall, PoA, shadow txs)
//! 3. Validated blocks wait for parent validation to complete
//! 4. Results are reported to the block tree service
//!
//! ## Priority System
//!
//! Blocks are prioritized by: canonical extension > canonical > fork > unknown,
//! then by height (lower first) and VDF steps (fewer first).

use irys_domain::{BlockTree, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, IrysBlockHeader, SealedBlock};
use irys_vdf::state::CancelEnum;
use priority_queue::PriorityQueue;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::{
    sync::Notify,
    task::{JoinHandle, JoinSet},
};
use tracing::{Instrument as _, debug, error, info, instrument, warn};

use crate::block_tree_service::ValidationResult;
use crate::validation_service::VdfValidationResult;
use crate::validation_service::block_validation_task::BlockValidationTask;

/// Block priority states for validation ordering
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum BlockPriority {
    /// Unknown/orphan blocks (lowest priority)
    Unknown,
    /// Fork blocks that don't extend the canonical tip (low priority)
    Fork,
    /// Canonical blocks already on chain (medium priority)
    Canonical,
    /// Canonical extensions that extend from the canonical tip (highest priority)
    CanonicalExtension,
}

/// Metadata struct that is used to inform block validation priority decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockPriorityMeta {
    pub height: u64,
    pub state: BlockPriority,
    pub vdf_step_count: u64,
}

impl BlockPriorityMeta {
    pub(super) fn new(block: &IrysBlockHeader, state: BlockPriority) -> Self {
        Self {
            height: block.height,
            state,
            vdf_step_count: block.vdf_limiter_info.steps.len() as u64,
        }
    }
}

impl Ord for BlockPriorityMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First compare by state (CanonicalExtension > Canonical > Fork > Unknown)
        self.state
            .cmp(&other.state)
            // Then by height (lower height = higher priority, so reverse the comparison)
            .then_with(|| other.height.cmp(&self.height))
            // Finally by VDF steps (fewer steps = higher priority, so reverse the comparison)
            .then_with(|| other.vdf_step_count.cmp(&self.vdf_step_count))
    }
}

impl PartialOrd for BlockPriorityMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Result from a concurrent validation task
#[derive(Debug)]
pub(super) struct ConcurrentValidationResult {
    pub block_hash: BlockHash,
    pub validation_result: ValidationResult,
}

/// VDF task with preemption support
pub(super) struct PreemptibleVdfTask {
    pub task: BlockValidationTask,
    pub cancel_u8: Arc<std::sync::atomic::AtomicU8>,
    vdf_notify: Arc<Notify>,
}

impl PreemptibleVdfTask {
    #[instrument(skip_all, fields(block.hash = %self.task.sealed_block.header().block_hash))]
    pub(super) async fn execute(self) -> (VdfValidationResult, BlockValidationTask) {
        let inner = Arc::clone(&self.task.service_inner);
        let header = self.task.sealed_block.header();
        let skip_vdf = self.task.skip_vdf_validation;

        // No bridge task needed - just use the AtomicU8 directly!
        let result = match inner
            .ensure_vdf_is_valid(header, self.cancel_u8.clone(), skip_vdf)
            .await
        {
            Ok(()) => VdfValidationResult::Valid,
            Err(e) => {
                // Check if we were cancelled by inspecting the AtomicU8
                if self.cancel_u8.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
                    VdfValidationResult::Cancelled
                } else {
                    VdfValidationResult::Invalid(e)
                }
            }
        };

        // Notify the main loop that VDF task completed so it can poll the result
        self.vdf_notify.notify_one();

        (result, self.task)
    }
}

/// Currently running VDF task
pub(super) struct CurrentVdfTask {
    pub hash: BlockHash,
    pub priority: BlockPriorityMeta,
    pub cancel_signal: Arc<std::sync::atomic::AtomicU8>,
    pub handle: JoinHandle<(VdfValidationResult, BlockValidationTask)>,
    pub sealed_block: Arc<SealedBlock>,
}

/// Simplified VDF scheduler with preemption
pub(super) struct VdfScheduler {
    /// Currently running VDF task
    pub current: Option<CurrentVdfTask>,

    /// Pending VDF tasks
    pub pending: PriorityQueue<BlockValidationTask, BlockPriorityMeta>,

    /// VDF task completion notifier
    vdf_notify: Arc<Notify>,
}

impl VdfScheduler {
    pub(super) fn new(vdf_notify: Arc<Notify>) -> Self {
        Self {
            current: None,
            pending: PriorityQueue::new(),
            vdf_notify,
        }
    }

    /// Submit a VDF task
    #[instrument(skip_all, fields(block.hash = %task.sealed_block.header().block_hash, ?priority))]
    pub(super) fn submit(&mut self, task: BlockValidationTask, priority: BlockPriorityMeta) {
        let hash = task.sealed_block.header().block_hash;

        // Check for duplicates
        if self.pending.get(&task).is_some() {
            return;
        }

        // Check if current task exists
        if let Some(current) = &self.current
            && current.hash == hash
        {
            return;
        }

        self.pending.push(task, priority);

        // Check if we should preempt current task
        self.check_preemption();

        // Notify the main loop to process the task
        self.vdf_notify.notify_one();
    }

    /// Check if current task should be preempted by higher priority pending task
    pub(super) fn check_preemption(&self) {
        let Some(current) = &self.current else {
            return;
        };

        let Some((_, pending_priority)) = self.pending.peek() else {
            return;
        };

        // Only preempt if pending task has HIGHER priority
        if pending_priority > &current.priority {
            current
                .cancel_signal
                .store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
        }
    }

    /// Start next VDF task if none running
    #[instrument(skip_all)]
    pub(super) fn start_next(&mut self) -> Option<()> {
        if self.current.is_some() {
            return None; // Already running
        }

        let (task, priority) = self.pending.pop()?;
        let hash = task.sealed_block.header().block_hash;

        // Create AtomicU8 for cancellation
        let cancel_u8 = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        let sealed_block = Arc::clone(&task.sealed_block);
        let preemptible = PreemptibleVdfTask {
            task,
            cancel_u8: Arc::clone(&cancel_u8),
            vdf_notify: Arc::clone(&self.vdf_notify),
        };
        let handle = tokio::spawn(
            preemptible
                .execute()
                .instrument(tracing::info_span!(
                    "vdf_validation",
                    block.hash = %hash,
                    block.priority = ?priority
                ))
                .in_current_span(),
        );
        self.current = Some(CurrentVdfTask {
            hash,
            priority,
            sealed_block,
            cancel_signal: cancel_u8,
            handle,
        });
        Some(())
    }

    /// Poll current VDF task
    #[instrument(skip_all)]
    pub(super) async fn poll_current(
        &mut self,
    ) -> Option<(BlockHash, VdfValidationResult, BlockValidationTask)> {
        let mut current = self.current.take()?;

        // Use poll_immediate to check without blocking
        let poll_result = futures::future::poll_immediate(&mut current.handle).await;

        match poll_result {
            Some(Ok((result, task))) => {
                // Task completed - no need to notify here since task already did
                Some((current.hash, result, task))
            }
            Some(Err(e)) => {
                error!("VDF task panicked: {}", e);
                // We lost the task on panic, cannot continue
                // Notify to ensure we can start the next task
                self.vdf_notify.notify_one();
                None
            }
            None => {
                // Still running, put it back
                self.current = Some(current);
                None
            }
        }
    }
}

/// Main validation coordinator
pub(super) struct ValidationCoordinator {
    /// VDF validation scheduler
    pub vdf_scheduler: VdfScheduler,

    /// Concurrent validation tasks
    pub concurrent_tasks: JoinSet<ConcurrentValidationResult>,

    /// Maps task IDs to block hashes for panic diagnostics
    pub concurrent_task_blocks: HashMap<tokio::task::Id, BlockHash>,

    /// Block tree for priority calculation
    pub block_tree_guard: BlockTreeReadGuard,
}

impl ValidationCoordinator {
    pub(super) fn new(block_tree_guard: BlockTreeReadGuard, vdf_notify: Arc<Notify>) -> Self {
        Self {
            vdf_scheduler: VdfScheduler::new(Arc::clone(&vdf_notify)),
            concurrent_tasks: JoinSet::new(),
            concurrent_task_blocks: HashMap::new(),
            block_tree_guard,
        }
    }

    /// Calculate priority for a block
    #[instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = %block.height))]
    pub(super) fn calculate_priority(&self, block: &IrysBlockHeader) -> BlockPriorityMeta {
        let block_tree = self.block_tree_guard.read();
        let block_hash = block.block_hash;

        let state = match block_tree.get_block_and_status(&block_hash) {
            Some((_, ChainState::Onchain)) => BlockPriority::Canonical,
            Some((_, ChainState::NotOnchain(_) | ChainState::Validated(_))) => {
                if self.is_canonical_extension(&block_hash, &block_tree) {
                    BlockPriority::CanonicalExtension
                } else {
                    BlockPriority::Fork
                }
            }
            None => BlockPriority::Unknown,
        };

        BlockPriorityMeta::new(block, state)
    }

    /// Check if block extends canonical tip
    #[instrument(level = "trace", skip_all, fields(block.hash = %block_hash))]
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTree) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash();

        let mut current = *block_hash;
        while let Some((block, _)) = block_tree.get_block_and_status(&current) {
            if current == canonical_tip {
                return true;
            }
            current = block.previous_block_hash;

            if let Some((_, ChainState::Onchain)) = block_tree.get_block_and_status(&current) {
                return current == canonical_tip;
            }
        }
        false
    }

    /// Submit a validation task
    #[instrument(skip_all, fields(block.hash = %task.sealed_block.header().block_hash, block.height = %task.sealed_block.header().height))]
    pub(super) fn submit_task(&mut self, task: BlockValidationTask) {
        let priority = self.calculate_priority(task.sealed_block.header());
        self.vdf_scheduler.submit(task, priority);
    }

    /// Process VDF completion
    #[instrument(skip_all)]
    pub(super) async fn process_vdf(&mut self) -> Option<(BlockHash, VdfValidationResult)> {
        // Poll current VDF task
        if let Some((hash, result, task)) = self.vdf_scheduler.poll_current().await {
            match &result {
                VdfValidationResult::Valid => {
                    let block_hash = task.sealed_block.header().block_hash;

                    let abort_handle = self.concurrent_tasks.spawn(
                        async move {
                            let validation_result = task.execute_concurrent().await;

                            ConcurrentValidationResult {
                                block_hash,
                                validation_result,
                            }
                        }
                        .instrument(tracing::error_span!(
                            "concurrent_validation",
                            block.hash = %block_hash
                        ))
                        .in_current_span(),
                    );
                    self.concurrent_task_blocks
                        .insert(abort_handle.id(), block_hash);
                }
                VdfValidationResult::Cancelled => {
                    // Re-queue the cancelled task with recalculated priority
                    let priority = self.calculate_priority(task.sealed_block.header());
                    self.vdf_scheduler.pending.push(task, priority);
                }
                VdfValidationResult::Invalid(error) => {
                    tracing::error!(block.hash =? hash, "invalid vdf {error}");
                    // Invalid tasks are not re-queued
                }
            }

            // Start next VDF task
            self.vdf_scheduler.start_next();
            return Some((hash, result));
        }

        // Try to start a VDF task if none running
        self.vdf_scheduler.start_next();
        None
    }

    /// Reevaluate all priorities after reorg
    #[instrument(level = "trace", skip_all)]
    pub(super) fn reevaluate_priorities(&mut self) {
        info!("Reevaluating priorities after reorg");

        // Reevaluate current VDF task
        self.reevaluate_current_vdf();

        // Reevaluate pending VDF tasks
        self.reevaluate_pending_vdf();
    }

    /// Reevaluate and potentially preempt current VDF task
    fn reevaluate_current_vdf(&mut self) {
        let Some(current) = &self.vdf_scheduler.current else {
            return;
        };

        // Calculate new priority (block is already a reference)
        let new_priority = self.calculate_priority(current.sealed_block.header());

        if new_priority == current.priority {
            return; // No change
        }

        debug!(
            block.hash = %current.hash,
            block.priority.old = ?current.priority,
            block.priority.new = ?new_priority,
            "Current VDF task priority changed after reorg"
        );

        // Update priority and check for preemption
        if let Some(current_mut) = &mut self.vdf_scheduler.current {
            current_mut.priority = new_priority;
        }

        // Use consolidated preemption check
        self.vdf_scheduler.check_preemption();
    }

    /// Reevaluate pending VDF task priorities
    fn reevaluate_pending_vdf(&mut self) {
        // Collect tasks that need priority updates (can't mutate while iterating)
        let tasks_to_update: Vec<_> = self
            .vdf_scheduler
            .pending
            .iter()
            .map(|(task, _priority)| task.clone())
            .collect();

        if tasks_to_update.is_empty() {
            return;
        }

        let mut updated_count = 0;
        for task in tasks_to_update {
            let new_priority = self.calculate_priority(task.sealed_block.header());
            // update_priority returns true if the item existed and was updated
            if self
                .vdf_scheduler
                .pending
                .change_priority(&task, new_priority)
                .is_some()
            {
                updated_count += 1;
            }
        }

        if updated_count > 0 {
            debug!(
                vdf.pending_updated = updated_count,
                "Reevaluated VDF pending task priorities after reorg"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::{
        BlockState, BlockTree, BlockTreeReadGuard, ChainState, CommitmentSnapshot,
        dummy_ema_snapshot, dummy_epoch_snapshot,
    };
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::{BlockBody, BlockHash, IrysBlockHeader, serialization::H256List};
    use priority_queue::PriorityQueue;
    use std::sync::{Arc, RwLock};

    /// Test that BlockPriorityMeta ordering works correctly with manual Ord
    #[test]
    fn test_validation_priority_ordering() {
        let mut header1 = IrysBlockHeader::new_mock_header();
        header1.height = 100;
        header1.vdf_limiter_info.steps = H256List(vec![Default::default(); 5]); // 5 VDF steps

        let mut header2 = IrysBlockHeader::new_mock_header();
        header2.height = 200;
        header2.vdf_limiter_info.steps = H256List(vec![Default::default(); 10]); // 10 VDF steps

        // Test 1: Canonical extension should have highest priority
        let p1 = BlockPriorityMeta::new(&header1, BlockPriority::CanonicalExtension);
        let p2 = BlockPriorityMeta::new(&header2, BlockPriority::Canonical);
        assert!(
            p1 > p2,
            "Canonical extension should have higher priority than canonical"
        );

        // Test 2: Among same type, lower height should have higher priority
        let p3 = BlockPriorityMeta::new(&header1, BlockPriority::Fork);
        let p4 = BlockPriorityMeta::new(&header2, BlockPriority::Fork);
        assert!(p3 > p4, "Lower height should have higher priority");

        // Test 3: Canonical should have higher priority than fork
        let p5 = BlockPriorityMeta::new(&header1, BlockPriority::Canonical);
        let p6 = BlockPriorityMeta::new(&header1, BlockPriority::Fork);
        assert!(p5 > p6, "Canonical should have higher priority than fork");

        // Test 4: Test BlockPriority enum ordering (higher priority > lower priority)
        assert!(BlockPriority::CanonicalExtension > BlockPriority::Canonical);
        assert!(BlockPriority::Canonical > BlockPriority::Fork);
        assert!(BlockPriority::Fork > BlockPriority::Unknown);

        // Test 5: VDF step count as tiebreaker (fewer steps = higher priority)
        let mut header3 = header1.clone();
        header3.vdf_limiter_info.steps = H256List(vec![Default::default(); 20]); // More steps
        let p7 = BlockPriorityMeta::new(&header1, BlockPriority::Fork); // 5 steps
        let p8 = BlockPriorityMeta::new(&header3, BlockPriority::Fork); // 20 steps
        assert!(p7 > p8, "Fewer VDF steps should have higher priority");
    }

    /// Tests BlockPriority ordering with PriorityQueue to ensure correct behavior.
    /// Setup: Create priorities with different states, heights, and VDF steps.
    /// Expected: Items popped in order of CanonicalExtension > Canonical > Fork,
    ///          with lower heights and fewer VDF steps having higher priority.
    /// Verifies: PriorityQueue correctly uses BlockPriorityMeta ordering.
    #[test]
    fn test_priority_queue_ordering() {
        // Create headers with different properties
        let mkheader = |height: u64, vdf_steps: usize| {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.block_hash = BlockHash::random();
            header.vdf_limiter_info.steps = H256List(vec![Default::default(); vdf_steps]);
            header
        };

        // Create priority metadata
        let mkprio =
            |header: &IrysBlockHeader, state: BlockPriority| BlockPriorityMeta::new(header, state);

        // Create a priority queue
        let mut queue: PriorityQueue<BlockHash, (BlockPriorityMeta, ())> = PriorityQueue::new();

        // Add items in random order
        let h1 = mkheader(10, 5);
        let h2 = mkheader(9, 10);
        let h3 = mkheader(10, 100);
        let h4 = mkheader(9, 1);

        // Expected order (highest priority first):
        // 1. CanonicalExtension, height 9, 10 VDF steps
        // 2. CanonicalExtension, height 10, 5 VDF steps
        // 3. CanonicalExtension, height 10, 100 VDF steps
        // 4. Canonical, height 9, 1 VDF step

        let items = [
            (
                h2.block_hash,
                mkprio(&h2, BlockPriority::CanonicalExtension),
            ),
            (
                h1.block_hash,
                mkprio(&h1, BlockPriority::CanonicalExtension),
            ),
            (
                h3.block_hash,
                mkprio(&h3, BlockPriority::CanonicalExtension),
            ),
            (h4.block_hash, mkprio(&h4, BlockPriority::Canonical)),
        ];

        // Insert in different order to test sorting
        queue.push(items[3].0, (items[3].1, ()));
        queue.push(items[1].0, (items[1].1, ()));
        queue.push(items[0].0, (items[0].1, ()));
        queue.push(items[2].0, (items[2].1, ()));

        // Pop items and verify order
        let result1 = queue.pop().unwrap();
        assert_eq!(
            result1.0, items[0].0,
            "First item should be CanonicalExtension with height 9"
        );

        let result2 = queue.pop().unwrap();
        assert_eq!(
            result2.0, items[1].0,
            "Second item should be CanonicalExtension with height 10, 5 steps"
        );

        let result3 = queue.pop().unwrap();
        assert_eq!(
            result3.0, items[2].0,
            "Third item should be CanonicalExtension with height 10, 100 steps"
        );

        let result4 = queue.pop().unwrap();
        assert_eq!(
            result4.0, items[3].0,
            "Fourth item should be Canonical with height 9"
        );
    }

    /// Helper function to setup a canonical chain scenario with n blocks
    fn setup_canonical_chain_scenario(
        max_height: u64,
    ) -> (BlockTreeReadGuard, Vec<Arc<IrysBlockHeader>>) {
        // Create genesis block
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.cumulative_diff = 0.into();
        genesis.test_sign();

        // Create block tree with genesis
        let mut block_tree = BlockTree::new(&genesis, irys_types::ConsensusConfig::testing());
        block_tree.mark_tip(&genesis.block_hash).unwrap();

        let mut blocks = vec![Arc::new(genesis.clone())];
        let mut last_hash = genesis.block_hash;

        // Create canonical chain
        for height in 1..=max_height {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.previous_block_hash = last_hash;
            header.cumulative_diff = height.into();
            header.test_sign();

            let sealed = Arc::new(
                SealedBlock::new(
                    header.clone(),
                    BlockBody {
                        block_hash: header.block_hash,
                        ..Default::default()
                    },
                )
                .expect("sealing block"),
            );
            block_tree
                .add_common(
                    header.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::Onchain,
                )
                .unwrap();

            block_tree.mark_tip(&header.block_hash).unwrap();
            last_hash = header.block_hash;
            blocks.push(Arc::new(header));
        }

        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        (block_tree_guard, blocks)
    }

    /// Tests priority calculation when a fork becomes the canonical chain.
    /// Setup: Canonical chain (0-3), canonical extensions (4-5), and fork chain (3-10) from height 2.
    /// Action: Make fork chain canonical by marking blocks 3-5 as canonical tip sequentially.
    /// Expected: Extension blocks (4-5) become Fork, fork blocks (3-5) become Canonical,
    ///          remaining fork blocks (6-10) become CanonicalExtension.
    /// Verifies: calculate_priority() correctly determines block priorities after reorg.
    #[test]
    fn test_priority_calculation_after_fork_becomes_canonical() {
        // Setup: Create initial canonical chain (height 0-3)
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let vdf_notify = Arc::new(Notify::new());
        let coordinator = ValidationCoordinator::new(block_tree_guard.clone(), vdf_notify);

        // Create canonical extension blocks (extending from canonical tip at height 3)
        let extension_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let tip = canonical_chain.last().unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = tip.block_hash();

            for height in 4..=5 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.cumulative_diff = height.into();
                header.test_sign();
                last_hash = header.block_hash;

                let sealed = Arc::new(
                    SealedBlock::new(
                        header.clone(),
                        BlockBody {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )
                    .expect("sealing block"),
                );
                tree.add_common(
                    header.block_hash,
                    &sealed,
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

        // Verify initial priorities - extension blocks should be CanonicalExtension
        for block in &extension_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::CanonicalExtension,
                "Extension block at height {} should be CanonicalExtension",
                block.height
            );
        }

        // First, let's add the extension blocks to the tree to establish them as part of the canonical extension
        // This ensures the fork blocks won't be seen as canonical extensions

        // Create fork blocks (extending from height 2, creating alternative chain)
        // These will compete with the canonical block at height 3
        let fork_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let fork_parent = canonical_chain.iter().find(|e| e.height() == 2).unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = fork_parent.block_hash();

            // Create an alternative block at height 3 (competing with canonical block at height 3)
            for height in 3..=10 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.cumulative_diff = height.into();
                header.test_sign();
                last_hash = header.block_hash;

                let sealed = Arc::new(
                    SealedBlock::new(
                        header.clone(),
                        BlockBody {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )
                    .expect("sealing block"),
                );
                tree.add_common(
                    header.block_hash,
                    &sealed,
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

        // Verify initial fork block priorities
        // All fork blocks will be CanonicalExtension because they form a chain
        // that extends from the canonical chain (at height 2) and creates a longer chain
        for block in &fork_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::CanonicalExtension,
                "Fork block at height {} is initially CanonicalExtension (extends from canonical chain)",
                block.height
            );
        }

        // Action: Make the fork chain canonical by marking blocks as valid and advancing tip
        {
            let mut tree = block_tree_guard.write();

            // Mark fork blocks as onchain to simulate them becoming canonical
            for i in 0..=5 {
                tree.mark_block_as_valid(&fork_blocks[i].block_hash)
                    .unwrap();
                tree.mark_tip(&fork_blocks[i].block_hash).unwrap();
            }
        }

        // Verify: Extension blocks (4-5) are now Fork priority (no longer extend canonical)
        for block in &extension_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::Fork,
                "Extension block at height {} should now be Fork priority after reorg",
                block.height
            );
        }

        // Verify: Fork blocks that are now on the canonical chain
        for (i, block) in fork_blocks.iter().enumerate() {
            let priority = coordinator.calculate_priority(block);

            if i <= 5 {
                // These blocks are now part of the canonical chain
                assert_eq!(
                    priority.state,
                    BlockPriority::Canonical,
                    "Fork block at height {} (index {}) should now be Canonical priority",
                    block.height,
                    i
                );
            } else {
                // These blocks extend the new canonical tip
                assert_eq!(
                    priority.state,
                    BlockPriority::CanonicalExtension,
                    "Fork block at height {} (index {}) should now be CanonicalExtension priority",
                    block.height,
                    i
                );
            }
        }
    }
}
