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
use irys_types::{BlockHash, IrysBlockHeader};
use irys_vdf::state::CancelEnum;
use priority_queue::PriorityQueue;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

use crate::block_tree_service::ValidationResult;
use crate::validation_service::block_validation_task::BlockValidationTask;
use crate::validation_service::VdfValidationResult;

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

/// Clean concurrent validation pool using JoinSet
pub(super) struct ConcurrentValidationPool {
    /// Active validation tasks
    pub tasks: JoinSet<ConcurrentValidationResult>,
}

impl ConcurrentValidationPool {
    pub(super) fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    /// Submit a task for validation - spawns immediately without limits
    #[instrument(skip_all, fields(block_hash = %task.block.block_hash))]
    pub fn submit(&mut self, task: BlockValidationTask, _priority: BlockPriorityMeta) {
        let block_hash = task.block.block_hash;

        debug!(
            block_hash = %block_hash,
            "Spawning concurrent validation"
        );

        self.tasks.spawn(
            async move {
                // Execute the validation and return the result
                let validation_result = task.execute_concurrent().await;

                ConcurrentValidationResult {
                    block_hash,
                    validation_result,
                }
            }
            .instrument(tracing::info_span!("concurrent_validation", block_hash = %block_hash))
            .in_current_span(),
        );
    }

    /// Poll for next completed task
    #[instrument(skip_all)]
    pub(super) async fn join_next(
        &mut self,
    ) -> Option<Result<ConcurrentValidationResult, tokio::task::JoinError>> {
        self.tasks.join_next().await
    }

    /// Reevaluate priorities after a reorg - no-op since we don't queue tasks
    #[instrument(skip_all)]
    pub(super) fn reevaluate_priorities<F>(&mut self, _recalc_fn: F)
    where
        F: Fn(&BlockHash) -> Option<BlockPriorityMeta>,
    {
        // No pending tasks to reevaluate since all tasks spawn immediately
        debug!("Reevaluate priorities called - no pending tasks to update");
    }
}

/// VDF task with preemption support
pub(super) struct PreemptibleVdfTask {
    pub task: BlockValidationTask,
    pub cancel_u8: Arc<std::sync::atomic::AtomicU8>,
}

impl PreemptibleVdfTask {
    #[instrument(skip_all, fields(block_hash = %self.task.block.block_hash))]
    pub(super) async fn execute(self) -> (VdfValidationResult, BlockValidationTask) {
        let inner = Arc::clone(&self.task.service_inner);
        let block = Arc::clone(&self.task.block);
        let skip_vdf = self.task.skip_vdf_validation;
        let vdf_notify = Arc::clone(&self.task.service_inner.vdf_notify);

        // No bridge task needed - just use the AtomicU8 directly!
        let result = match inner
            .ensure_vdf_is_valid(&block, self.cancel_u8.clone(), skip_vdf)
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

        // Notify that VDF task has completed so it can be collected
        vdf_notify.notify_one();

        (result, self.task)
    }
}

/// Simplified VDF scheduler with preemption
pub(super) struct VdfScheduler {
    /// Currently running VDF task with priority and cancellation signal
    pub current: Option<(
        BlockHash,
        BlockPriorityMeta,
        Arc<std::sync::atomic::AtomicU8>,
        JoinHandle<(VdfValidationResult, BlockValidationTask)>,
    )>,

    /// Pending VDF tasks
    pub pending: PriorityQueue<BlockHash, (BlockPriorityMeta, BlockValidationTask)>,
}

impl VdfScheduler {
    pub(super) fn new() -> Self {
        Self {
            current: None,
            pending: PriorityQueue::new(),
        }
    }

    /// Submit a VDF task
    #[instrument(skip_all, fields(block_hash = %task.block.block_hash, ?priority))]
    pub(super) fn submit(&mut self, task: BlockValidationTask, priority: BlockPriorityMeta) {
        let hash = task.block.block_hash;

        // Check for duplicates
        if self.pending.get(&hash).is_some() {
            debug!("VDF task for {} already pending", hash);
            return;
        }

        // Check if current task exists
        if let Some((current_hash, _, _, _)) = &self.current {
            if *current_hash == hash {
                debug!("VDF task for {} already running", hash);
                return;
            }
        }

        self.pending.push(hash, (priority, task));

        // Check if we should preempt current task
        if let Some((current_hash, current_priority, cancel_u8, _)) = &self.current {
            if let Some((_, (new_priority, _))) = self.pending.peek() {
                // Only preempt if new task has HIGHER priority
                if new_priority > current_priority {
                    info!(
                        "Preempting VDF task {} (priority {:?}) for higher priority {:?}",
                        current_hash, current_priority, new_priority
                    );
                    cancel_u8.store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
                }
            }
        }
    }

    /// Start next VDF task if none running
    #[instrument(skip_all)]
    pub(super) fn start_next(&mut self) -> Option<()> {
        if self.current.is_some() {
            return None; // Already running
        }

        let (hash, (priority, task)) = self.pending.pop()?;

        // Create AtomicU8 for cancellation
        let cancel_u8 = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));

        let preemptible = PreemptibleVdfTask {
            task,
            cancel_u8: Arc::clone(&cancel_u8),
        };

        let handle = tokio::spawn(
            preemptible
                .execute()
                .instrument(tracing::info_span!("vdf_validation", block_hash = %hash, ?priority))
                .in_current_span(),
        );

        self.current = Some((hash, priority, cancel_u8, handle));

        debug!(
            "Started VDF validation for {} with priority {:?}",
            hash, priority
        );
        Some(())
    }

    /// Poll current VDF task
    #[instrument(skip_all)]
    pub(super) async fn poll_current(
        &mut self,
    ) -> Option<(BlockHash, VdfValidationResult, BlockValidationTask)> {
        let (hash, priority, cancel_u8, mut handle) = self.current.take()?;

        // Use poll_immediate to check without blocking
        let poll_result = futures::future::poll_immediate(&mut handle).await;

        match poll_result {
            Some(Ok((result, task))) => {
                // Task completed
                Some((hash, result, task))
            }
            Some(Err(e)) => {
                error!("VDF task panicked: {}", e);
                // We lost the task on panic, cannot continue
                None
            }
            None => {
                // Still running, put it back
                self.current = Some((hash, priority, cancel_u8, handle));
                None
            }
        }
    }
}

/// Main validation coordinator
pub(super) struct ValidationCoordinator {
    /// VDF validation scheduler
    pub vdf_scheduler: VdfScheduler,

    /// Concurrent validation pool
    pub concurrent_pool: ConcurrentValidationPool,

    /// Block tree for priority calculation
    pub block_tree_guard: BlockTreeReadGuard,

    /// VDF task completion notifier
    vdf_notify: Arc<Notify>,
}

impl ValidationCoordinator {
    pub(super) fn new(block_tree_guard: BlockTreeReadGuard, vdf_notify: Arc<Notify>) -> Self {
        Self {
            vdf_scheduler: VdfScheduler::new(),
            concurrent_pool: ConcurrentValidationPool::new(),
            block_tree_guard,
            vdf_notify,
        }
    }

    /// Calculate priority for a block
    #[instrument(skip_all, fields(block_hash = %block.block_hash, block_height = %block.height))]
    pub(super) fn calculate_priority(&self, block: &Arc<IrysBlockHeader>) -> BlockPriorityMeta {
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
    #[instrument(skip_all, fields(%block_hash))]
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTree) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash;

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
    #[instrument(skip_all, fields(block_hash = %task.block.block_hash, block_height = %task.block.height))]
    pub(super) fn submit_task(&mut self, task: BlockValidationTask) {
        let priority = self.calculate_priority(&task.block);
        self.vdf_scheduler.submit(task, priority);

        // Notify to start processing if VDF is idle
        if self.vdf_scheduler.current.is_none() && !self.vdf_scheduler.pending.is_empty() {
            self.vdf_notify.notify_one();
        }
    }

    /// Process VDF completion
    #[instrument(skip_all)]
    pub(super) async fn process_vdf(&mut self) -> Option<(BlockHash, VdfValidationResult)> {
        // Poll current VDF task
        if let Some((hash, result, task)) = self.vdf_scheduler.poll_current().await {
            debug!(?hash, ?result, "VDF completed");

            if matches!(result, VdfValidationResult::Valid) {
                let priority = self.calculate_priority(&task.block);
                self.concurrent_pool.submit(task, priority);
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
    #[instrument(skip_all)]
    pub(super) fn reevaluate_priorities(&mut self) {
        info!("Reevaluating priorities after reorg");

        // Reevaluate concurrent pool
        let block_tree_guard = &self.block_tree_guard;
        self.concurrent_pool.reevaluate_priorities(|hash| {
            let block_tree = block_tree_guard.read();
            block_tree.get_block(hash).map(|block| {
                let state = match block_tree.get_block_and_status(hash) {
                    Some((_, ChainState::Onchain)) => BlockPriority::Canonical,
                    Some((_, _)) => {
                        // Simplified check for canonical extension
                        let (canonical_chain, _) = block_tree.get_canonical_chain();
                        let canonical_tip = canonical_chain.last().unwrap().block_hash;
                        if block.previous_block_hash == canonical_tip {
                            BlockPriority::CanonicalExtension
                        } else {
                            BlockPriority::Fork
                        }
                    }
                    None => BlockPriority::Unknown,
                };
                BlockPriorityMeta::new(block, state)
            })
        });

        // Reevaluate VDF pending
        let old_pending = std::mem::take(&mut self.vdf_scheduler.pending);
        for (hash, (_old_priority, task)) in old_pending {
            let new_priority = self.calculate_priority(&task.block);
            self.vdf_scheduler.pending.push(hash, (new_priority, task));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::{
        dummy_ema_snapshot, dummy_epoch_snapshot, BlockState, BlockTree, BlockTreeReadGuard,
        ChainState, CommitmentSnapshot,
    };
    use irys_types::{IrysBlockHeader, H256};
    use std::sync::{Arc, RwLock};

    /// Test that BlockPriorityMeta ordering works correctly with manual Ord
    #[test]
    fn test_validation_priority_ordering() {
        let mut header1 = IrysBlockHeader::new_mock_header();
        header1.height = 100;

        let mut header2 = IrysBlockHeader::new_mock_header();
        header2.height = 200;

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
    }

    /// Helper function to setup a canonical chain scenario with n blocks  
    fn setup_canonical_chain_scenario(
        max_height: u64,
    ) -> (BlockTreeReadGuard, Vec<Arc<IrysBlockHeader>>) {
        // Create genesis block
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.block_hash = H256::random();
        genesis.cumulative_diff = 0.into();

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
            header.block_hash = H256::random();
            header.cumulative_diff = height.into();

            block_tree
                .add_common(
                    header.block_hash,
                    &header,
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
            let fork_parent = canonical_chain.iter().find(|e| e.height == 2).unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = fork_parent.block_hash;

            // Create an alternative block at height 3 (competing with canonical block at height 3)
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
